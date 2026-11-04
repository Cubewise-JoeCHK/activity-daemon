//! Integration tests hitting the router via tower's oneshot test utility.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use activity_core::Db;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
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
    test_state_auth(tag, AuthConfig::disabled())
}

fn test_state_auth(tag: &str, auth: AuthConfig) -> (TempDir, AppState) {
    let dir = TempDir::new(tag);
    let state = AppState::open_with_auth(&dir.0.join("activity.db"), dir.0.join("media"), auth)
        .expect("test app state");
    (dir, state)
}

async fn body_json(res: axum::response::Response) -> serde_json::Value {
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

fn post_json(uri: &str, value: &serde_json::Value) -> Request<Body> {
    post_json_with_headers(uri, value, &[])
}

fn post_json_with_headers(
    uri: &str,
    value: &serde_json::Value,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    builder
        .body(Body::from(value.to_string()))
        .expect("request")
}

fn put(uri: &str) -> Request<Body> {
    put_with_headers(uri, &[])
}

fn put_with_headers(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method("PUT").uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    builder
        .body(Body::from("fake-media-bytes"))
        .expect("request")
}

fn good_event(i: usize) -> serde_json::Value {
    json!({
        "bucket": "window-watcher",
        "timestamp": "2026-11-04T09:14:32+08:00",
        "duration_s": 1.0 * i as f64,
        "data": { "app": "Code.exe", "i": i }
    })
}

#[tokio::test]
async fn health_reports_ok_and_event_count() {
    let (dir, state) = test_state("health");
    let app = build_router(state);
    let res = app.clone().oneshot(get("/health")).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], "ok");
    // Version feeds the hub updater (#32): matches the workspace version.
    assert_eq!(v["version"], server::VERSION);
    assert_eq!(v["events_count"], 0);

    // seed one event through the API, count follows
    let body = json!({
        "device": "dev-laptop",
        "events": [good_event(1)]
    });
    let res = app
        .clone()
        .oneshot(post_json("/ingest/events", &body))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let res = app.clone().oneshot(get("/health")).await.expect("oneshot");
    let v = body_json(res).await;
    assert_eq!(v["events_count"], 1);
    drop(dir);
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
        .oneshot(post_json("/ingest/events", &body))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["accepted"], 2);
    assert_eq!(v["rejected"].as_array().expect("rejected").len(), 0);

    // Verify against the store through a fresh connection.
    let db = Db::open_db(&dir.0.join("activity.db")).expect("reopen db");
    assert_eq!(db.event_count().expect("count"), 2);
    // The hub marks ingested rows synced.
    assert_eq!(db.unsynced_count().expect("unsynced"), 0);
    let counts = db.counts_by_bucket().expect("counts");
    assert_eq!(counts.len(), 2);
}

#[tokio::test]
async fn ingest_events_partial_acceptance_reports_rejections() {
    let (dir, state) = test_state("events-partial");
    let app = build_router(state);
    let bad = json!({
        "bucket": "window-watcher",
        "timestamp": "2026-11-04T09:14:32", // missing timezone offset
        "duration_s": 1.0,
        "data": {}
    });
    let body = json!({
        "device": "dev-laptop",
        "events": [good_event(0), bad, good_event(2)]
    });
    let res = app
        .oneshot(post_json("/ingest/events", &body))
        .await
        .expect("oneshot");
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "200 while anything was accepted"
    );
    let v = body_json(res).await;
    assert_eq!(v["accepted"], 2);
    let rejected = v["rejected"].as_array().expect("rejected");
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0]["index"], 1);
    assert!(!rejected[0]["reason"].as_str().expect("reason").is_empty());

    let db = Db::open_db(&dir.0.join("activity.db")).expect("reopen db");
    assert_eq!(db.event_count().expect("count"), 2);
}

#[tokio::test]
async fn ingest_events_all_rejected_is_400_with_reasons() {
    let (_dir, state) = test_state("events-allbad");
    let app = build_router(state);
    let body = json!({
        "device": "dev-laptop",
        "events": [
            { "bucket": "", "timestamp": "2026-11-04T09:14:32+08:00", "duration_s": 1.0, "data": {} },
            { "bucket": "x", "timestamp": "nope", "duration_s": 1.0, "data": {} }
        ]
    });
    let res = app
        .oneshot(post_json("/ingest/events", &body))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let v = body_json(res).await;
    assert_eq!(v["accepted"], 0);
    assert_eq!(v["rejected"].as_array().expect("rejected").len(), 2);
}

#[tokio::test]
async fn ingest_events_unparseable_json_is_400() {
    let (_dir, state) = test_state("events-badjson");
    let app = build_router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/ingest/events")
                .header("content-type", "application/json")
                .body(Body::from("{not json"))
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ingest_events_empty_batch_is_400() {
    let (_dir, state) = test_state("events-empty");
    let app = build_router(state);
    let body = json!({ "device": "dev-laptop", "events": [] });
    let res = app
        .oneshot(post_json("/ingest/events", &body))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ingest_events_rejects_empty_device() {
    let (_dir, state) = test_state("events-bad");
    let app = build_router(state);
    let body = json!({ "device": "", "events": [] });
    let res = app
        .oneshot(post_json("/ingest/events", &body))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ingest_events_stores_media_index_rows_as_plain_events() {
    // The PUT endpoint only stores bytes; media index rows arrive here like
    // any other event (file first, index row after).
    let (dir, state) = test_state("events-media-index");
    let app = build_router(state);
    let body = json!({
        "device": "dev-laptop",
        "events": [ {
            "bucket": "media",
            "timestamp": "2026-11-04T01:14:32Z",
            "duration_s": 0.0,
            "data": {
                "kind": "screen-frame",
                "path": "media/screen/2026/11/04/frame-1.jpg",
                "bytes": 15,
                "sha16": "a1b2c3d4e5f60718",
                "producer": "watcher-screen"
            }
        } ]
    });
    let res = app
        .oneshot(post_json("/ingest/events", &body))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let db = Db::open_db(&dir.0.join("activity.db")).expect("reopen db");
    let events = db.query_events("media", None, None).expect("query");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].data["path"],
        "media/screen/2026/11/04/frame-1.jpg"
    );
}

#[tokio::test]
async fn media_put_stores_file_in_dated_layout() {
    let (dir, state) = test_state("media");
    let app = build_router(state);
    let res = app
        .oneshot(put("/ingest/media/screen/2026-11-04/frame_001.jpg"))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["stored"], "media/screen/2026/11/04/frame_001.jpg");
    assert_eq!(v["bytes"], "fake-media-bytes".len() as u64);

    let stored = dir
        .0
        .join("media")
        .join("screen")
        .join("2026")
        .join("11")
        .join("04")
        .join("frame_001.jpg");
    let content = std::fs::read(&stored).expect("stored file");
    assert_eq!(content, b"fake-media-bytes");
}

#[tokio::test]
async fn media_put_verifies_sha256_and_deletes_on_mismatch() {
    let (dir, state) = test_state("media-sha");
    let app = build_router(state);
    let good = "f524f7ac9d9733793e107295719abe1ebf17f5c9de2afae0fc8af7c725d6d4ab"; // sha256("fake-media-bytes")

    // matching hash is accepted
    let res = app
        .clone()
        .oneshot(put_with_headers(
            "/ingest/media/screen/2026-11-04/ok.jpg",
            &[("X-SHA256", good)],
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let stored_ok = dir.0.join("media/screen/2026/11/04/ok.jpg");
    assert!(stored_ok.exists());

    // uppercase hex is normalized
    let res = app
        .clone()
        .oneshot(put_with_headers(
            "/ingest/media/screen/2026-11-04/upper.jpg",
            &[("X-SHA256", &good.to_ascii_uppercase())],
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    let _ = std::fs::remove_file(dir.0.join("media/screen/2026/11/04/upper.jpg"));

    // wrong hash deletes the file and returns 400
    let res = app
        .clone()
        .oneshot(put_with_headers(
            "/ingest/media/screen/2026-11-04/bad.jpg",
            &[("X-SHA256", &"0".repeat(64))],
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(
        !dir.0.join("media/screen/2026/11/04/bad.jpg").exists(),
        "mismatched upload must be deleted"
    );

    // malformed header is rejected outright
    let res = app
        .oneshot(put_with_headers(
            "/ingest/media/screen/2026-11-04/nothex.jpg",
            &[("X-SHA256", "zz")],
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn media_put_is_idempotent_overwrite() {
    let (dir, state) = test_state("media-repeat");
    let app = build_router(state);
    for _ in 0..2 {
        let res = app
            .clone()
            .oneshot(put("/ingest/media/screen/2026-11-04/frame.jpg"))
            .await
            .expect("oneshot");
        assert_eq!(res.status(), StatusCode::OK);
    }
    let content = std::fs::read(dir.0.join("media/screen/2026/11/04/frame.jpg")).expect("file");
    assert_eq!(content, b"fake-media-bytes");
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

    // traversal-shaped but route-matching inputs are sanitized too
    for uri in [
        "/ingest/media/../2026-11-04/frame.jpg",     // kind = ".."
        "/ingest/media/screen/2026-11-04/..jpg",     // filename "..jpg" is fine, dots allowed
        "/ingest/media/screen/0000-00-00/frame.jpg", // numeric-only date passes shape checks
    ] {
        let res = app.clone().oneshot(put(uri)).await.expect("oneshot");
        let status = res.status();
        if uri.contains("/../") {
            assert_eq!(status, StatusCode::BAD_REQUEST, "uri {uri}");
        } else {
            assert_eq!(status, StatusCode::OK, "uri {uri}");
        }
    }
}

#[tokio::test]
async fn token_auth_required_when_configured() {
    let (_dir, state) = test_state_auth(
        "auth-token",
        AuthConfig::from_env(Some("s3cret".to_string()), false),
    );
    let app = build_router(state);
    let body = json!({
        "device": "dev-laptop",
        "events": [good_event(0)]
    });

    // no header -> 401
    let res = app
        .clone()
        .oneshot(post_json("/ingest/events", &body))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // wrong header -> 401
    let res = app
        .clone()
        .oneshot(post_json_with_headers(
            "/ingest/events",
            &body,
            &[("X-Activity-Token", "wrong")],
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // correct header -> accepted
    let res = app
        .clone()
        .oneshot(post_json_with_headers(
            "/ingest/events",
            &body,
            &[("X-Activity-Token", "s3cret")],
        ))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);

    // health stays open for probes without the token
    let res = app.oneshot(get("/health")).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn token_auth_not_required_when_absent() {
    let (_dir, state) = test_state("auth-open");
    let app = build_router(state);
    let body = json!({
        "device": "dev-laptop",
        "events": [good_event(0)]
    });
    let res = app
        .oneshot(post_json("/ingest/events", &body))
        .await
        .expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
}
