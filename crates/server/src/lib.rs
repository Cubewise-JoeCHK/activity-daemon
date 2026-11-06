//! Central ingestion server for activity-daemon, deployed on the WSL hub
//! (ADR revisions 3/5/6).
//!
//! Endpoints:
//! - `GET  /health` -
//!   `{"ok": true, "status": "ok", "version": "...", "events_count": n}`
//!   (the `version` field feeds the hub updater, issue #32).
//! - `POST /ingest/events` - body
//!   `{"device": "...", "events": [{bucket, timestamp, duration_s, data}]}`
//!   (RFC3339 timestamp with offset). Each event is validated through core's
//!   `EventEnvelope::validate` and inserted append-only, then marked synced
//!   (rows arriving over the wire are, by definition, synced on the hub).
//!   Response: `{"accepted": n, "rejected": [{"index", "reason"}]}` where
//!   `index` refers to positions in the posted `events` array. Status 200
//!   if anything was accepted; 400 if the whole batch was malformed
//!   (unparseable JSON, empty device, empty events, or every event
//!   rejected). Idempotency is the client's job: it marks rows synced after
//!   an accepted batch.
//! - `PUT  /ingest/media/{kind}/{date}/{filename}` - date is a single
//!   `YYYY-MM-DD` segment; the body is streamed to
//!   `<media_root>/<kind>/<YYYY>/<MM>/<DD>/<filename>`. Optional
//!   `X-SHA256` header (lowercase hex): when present the stored bytes are
//!   verified after the write and a mismatch deletes the file and returns
//!   400. The ack is durability-gated (sync contract v2, issue #40): the
//!   file is fsynced BEFORE the response and the ack echoes the stored
//!   bytes' checksum, `{"stored": "media/...", "bytes": n, "sha256":
//!   "<hex>"}` - the client treats this verified ack as deletion clearance
//!   for its local copy. The media index row is NOT written here - it
//!   arrives as an ordinary event via `POST /ingest/events` (file first,
//!   index row after, ADR rev 5).
//!
//! Auth modes (issue #18; no passwords anywhere):
//! 1. **tailscale-whois** - when the tailscaled socket
//!    (`/var/run/tailscale/tailscaled.sock`) is mounted at startup, the
//!    source IP of each ingest request is resolved to a device name via the
//!    tailscaled whois API and overrides the `device` in the request body.
//!    Identity and encryption are the tailnet's job.
//! 2. **tailnet-trust** (default) - no socket mounted: requests are trusted
//!    as coming from the tailnet; binding the port to an untrusted network
//!    is a deployment error (see docs/sync.md). A log line states the mode
//!    at startup.
//! 3. **token** - when `ACTIVITY_TOKEN` is set, every `/ingest` request
//!    must additionally carry a matching `X-Activity-Token` header (401
//!    otherwise). Composable with modes 1 and 2; `GET /health` stays open
//!    for probes.
//!
//! Storage: core `Db` at `ACTIVITY_DB` (default `./data/activity.db`), media
//! root at `ACTIVITY_DATA/media` (default `./data/media`).
//!
//! Web UI (read-only, `web` module): `GET /` serves the embedded Graphite
//! UI (machine status + behaviour calendar), backed by `GET /api/machines`
//! and `GET /api/calendar`. `/api` reads follow the `/health` precedent:
//! open, tailnet-only, no token (writes stay guarded).
//!
//! Retention (issue #38, `retention` module): finished months of raw media
//! are bundled into `bundles/<YYYY-MM>.tar.zst` once they age beyond the
//! hot window (`[retention]` in the server config file, disabled by
//! default); `GET /api/retention` reports the stats the client's
//! `status --json` surfaces.

pub mod retention;
pub mod web;
pub mod whois;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use activity_core::{CoreError, Db, EventEnvelope};
use axum::extract::{ConnectInfo, Extension, Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Digest;

use crate::whois::device_for_ip;

/// Path of the tailscaled local API socket inside the container/host.
pub const TAILSCALE_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

/// Server (workspace) version, reported by `/health` and `--version` so the
/// hub updater (issue #32) can compare the running release against the
/// latest GitHub release.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared server state.
#[derive(Clone)]
pub struct AppState {
    db: Arc<Mutex<Db>>,
    media_dir: PathBuf,
    /// Retention paths (`media/` root + `bundles/` output) and settings;
    /// issue #38.
    retention_store: retention::RetentionStore,
    retention: retention::RetentionConfig,
    auth: AuthConfig,
    /// When this state was constructed (= hub process start in production);
    /// surfaces as `hub.uptime_s` in the web UI's machine view.
    started_at: Instant,
}

/// Authentication configuration (see crate docs for the modes).
#[derive(Debug, Clone, PartialEq)]
pub struct AuthConfig {
    /// When set, `/ingest` requests must carry `X-Activity-Token`.
    pub token: Option<String>,
    /// When true, the tailscaled socket was detected and device identity is
    /// resolved from the peer IP via whois.
    pub whois: bool,
}

impl AuthConfig {
    /// Disabled auth: plain tailnet-trust (also the deterministic default
    /// for tests).
    pub fn disabled() -> AuthConfig {
        AuthConfig {
            token: None,
            whois: false,
        }
    }

    /// Detect the mode from the environment: `ACTIVITY_TOKEN` and the
    /// presence of the tailscaled socket.
    pub fn detect() -> AuthConfig {
        AuthConfig::from_env(
            std::env::var("ACTIVITY_TOKEN").ok(),
            Path::new(TAILSCALE_SOCKET).exists(),
        )
    }

    /// Pure form of [`AuthConfig::detect`].
    pub fn from_env(token: Option<String>, socket_present: bool) -> AuthConfig {
        AuthConfig {
            token,
            whois: socket_present,
        }
    }

    /// Human-readable mode for the startup log line.
    pub fn describe(&self) -> String {
        match (self.whois, self.token.is_some()) {
            (true, true) => "tailscale-whois + X-Activity-Token".to_string(),
            (true, false) => "tailscale-whois (peer IP resolved to device name)".to_string(),
            (false, true) => {
                "token (X-Activity-Token required; tailnet-trust otherwise)".to_string()
            }
            (false, false) => {
                "tailnet-trust (no tailscaled socket mounted; bind exposure is the tailnet's job)"
                    .to_string()
            }
        }
    }
}

impl AppState {
    /// Open the central database and point media storage at a directory,
    /// with auth disabled (tailnet-trust; used by tests and embedding).
    pub fn open(db_path: &Path, media_dir: PathBuf) -> Result<AppState, CoreError> {
        AppState::open_with_auth(db_path, media_dir, AuthConfig::disabled())
    }

    /// [`AppState::open`] with an explicit auth configuration.
    pub fn open_with_auth(
        db_path: &Path,
        media_dir: PathBuf,
        auth: AuthConfig,
    ) -> Result<AppState, CoreError> {
        AppState::open_with_retention(
            db_path,
            media_dir,
            auth,
            retention::RetentionConfig::default(),
        )
    }

    /// [`AppState::open_with_auth`] with explicit retention settings
    /// (issue #38); production passes the parsed server config.
    pub fn open_with_retention(
        db_path: &Path,
        media_dir: PathBuf,
        auth: AuthConfig,
        retention: retention::RetentionConfig,
    ) -> Result<AppState, CoreError> {
        Ok(AppState {
            db: Arc::new(Mutex::new(Db::open_db(db_path)?)),
            media_dir: media_dir.clone(),
            retention_store: retention::RetentionStore::from_media_dir(&media_dir),
            retention,
            auth,
            started_at: Instant::now(),
        })
    }
}

/// Build the application router (exposed for tests and embedding). Serve it
/// with `into_make_service_with_connect_info::<SocketAddr>()` so whois mode
/// can see the peer address.
pub fn build_router(state: AppState) -> Router {
    let guarded = Router::new()
        .route("/ingest/events", post(ingest_events))
        .route("/ingest/media/{kind}/{date}/{filename}", put(ingest_media))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            token_guard,
        ));
    Router::new()
        .route("/health", get(health))
        .merge(guarded)
        .merge(web::web_router())
        .with_state(state)
}

// ---------------------------------------------------------------- responses

fn bad_request(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

fn internal_error(msg: impl std::fmt::Display) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("storage error: {msg}"),
    )
        .into_response()
}

fn json_response<T: Serialize>(status: StatusCode, value: T) -> Response {
    (status, Json(value)).into_response()
}

// ------------------------------------------------------------------- health

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let events_count = {
        let db = state.db.lock().expect("db lock");
        db.event_count().unwrap_or(0)
    };
    Json(json!({
        "ok": true,
        "status": "ok",
        "version": VERSION,
        "events_count": events_count
    }))
}

// ------------------------------------------------------------- auth / device

/// Require `X-Activity-Token` on `/ingest` routes when a token is
/// configured. `GET /health` is not wrapped and stays open for probes.
async fn token_guard(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(expected) = &state.auth.token {
        let provided = request
            .headers()
            .get("X-Activity-Token")
            .and_then(|v| v.to_str().ok());
        if provided != Some(expected.as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    next.run(request).await
}

/// Device identity for an ingest request: in whois mode the peer IP wins
/// (resolved through tailscaled); otherwise (and on whois failure) the
/// request body's device is used.
fn resolve_device(auth: &AuthConfig, peer: Option<IpAddr>, body_device: &str) -> String {
    if auth.whois {
        if let Some(ip) = peer.filter(|ip| !ip.is_loopback() && !ip.is_unspecified()) {
            if let Some(name) = device_for_ip(ip) {
                return name;
            }
        }
    }
    body_device.to_string()
}

// ------------------------------------------------------------ POST /ingest/events

#[derive(Debug, Deserialize)]
pub struct IngestBody {
    pub device: String,
    pub events: Vec<EventEnvelope>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct RejectedEvent {
    pub index: usize,
    pub reason: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct IngestResponse {
    pub accepted: usize,
    pub rejected: Vec<RejectedEvent>,
}

async fn ingest_events(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(body): Json<IngestBody>,
) -> Response {
    if body.device.trim().is_empty() {
        return bad_request("device must not be empty");
    }
    if body.events.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            IngestResponse {
                accepted: 0,
                rejected: Vec::new(),
            },
        );
    }
    let peer_ip = peer.map(|Extension(ConnectInfo(addr))| addr.ip());
    let auth = state.auth.clone();
    let events = body.events;
    let db = state.db.clone();
    // Blocking SQLite work stays off the async runtime.
    let outcome = tokio::task::spawn_blocking(move || {
        let device = resolve_device(&auth, peer_ip, &body.device);
        ingest_batch(&db, &device, events)
    })
    .await;
    match outcome {
        Ok(Ok(response)) => {
            // 200 when anything was accepted, 400 when the whole batch was malformed.
            let status = if response.accepted > 0 {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            };
            json_response(status, response)
        }
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("ingest task failed: {e}")),
    }
}

/// Insert one batch append-only, collecting per-event validation failures.
/// Storage-level failures abort with an error (HTTP 500); validation
/// failures are per-event rejections.
fn ingest_batch(
    db: &Arc<Mutex<Db>>,
    device: &str,
    events: Vec<EventEnvelope>,
) -> Result<IngestResponse, CoreError> {
    let db = db
        .lock()
        .map_err(|_| CoreError::Config("database lock poisoned".to_string()))?;
    let mut accepted = 0usize;
    let mut rejected = Vec::new();
    let mut ids = Vec::with_capacity(events.len());
    for (index, event) in events.into_iter().enumerate() {
        match db.insert_event(&event, device) {
            Ok(id) => {
                ids.push(id);
                accepted += 1;
            }
            Err(CoreError::Validation(reason)) => rejected.push(RejectedEvent { index, reason }),
            Err(e) => return Err(e),
        }
    }
    // Rows arriving over the wire are, by definition, synced on the hub.
    db.mark_synced(&ids)?;
    Ok(IngestResponse { accepted, rejected })
}

// ------------------------------------------------------------ PUT /ingest/media/...

async fn ingest_media(
    State(state): State<AppState>,
    AxumPath((kind, date, filename)): AxumPath<(String, String, String)>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    if !valid_component(&kind) {
        return bad_request("invalid media kind");
    }
    let (year, month, day) = match valid_date(&date) {
        Some(ymd) => ymd,
        None => return bad_request("date must be YYYY-MM-DD"),
    };
    if !valid_component(&filename) {
        return bad_request("invalid filename");
    }
    let expected_sha = match headers.get("X-SHA256").map(|v| v.to_str()) {
        None => None,
        Some(Err(_)) => return bad_request("X-SHA256 header is not valid ASCII"),
        Some(Ok(raw)) => match normalize_sha256(raw) {
            Some(hex) => Some(hex),
            None => return bad_request("X-SHA256 must be 64 hex characters"),
        },
    };

    let dir = state
        .media_dir
        .join(&kind)
        .join(&year)
        .join(&month)
        .join(&day);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return internal_error(e);
    }
    let dest = dir.join(&filename);

    // Stream to disk (file-first durability: fsync before the index row can
    // ever arrive), hashing on the way.
    let mut file = match tokio::fs::File::create(&dest).await {
        Ok(f) => f,
        Err(e) => return internal_error(e),
    };
    let mut hasher = sha2::Sha256::new();
    let mut bytes: u64 = 0;
    let mut body_error: Option<String> = None;
    use tokio::io::AsyncWriteExt;
    {
        use http_body_util::BodyExt;
        let mut body = body;
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(frame) => {
                    if let Some(data) = frame.data_ref() {
                        hasher.update(data);
                        bytes += data.len() as u64;
                        if let Err(e) = file.write_all(data).await {
                            body_error = Some(format!("write failed: {e}"));
                            break;
                        }
                    }
                }
                Err(e) => {
                    body_error = Some(format!("body read failed: {e}"));
                    break;
                }
            }
        }
    }
    if let Some(msg) = body_error {
        // Never keep a partial or failed upload behind.
        let _ = tokio::fs::remove_file(&dest).await;
        return bad_request(msg);
    }
    if let Err(e) = file.sync_all().await {
        return internal_error(e);
    }
    drop(file);

    // The checksum of the durably stored bytes, computed while streaming.
    // Sync contract v2 (issue #40): the ack only leaves here - after the
    // fsync above - and echoes this value so the client can verify the
    // write before deleting its local copy. An ack before the durable
    // write is a contract violation.
    let actual_sha = format!("{:x}", sha2::Sha256::finalize(hasher));

    // Optional integrity check: on mismatch the file is deleted and the
    // client retries later (the index row never made it here).
    if let Some(expected) = expected_sha {
        if actual_sha != expected {
            let _ = tokio::fs::remove_file(&dest).await;
            return bad_request("sha256 mismatch: file deleted");
        }
    }

    // The stored path mirrors the client's `data.path` convention
    // (`media/<kind>/<YYYY>/<MM>/<DD>/<filename>`, relative to the data
    // root), built from validated components so it is identical on every OS.
    let media_root = state
        .media_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("media");
    let rel = format!("{media_root}/{kind}/{year}/{month}/{day}/{filename}");
    json_response(
        StatusCode::OK,
        json!({ "stored": rel, "bytes": bytes, "sha256": actual_sha }),
    )
}

/// Validate and lowercase a sha256 header value.
fn normalize_sha256(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.len() != 64 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(raw.to_ascii_lowercase())
}

/// Single path component: non-empty ASCII alphanumerics plus `.` `-` `_`,
/// never `.` or `..` (rejects path traversal in every route segment).
fn valid_component(s: &str) -> bool {
    if s.is_empty() || s == "." || s == ".." {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// `YYYY-MM-DD` -> `(YYYY, MM, DD)`, all numeric.
fn valid_date(date: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    let numeric = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if y.len() == 4 && m.len() == 2 && d.len() == 2 && numeric(y) && numeric(m) && numeric(d) {
        Some((y.to_string(), m.to_string(), d.to_string()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_component_rules() {
        for ok in ["screen", "frame_001.jpg", "a-b.c_d", "A9"] {
            assert!(valid_component(ok), "{ok} must pass");
        }
        for bad in [
            "",
            ".",
            "..",
            "sub/dir",
            "..\\x",
            "sp ace",
            "ünïcode",
            "a\nb",
        ] {
            assert!(!valid_component(bad), "{bad} must be rejected");
        }
    }

    #[test]
    fn valid_date_rules() {
        assert_eq!(
            valid_date("2026-11-04"),
            Some(("2026".to_string(), "11".to_string(), "04".to_string()))
        );
        for bad in [
            "",
            "2026-11",
            "2026-11-04-05",
            "26-11-04",
            "2026-1-04",
            "abcd-11-04",
            "2026/11/04",
        ] {
            assert!(valid_date(bad).is_none(), "{bad} must be rejected");
        }
    }

    #[test]
    fn normalize_sha256_rules() {
        let hex = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(normalize_sha256(hex), Some(hex.to_string()));
        assert_eq!(
            normalize_sha256(&hex.to_ascii_uppercase()),
            Some(hex.to_string())
        );
        assert_eq!(
            normalize_sha256(&format!("  {hex}  ")),
            Some(hex.to_string())
        );
        assert_eq!(normalize_sha256("abc"), None);
        assert_eq!(normalize_sha256(&"g".repeat(64)), None);
        assert_eq!(normalize_sha256(&"a".repeat(63)), None);
    }

    #[test]
    fn auth_config_modes() {
        assert_eq!(AuthConfig::disabled(), AuthConfig::from_env(None, false));
        assert!(!AuthConfig::disabled().whois);
        let whois = AuthConfig::from_env(None, true);
        assert!(whois.describe().contains("tailscale-whois"));
        let token = AuthConfig::from_env(Some("s3cret".to_string()), false);
        assert!(token.describe().contains("X-Activity-Token"));
        let both = AuthConfig::from_env(Some("s3cret".to_string()), true);
        assert!(both
            .describe()
            .contains("tailscale-whois + X-Activity-Token"));
        assert!(AuthConfig::disabled().describe().contains("tailnet-trust"));
    }

    #[test]
    fn resolve_device_whois_absent_or_loopback_falls_back_to_body() {
        let auth = AuthConfig::from_env(None, true);
        // whois cannot succeed in this environment (no socket on CI), so the
        // body device wins through the fallback path
        assert_eq!(
            resolve_device(&auth, Some("10.1.2.3".parse().expect("ip")), "laptop"),
            "laptop"
        );
        assert_eq!(
            resolve_device(&AuthConfig::disabled(), None, "laptop"),
            "laptop"
        );
    }

    #[test]
    fn whois_response_parsing() {
        let body = r#"{"Node": {"HostName": "laptop-x", "OS": "windows"},
                        "UserProfile": {"LoginName": "joe@tailnet", "DisplayName": "Joe Laptop"}}"#;
        assert_eq!(
            whois::parse_whois_device(body),
            Some("laptop-x".to_string())
        );
        let no_node = r#"{"UserProfile": {"DisplayName": "Joe Laptop"}}"#;
        assert_eq!(
            whois::parse_whois_device(no_node),
            Some("Joe Laptop".to_string())
        );
        assert_eq!(whois::parse_whois_device("{}"), None);
        assert_eq!(whois::parse_whois_device("{nope"), None);
    }

    #[test]
    fn whois_http_body_extraction() {
        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"Node\":{}}";
        assert_eq!(whois::http_ok_body(resp), Some("{\"Node\":{}}"));
        let denied = "HTTP/1.1 500 Internal Server Error\r\n\r\nboom";
        assert_eq!(whois::http_ok_body(denied), None);
        assert_eq!(whois::http_ok_body("garbage"), None);
    }
}
