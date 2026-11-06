//! Integration tests for the Timesheet AI question queue (issue #44):
//! `GET /api/timesheet/questions`, `GET .../{id}/evidence/{n}` and
//! `POST .../{id}/answer`, following the `web_ui.rs` oneshot pattern.
//! Questions are seeded the way the analysis worker (issue #43) will
//! create them: through a `TsQuestionStore` connection to the hub store.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use refiner_timesheet::{EvidenceFrame, TsQuestionStore};
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
        let dir = std::env::temp_dir().join(format!("activity-server-ts-questions-{tag}-{nanos}"));
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

/// Seed through the same store handle the analysis worker will use (a
/// dedicated connection to the hub store file).
fn seed_question(dir: &TempDir, day: &str, question: &str, frames: &[EvidenceFrame]) -> i64 {
    let store = TsQuestionStore::open(&dir.0.join("activity.db")).expect("seed store");
    store
        .create_question(day, question, frames)
        .expect("seed create")
}

fn frame(path: &str) -> EvidenceFrame {
    EvidenceFrame {
        frame_path: path.to_string(),
        frame_sha16: "a1b2c3d4e5f60718".to_string(),
        ts: "2026-11-06T09:14:32+08:00".to_string(),
    }
}

// ------------------------------------------------------------- GET list

#[tokio::test]
async fn empty_queue_lists_no_questions() {
    let (dir, state) = test_state("empty");
    let app = build_router(state);
    let res = app
        .oneshot(get("/api/timesheet/questions"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["questions"].as_array().expect("questions").len(), 0);
    drop(dir);
}

#[tokio::test]
async fn list_filters_by_status_newest_first_with_evidence_urls() {
    let (dir, state) = test_state("filters");
    let app = build_router(state);
    let first = seed_question(
        &dir,
        "2026-11-04",
        "q1",
        &[frame("media/screen/2026/11/04/f1.jpg")],
    );
    let second = seed_question(&dir, "2026-11-05", "q2", &[]);
    let third = seed_question(
        &dir,
        "2026-11-06",
        "q3",
        &[frame("media/screen/2026/11/06/f3.jpg")],
    );

    // answer the middle question through the API (also covers the POST)
    let res = app
        .clone()
        .oneshot(post_json(
            &format!("/api/timesheet/questions/{second}/answer"),
            &json!({ "answer": "it was the build" }),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);

    // default filter is open, newest first, evidence carries the URL
    let res = app
        .clone()
        .oneshot(get("/api/timesheet/questions"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    let ids: Vec<i64> = v["questions"]
        .as_array()
        .expect("questions")
        .iter()
        .map(|q| q["id"].as_i64().expect("id"))
        .collect();
    assert_eq!(ids, vec![third, first]);
    assert_eq!(v["questions"][0]["status"], "open");
    assert_eq!(
        v["questions"][0]["evidence"][0]["url"],
        format!("/api/timesheet/questions/{third}/evidence/0")
    );
    assert_eq!(
        v["questions"][0]["evidence"][0]["frame_path"],
        "media/screen/2026/11/06/f3.jpg"
    );
    assert_eq!(ids.len(), 2);

    // answered filter; the answered question was seeded without frames and
    // lists an empty evidence array
    let res = app
        .clone()
        .oneshot(get("/api/timesheet/questions?status=answered"))
        .await
        .expect("oneshot");
    let v = body_json(res).await;
    let answered = v["questions"].as_array().expect("questions");
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0]["id"], second);
    assert_eq!(answered[0]["status"], "answered");
    assert_eq!(answered[0]["answer"], "it was the build");
    assert!(answered[0]["answered_at"].as_str().is_some());
    assert_eq!(
        answered[0]["evidence"].as_array().expect("evidence").len(),
        0
    );

    // all filter, newest first
    let res = app
        .clone()
        .oneshot(get("/api/timesheet/questions?status=all"))
        .await
        .expect("oneshot");
    let v = body_json(res).await;
    let ids: Vec<i64> = v["questions"]
        .as_array()
        .expect("questions")
        .iter()
        .map(|q| q["id"].as_i64().expect("id"))
        .collect();
    assert_eq!(ids, vec![third, second, first]);

    // unknown status value is a 400
    let res = app
        .oneshot(get("/api/timesheet/questions?status=maybe"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    drop(dir);
}

// ---------------------------------------------------------- GET evidence

#[tokio::test]
async fn evidence_serves_frame_bytes_from_media_root() {
    let (dir, state) = test_state("evidence");
    let app = build_router(state);

    // a fake "JPEG" frame on disk under the media root, at the exact path
    // the media convention names in the evidence row
    let frame_bytes: Vec<u8> = (0..64u8).collect();
    let on_disk = dir.0.join("media/screen/2026/11/06");
    std::fs::create_dir_all(&on_disk).expect("create frame dir");
    std::fs::write(on_disk.join("frame-000123.jpg"), &frame_bytes).expect("write frame");

    let id = seed_question(
        &dir,
        "2026-11-06",
        "What is on screen here?",
        &[frame("media/screen/2026/11/06/frame-000123.jpg")],
    );

    let res = app
        .clone()
        .oneshot(get(&format!("/api/timesheet/questions/{id}/evidence/0")))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .expect("content-type")
        .to_string();
    assert_eq!(content_type, "image/jpeg");
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    assert_eq!(bytes, frame_bytes);

    // acceptance criterion: answering keeps the evidence link working
    let res = app
        .clone()
        .oneshot(post_json(
            &format!("/api/timesheet/questions/{id}/answer"),
            &json!({ "answer": "the hub dashboard" }),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .oneshot(get(&format!("/api/timesheet/questions/{id}/evidence/0")))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK, "evidence survives the answer");
    drop(dir);
}

#[tokio::test]
async fn evidence_not_found_cases() {
    let (dir, state) = test_state("evidence-missing");
    let app = build_router(state);

    let known = seed_question(
        &dir,
        "2026-11-06",
        "q",
        &[
            frame("media/screen/2026/11/06/never-written.jpg"),
            frame("media/../secret.jpg"),
        ],
    );

    // unknown question id
    let res = app
        .clone()
        .oneshot(get("/api/timesheet/questions/999/evidence/0"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // out-of-range frame index
    let res = app
        .clone()
        .oneshot(get(&format!("/api/timesheet/questions/{known}/evidence/5")))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // non-numeric path parameters are rejected by the router
    let res = app
        .clone()
        .oneshot(get("/api/timesheet/questions/not-a-number/evidence/0"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // frame file missing on disk (valid path, no bytes)
    let res = app
        .clone()
        .oneshot(get(&format!("/api/timesheet/questions/{known}/evidence/0")))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // traversal-shaped stored path never resolves - and nothing outside
    // the media root leaks even if a file exists there
    std::fs::write(dir.0.join("secret.jpg"), b"top secret").expect("write decoy");
    let res = app
        .oneshot(get(&format!("/api/timesheet/questions/{known}/evidence/1")))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body = body_text(res).await;
    assert!(
        !body.contains("top secret"),
        "media-root escape must not leak bytes"
    );
    drop(dir);
}

// --------------------------------------------------------- POST answer

#[tokio::test]
async fn answer_marks_answered_and_unknown_or_empty_answers_rejected() {
    let (dir, state) = test_state("answer");
    let app = build_router(state);
    let id = seed_question(&dir, "2026-11-06", "What was this?", &[]);

    // unknown id first (question still open)
    let res = app
        .clone()
        .oneshot(post_json(
            "/api/timesheet/questions/999/answer",
            &json!({ "answer": "late answer" }),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // empty answer (whitespace only) is a 400, question stays open
    let res = app
        .clone()
        .oneshot(post_json(
            &format!("/api/timesheet/questions/{id}/answer"),
            &json!({ "answer": "   " }),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // the real answer: 200 with the updated question payload
    let res = app
        .clone()
        .oneshot(post_json(
            &format!("/api/timesheet/questions/{id}/answer"),
            &json!({ "answer": "  triaging the flaky test  " }),
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["id"], id);
    assert_eq!(v["status"], "answered");
    assert_eq!(v["answer"], "triaging the flaky test", "answer is trimmed");
    assert!(v["answered_at"].as_str().is_some());

    // visible under the answered filter
    let res = app
        .oneshot(get("/api/timesheet/questions?status=answered"))
        .await
        .expect("oneshot");
    let v = body_json(res).await;
    assert_eq!(v["questions"][0]["id"], id);
    drop(dir);
}

#[tokio::test]
async fn health_is_untouched_by_the_question_queue() {
    let (dir, state) = test_state("health");
    let app = build_router(state);
    let res = app.oneshot(get("/health")).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], "ok");
    drop(dir);
}
