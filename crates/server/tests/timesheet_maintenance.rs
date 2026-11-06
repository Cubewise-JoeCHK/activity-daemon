//! Integration tests for the question debris cleanup (issues #66 + #68):
//! the regeneration guard (`POST /api/timesheet/draft` closes superseded
//! questions and reports `questions_closed`) and the one-off maintenance
//! endpoint (`POST /api/timesheet/questions/maintenance`, idempotent).
//! The fixtures mirror the pilot debris patterns: zero-length
//! "HH:MM-HH:MM" compiler questions, analysis-worker instant questions -
//! one covered by the recompiled drafts, one still grey - and the issue
//! #68 stale pre-segmentation asks (minute-truncated session asks inside
//! an open stretch question's span, and session asks a voted segment
//! absorbed).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use activity_core::EventEnvelope;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use refiner_timesheet::{
    EvidenceFrame, StatusFilter, TimesheetConfig, TsQuestionStatus, TsQuestionStore,
};
use serde_json::{json, Value};
use server::{build_router, AppState, AuthConfig};
use tower::ServiceExt;

/// Self-cleaning temp directory (the db and media root live in here).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("activity-server-ts-maintenance-{tag}-{nanos}"));
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
    // Heuristic titling: no Lemonade in CI, no network in tests.
    let timesheet = TimesheetConfig {
        use_llm_titles: false,
        ..TimesheetConfig::default()
    };
    let state = AppState::open_with_retention(
        &dir.0.join("activity.db"),
        dir.0.join("media"),
        AuthConfig::disabled(),
        server::retention::RetentionConfig::default(),
        timesheet,
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

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

fn post(uri: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

/// Seed device events through the real ingest path.
async fn seed_events(app: &axum::Router, device: &str, events: Vec<EventEnvelope>) {
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/ingest/events")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "device": device, "events": events }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK, "ingest must accept");
}

fn session(hour: u32, dur_s: f64, app_name: &str) -> EventEnvelope {
    EventEnvelope {
        bucket: "window-watcher".to_string(),
        timestamp: format!("2026-11-06T{hour:02}:00:00+08:00"),
        duration_s: dur_s,
        data: json!({ "app": app_name, "title": "window title" }),
    }
}

fn analysis(
    hour: u32,
    minute: u32,
    project: &str,
    category: &str,
    confidence: f64,
) -> EventEnvelope {
    EventEnvelope {
        bucket: "frame-analysis".to_string(),
        timestamp: format!("2026-11-06T{hour:02}:{minute:02}:00+08:00"),
        duration_s: 0.0,
        data: json!({
            "frame_sha16": format!("sha-{hour:02}{minute:02}"),
            "frame_path": format!("media/screen/2026/11/06/f{hour}{minute}.jpg"),
            "activity": format!("working on {project}"),
            "project_guess": project,
            "category": category,
            "confidence": confidence,
            "evidence": [],
            "model": "qwen",
            "sampled": true,
        }),
    }
}

/// Seed a question through the same store handle the workers use.
fn seed_question(dir: &TempDir, day: &str, question: &str, frames: &[EvidenceFrame]) -> i64 {
    let store = TsQuestionStore::open(&dir.0.join("activity.db")).expect("seed store");
    store
        .create_question(day, question, frames)
        .expect("seed create")
}

fn frame(ts: &str) -> EvidenceFrame {
    EvidenceFrame {
        frame_path: "media/frames/2026/11/06/213403_DISPLAY7_6a17e7484e59b1b9.jpg".to_string(),
        // Distinct from every seeded analysis sha, so the question never
        // couples to a session through the compiler's frame arm.
        frame_sha16: "aaaabbbbccccdddd".to_string(),
        ts: ts.to_string(),
    }
}

fn debris_text(start: &str, app: &str) -> String {
    format!("What were you working on {start}-{start} ({app})?")
}

fn worker_text(ts: &str) -> String {
    format!("What were you working on at {ts}? (auto-detected: Creating a new project in the IDE; confidence 0.30)")
}

/// The pilot-like raw day: two resolved sessions (09:00-09:30 and
/// 10:00-11:00, each with a 0.9 analysis inside), nothing else.
async fn seed_pilot_day(app: &axum::Router) {
    seed_events(
        app,
        "laptop",
        vec![
            session(9, 1800.0, "Code.exe"),
            session(10, 3600.0, "Code.exe"),
            analysis(9, 15, "C1234", "Consulting", 0.9),
            analysis(10, 30, "C1234", "Consulting", 0.9),
        ],
    )
    .await;
}

/// The pilot debris mix: `n` zero-length compiler questions ("08:MM"),
/// an analysis-worker instant question covered by the 09:00-09:30 draft,
/// and one whose 11:45 instant stays grey.
fn seed_pilot_questions(dir: &TempDir, debris_count: usize) {
    for minute in 0..debris_count {
        seed_question(
            dir,
            "2026-11-06",
            &debris_text(&format!("08:{minute:02}"), "explorer.exe"),
            &[],
        );
    }
    seed_question(
        dir,
        "2026-11-06",
        &worker_text("2026-11-06T09:15:00+08:00"),
        &[frame("2026-11-06T09:15:00+08:00")],
    );
    seed_question(
        dir,
        "2026-11-06",
        &worker_text("2026-11-06T11:45:00+08:00"),
        &[frame("2026-11-06T11:45:00+08:00")],
    );
}

// ------------------------------------------- regeneration closes superseded

#[tokio::test]
async fn regeneration_closes_superseded_questions_and_reports_the_count() {
    let (dir, state) = test_state("regen-guard");
    let app = build_router(state);
    seed_pilot_day(&app).await;
    // The debris exists BEFORE the recompile - the pilot's situation.
    seed_pilot_questions(&dir, 20);
    // An owner answer wins over supersession (answered rows never close).
    let answered_id = seed_question(
        &dir,
        "2026-11-06",
        "What were you working on 09:05-09:20 (Legacy.exe)?",
        &[],
    );
    let store = TsQuestionStore::open(&dir.0.join("activity.db")).expect("answer store");
    store
        .answer_question(answered_id, "legacy work")
        .expect("answer");

    let res = app
        .clone()
        .oneshot(post("/api/timesheet/draft?date=2026-11-06"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let payload = body_json(res).await;
    // 20 zero-length debris + the covered 09:15 instant; the 11:45 grey
    // instant and the answered question stay.
    assert_eq!(payload["questions_closed"].as_i64().expect("closed"), 21);
    assert_eq!(
        payload["questions_created"].as_i64().expect("created"),
        0,
        "both sessions resolve - nothing new asked"
    );
    assert_eq!(payload["drafts"].as_array().expect("drafts").len(), 2);

    // The owner's open queue: only the still-grey instant.
    let res = app
        .clone()
        .oneshot(get("/api/timesheet/questions"))
        .await
        .expect("oneshot");
    let open = body_json(res).await;
    let open_items = open["questions"].as_array().expect("questions");
    assert_eq!(open_items.len(), 1, "single-digit open queue");
    assert!(open_items[0]["question"]
        .as_str()
        .expect("text")
        .contains("11:45:00"));

    // The answered queue is untouched by supersession; the superseded
    // rows surface only in the audit (all) view.
    let res = app
        .clone()
        .oneshot(get("/api/timesheet/questions?status=answered"))
        .await
        .expect("oneshot");
    let answered_list = body_json(res).await;
    assert_eq!(
        answered_list["questions"]
            .as_array()
            .expect("questions")
            .len(),
        1
    );
    let superseded_count = store
        .list_questions(StatusFilter::All)
        .expect("all")
        .iter()
        .filter(|q| q.status == TsQuestionStatus::Superseded)
        .count();
    assert_eq!(superseded_count, 21, "audit view keeps the closed rows");
    drop(dir);
}

// ------------------------------------------- maintenance endpoint

#[tokio::test]
async fn maintenance_cleans_existing_rows_and_is_idempotent() {
    let (dir, state) = test_state("maintenance");
    let app = build_router(state);
    // The hub's real sequence: drafts already exist (the v0.2.2
    // recompile), the debris questions were left by the earlier compile.
    seed_pilot_day(&app).await;
    let res = app
        .clone()
        .oneshot(post("/api/timesheet/draft?date=2026-11-06"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    seed_pilot_questions(&dir, 30);
    // A wide span fully covered by the 09:00-09:30 draft closes too; a
    // partially covered one (09:10-09:40 vs the same draft) stays open.
    seed_question(
        &dir,
        "2026-11-06",
        "What were you working on 09:05-09:20 (Legacy.exe)?",
        &[],
    );
    seed_question(
        &dir,
        "2026-11-06",
        "What were you working on 09:10-09:40 (Legacy.exe)?",
        &[],
    );

    let res = app
        .clone()
        .oneshot(post("/api/timesheet/questions/maintenance?date=2026-11-06"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let payload = body_json(res).await;
    // 30 debris + the covered 09:15 instant + the fully covered wide
    // span; the grey 11:45 instant and the partially covered wide span
    // stay open.
    assert_eq!(payload["closed_ids"].as_array().expect("ids").len(), 32);
    assert_eq!(payload["open_remaining"].as_i64().expect("remaining"), 2);

    // Idempotent: the re-run closes nothing and the queue is unchanged.
    let res = app
        .clone()
        .oneshot(post("/api/timesheet/questions/maintenance?date=2026-11-06"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let rerun = body_json(res).await;
    assert_eq!(
        rerun["closed_ids"].as_array().expect("ids").len(),
        0,
        "second run is a no-op"
    );
    assert_eq!(rerun["open_remaining"].as_i64().expect("remaining"), 2);

    let res = app
        .oneshot(get("/api/timesheet/questions"))
        .await
        .expect("oneshot");
    let open = body_json(res).await;
    assert_eq!(open["questions"].as_array().expect("questions").len(), 2);
    drop(dir);
}

// ------------------------------------- stale pre-segmentation asks (#68)

#[tokio::test]
async fn maintenance_supersedes_stale_pre_segmentation_asks() {
    // The pilot pattern (issue #68): a voted block absorbed a session a
    // stale ask still asks about (its minute-truncated span is NOT draft
    // covered - the seconds never line up), and a grey stretch raised the
    // wide question that owns its span. The maintenance endpoint closes
    // both stale asks and leaves the stretch question open, idempotently.
    // The stretch spans two sessions, so its question text matches no
    // single session's text and rule (c) never touches it.
    let (dir, state) = test_state("maintenance-stale");
    let app = build_router(state);
    // The voted block: 09:00:01-09:29:59 (Code.exe) with a 0.9 analysis
    // inside - the ask text is the session's deterministic text.
    seed_events(
        &app,
        "laptop",
        vec![
            EventEnvelope {
                bucket: "window-watcher".to_string(),
                timestamp: "2026-11-06T09:00:01+08:00".to_string(),
                duration_s: 1798.0,
                data: json!({ "app": "Code.exe", "title": "window title" }),
            },
            // The grey stretch: 15:19:37-15:31:12 (Chrome.exe) across
            // two adjacent sessions, no analyses - never drafted, asked
            // as one wide question (like the pilot's stretches).
            EventEnvelope {
                bucket: "window-watcher".to_string(),
                timestamp: "2026-11-06T15:19:37+08:00".to_string(),
                duration_s: 300.0,
                data: json!({ "app": "Chrome.exe", "title": "window title" }),
            },
            EventEnvelope {
                bucket: "window-watcher".to_string(),
                timestamp: "2026-11-06T15:24:37+08:00".to_string(),
                duration_s: 395.0,
                data: json!({ "app": "Chrome.exe", "title": "window title" }),
            },
            analysis(9, 15, "C1234", "Consulting", 0.9),
        ],
    )
    .await;
    // First compile: drafts + the stretch question; nothing to close yet.
    let res = app
        .clone()
        .oneshot(post("/api/timesheet/draft?date=2026-11-06"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let payload = body_json(res).await;
    assert_eq!(payload["questions_created"].as_i64().expect("created"), 1);
    assert_eq!(payload["questions_closed"].as_i64().expect("closed"), 0);

    // The pre-segmentation debris: the block session's own ask (voted
    // segment absorbed it) and a minute ask inside the stretch.
    seed_question(
        &dir,
        "2026-11-06",
        "What were you working on 09:00-09:29 (Code.exe)?",
        &[],
    );
    seed_question(
        &dir,
        "2026-11-06",
        "What were you working on 15:20-15:24 (Chrome.exe)?",
        &[],
    );

    let res = app
        .clone()
        .oneshot(post("/api/timesheet/questions/maintenance?date=2026-11-06"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let payload = body_json(res).await;
    assert_eq!(payload["closed_ids"].as_array().expect("ids").len(), 2);
    assert_eq!(payload["open_remaining"].as_i64().expect("remaining"), 1);

    // Idempotent: the re-run closes nothing.
    let res = app
        .clone()
        .oneshot(post("/api/timesheet/questions/maintenance?date=2026-11-06"))
        .await
        .expect("oneshot");
    let rerun = body_json(res).await;
    assert_eq!(
        rerun["closed_ids"].as_array().expect("ids").len(),
        0,
        "second run is a no-op"
    );
    assert_eq!(rerun["open_remaining"].as_i64().expect("remaining"), 1);

    // The stretch question stays: it owns the grey time.
    let res = app
        .oneshot(get("/api/timesheet/questions"))
        .await
        .expect("oneshot");
    let open = body_json(res).await;
    let open_items = open["questions"].as_array().expect("questions");
    assert_eq!(open_items.len(), 1);
    assert!(open_items[0]["question"]
        .as_str()
        .expect("text")
        .contains("15:19-15:31"));
    drop(dir);
}

#[tokio::test]
async fn maintenance_validates_the_day_parameter() {
    let (_dir, state) = test_state("maintenance-day");
    let app = build_router(state);
    let res = app
        .oneshot(post("/api/timesheet/questions/maintenance?date=2026-11-6"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    drop(_dir);
}
