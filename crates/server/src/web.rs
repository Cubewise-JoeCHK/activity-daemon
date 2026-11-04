//! Read-only web UI for the hub: static assets (embedded at compile time,
//! nothing is fetched from outside the tailnet) plus two JSON GET endpoints
//! backing it:
//!
//! - `GET /` and `GET /assets/{file}` - the Graphite UI (Teams-style mono
//!   light theme, see the design contest under `design-contest/` in the
//!   workspace root).
//! - `GET /api/machines` - per-device summaries for the machine status view:
//!   last arrival, active time today, foreground app, per-bucket counts and
//!   the 30 most recent events per device; plus hub-level facts (version,
//!   uptime, stored media bytes per kind).
//! - `GET /api/calendar?from=YYYY-MM-DD&to=YYYY-MM-DD[&device=...]` -
//!   window sessions and AFK spans grouped by day for the behaviour
//!   calendar. Without `device` the device with the most window events in
//!   the range wins (single-owner hub: usually every day has exactly one).
//!
//! Day boundaries ("today", calendar day bucketing) use the SERVER's local
//! timezone. A single owner's devices share that timezone, and window
//! events already carry the local offset, so this matches the owner's day.
//! `/api` reads follow the `/health` precedent: open under tailnet-trust,
//! never proxied outside the tailnet. The `/ingest` write paths keep their
//! token guard.
//!
//! All storage work runs on `spawn_blocking`, exactly like `/ingest/events`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use activity_core::{CoreError, Db, BUCKET_AFK, BUCKET_WINDOW};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use chrono::{
    DateTime, Duration as ChronoDuration, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone,
    Utc,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{bad_request, internal_error, json_response, AppState, VERSION};

/// Compile-time embedded assets: no CDN, no external fetch, the container
/// image is self-contained.
const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");

/// Maximum calendar range in days (a couple of months is plenty for a
/// behaviour overview; keeps queries bounded).
const CALENDAR_MAX_DAYS: i64 = 62;
/// Foreground lookback, mirroring the client's `status` command convention.
const FOREGROUND_LOOKBACK_DAYS: i64 = 30;
/// Events per device returned in the machine feed.
const FEED_LIMIT: u32 = 30;

/// Routes for the web UI and its read APIs (merged into `build_router`).
pub fn web_router() -> Router<AppState> {
    Router::new()
        .route("/", get(ui_index))
        .route("/assets/{file}", get(ui_asset))
        .route("/api/machines", get(api_machines))
        .route("/api/calendar", get(api_calendar))
}

// ------------------------------------------------------------------ assets

async fn ui_index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn ui_asset(AxumPath(file): AxumPath<String>) -> Response {
    match file.as_str() {
        "app.css" => ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS).into_response(),
        "app.js" => (
            [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
            APP_JS,
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

// ------------------------------------------------------------ GET /api/machines

async fn api_machines(State(state): State<AppState>) -> Response {
    let db = state.db.clone();
    let media_dir = state.media_dir.clone();
    let started_at = state.started_at;
    let outcome =
        tokio::task::spawn_blocking(move || machines_payload(&db, &media_dir, started_at)).await;
    match outcome {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("machines task failed: {e}")),
    }
}

fn machines_payload(
    db: &Arc<Mutex<Db>>,
    media_dir: &Path,
    started_at: Instant,
) -> Result<Value, CoreError> {
    let now = Local::now().fixed_offset();
    let midnight = local_midnight_rfc3339(&now);
    let lookback = (now - ChronoDuration::days(FOREGROUND_LOOKBACK_DAYS)).to_rfc3339();

    let db = db
        .lock()
        .map_err(|_| CoreError::Config("database lock poisoned".to_string()))?;

    let mut devices = Vec::new();
    for overview in db.device_overview()? {
        let device = overview.device.clone();
        let last_seen_age_s = overview
            .last_seen
            .as_deref()
            .map(parse_instant)
            .transpose()?
            .map(|seen| (now - seen).num_seconds().max(0) as f64);

        // Window sessions since local midnight, summed like the client's
        // `status` command (AFK already excludes itself: the coalescer ends
        // the open session when the user goes idle). Explicit fold: `.sum()`
        // starts from -0.0 and an empty day would serialize as "-0.0".
        let active_today_s: f64 = db
            .query_events_in(Some(BUCKET_WINDOW), Some(&device), Some(&midnight), None)?
            .iter()
            .map(|e| e.duration_s)
            .fold(0.0, |acc, d| acc + d);

        let events_today = db.count_device_events_since(&device, &midnight)?;

        // Foreground: the newest window session within the lookback window.
        let foreground = db
            .query_events_in(Some(BUCKET_WINDOW), Some(&device), Some(&lookback), None)?
            .last()
            .map(|e| {
                json!({
                    "app": e.data.get("app").and_then(Value::as_str).unwrap_or(""),
                    "title": e.data.get("title").and_then(Value::as_str).unwrap_or(""),
                    "timestamp": e.timestamp,
                    "duration_s": e.duration_s,
                })
            });

        let mut buckets = serde_json::Map::new();
        for (dev, bucket, count) in db.counts_by_device_bucket()? {
            if dev == device {
                buckets.insert(bucket, json!(count));
            }
        }

        let mut recent = Vec::new();
        for event in db.latest_events(Some(&device), FEED_LIMIT)? {
            recent.push(json!({
                "bucket": event.bucket,
                "timestamp": event.timestamp,
                "duration_s": event.duration_s,
                "data": event.data,
            }));
        }

        devices.push(json!({
            "device": device,
            "last_seen": overview.last_seen,
            "last_seen_age_s": last_seen_age_s,
            "events_total": overview.events_total,
            "events_today": events_today,
            "active_today_s": active_today_s,
            "foreground": foreground,
            "buckets": Value::Object(buckets),
            "recent": recent,
        }));
    }

    Ok(json!({
        "server_time": now.to_rfc3339(),
        "hub": {
            "version": VERSION,
            "events_total": db.event_count()?,
            "uptime_s": started_at.elapsed().as_secs(),
            "media_bytes": media_bytes_by_kind(media_dir),
        },
        "devices": devices,
    }))
}

/// Stored bytes per media kind: one `dir_size` per child directory of the
/// media root (`<media_root>/<kind>/...`), same accounting as the client's
/// `status` command applies to its own budget view.
fn media_bytes_by_kind(media_dir: &Path) -> Value {
    let mut kinds = serde_json::Map::new();
    if let Ok(entries) = std::fs::read_dir(media_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) {
                    kinds.insert(name.to_string(), json!(dir_size(&path)));
                }
            }
        }
    }
    Value::Object(kinds)
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_size(&path);
            } else if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total += meta.len();
                }
            }
        }
    }
    total
}

// ------------------------------------------------------- GET /api/calendar

#[derive(Debug, Deserialize)]
pub struct CalendarQuery {
    /// `YYYY-MM-DD`, inclusive.
    from: String,
    /// `YYYY-MM-DD`, inclusive.
    to: String,
    /// Optional device filter; defaults to the device with the most window
    /// events in the range.
    device: Option<String>,
}

async fn api_calendar(
    State(state): State<AppState>,
    Query(query): Query<CalendarQuery>,
) -> Response {
    let from = match NaiveDate::parse_from_str(&query.from, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return bad_request("from must be YYYY-MM-DD"),
    };
    let to = match NaiveDate::parse_from_str(&query.to, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return bad_request("to must be YYYY-MM-DD"),
    };
    if from > to {
        return bad_request("from must not be after to");
    }
    let days = (to - from).num_days() + 1;
    if days > CALENDAR_MAX_DAYS {
        return bad_request(format!("range too long (max {CALENDAR_MAX_DAYS} days)"));
    }

    let db = state.db.clone();
    let device = query.device.clone();
    let outcome =
        tokio::task::spawn_blocking(move || calendar_payload(&db, from, to, device.as_deref()))
            .await;
    match outcome {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("calendar task failed: {e}")),
    }
}

fn calendar_payload(
    db: &Arc<Mutex<Db>>,
    from: NaiveDate,
    to: NaiveDate,
    device: Option<&str>,
) -> Result<Value, CoreError> {
    let (range_start, range_end) = local_day_bounds_rfc3339(from, to);

    let db = db
        .lock()
        .map_err(|_| CoreError::Config("database lock poisoned".to_string()))?;

    // One range query for window sessions across all devices: a coalesced
    // week/month of window sessions is small, and it gives the default
    // device pick for free.
    let windows = db.query_events_in(
        Some(BUCKET_WINDOW),
        None,
        Some(&range_start),
        Some(&range_end),
    )?;
    let device: Option<String> = match device {
        Some(device) => Some(device.to_string()),
        None => {
            // The device with the most window events in the range; ties go
            // to the alphabetically first device (BTreeMap for determinism).
            // An empty range is a valid empty calendar, not an error.
            let mut counts = std::collections::BTreeMap::new();
            for event in &windows {
                *counts.entry(event.device.clone()).or_insert(0i64) += 1;
            }
            counts
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(device, _)| device)
        }
    };

    let afks = match &device {
        Some(device) => db.query_events_in(
            Some(BUCKET_AFK),
            Some(device),
            Some(&range_start),
            Some(&range_end),
        )?,
        None => Vec::new(),
    };

    // Day skeletons for every date in the range (empty days included).
    let mut days: Vec<(NaiveDate, Value)> = Vec::new();
    let mut day = from;
    while day <= to {
        days.push((
            day,
            json!({
                "date": day.format("%Y-%m-%d").to_string(),
                "active_s": 0.0,
                "sessions": Vec::<Value>::new(),
                "afk": Vec::<Value>::new(),
            }),
        ));
        day += ChronoDuration::days(1);
    }

    let mut sessions_by_day: std::collections::HashMap<NaiveDate, Vec<Value>> =
        std::collections::HashMap::new();
    let mut active_by_day: std::collections::HashMap<NaiveDate, f64> =
        std::collections::HashMap::new();
    for event in windows
        .into_iter()
        .filter(|e| Some(&e.device) == device.as_ref())
    {
        let date = local_date_of(&event.timestamp)?;
        *active_by_day.entry(date).or_insert(0.0) += event.duration_s;
        sessions_by_day.entry(date).or_default().push(json!({
            "start": event.timestamp,
            "duration_s": event.duration_s,
            "app": event.data.get("app").and_then(Value::as_str).unwrap_or(""),
            "title": event.data.get("title").and_then(Value::as_str).unwrap_or(""),
        }));
    }

    // One AFK episode = the `state: "active"` event (its timestamp is the
    // episode start and its duration the episode length); the paired
    // `state: "afk"` marker (duration 0) is ignored here.
    let mut afk_by_day: std::collections::HashMap<NaiveDate, Vec<Value>> =
        std::collections::HashMap::new();
    for event in afks {
        if event.data.get("state").and_then(Value::as_str) != Some("active") {
            continue;
        }
        let date = local_date_of(&event.timestamp)?;
        afk_by_day.entry(date).or_default().push(json!({
            "start": event.timestamp,
            "duration_s": event.duration_s,
        }));
    }

    let days: Vec<Value> = days
        .into_iter()
        .map(|(date, mut day)| {
            if let Some(active) = active_by_day.get(&date) {
                day["active_s"] = json!(active);
            }
            if let Some(sessions) = sessions_by_day.get(&date) {
                day["sessions"] = json!(sessions);
            }
            if let Some(afk) = afk_by_day.get(&date) {
                day["afk"] = json!(afk);
            }
            day
        })
        .collect();

    Ok(json!({
        "device": device,
        "from": from.format("%Y-%m-%d").to_string(),
        "to": to.format("%Y-%m-%d").to_string(),
        "days": days,
    }))
}

// ----------------------------------------------------------------- time utils

fn parse_instant(ts: &str) -> Result<DateTime<FixedOffset>, CoreError> {
    DateTime::parse_from_rfc3339(ts)
        .map_err(|e| CoreError::Validation(format!("bad timestamp '{ts}': {e}")))
}

/// Server-local midnight for the given instant's date, as RFC3339.
fn local_midnight_rfc3339(now: &DateTime<FixedOffset>) -> String {
    let date = now.with_timezone(&Local).date_naive();
    local_day_bounds_rfc3339(date, date).0
}

/// RFC3339 bounds covering whole local days `from..=to` (inclusive).
fn local_day_bounds_rfc3339(from: NaiveDate, to: NaiveDate) -> (String, String) {
    let start = map_local(from.and_hms_opt(0, 0, 0).expect("00:00:00 is valid"));
    let end = map_local(to.and_hms_opt(23, 59, 59).expect("23:59:59 is valid"));
    (start.to_rfc3339(), end.to_rfc3339())
}

/// Interpret a naive local time in the server's timezone, tolerating DST
/// transitions (fall-back: take the earliest; spring-forward gap: fall back
/// to interpreting the wall clock as UTC).
fn map_local(naive: NaiveDateTime) -> DateTime<FixedOffset> {
    Local
        .from_local_datetime(&naive)
        .earliest()
        .unwrap_or_else(|| Utc.from_utc_datetime(&naive).with_timezone(&Local))
        .fixed_offset()
}

/// The server-local calendar date an event belongs to.
fn local_date_of(ts: &str) -> Result<NaiveDate, CoreError> {
    Ok(parse_instant(ts)?.with_timezone(&Local).date_naive())
}
