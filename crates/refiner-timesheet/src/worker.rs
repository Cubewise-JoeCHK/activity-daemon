//! The frame analysis worker (issue #43): hub-side loop consuming new
//! `media` arrivals and writing `frame-analysis` events.
//!
//! Scheduling: poll the store every `[timesheet] poll_interval_s`, one
//! batch per pass; every blocking step (SQLite, JPEG file read, Lemonade
//! HTTP call) runs inside `tokio::task::spawn_blocking` so ingestion on the
//! async runtime is never blocked. After a failed analysis the loop backs
//! off exponentially (capped at `backoff_max_s`) and retries the same
//! frame - a dead Lemonade costs analysis, not capture.
//!
//! Concurrency (issue #70): the worker owns a dedicated connection to the
//! hub SQLite file (WAL mode). Analysis passes - including the long
//! Lemonade vision calls - hold no shared lock, so hub API reads are
//! never starved behind a running pass.
//!
//! Ordering: arrivals are consumed strictly in event-id order; the id of a
//! row is only passed (and the durable cursor advanced) after its work is
//! durably done, so crashes and failures re-process that row at least once.
//!
//! Durable vs in-memory state: the arrival cursor persists in the core
//! `refiner_cursors` table (hub restarts resume instead of re-analysing
//! history); sampling decisions and skip markers stay in memory only per
//! the issue contract.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use activity_core::{
    CoreError, Db, EventEnvelope, BUCKET_FRAME_ANALYSIS, BUCKET_MEDIA, BUCKET_WINDOW,
};
use chrono::{DateTime, FixedOffset};
use serde_json::json;
use tokio::sync::watch;

use crate::config::TimesheetConfig;
use crate::lemonade::{FrameAnalysis, LemonadeClient};
use crate::sampler::{SampleDecision, SamplerState};
use crate::storage::{EvidenceFrame, TsQuestionStore};
use crate::vocab::CubewiseVocab;

/// Cursor name under the core `refiner_cursors` table.
pub const CURSOR_NAME: &str = "timesheet-frame-analysis";

/// Rows consumed per pass; small enough that the loop keeps checking the
/// shutdown signal between batches of a large backlog.
const BATCH: u32 = 32;

/// Everything the worker needs besides the `Db` handle. Built once at
/// server startup; `Err` when the vocabulary fails to load (the caller logs
/// and skips the worker - analysis off, ingestion unaffected).
#[derive(Clone)]
pub struct WorkerContext {
    pub cfg: TimesheetConfig,
    /// Hub media root (`<data>/media`); frame event paths are relative to
    /// it (after their `media/` prefix).
    pub media_dir: PathBuf,
    pub vocab: CubewiseVocab,
    /// Shared question queue (issue #44): low-confidence answers become
    /// questions with frame evidence, answered async by the owner.
    pub questions: Arc<Mutex<TsQuestionStore>>,
}

impl WorkerContext {
    pub fn build(
        media_dir: PathBuf,
        questions: Arc<Mutex<TsQuestionStore>>,
        cfg: TimesheetConfig,
    ) -> Result<WorkerContext, String> {
        let vocab = CubewiseVocab::load(cfg.vocab_path.as_deref()).map_err(|e| e.to_string())?;
        Ok(WorkerContext {
            cfg,
            media_dir,
            vocab,
            questions,
        })
    }
}

/// What one pass over the store did.
#[derive(Debug, Default, PartialEq)]
pub struct BatchOutcome {
    /// `frame-analysis` events written.
    pub analysed: usize,
    /// Arrivals the sampler skipped (in memory only).
    pub skipped: usize,
    /// Unusable arrivals (not a frame, unsafe path, unreadable file):
    /// advanced past, never analysed, never retried.
    pub invalid: usize,
    /// Set when a row failed mid-batch (Lemonade or storage error): the
    /// cursor was NOT advanced past that row and the loop will retry it.
    pub failure: Option<String>,
}

/// Run the worker until `shutdown` carries `true`. Returns on graceful
/// shutdown only; all runtime failures are retried with backoff instead.
///
/// `db` is the worker's OWN connection to the hub SQLite file (WAL mode,
/// issue #70): passes take a worker-private mutex that no API path ever
/// touches, so hub API reads stay responsive while a Lemonade vision call
/// is in flight. The server opens this connection separately from the API
/// store; `busy_timeout` covers the rare write overlap between the two.
pub async fn run_worker(db: Db, ctx: WorkerContext, mut shutdown: watch::Receiver<bool>) {
    // Worker-private mutex: it exists only to give the !Sync SQLite
    // connection a Send+Sync home for spawn_blocking. No other thread ever
    // takes it, so a pass never contends with hub API reads (issue #70).
    let db = Arc::new(Mutex::new(db));
    let client = LemonadeClient::new(&ctx.cfg);
    let mut sampler = SamplerState::new(ctx.cfg.sampling_mode, ctx.cfg.max_per_hour);
    let mut next_id = match db.lock() {
        Ok(guard) => match guard.refiner_cursor(CURSOR_NAME) {
            Ok(Some(last)) => last + 1,
            Ok(None) => 0,
            Err(e) => {
                log::error!("timesheet worker: cannot read cursor ({e}); worker not started");
                return;
            }
        },
        Err(_) => {
            log::error!("timesheet worker: worker store lock poisoned; worker not started");
            return;
        }
    };
    let poll = Duration::from_secs(ctx.cfg.poll_interval_s.max(1));
    let mut consecutive_failures: u32 = 0;
    log::info!(
        "timesheet frame worker started: mode {:?}, max/hour {}, lemonade {} model {}, resuming after event id {}",
        ctx.cfg.sampling_mode,
        ctx.cfg.max_per_hour,
        ctx.cfg.lemonade_base_url,
        ctx.cfg.lemonade_model,
        next_id.saturating_sub(1)
    );
    loop {
        let delay = if consecutive_failures == 0 {
            poll
        } else {
            backoff_delay(&ctx.cfg, consecutive_failures)
        };
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.wait_for(|seen| *seen) => break,
        }
        let pass_db = db.clone();
        let pass_ctx = ctx.clone();
        let pass_client = client.clone();
        let pass_sampler = sampler.clone();
        let pass_next_id = next_id;
        // The pass works on a snapshot of the sampler state; the outer state
        // and cursor advance only when the pass outcome returns them. On a
        // join failure the snapshot simply stays authoritative.
        let pass = tokio::task::spawn_blocking(move || {
            let mut sampler = pass_sampler;
            let mut next_id = pass_next_id;
            let outcome = match pass_db.lock() {
                Ok(guard) => {
                    process_batch(&guard, &pass_ctx, &pass_client, &mut sampler, &mut next_id)
                }
                Err(_) => Err(CoreError::Config("worker store lock poisoned".to_string())),
            };
            (outcome, sampler, next_id)
        })
        .await;
        match pass {
            Ok((Ok(outcome), new_sampler, new_next)) => {
                sampler = new_sampler;
                next_id = new_next;
                match outcome.failure {
                    None => {
                        if consecutive_failures > 0 {
                            log::info!("timesheet worker: Lemonade reachable again, resuming");
                        }
                        if outcome.analysed > 0 || outcome.skipped > 0 || outcome.invalid > 0 {
                            log::info!(
                                "timesheet worker: {} analysed, {} skipped, {} invalid (at event id {})",
                                outcome.analysed,
                                outcome.skipped,
                                outcome.invalid,
                                next_id
                            );
                        }
                        consecutive_failures = 0;
                    }
                    Some(failure) => {
                        consecutive_failures += 1;
                        log::warn!(
                            "timesheet worker: pass stopped at event id {} ({}); retrying in {} s",
                            next_id + 1,
                            failure,
                            backoff_delay(&ctx.cfg, consecutive_failures).as_secs()
                        );
                    }
                }
            }
            Ok((Err(e), new_sampler, new_next)) => {
                sampler = new_sampler;
                next_id = new_next;
                consecutive_failures += 1;
                log::warn!(
                    "timesheet worker: pass failed ({e}); retrying in {} s",
                    backoff_delay(&ctx.cfg, consecutive_failures).as_secs()
                );
            }
            Err(e) => {
                consecutive_failures += 1;
                log::warn!(
                    "timesheet worker: pass task failed ({e}); retrying in {} s",
                    backoff_delay(&ctx.cfg, consecutive_failures).as_secs()
                );
            }
        }
    }
    log::info!("timesheet frame worker stopped");
}

/// Exponential backoff: `initial * 2^(failures-1)`, capped at `backoff_max_s`.
fn backoff_delay(cfg: &TimesheetConfig, failures: u32) -> Duration {
    let initial = cfg.backoff_initial_s.max(1);
    let secs = initial
        .saturating_mul(2u64.saturating_pow(failures.saturating_sub(1)))
        .min(cfg.backoff_max_s.max(initial));
    Duration::from_secs(secs)
}

/// One pass: fetch the next batch of media arrivals (in id order) and
/// process each. Returns early with `failure` set when a row cannot be
/// analysed, leaving `next_id` (and the durable cursor) before it.
pub fn process_batch(
    db: &Db,
    ctx: &WorkerContext,
    client: &LemonadeClient,
    sampler: &mut SamplerState,
    next_id: &mut i64,
) -> Result<BatchOutcome, CoreError> {
    let mut outcome = BatchOutcome::default();
    let rows = db.fetch_bucket_events_after(BUCKET_MEDIA, *next_id, BATCH)?;
    for row in rows {
        let data: serde_json::Value = match serde_json::from_str(&row.data) {
            Ok(data) => data,
            Err(e) => {
                log::warn!("media row {}: unreadable data JSON ({e}); skipped", row.id);
                sampler.note_skipped();
                outcome.invalid += 1;
                *next_id = row.id;
                continue;
            }
        };
        let kind = data.get("kind").and_then(serde_json::Value::as_str);
        if kind != Some("frame") {
            // Other media kinds (e.g. meeting audio later) are not this
            // worker's input; just move on.
            log::debug!("media row {}: kind {:?} not a frame; ignored", row.id, kind);
            *next_id = row.id;
            continue;
        }
        let rel_path = data
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let sha16 = data
            .get("sha16")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let frame_ts = match DateTime::parse_from_rfc3339(&row.ts_start) {
            Ok(ts) => ts,
            Err(e) => {
                log::warn!(
                    "media row {}: unparsable timestamp {} ({e}); skipped",
                    row.id,
                    row.ts_start
                );
                sampler.note_skipped();
                outcome.invalid += 1;
                *next_id = row.id;
                continue;
            }
        };
        let frame_path = match resolve_frame_path(&ctx.media_dir, &rel_path) {
            Ok(path) => path,
            Err(e) => {
                log::warn!("media row {}: unusable frame path ({e}); skipped", row.id);
                sampler.note_skipped();
                outcome.invalid += 1;
                *next_id = row.id;
                continue;
            }
        };

        let session_changed = match sampler.last_analysed_ts() {
            None => false,
            Some(prev) => window_session_changed(db, prev, &row.ts_start)?,
        };
        let hour = hour_key(&frame_ts);
        match sampler.decide(&hour, session_changed) {
            SampleDecision::Skip { reason } => {
                log::debug!("media row {}: skipped ({reason})", row.id);
                sampler.note_skipped();
                outcome.skipped += 1;
                *next_id = row.id;
            }
            SampleDecision::Analyse { reason } => {
                log::debug!("media row {}: sampled ({reason})", row.id);
                let jpeg = match std::fs::read(&frame_path) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        // The hub stores the file before the index row (ADR
                        // rev 5), so a missing file means retention or
                        // manual deletion: permanent, not transient.
                        log::warn!(
                            "media row {}: frame file {} unreadable ({e}); skipped",
                            row.id,
                            rel_path
                        );
                        sampler.note_skipped();
                        outcome.invalid += 1;
                        *next_id = row.id;
                        continue;
                    }
                };
                // The ACTIVE PROJECTS shortlist (issue #69) constrains the
                // prompt's project_guess options; the snapshot semantics
                // match the style block (worker restart picks up edits).
                match client.analyze_jpeg(
                    &jpeg,
                    &ctx.vocab,
                    &ctx.cfg.style,
                    &ctx.cfg.active_projects,
                ) {
                    Err(e) => {
                        // Failure contract: stop before this row, the loop
                        // backs off and retries it.
                        log::warn!("media row {}: analysis failed ({e})", row.id);
                        outcome.failure = Some(e.to_string());
                        return Ok(outcome);
                    }
                    Ok(analysis) => {
                        let event = EventEnvelope {
                            bucket: BUCKET_FRAME_ANALYSIS.to_string(),
                            timestamp: row.ts_start.clone(),
                            duration_s: 0.0,
                            data: json!({
                                "frame_sha16": sha16,
                                "frame_path": rel_path,
                                "activity": analysis.activity,
                                "project_guess": analysis.project_guess,
                                "category": analysis.category,
                                "confidence": analysis.confidence,
                                "evidence": analysis.evidence,
                                "model": client.model(),
                                "sampled": true,
                            }),
                        };
                        let stored = db
                            .insert_event(&event, &ctx.cfg.device_name)
                            .and_then(|_| db.set_refiner_cursor(CURSOR_NAME, row.id));
                        match stored {
                            Ok(_) => {
                                sampler.commit_analysis(&hour, &row.ts_start);
                                outcome.analysed += 1;
                                if analysis.confidence < ctx.cfg.confidence_threshold {
                                    hand_off_low_confidence(
                                        &ctx.questions,
                                        &analysis,
                                        &rel_path,
                                        &sha16,
                                        &row.ts_start,
                                    );
                                }
                                *next_id = row.id;
                            }
                            Err(e) => {
                                log::error!(
                                    "media row {}: writing frame-analysis failed ({e})",
                                    row.id
                                );
                                outcome.failure = Some(e.to_string());
                                return Ok(outcome);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(outcome)
}

/// Whether a `window-watcher` session started in `(prev_analysed_ts,
/// frame_ts]` - the keyframe signal. Window events carry their session
/// start as `timestamp`; the event that may have triggered the previous
/// analysis (exactly at `prev_analysed_ts`) is excluded.
fn window_session_changed(
    db: &Db,
    prev_analysed_ts: &str,
    frame_ts: &str,
) -> Result<bool, CoreError> {
    let prev = DateTime::parse_from_rfc3339(prev_analysed_ts).map_err(|e| {
        CoreError::Validation(format!(
            "previous analysis timestamp '{prev_analysed_ts}' unparsable: {e}"
        ))
    })?;
    let sessions = db.query_events(BUCKET_WINDOW, Some(prev_analysed_ts), Some(frame_ts))?;
    Ok(sessions.iter().any(|session| {
        DateTime::parse_from_rfc3339(&session.timestamp)
            .map(|ts| ts > prev)
            .unwrap_or(false)
    }))
}

/// Wall-clock hour key in the frame's own timezone offset
/// (`2026-11-04T09`): the per-hour cap follows the owner's local hours.
fn hour_key(ts: &DateTime<FixedOffset>) -> String {
    ts.format("%Y-%m-%dT%H").to_string()
}

/// Resolve a media event's `path` against the media root. Defense in depth:
/// the path arrives in client-supplied event JSON, so only plain
/// `media/frames/<Y>/<M>/<D>/<file>.jpg` relative paths are accepted -
/// components limited to `[A-Za-z0-9._-]`, no traversal, no separators.
fn resolve_frame_path(media_dir: &Path, rel_path: &str) -> Result<PathBuf, String> {
    let rest = rel_path
        .strip_prefix("media/frames/")
        .ok_or_else(|| format!("path {rel_path:?} does not start with media/frames/"))?;
    let mut full = media_dir.to_path_buf();
    full.push("frames");
    for component in rest.split('/') {
        let valid = !component.is_empty()
            && component != "."
            && component != ".."
            && component
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !valid {
            return Err(format!(
                "unsafe path component {component:?} in {rel_path:?}"
            ));
        }
        full.push(component);
    }
    if full.extension().and_then(|extension| extension.to_str()) != Some("jpg") {
        return Err(format!("frame path {rel_path:?} must end in .jpg"));
    }
    Ok(full)
}

/// Low-confidence hand-off (issue #43 step 5): create one question in the
/// `ts_questions` queue (issue #44) with the frame as evidence, so the
/// owner can answer it asynchronously through the hub API. Fail-soft: a
/// queue error is logged and the hand-off lost - the `frame-analysis`
/// event already carries the full answer either way.
fn hand_off_low_confidence(
    questions: &Arc<Mutex<TsQuestionStore>>,
    analysis: &FrameAnalysis,
    frame_path: &str,
    frame_sha16: &str,
    frame_ts: &str,
) {
    let day = DateTime::parse_from_rfc3339(frame_ts)
        .map(|ts| ts.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    let question = format!(
        "What were you working on at {frame_ts}? (auto-detected: {}; confidence {:.2})",
        analysis.activity, analysis.confidence
    );
    let evidence = [EvidenceFrame {
        frame_path: frame_path.to_string(),
        frame_sha16: frame_sha16.to_string(),
        ts: frame_ts.to_string(),
    }];
    let store = questions
        .lock()
        .map_err(|_| CoreError::Config("question queue lock poisoned".to_string()));
    match store.and_then(|store| store.create_question(&day, &question, &evidence)) {
        Ok(id) => log::info!(
            "timesheet: low confidence {:.2} on frame {frame_ts} - question #{id} created",
            analysis.confidence
        ),
        Err(e) => log::warn!(
            "timesheet: low confidence {:.2} on frame {frame_ts} - question NOT created ({e})",
            analysis.confidence
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_DEVICE_NAME;
    use crate::lemonade::LemonadeError;
    use activity_core::BUCKET_MEDIA;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // ------------------------------------------------------------ helpers

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "refiner-worker-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn insert_media_frame(db: &Db, ts: &str, path: &str, sha16: &str) -> i64 {
        db.insert_event(
            &EventEnvelope {
                bucket: BUCKET_MEDIA.to_string(),
                timestamp: ts.to_string(),
                duration_s: 0.0,
                data: json!({
                    "kind": "frame",
                    "path": path,
                    "bytes": 100,
                    "sha16": sha16,
                    "monitor": "MONITOR1",
                    "producer": "watcher-screen",
                }),
            },
            "client-device",
        )
        .expect("insert media frame")
    }

    fn insert_window_session(db: &Db, ts: &str, app: &str) {
        db.insert_event(
            &EventEnvelope {
                bucket: BUCKET_WINDOW.to_string(),
                timestamp: ts.to_string(),
                duration_s: 60.0,
                data: json!({"app": app, "title": "untitled"}),
            },
            "client-device",
        )
        .expect("insert window session");
    }

    fn write_frame_file(media_dir: &Path, rel: &str, bytes: &[u8]) {
        let full = media_dir.join(rel.strip_prefix("media/").expect("media prefix"));
        std::fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
        std::fs::write(full, bytes).expect("write frame");
    }

    /// Analysis content the mock serves while healthy.
    fn good_content() -> String {
        r#"{"activity": "Editing rules", "project_guess": "A143P010 - Frasers - H2 FH - BU Core", "category": "Consulting", "confidence": 0.9, "evidence": "IDE visible"}"#.to_string()
    }

    #[derive(Clone)]
    struct MockState {
        healthy: Arc<AtomicBool>,
        requests: Arc<AtomicUsize>,
        content: Arc<String>,
        /// Seconds to stall before answering (timeout tests).
        delay_s: u64,
        /// When non-empty, the required Authorization header value.
        require_auth: Arc<String>,
    }

    async fn chat(
        State(state): State<MockState>,
        headers: HeaderMap,
        _body: axum::body::Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        state.requests.fetch_add(1, Ordering::SeqCst);
        if !state.require_auth.is_empty()
            && headers
                .get("Authorization")
                .and_then(|value| value.to_str().ok())
                != Some(state.require_auth.as_str())
        {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": {"message": "bad api key"}})),
            );
        }
        if state.delay_s > 0 {
            tokio::time::sleep(Duration::from_secs(state.delay_s)).await;
        }
        if !state.healthy.load(Ordering::SeqCst) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": "model not loaded"}})),
            );
        }
        (
            StatusCode::OK,
            Json(json!({
                "choices": [{"message": {"role": "assistant", "content": state.content.as_ref().clone()}}]
            })),
        )
    }

    fn spawn_mock(state: MockState) -> std::net::SocketAddr {
        let app = Router::new()
            .route("/v1/chat/completions", post(chat))
            .with_state(state);
        // std bind to pick a free port, then hand the socket to tokio
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("mock addr");
        let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock serve");
        });
        addr
    }

    fn mock_state(content: String) -> MockState {
        MockState {
            healthy: Arc::new(AtomicBool::new(true)),
            requests: Arc::new(AtomicUsize::new(0)),
            content: Arc::new(content),
            delay_s: 0,
            require_auth: Arc::new(String::new()),
        }
    }

    fn ctx_for(
        media_dir: PathBuf,
        base_url: String,
        cfg_overrides: impl FnOnce(&mut TimesheetConfig),
    ) -> WorkerContext {
        let mut cfg = TimesheetConfig {
            lemonade_base_url: base_url,
            poll_interval_s: 1,
            request_timeout_s: 15,
            backoff_initial_s: 1,
            backoff_max_s: 5,
            ..TimesheetConfig::default()
        };
        cfg_overrides(&mut cfg);
        WorkerContext {
            cfg,
            media_dir,
            vocab: CubewiseVocab::embedded().clone(),
            questions: Arc::new(Mutex::new(
                TsQuestionStore::open_in_memory().expect("question store"),
            )),
        }
    }

    // ------------------------------------------------- process_batch pass

    // multi-thread flavor: the blocking ureq call in process_batch must not
    // starve the mock server task on a current-thread runtime
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn process_batch_analyses_and_writes_frame_analysis_events() {
        let dir = temp_dir("analyse");
        let media_dir = dir.join("media");
        let db = Db::open_in_memory().expect("db");
        let rel = "media/frames/2026/11/04/090005_MONITOR1_a1b2c3d4e5f60718.jpg";
        write_frame_file(&media_dir, rel, b"jpeg-bytes");
        let row_id = insert_media_frame(&db, "2026-11-04T09:00:05+08:00", rel, "a1b2c3d4e5f60718");

        let addr = spawn_mock(mock_state(good_content()));
        let ctx = ctx_for(media_dir, format!("http://{addr}/v1"), |_| {});
        let client = LemonadeClient::new(&ctx.cfg);
        let mut sampler = SamplerState::new(crate::config::SamplingMode::All, 8);
        let mut next_id = 0;

        let outcome = process_batch(&db, &ctx, &client, &mut sampler, &mut next_id).expect("pass");
        assert_eq!(
            outcome,
            BatchOutcome {
                analysed: 1,
                skipped: 0,
                invalid: 0,
                failure: None,
            }
        );
        assert_eq!(next_id, row_id);

        let events = db
            .query_events(BUCKET_FRAME_ANALYSIS, None, None)
            .expect("query");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.bucket, "frame-analysis");
        assert_eq!(event.timestamp, "2026-11-04T09:00:05+08:00");
        assert_eq!(event.duration_s, 0.0);
        assert_eq!(event.device, DEFAULT_DEVICE_NAME);
        assert_eq!(event.data["frame_sha16"], "a1b2c3d4e5f60718");
        assert_eq!(event.data["frame_path"], rel);
        assert_eq!(event.data["activity"], "Editing rules");
        assert_eq!(
            event.data["project_guess"],
            "A143P010 - Frasers - H2 FH - BU Core"
        );
        assert_eq!(event.data["category"], "Consulting");
        assert_eq!(event.data["confidence"], 0.9);
        assert_eq!(event.data["evidence"], "IDE visible");
        assert_eq!(event.data["model"], "Qwen3.6-35B-A3B-MTP-GGUF");
        assert_eq!(event.data["sampled"], true);

        // the durable cursor moved with the work
        assert_eq!(
            db.refiner_cursor(CURSOR_NAME).expect("cursor"),
            Some(row_id)
        );
        // a second pass over a drained store is a clean no-op
        let outcome =
            process_batch(&db, &ctx, &client, &mut sampler, &mut next_id).expect("pass 2");
        assert_eq!(outcome, BatchOutcome::default());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // multi-thread flavor: see process_batch_analyses_... above
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lemonade_failure_stops_the_batch_and_preserves_the_cursor() {
        let dir = temp_dir("failure");
        let media_dir = dir.join("media");
        let db = Db::open_in_memory().expect("db");
        let rel = "media/frames/2026/11/04/090005_MONITOR1_aaaaaaaaaaaaaaaa.jpg";
        write_frame_file(&media_dir, rel, b"jpeg");
        let row_id = insert_media_frame(&db, "2026-11-04T09:00:05+08:00", rel, "aaaaaaaaaaaaaaaa");

        let state = mock_state(good_content());
        let healthy = state.healthy.clone();
        healthy.store(false, Ordering::SeqCst);
        let addr = spawn_mock(state);
        let ctx = ctx_for(media_dir, format!("http://{addr}/v1"), |_| {});
        let client = LemonadeClient::new(&ctx.cfg);
        let mut sampler = SamplerState::new(crate::config::SamplingMode::All, 8);
        let mut next_id = 0;

        let outcome = process_batch(&db, &ctx, &client, &mut sampler, &mut next_id).expect("pass");
        assert!(
            outcome.failure.is_some(),
            "unhealthy mock must fail the pass"
        );
        assert_eq!(outcome.analysed, 0);
        assert_eq!(next_id, 0, "cursor must not move past a failed frame");
        assert!(db
            .query_events(BUCKET_FRAME_ANALYSIS, None, None)
            .expect("query")
            .is_empty());
        assert_eq!(db.refiner_cursor(CURSOR_NAME).expect("cursor"), None);

        // Lemonade comes back: the same frame is retried and succeeds
        healthy.store(true, Ordering::SeqCst);
        let outcome = process_batch(&db, &ctx, &client, &mut sampler, &mut next_id).expect("retry");
        assert_eq!(outcome.failure, None);
        assert_eq!(outcome.analysed, 1);
        assert_eq!(next_id, row_id);
        assert_eq!(
            db.refiner_cursor(CURSOR_NAME).expect("cursor"),
            Some(row_id)
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // multi-thread flavor: see process_batch_analyses_... above
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keyframe_sampling_skips_same_session_and_honours_the_hour_cap() {
        let dir = temp_dir("keyframe");
        let media_dir = dir.join("media");
        let db = Db::open_in_memory().expect("db");

        // W1 opens before the first frame; the first arrival is a keyframe
        // by the first-frame rule.
        insert_window_session(&db, "2026-11-04T09:00:00+08:00", "Code.exe");
        insert_media_frame(
            &db,
            "2026-11-04T09:00:05+08:00",
            "media/frames/2026/11/04/090005_MONITOR1_0000000000000001.jpg",
            "0000000000000001",
        );
        // no window session since F1 -> same session, skipped
        insert_media_frame(
            &db,
            "2026-11-04T09:00:10+08:00",
            "media/frames/2026/11/04/090010_MONITOR1_0000000000000002.jpg",
            "0000000000000002",
        );
        // W2 opens -> F3 is a session-change keyframe (second this hour)
        insert_window_session(&db, "2026-11-04T09:00:15+08:00", "Teams.exe");
        insert_media_frame(
            &db,
            "2026-11-04T09:00:20+08:00",
            "media/frames/2026/11/04/090020_MONITOR1_0000000000000003.jpg",
            "0000000000000003",
        );
        // W3 opens -> F4 is a candidate but the cap (2/hour) is exhausted
        insert_window_session(&db, "2026-11-04T09:00:25+08:00", "Excel.exe");
        insert_media_frame(
            &db,
            "2026-11-04T09:00:30+08:00",
            "media/frames/2026/11/04/090030_MONITOR1_0000000000000004.jpg",
            "0000000000000004",
        );
        // W4 opens in the next wall-clock hour -> F5 analyses again
        insert_window_session(&db, "2026-11-04T10:00:00+08:00", "Word.exe");
        let f5 = insert_media_frame(
            &db,
            "2026-11-04T10:00:05+08:00",
            "media/frames/2026/11/04/100005_MONITOR1_0000000000000005.jpg",
            "0000000000000005",
        );
        // only analysed frames need files on disk
        for rel in [
            "media/frames/2026/11/04/090005_MONITOR1_0000000000000001.jpg",
            "media/frames/2026/11/04/090020_MONITOR1_0000000000000003.jpg",
            "media/frames/2026/11/04/100005_MONITOR1_0000000000000005.jpg",
        ] {
            write_frame_file(&media_dir, rel, b"jpeg");
        }

        let addr = spawn_mock(mock_state(good_content()));
        let ctx = ctx_for(media_dir, format!("http://{addr}/v1"), |cfg| {
            cfg.sampling_mode = crate::config::SamplingMode::Keyframe;
            cfg.max_per_hour = 2;
        });
        let client = LemonadeClient::new(&ctx.cfg);
        let mut sampler = SamplerState::new(ctx.cfg.sampling_mode, ctx.cfg.max_per_hour);
        let mut next_id = 0;

        let outcome = process_batch(&db, &ctx, &client, &mut sampler, &mut next_id).expect("pass");
        assert_eq!(outcome.analysed, 3, "F1, F3, F5");
        assert_eq!(outcome.skipped, 2, "F2 same-session, F4 cap");
        assert_eq!(outcome.failure, None);
        assert_eq!(next_id, f5);
        assert_eq!(sampler.skipped(), 2);
        assert_eq!(
            sampler.last_analysed_ts(),
            Some("2026-11-04T10:00:05+08:00")
        );

        let events = db
            .query_events(BUCKET_FRAME_ANALYSIS, None, None)
            .expect("query");
        assert_eq!(events.len(), 3);
        // exactly F1, F3 and F5 were analysed - F2 and F4 left no events
        let paths: Vec<&str> = events
            .iter()
            .map(|event| event.data["frame_path"].as_str().expect("frame_path"))
            .collect();
        assert_eq!(
            paths,
            [
                "media/frames/2026/11/04/090005_MONITOR1_0000000000000001.jpg",
                "media/frames/2026/11/04/090020_MONITOR1_0000000000000003.jpg",
                "media/frames/2026/11/04/100005_MONITOR1_0000000000000005.jpg",
            ]
        );
        assert_eq!(db.refiner_cursor(CURSOR_NAME).expect("cursor"), Some(f5));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test]
    async fn unusable_arrivals_advance_without_analysis() {
        let dir = temp_dir("unusable");
        let media_dir = dir.join("media");
        let db = Db::open_in_memory().expect("db");
        // not this worker's kind: advances silently
        let _audio_row = db
            .insert_event(
                &EventEnvelope {
                    bucket: BUCKET_MEDIA.to_string(),
                    timestamp: "2026-11-04T09:00:01+08:00".to_string(),
                    duration_s: 0.0,
                    data: json!({"kind": "audio", "path": "media/audio/x.wav"}),
                },
                "client-device",
            )
            .expect("audio row");
        // path traversal attempt: counted invalid, never analysed
        let _traversal_row = insert_media_frame(
            &db,
            "2026-11-04T09:00:02+08:00",
            "media/frames/../../secrets/evil.jpg",
            "bbbbbbbbbbbbbbbb",
        );
        // clean path but no file behind it (retention deleted it)
        let missing_row = insert_media_frame(
            &db,
            "2026-11-04T09:00:03+08:00",
            "media/frames/2026/11/04/090003_MONITOR1_cccccccccccccccc.jpg",
            "cccccccccccccccc",
        );

        let addr = spawn_mock(mock_state(good_content()));
        let ctx = ctx_for(media_dir, format!("http://{addr}/v1"), |_| {});
        let client = LemonadeClient::new(&ctx.cfg);
        let mut sampler = SamplerState::new(crate::config::SamplingMode::All, 8);
        let mut next_id = 0;

        let outcome = process_batch(&db, &ctx, &client, &mut sampler, &mut next_id).expect("pass");
        assert_eq!(outcome.analysed, 0);
        assert_eq!(outcome.invalid, 2, "traversal + missing file");
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.failure, None);
        assert_eq!(next_id, missing_row, "audio row advanced silently");
        assert_eq!(sampler.skipped(), 2);
        assert!(db
            .query_events(BUCKET_FRAME_ANALYSIS, None, None)
            .expect("query")
            .is_empty());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // multi-thread flavor: see process_batch_analyses_... above
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn low_confidence_answer_still_writes_its_event() {
        let dir = temp_dir("lowconf");
        let media_dir = dir.join("media");
        let db = Db::open_in_memory().expect("db");
        let rel = "media/frames/2026/11/04/090005_MONITOR1_dddddddddddddddd.jpg";
        write_frame_file(&media_dir, rel, b"jpeg");
        insert_media_frame(&db, "2026-11-04T09:00:05+08:00", rel, "dddddddddddddddd");

        let unsure = r#"{"activity": "Something unclear", "project_guess": "", "category": "Support", "confidence": 0.2, "evidence": "mostly desktop"}"#;
        let addr = spawn_mock(mock_state(unsure.to_string()));
        let ctx = ctx_for(media_dir, format!("http://{addr}/v1"), |cfg| {
            cfg.confidence_threshold = 0.5;
        });
        let client = LemonadeClient::new(&ctx.cfg);
        let mut sampler = SamplerState::new(crate::config::SamplingMode::All, 8);
        let mut next_id = 0;

        let outcome = process_batch(&db, &ctx, &client, &mut sampler, &mut next_id).expect("pass");
        // the event is stored either way, and the low-confidence answer
        // becomes a question in the queue (issue #44) with frame evidence
        assert_eq!(outcome.analysed, 1);
        let events = db
            .query_events(BUCKET_FRAME_ANALYSIS, None, None)
            .expect("query");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["confidence"], 0.2);
        let open = ctx
            .questions
            .lock()
            .expect("lock")
            .list_questions(crate::storage::StatusFilter::Open)
            .expect("list questions");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].day, "2026-11-04", "the frame's own local day");
        assert!(open[0]
            .question
            .contains("What were you working on at 2026-11-04T09:00:05+08:00?"));
        assert!(open[0].question.contains("Something unclear"));
        assert_eq!(open[0].evidence.len(), 1);
        assert_eq!(open[0].evidence[0].frame_path, rel);
        assert_eq!(open[0].evidence[0].frame_sha16, "dddddddddddddddd");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // multi-thread flavor: see process_batch_analyses_... above
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn optional_api_key_is_sent_and_enforced() {
        let dir = temp_dir("auth");
        let media_dir = dir.join("media");
        let db = Db::open_in_memory().expect("db");
        let rel = "media/frames/2026/11/04/090005_MONITOR1_eeeeeeeeeeeeeeee.jpg";
        write_frame_file(&media_dir, rel, b"jpeg");
        insert_media_frame(&db, "2026-11-04T09:00:05+08:00", rel, "eeeeeeeeeeeeeeee");

        let mut state = mock_state(good_content());
        state.require_auth = Arc::new("Bearer sk-tailnet-secret".to_string());
        let requests = state.requests.clone();
        let addr = spawn_mock(state);
        let ctx = ctx_for(media_dir.clone(), format!("http://{addr}/v1"), |cfg| {
            cfg.lemonade_api_key = "sk-tailnet-secret".to_string();
        });
        let client = LemonadeClient::new(&ctx.cfg);
        let mut sampler = SamplerState::new(crate::config::SamplingMode::All, 8);
        let mut next_id = 0;
        let outcome = process_batch(&db, &ctx, &client, &mut sampler, &mut next_id).expect("pass");
        assert_eq!(outcome.analysed, 1, "matching key is accepted");

        // no key: the mock 401s and the pass fails softly
        let ctx_no_key = ctx_for(media_dir.clone(), format!("http://{addr}/v1"), |_| {});
        let client_no_key = LemonadeClient::new(&ctx_no_key.cfg);
        insert_media_frame(
            &db,
            "2026-11-04T09:00:06+08:00",
            "media/frames/2026/11/04/090006_MONITOR1_ffffffffffffffff.jpg",
            "ffffffffffffffff",
        );
        std::fs::write(
            media_dir.join("frames/2026/11/04/090006_MONITOR1_ffffffffffffffff.jpg"),
            b"jpeg",
        )
        .expect("second frame file");
        let mut sampler2 = SamplerState::new(crate::config::SamplingMode::All, 8);
        let mut next_id2 = 0;
        let outcome = process_batch(
            &db,
            &ctx_no_key,
            &client_no_key,
            &mut sampler2,
            &mut next_id2,
        )
        .expect("pass");
        assert!(outcome.failure.is_some(), "missing key must fail the pass");
        assert_eq!(next_id2, 0);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "the mock saw exactly one request per pass"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test]
    async fn slow_lemonade_times_out_as_an_http_failure() {
        let dir = temp_dir("timeout");
        let media_dir = dir.join("media");
        let db = Db::open_in_memory().expect("db");
        let rel = "media/frames/2026/11/04/090005_MONITOR1_1111111111111111.jpg";
        write_frame_file(&media_dir, rel, b"jpeg");
        insert_media_frame(&db, "2026-11-04T09:00:05+08:00", rel, "1111111111111111");

        let mut state = mock_state(good_content());
        state.delay_s = 3;
        let addr = spawn_mock(state);
        let ctx = ctx_for(media_dir, format!("http://{addr}/v1"), |cfg| {
            cfg.request_timeout_s = 1;
        });
        let client = LemonadeClient::new(&ctx.cfg);
        let outcome = tokio::task::spawn_blocking(move || {
            let vocab = CubewiseVocab::embedded().clone();
            client.analyze_jpeg(b"jpeg-bytes", &vocab, &ctx.cfg.style, &[])
        })
        .await
        .expect("join");
        assert!(matches!(outcome, Err(LemonadeError::Http(_))));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // ------------------------------------------------------- worker loop

    #[tokio::test]
    async fn run_worker_processes_arrivals_and_stops_on_shutdown() {
        let dir = temp_dir("loop");
        let media_dir = dir.join("media");
        // The production split (issue #70): the worker owns one connection,
        // the assertions use a second open on the same WAL file.
        let db_path = dir.join("activity.db");
        let worker_db = Db::open_db(&db_path).expect("worker db");
        let rel = "media/frames/2026/11/04/090005_MONITOR1_2222222222222222.jpg";
        write_frame_file(&media_dir, rel, b"jpeg");
        insert_media_frame(
            &worker_db,
            "2026-11-04T09:00:05+08:00",
            rel,
            "2222222222222222",
        );
        let hub_db = Db::open_db(&db_path).expect("hub db");

        let addr = spawn_mock(mock_state(good_content()));
        let ctx = ctx_for(media_dir, format!("http://{addr}/v1"), |cfg| {
            cfg.poll_interval_s = 1;
        });
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(run_worker(worker_db, ctx, shutdown_rx));

        // wait for the analysis to land (poll_interval_s = 1)
        let mut events = Vec::new();
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            events = hub_db
                .query_events(BUCKET_FRAME_ANALYSIS, None, None)
                .expect("query");
            if !events.is_empty() {
                break;
            }
        }
        assert_eq!(events.len(), 1, "worker must analyse the arrival");
        assert_eq!(events[0].data["frame_path"], rel);
        assert!(hub_db
            .refiner_cursor(CURSOR_NAME)
            .expect("cursor")
            .is_some());

        shutdown_tx.send(true).expect("shutdown send");
        tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .expect("worker must stop on shutdown")
            .expect("worker task");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// Regression for issue #70: while a Lemonade analysis is in flight
    /// (the mock stalls 3 s), a read on a second connection - the hub
    /// API's view - answers immediately. The old shared-mutex design made
    /// hub reads wait out the whole pass behind that call.
    #[tokio::test]
    async fn hub_reads_are_not_blocked_while_an_analysis_is_in_flight() {
        let dir = temp_dir("nolock");
        let media_dir = dir.join("media");
        let db_path = dir.join("activity.db");
        let worker_db = Db::open_db(&db_path).expect("worker db");
        let rel = "media/frames/2026/11/04/090005_MONITOR1_3333333333333333.jpg";
        write_frame_file(&media_dir, rel, b"jpeg");
        insert_media_frame(
            &worker_db,
            "2026-11-04T09:00:05+08:00",
            rel,
            "3333333333333333",
        );
        let hub_db = Db::open_db(&db_path).expect("hub db");

        let mut state = mock_state(good_content());
        state.delay_s = 3;
        let requests = state.requests.clone();
        let addr = spawn_mock(state);
        let ctx = ctx_for(media_dir, format!("http://{addr}/v1"), |cfg| {
            cfg.poll_interval_s = 1;
        });
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(run_worker(worker_db, ctx, shutdown_rx));

        // wait until the worker is inside the stalled Lemonade call
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if requests.load(Ordering::SeqCst) > 0 {
                break;
            }
        }
        assert_eq!(requests.load(Ordering::SeqCst), 1, "analysis started");

        // the hub API's read answers while the analysis is still running
        let started = std::time::Instant::now();
        let events = hub_db
            .query_events(BUCKET_FRAME_ANALYSIS, None, None)
            .expect("query");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "hub read must not wait behind the in-flight analysis"
        );
        assert!(events.is_empty(), "nothing stored yet - still analysing");

        shutdown_tx.send(true).expect("shutdown send");
        tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .expect("worker must stop")
            .expect("worker task");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // ------------------------------------------------------- unit pieces

    #[test]
    fn backoff_doubles_and_caps() {
        let cfg = TimesheetConfig::default();
        assert_eq!(backoff_delay(&cfg, 1).as_secs(), 5);
        assert_eq!(backoff_delay(&cfg, 2).as_secs(), 10);
        assert_eq!(backoff_delay(&cfg, 3).as_secs(), 20);
        assert_eq!(backoff_delay(&cfg, 10).as_secs(), 300, "capped");
        assert_eq!(backoff_delay(&cfg, u32::MAX).as_secs(), 300, "no overflow");
    }

    #[test]
    fn resolve_frame_path_rules() {
        let media_dir = PathBuf::from("/data/media");
        let ok = resolve_frame_path(
            &media_dir,
            "media/frames/2026/11/04/090005_MONITOR1_abc123.jpg",
        )
        .expect("valid frame path");
        assert_eq!(
            ok,
            PathBuf::from("/data/media/frames/2026/11/04/090005_MONITOR1_abc123.jpg")
        );
        for bad in [
            "media/screen/2026/11/04/x.jpg",
            "media/frames/2026/11/04/x.png",
            "media/frames/../../etc/passwd.jpg",
            "media/frames/2026/11/04/../04/x.jpg",
            "media/frames/2026/11/04/sub dir/x.jpg",
            "/etc/passwd",
            "media/frames/2026/11/04/",
        ] {
            assert!(
                resolve_frame_path(&media_dir, bad).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn hour_key_uses_the_frames_own_offset() {
        let ts = DateTime::parse_from_rfc3339("2026-11-04T09:00:05+08:00").expect("ts");
        assert_eq!(hour_key(&ts), "2026-11-04T09");
        let utc = DateTime::parse_from_rfc3339("2026-11-04T01:30:00Z").expect("ts");
        assert_eq!(hour_key(&utc), "2026-11-04T01");
    }
}
