//! Integration tests for the read-only web UI (`/`, `/assets/*`,
//! `/api/machines`, `/api/calendar`), following the `ingest.rs` oneshot
//! pattern. Day-related assertions mirror the server's local-timezone
//! bucketing; seeded timestamps are mid-day +08:00 instants that land on the
//! same calendar date in every timezone between UTC and +14 (CI runs UTC,
//! the owner runs +08). The `/api/machines` summary seeds are anchored to
//! local midnight (`minutes_into_today`) instead of wall-clock now, so
//! today-boundary assertions hold at every run time (#54).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Local, SecondsFormat, TimeZone, Utc};
use serde_json::{json, Value};
use server::{build_router, AppState, AuthConfig};
use tower::ServiceExt;

/// Self-cleaning temp directory (children of the test state live in here).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("activity-server-web-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_state(tag: &str) -> (TempDir, AppState) {
    let dir = TempDir::new(tag);
    let state = AppState::open_with_auth(
        &dir.0.join("activity.db"),
        dir.0.join("media"),
        AuthConfig::disabled(),
    )
    .expect("test app state");
    (dir, state)
}

async fn body_json(res: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json body")
}

async fn body_text(res: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

fn post_json(uri: &str, value: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .expect("request")
}

/// Server-local midnight of today. Same DST-tolerant interpretation as the
/// summary's `map_local` in web.rs: take the earliest on a fall-back, and
/// fall back to a UTC reading inside a spring-forward gap.
fn local_midnight() -> DateTime<Local> {
    let midnight = Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 is valid");
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .unwrap_or_else(|| Utc.from_utc_datetime(&midnight).with_timezone(&Local))
}

/// RFC3339 timestamp `minutes` after local midnight today, so every seed is
/// unambiguously "today" at any wall-clock run time - unlike now-relative
/// seeds, which sit on yesterday during the first minutes of the local day.
fn minutes_into_today(minutes: i64) -> String {
    (local_midnight() + Duration::minutes(minutes)).to_rfc3339_opts(SecondsFormat::Secs, false)
}

async fn seed(state: &AppState, device: &str, events: Value) {
    let app = build_router(state.clone());
    let res = app
        .oneshot(post_json(
            "/ingest/events",
            &json!({ "device": device, "events": events }),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK, "seed ingest must be accepted");
}

fn window_event(ts: &str, duration_s: f64, app: &str, title: &str) -> Value {
    json!({
        "bucket": "window-watcher",
        "timestamp": ts,
        "duration_s": duration_s,
        "data": { "app": app, "title": title }
    })
}

fn afk_span_event(ts: &str, duration_s: f64) -> Value {
    json!({
        "bucket": "afk-watcher",
        "timestamp": ts,
        "duration_s": duration_s,
        "data": { "state": "active" }
    })
}

fn afk_marker_event(ts: &str) -> Value {
    json!({
        "bucket": "afk-watcher",
        "timestamp": ts,
        "duration_s": 0.0,
        "data": { "state": "afk" }
    })
}

fn media_event(ts: &str) -> Value {
    json!({
        "bucket": "media",
        "timestamp": ts,
        "duration_s": 0.0,
        "data": { "kind": "screen-frame", "bytes": 48213, "path": "media/screen/x.jpg" }
    })
}

/// An owner-approved timesheet activity (#47's writer shape: the draft JSON
/// plus `approved_at`), seeded through the normal ingest path - the same
/// rows `GET /api/calendar` must surface as coloured blocks.
fn approved_event(ts: &str, end: &str, project: &str, category: &str) -> Value {
    json!({
        "bucket": "timesheet-approved",
        "timestamp": ts,
        "duration_s": 3600.0,
        "data": {
            "start": ts,
            "end": end,
            "project_guess": project,
            "category": category,
            "task": "client work",
            "comment": "",
            "confidence": 0.87,
            "sources": { "frame_sha16s": [], "session_ids": [1], "question_ids": [] },
            "approved_at": "2026-03-02T18:00:00+08:00"
        }
    })
}

/// A daily-compiler draft (#45 shape, no `approved_at`). The calendar must
/// never surface these: drafts are reviewed data, the calendar shows the
/// approved result.
fn draft_event(ts: &str, end: &str) -> Value {
    json!({
        "bucket": "timesheet-draft",
        "timestamp": ts,
        "duration_s": 3600.0,
        "data": {
            "start": ts,
            "end": end,
            "project_guess": "DRAFT-ONLY PROJECT",
            "category": "Support",
            "task": "draft work",
            "comment": "",
            "confidence": 0.9,
            "sources": { "frame_sha16s": [], "session_ids": [2], "question_ids": [] }
        }
    })
}

// ------------------------------------------------------------------ assets

#[tokio::test]
async fn ui_index_and_assets_are_served() {
    let (dir, state) = test_state("ui-index");
    let app = build_router(state);

    let res = app.clone().oneshot(get("/")).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_text(res).await;
    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "index must be the UI shell"
    );
    assert!(html.contains("activity-daemon"), "index names the project");
    assert!(
        html.contains("/assets/app.js"),
        "index loads the app script"
    );

    for (path, mime_part) in [
        ("/assets/app.css", "text/css"),
        ("/assets/app.js", "javascript"),
    ] {
        let res = app.clone().oneshot(get(path)).await.expect("oneshot");
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .expect("content-type")
            .to_string();
        assert!(content_type.contains(mime_part), "{path} -> {content_type}");
    }

    let res = app.oneshot(get("/assets/nope.js")).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::NOT_FOUND, "unknown asset");
    drop(dir);
}

#[tokio::test]
async fn timesheet_view_is_served_in_the_ui_shell() {
    let (dir, state) = test_state("ui-timesheet");
    let app = build_router(state);

    // The shell carries the nav entry, the view section and its panels.
    let res = app.clone().oneshot(get("/")).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let html = body_text(res).await;
    assert!(
        html.contains(r#"id="rail-ts""#),
        "nav entry for the timesheet view"
    );
    assert!(html.contains(r#"id="view-ts""#), "timesheet view section");
    assert!(html.contains(r#"id="ts-questions""#), "questions panel");
    assert!(html.contains(r#"id="ts-style""#), "style panel");
    assert!(
        html.contains(r#"id="ts-export-month""#),
        "export month picker"
    );

    // The script wires every timesheet endpoint the page needs (issue #47),
    // including the style contract of issue #53 (raw prompt in, restart
    // hint out — the worker only re-reads style on hub restart).
    let res = app.oneshot(get("/assets/app.js")).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let js = body_text(res).await;
    for needle in [
        "/api/timesheet/days",
        "/api/timesheet/draft?",
        "/api/timesheet/draft/",
        "/api/timesheet/approve",
        "/api/timesheet/vocab",
        "/api/timesheet/frame/",
        "/api/timesheet/style",
        "/api/timesheet/questions?status=open",
        "/api/timesheet/export?month=",
        "hub restart",
    ] {
        assert!(js.contains(needle), "app.js wires {needle}");
    }
    drop(dir);
}

// -------------------------------------------------------------- /api/machines

#[tokio::test]
async fn machines_empty_db_is_zero_state() {
    let (dir, state) = test_state("machines-empty");
    let app = build_router(state);
    let res = app.oneshot(get("/api/machines")).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["hub"]["events_total"], 0);
    assert_eq!(v["hub"]["version"], server::VERSION);
    let uptime = v["hub"]["uptime_s"].as_u64().expect("uptime_s");
    assert!(uptime < 3600, "fresh hub uptime, got {uptime}");
    assert!(v["hub"]["media_bytes"].is_object());
    assert_eq!(v["devices"].as_array().expect("devices").len(), 0);
    assert!(v["server_time"].as_str().is_some());
    drop(dir);
}

#[tokio::test]
async fn machines_reports_per_device_summary_and_feed() {
    let (dir, state) = test_state("machines-summary");
    let app = build_router(state.clone());

    // midnight-anchored seeds: window sessions (counted as active time), an
    // AFK span (event, but not active time), a media index row. Batch order
    // is the arrival order the feed assertions below rely on.
    seed(
        &state,
        "dev-laptop",
        json!([
            window_event(
                &minutes_into_today(35),
                600.0,
                "Code.exe",
                "main.rs - activity-daemon"
            ),
            window_event(&minutes_into_today(5), 300.0, "chrome.exe", "MS Graph docs"),
            afk_span_event(&minutes_into_today(120), 2820.0),
            media_event(&minutes_into_today(10)),
        ]),
    )
    .await;
    // a second, long-silent device
    seed(
        &state,
        "dev-lab",
        json!([window_event(
            "2026-01-05T09:00:00+08:00",
            60.0,
            "notepad.exe",
            "todo.txt"
        )]),
    )
    .await;

    let res = app.oneshot(get("/api/machines")).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["hub"]["events_total"], 5);

    let devices = v["devices"].as_array().expect("devices");
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0]["device"], "dev-lab"); // ordered by device name
    assert_eq!(devices[1]["device"], "dev-laptop");

    let laptop = &devices[1];
    // 10 min (Code.exe) + 5 min (chrome.exe) of window sessions, all seeded
    // after today's local midnight - at any wall-clock run time
    let active = laptop["active_today_s"].as_f64().expect("active_today_s");
    assert!(
        (active - 900.0).abs() < 1.0,
        "active today 900, got {active}"
    );
    // AFK spans are events but never active time
    assert_eq!(laptop["events_today"], 4);
    assert_eq!(laptop["events_total"], 4);
    assert_eq!(laptop["foreground"]["app"], "Code.exe");
    assert_eq!(laptop["foreground"]["title"], "main.rs - activity-daemon");
    assert_eq!(laptop["buckets"]["window-watcher"], 2);
    assert_eq!(laptop["buckets"]["afk-watcher"], 1);
    assert_eq!(laptop["buckets"]["media"], 1);
    let recent = laptop["recent"].as_array().expect("recent");
    assert_eq!(recent.len(), 4);
    // newest ARRIVAL first: the media row was inserted last in the batch
    assert_eq!(recent[0]["bucket"], "media");
    assert_eq!(recent[1]["bucket"], "afk-watcher");
    assert_eq!(recent[2]["bucket"], "window-watcher");
    assert_eq!(recent[3]["bucket"], "window-watcher");
    assert_eq!(recent[2]["data"]["app"], "chrome.exe");
    assert_eq!(recent[3]["data"]["app"], "Code.exe");
    // last arrival = the media row seeded 10 minutes into today; the server
    // clamps the age at 0 while that instant is still ahead of the clock
    // (runs inside the first ten minutes of the local day)
    let age = laptop["last_seen_age_s"].as_f64().expect("age");
    let expected_age = (Local::now() - (local_midnight() + Duration::minutes(10)))
        .num_seconds()
        .max(0) as f64;
    assert!(
        (age - expected_age).abs() < 60.0,
        "last seen ~{expected_age} s past its seed, got {age}"
    );
    // the silent device has no today activity
    assert_eq!(devices[0]["events_today"], 0);
    assert_eq!(devices[0]["active_today_s"], 0.0);
    assert!(devices[0]["foreground"].is_null());
    drop(dir);
}

// -------------------------------------------------------------- /api/calendar

#[tokio::test]
async fn calendar_groups_sessions_and_afk_by_day() {
    let (dir, state) = test_state("calendar-days");
    let app = build_router(state.clone());

    seed(
        &state,
        "dev-laptop",
        json!([
            window_event("2026-03-02T14:00:00+08:00", 3600.0, "Code.exe", "web UI"),
            window_event("2026-03-03T10:00:00+08:00", 1800.0, "chrome.exe", "docs"),
            afk_marker_event("2026-03-02T12:00:00+08:00"),
            afk_span_event("2026-03-02T12:00:00+08:00", 2820.0),
        ]),
    )
    .await;
    // another device's event must not leak into the explicit-device query
    seed(
        &state,
        "dev-lab",
        json!([window_event(
            "2026-03-02T15:00:00+08:00",
            60.0,
            "notepad.exe",
            "x"
        )]),
    )
    .await;

    let res = app
        .oneshot(get(
            "/api/calendar?from=2026-03-02&to=2026-03-03&device=dev-laptop",
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["device"], "dev-laptop");
    let days = v["days"].as_array().expect("days");
    assert_eq!(days.len(), 2, "empty days stay in the response");

    assert_eq!(days[0]["date"], "2026-03-02");
    let sessions = days[0]["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["app"], "Code.exe");
    assert_eq!(days[0]["active_s"], 3600.0);
    let afk = days[0]["afk"].as_array().expect("afk");
    assert_eq!(afk.len(), 1, "the duration-0 'afk' marker is skipped");
    assert_eq!(afk[0]["duration_s"], 2820.0);
    // additive field (#48): present and empty while no approved rows exist
    assert_eq!(
        days[0]["approved"].as_array().expect("approved").len(),
        0,
        "absent bucket = empty approved array, never an error"
    );

    assert_eq!(days[1]["date"], "2026-03-03");
    assert_eq!(days[1]["sessions"].as_array().expect("sessions").len(), 1);
    assert_eq!(days[1]["afk"].as_array().expect("afk").len(), 0);
    assert_eq!(days[1]["active_s"], 1800.0);
    drop(dir);
}

#[tokio::test]
async fn calendar_surfaces_approved_activities_and_never_drafts() {
    let (dir, state) = test_state("calendar-approved");
    let app = build_router(state.clone());

    seed(
        &state,
        "dev-laptop",
        json!([
            // the raw backbone the grey-uncovered rendering derives from
            window_event("2026-03-02T14:00:00+08:00", 7200.0, "Code.exe", "web UI"),
            // two approved blocks on 2026-03-02, one on another device that
            // must not leak into the explicit-device query
            approved_event(
                "2026-03-02T14:00:00+08:00",
                "2026-03-02T15:00:00+08:00",
                "A1234 Client",
                "Consulting"
            ),
            approved_event(
                "2026-03-02T16:00:00+08:00",
                "2026-03-02T17:30:00+08:00",
                "C000P1 Internal",
                "Management"
            ),
            // drafts exist alongside - and must stay invisible
            draft_event("2026-03-02T18:00:00+08:00", "2026-03-02T19:00:00+08:00"),
            afk_span_event("2026-03-02T20:00:00+08:00", 1800.0),
        ]),
    )
    .await;
    seed(
        &state,
        "dev-lab",
        json!([approved_event(
            "2026-03-02T09:00:00+08:00",
            "2026-03-02T10:00:00+08:00",
            "Other Device",
            "QA"
        )]),
    )
    .await;

    let res = app
        .oneshot(get(
            "/api/calendar?from=2026-03-02&to=2026-03-02&device=dev-laptop",
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;

    let day = &v["days"][0];
    let approved = day["approved"].as_array().expect("approved array");
    assert_eq!(approved.len(), 2, "only this device's approved rows");
    assert_eq!(approved[0]["start"], "2026-03-02T14:00:00+08:00");
    assert_eq!(approved[0]["end"], "2026-03-02T15:00:00+08:00");
    assert_eq!(approved[0]["project"], "A1234 Client");
    assert_eq!(approved[0]["category"], "Consulting");
    assert_eq!(approved[0]["task"], "client work");
    assert_eq!(approved[0]["confidence"], 0.87);
    assert_eq!(approved[1]["category"], "Management");

    // existing fields stay byte-identical (backwards compatibility)
    assert_eq!(day["sessions"].as_array().expect("sessions").len(), 1);
    assert_eq!(day["afk"].as_array().expect("afk").len(), 1);
    assert_eq!(day["active_s"], 7200.0);

    // drafts NEVER appear on the calendar (#48 acceptance): the payload is
    // scanned whole so even an accidental extra field would fail this
    let raw = serde_json::to_string(&v).expect("serialize");
    assert!(
        !raw.contains("DRAFT-ONLY PROJECT"),
        "draft activities must never reach the calendar payload"
    );
    drop(dir);
}

#[tokio::test]
async fn calendar_defaults_to_the_busiest_device() {
    let (dir, state) = test_state("calendar-default-device");
    let app = build_router(state.clone());
    seed(
        &state,
        "dev-a",
        json!([
            window_event("2026-03-02T14:00:00+08:00", 60.0, "a1", "t"),
            window_event("2026-03-02T15:00:00+08:00", 60.0, "a2", "t"),
            window_event("2026-03-02T16:00:00+08:00", 60.0, "a3", "t"),
        ]),
    )
    .await;
    seed(
        &state,
        "dev-b",
        json!([window_event("2026-03-02T17:00:00+08:00", 60.0, "b1", "t")]),
    )
    .await;

    let res = app
        .oneshot(get("/api/calendar?from=2026-03-02&to=2026-03-02"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["device"], "dev-a");
    assert_eq!(
        v["days"][0]["sessions"].as_array().expect("sessions").len(),
        3
    );
    drop(dir);
}

#[tokio::test]
async fn calendar_empty_range_is_an_empty_calendar_not_an_error() {
    let (dir, state) = test_state("calendar-empty");
    let app = build_router(state);
    let res = app
        .oneshot(get("/api/calendar?from=2026-03-02&to=2026-03-02"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert!(v["device"].is_null());
    assert_eq!(v["days"][0]["active_s"], 0.0);
    // #48: the approved field is part of every day skeleton, even on an
    // empty hub with no timesheet bucket at all
    assert_eq!(v["days"][0]["approved"], json!([]));
    drop(dir);
}

#[tokio::test]
async fn calendar_validates_the_query() {
    let (dir, state) = test_state("calendar-validate");
    let app = build_router(state);
    for uri in [
        "/api/calendar?from=nope&to=2026-03-02",
        "/api/calendar?from=2026-03-02&to=garbage",
        "/api/calendar?from=2026-03-03&to=2026-03-02",
        "/api/calendar?from=2026-01-01&to=2026-04-01", // 91 days > 62
        "/api/calendar?from=2026-03-02",
        "/api/calendar",
    ] {
        let res = app.clone().oneshot(get(uri)).await.expect("oneshot");
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{uri}");
    }
    drop(dir);
}
