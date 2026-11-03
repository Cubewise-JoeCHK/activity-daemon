//! Integration tests hitting the router via tower's oneshot test utility.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use activity_core::Db;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use server::{build_router, AppState};
use tower::ServiceExt;

/// Self-cleaning temp directory (children of the test state live in here).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("activity-server-{tag}-{nanos}"));
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
    let state =
        AppState::open(&dir.0.join("activity.db"), dir.0.join("media")).expect("test app state");
    (dir, state)
}

#[tokio::test]
async fn health_reports_ok() {
    let (_dir, state) = test_state("health");
    let app = build_router(state);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(v["status"], "ok");
}

#[tokio::test]
async fn ingest_events_appends_and_marks_synced() {
    let (dir, state) = test_state("events");
    let app = build_router(state);
    let body = json!({
        "device": "dev-laptop",
        "events": [
            {
                "bucket": "window-watcher",
                "timestamp": "2026-11-04T09:14:32+08:00",
                "duration_s": 123.4,
                "data": { "app": "Code.exe", "title": "main.rs" }
            },
            {
                "bucket": "afk-watcher",
                "timestamp": "2026-11-04T09:20:00+08:00",
                "duration_s": 0.0,
                "data": { "state": "afk" }
            }
        ]
    });
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ingest/events")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(v["inserted"], 2);

    // Verify against the store through a fresh connection.
    let db = Db::open_db(&dir.0.join("activity.db")).expect("reopen db");
    assert_eq!(db.event_count().expect("count"), 2);
    // The hub marks ingested rows synced.
    assert_eq!(db.unsynced_count().expect("unsynced"), 0);
    let counts = db.counts_by_bucket().expect("counts");
    assert_eq!(counts.len(), 2);
}

#[tokio::test]
async fn media_put_stores_file_in_dated_layout() {
    let (dir, state) = test_state("media");
    let app = build_router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/ingest/media/screen/2026-11-04/frame_001.jpg")
                .body(Body::from("fake-jpeg-bytes"))
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);

    let stored = dir
        .0
        .join("media")
        .join("screen")
        .join("2026")
        .join("11")
        .join("04")
        .join("frame_001.jpg");
    let content = std::fs::read(&stored).expect("stored file");
    assert_eq!(content, b"fake-jpeg-bytes");
}

#[tokio::test]
async fn media_put_rejects_bad_inputs() {
    let (_dir, state) = test_state("media-bad");
    let app = build_router(state);

    for uri in [
        "/ingest/media/screen/not-a-date/frame_001.jpg",
        "/ingest/media/../../etc/passwd.jpg", // traversal via kind
        "/ingest/media/screen/2026-11-04/sub/frame.jpg", // unknown route
    ] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(uri)
                    .body(Body::from("x"))
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        assert!(
            res.status() == StatusCode::BAD_REQUEST || res.status() == StatusCode::NOT_FOUND,
            "uri {uri} should be rejected, got {}",
            res.status()
        );
    }
}

#[tokio::test]
async fn ingest_events_rejects_empty_device() {
    let (_dir, state) = test_state("events-bad");
    let app = build_router(state);
    let body = json!({ "device": "", "events": [] });
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ingest/events")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}
