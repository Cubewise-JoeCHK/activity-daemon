//! Integration tests for the daily itinerary compiler API (issue #45):
//! `GET/POST /api/timesheet/draft?date=YYYY-MM-DD`, following the
//! `timesheet_questions.rs` oneshot pattern. Events are seeded the way the
//! devices deliver them: through `POST /ingest/events`.
//!
//! Timesheet config pins `use_llm_titles = false` so the compile runs are
//! hermetic (the heuristic titler needs no network); the LLM path's
//! fail-soft behavior is covered by the refiner-timesheet unit tests.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use activity_core::EventEnvelope;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use refiner_timesheet::TimesheetConfig;
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
        let dir = std::env::temp_dir().join(format!("activity-server-ts-draft-{tag}-{nanos}"));
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

const DAY_URI: &str = "/api/timesheet/draft?date=2026-11-06";

#[tokio::test]
async fn compile_merge_roundtrip_get_and_post() {
    let (_dir, state) = test_state("roundtrip");
    let app = build_router(state);

    // Two adjacent Code.exe sessions, both classified C1234/Consulting
    // with high confidence: they merge into ONE draft activity.
    seed_events(
        &app,
        "windows-client",
        vec![
            session(9, 3600.0, "Code.exe"),
            session(10, 3600.0, "Code.exe"),
            analysis(9, 30, "C1234", "Consulting", 0.9),
            analysis(10, 30, "C1234", "Consulting", 0.85),
        ],
    )
    .await;

    // POST = regenerate: one merged draft, no questions needed.
    let res = app.clone().oneshot(post(DAY_URI)).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["day"], "2026-11-06");
    assert_eq!(v["status"]["count"], 1);
    assert_eq!(v["questions_created"], 0);
    assert_eq!(v["superseded"], 0);
    let drafts = v["drafts"].as_array().expect("drafts");
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0]["start"], "2026-11-06T09:00:00+08:00");
    assert_eq!(drafts[0]["end"], "2026-11-06T11:00:00+08:00");
    assert_eq!(drafts[0]["project_guess"], "C1234");
    assert_eq!(drafts[0]["category"], "Consulting");
    assert_eq!(
        drafts[0]["sources"]["session_ids"]
            .as_array()
            .expect("ids")
            .len(),
        2
    );
    assert_eq!(
        drafts[0]["sources"]["frame_sha16s"]
            .as_array()
            .expect("shas")
            .len(),
        2
    );
    assert!(!drafts[0]["task"].as_str().expect("task").is_empty());
    assert!(drafts[0]["batch"].as_str().is_some());
    assert!(drafts[0]["confidence"].as_f64().is_some());

    // GET = the same current drafts, status carries the batch stamp.
    let res = app.clone().oneshot(get(DAY_URI)).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["status"]["count"], 1);
    assert_eq!(
        v["status"]["generated_at"],
        drafts[0]["batch"].as_str().expect("batch")
    );
    assert_eq!(v["drafts"].as_array().expect("drafts").len(), 1);

    // POST again = regeneration: old draft superseded, still one current.
    let res = app.clone().oneshot(post(DAY_URI)).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["status"]["count"], 1, "status.count covers current rows");
    assert_eq!(v["superseded"], 1);
    // The payload exposes the replaced row as `superseded` (issue #60)
    // alongside the fresh current one. (Batch stamps are
    // millisecond-resolution and can collide on quick regenerations -
    // the row id is the collision-free proof of a new draft.)
    let all = v["drafts"].as_array().expect("drafts");
    assert_eq!(all.len(), 2, "current + superseded listed");
    assert!(all.iter().any(|d| d["state"] == "superseded"));
    let current = all
        .iter()
        .find(|d| d["state"] != "superseded")
        .expect("current draft");
    assert_ne!(
        current["id"].as_i64().expect("new draft id"),
        drafts[0]["id"].as_i64().expect("old draft id")
    );
}

#[tokio::test]
async fn low_confidence_span_becomes_a_question_item() {
    let (_dir, state) = test_state("question");
    let app = build_router(state);

    seed_events(
        &app,
        "windows-client",
        vec![
            session(14, 1800.0, "Photoshop.exe"),
            analysis(14, 10, "?", "?", 0.3),
        ],
    )
    .await;

    let res = app.clone().oneshot(post(DAY_URI)).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["status"]["count"], 0, "unresolvable span is no draft");
    assert_eq!(v["questions_created"], 1);

    // The question landed in the issue #44 queue for the same day.
    let res = app
        .clone()
        .oneshot(get("/api/timesheet/questions?status=open"))
        .await
        .expect("oneshot");
    let v = body_json(res).await;
    let questions = v["questions"].as_array().expect("questions");
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["day"], "2026-11-06");
    assert!(questions[0]["question"]
        .as_str()
        .expect("text")
        .contains("14:00-14:30"));
    assert_eq!(questions[0]["evidence"][0]["frame_sha16"], "sha-1410");

    // Regenerating does not duplicate the open question.
    let res = app.clone().oneshot(post(DAY_URI)).await.expect("oneshot");
    let v = body_json(res).await;
    assert_eq!(v["questions_created"], 0);
}

#[tokio::test]
async fn afk_between_same_project_sessions_prevents_merge() {
    let (_dir, state) = test_state("afk");
    let app = build_router(state);

    seed_events(
        &app,
        "windows-client",
        vec![
            session(9, 3600.0, "Code.exe"),
            session(11, 3600.0, "Code.exe"),
            EventEnvelope {
                bucket: "afk-watcher".to_string(),
                timestamp: "2026-11-06T10:00:00+08:00".to_string(),
                duration_s: 3600.0,
                data: json!({ "reason": "idle" }),
            },
            analysis(9, 30, "C1234", "Consulting", 0.9),
            analysis(11, 30, "C1234", "Consulting", 0.9),
        ],
    )
    .await;

    let res = app.clone().oneshot(post(DAY_URI)).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["status"]["count"], 2, "AFK breaks adjacency");
}

#[tokio::test]
async fn empty_day_yields_no_drafts_and_bad_dates_are_rejected() {
    let (_dir, state) = test_state("empty");
    let app = build_router(state);

    // A day with no events: valid query, nothing to report.
    let res = app
        .clone()
        .oneshot(get("/api/timesheet/draft?date=2026-11-01"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["status"]["count"], 0);
    assert_eq!(v["status"]["generated_at"], Value::Null);
    assert_eq!(v["drafts"].as_array().expect("drafts").len(), 0);

    // Non-canonical dates are a bad request on both verbs.
    for uri in [
        "/api/timesheet/draft?date=2026-11-6",
        "/api/timesheet/draft?date=not-a-day",
        "/api/timesheet/draft?date=",
    ] {
        let res = app.clone().oneshot(get(uri)).await.expect("oneshot");
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{uri}");
        let res = app.clone().oneshot(post(uri)).await.expect("oneshot");
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{uri}");
    }
}
