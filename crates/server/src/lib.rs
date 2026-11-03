//! Central ingestion server for activity-daemon, deployed on the WSL hub
//! (ADR revisions 3/5/6). Skeleton endpoints; device identity/auth via
//! Tailscale is TODO (#18): until then this must only listen on the
//! tailnet, never on an untrusted network.
//!
//! Endpoints:
//! - `GET  /health` - liveness probe.
//! - `POST /ingest/events` - append-only event ingestion; device comes from
//!   the request body. Rows are marked synced on arrival (the central store
//!   is the sync target).
//! - `PUT  /ingest/media/{kind}/{date}/{filename}` - store a media file
//!   (date = `YYYY-MM-DD`) under `media/<kind>/<YYYY>/<MM>/<DD>/`, file
//!   first, index row after (ADR rev 5). TODO: sha256 verify.
//!
//! Append-only means no conflict handling: rows are keyed by device +
//! timestamp at query time, inserts never update.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use activity_core::{CoreError, Db, EventEnvelope};
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Shared server state.
#[derive(Clone)]
pub struct AppState {
    db: Arc<Mutex<Db>>,
    media_dir: PathBuf,
}

impl AppState {
    /// Open the central database and point media storage at a directory.
    pub fn open(db_path: &Path, media_dir: PathBuf) -> Result<AppState, CoreError> {
        Ok(AppState {
            db: Arc::new(Mutex::new(Db::open_db(db_path)?)),
            media_dir,
        })
    }
}

/// Build the application router (exposed for tests and embedding).
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ingest/events", post(ingest_events))
        .route("/ingest/media/{kind}/{date}/{filename}", put(ingest_media))
        .with_state(state)
}

fn internal_error<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("storage error: {e}"),
    )
}

fn bad_request(msg: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

#[derive(Debug, Deserialize)]
pub struct IngestBody {
    pub device: String,
    pub events: Vec<EventEnvelope>,
}

#[derive(Debug, Serialize)]
pub struct IngestResponse {
    pub inserted: usize,
}

async fn ingest_events(
    State(state): State<AppState>,
    Json(body): Json<IngestBody>,
) -> Result<Json<IngestResponse>, (StatusCode, String)> {
    // TODO(#18): derive device identity from the Tailscale source address
    // (tailscale whois) instead of trusting the request body.
    if body.device.is_empty() {
        return Err(bad_request("device must not be empty"));
    }
    // TODO: move blocking SQLite work to spawn_blocking / a small pool.
    let db = state.db.lock().map_err(internal_error)?;
    let mut ids = Vec::with_capacity(body.events.len());
    for event in &body.events {
        let id = db
            .insert_event(event, &body.device)
            .map_err(internal_error)?;
        ids.push(id);
    }
    // Rows arriving over the wire are, by definition, synced on the hub.
    db.mark_synced(&ids).map_err(internal_error)?;
    Ok(Json(IngestResponse {
        inserted: ids.len(),
    }))
}

async fn ingest_media(
    State(state): State<AppState>,
    AxumPath((kind, date, filename)): AxumPath<(String, String, String)>,
    _headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !valid_component(&kind) {
        return Err(bad_request("invalid media kind"));
    }
    let (year, month, day) =
        valid_date(&date).ok_or_else(|| bad_request("date must be YYYY-MM-DD"))?;
    if !valid_component(&filename) {
        return Err(bad_request("invalid filename"));
    }
    // TODO(#18/#10): verify sha256 (x-content-sha256 header) and store the
    // media index row in the `media` bucket AFTER the file is durable
    // (file first, index row after - ADR rev 5). Large bodies should be
    // streamed to disk instead of buffered.
    let dir = state.media_dir.join(&kind).join(year).join(month).join(day);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(internal_error)?;
    let dest = dir.join(&filename);
    tokio::fs::write(&dest, &body)
        .await
        .map_err(internal_error)?;
    let rel = dest
        .strip_prefix(&state.media_dir)
        .unwrap_or(&dest)
        .display()
        .to_string();
    Ok(Json(json!({ "stored": rel, "bytes": body.len() })))
}

/// Single path component: non-empty ASCII alphanumerics plus `.` `-` `_`,
/// never `.` or `..`.
fn valid_component(s: &str) -> bool {
    if s.is_empty() || s == "." || s == ".." {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// `YYYY-MM-DD` -> `(YYYY, MM, DD)`, all numeric.
fn valid_date(date: &str) -> Option<(&str, &str, &str)> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    let numeric = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if y.len() == 4 && m.len() == 2 && d.len() == 2 && numeric(y) && numeric(m) && numeric(d) {
        Some((y, m, d))
    } else {
        None
    }
}
