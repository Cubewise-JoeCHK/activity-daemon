//! Sync contract v2 integration tests (issue #40): the real client
//! forwarder (`activity_sync::sync_once` with the real `UreqClient`) driven
//! over loopback HTTP against the real hub router. Covers the
//! delete-on-ack media pass-through end to end: a verified ack deletes the
//! client file (hub holds the exact bytes, counters stay correct); an
//! unreachable hub - the same client path as a hub killed before the
//! durable write - keeps the file and the counters untouched.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use activity_core::{Db, EventEnvelope};
use activity_sync::{sync_once, SyncSettings, UreqClient};
use server::{build_router, AppState};

/// Self-cleaning temp directory (children of the test state live in here).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("activity-sync-contract-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Serve the hub router on a random loopback port.
async fn spawn_hub(state: AppState) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let app = build_router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("hub server stopped cleanly");
    });
    addr
}

fn insert_plain(db: &Db) {
    db.insert_event(
        &EventEnvelope {
            bucket: "window-watcher".to_string(),
            timestamp: "2026-11-04T09:14:32+08:00".to_string(),
            duration_s: 1.0,
            data: serde_json::json!({ "marker": "a" }),
        },
        "laptop",
    )
    .expect("insert plain event");
}

/// One unsynced media index row plus its on-disk file in the client tree.
fn seed_media(db: &Db, data_dir: &Path, bytes: &[u8]) -> PathBuf {
    let rel = "media/screen/2026/11/03/frame-1.jpg";
    let file = data_dir.join(rel);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, bytes).expect("write media file");
    db.insert_media_index(
        "screen",
        rel,
        bytes.len() as u64,
        "aa16bb16cc16dd16",
        "watcher-screen",
        "laptop",
    )
    .expect("insert media index");
    file
}

/// Multi-thread runtime: the forwarder's ureq calls are blocking, so the
/// hub's serve task needs its own worker (a current-thread runtime would
/// starve it until the 300 s request timeout).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verified_ack_deletes_client_file_and_counters_stay_correct() {
    let hub_dir = TempDir::new("hub");
    let client_dir = TempDir::new("client");
    let db = Db::open_in_memory().expect("client db");
    let state =
        AppState::open(&hub_dir.0.join("activity.db"), hub_dir.0.join("media")).expect("hub state");
    let addr = spawn_hub(state).await;

    insert_plain(&db);
    let bytes = b"jpeg-bytes-040";
    let client_file = seed_media(&db, &client_dir.0, bytes);
    assert_eq!(db.unsynced_count().expect("count"), 2);

    let settings = SyncSettings::new(&format!("http://127.0.0.1:{}", addr.port()), "laptop");
    let stats = sync_once(&UreqClient::new(), &settings, &db, &client_dir.0).expect("round");

    // unsynced counters: both rows uploaded/posted and marked synced
    assert_eq!(stats.uploaded_files, 1);
    assert_eq!(stats.accepted, 2);
    assert_eq!(db.unsynced_count().expect("count"), 0);
    // pass-through: the client file is gone ...
    assert!(
        !client_file.exists(),
        "a checksum-verified ack must delete the local media file"
    );
    // ... and the hub durably holds the exact bytes plus the index row.
    let hub_copy = hub_dir.0.join("media/screen/2026/11/03/frame-1.jpg");
    assert_eq!(std::fs::read(&hub_copy).expect("hub copy"), bytes);
    let hub_db = Db::open_db(&hub_dir.0.join("activity.db")).expect("hub db");
    assert_eq!(hub_db.event_count().expect("hub events"), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_hub_keeps_client_file_and_counters_untouched() {
    // Bind a port and release it: connections are refused, so the PUT gets
    // a transport error (no ack, nothing durably written) - the same client
    // path as a hub killed before the durable write.
    let dead_port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        listener.local_addr().expect("local addr").port()
    };
    let client_dir = TempDir::new("down");
    let db = Db::open_in_memory().expect("client db");
    insert_plain(&db);
    let client_file = seed_media(&db, &client_dir.0, b"jpeg-bytes");
    assert_eq!(db.unsynced_count().expect("count"), 2);

    let settings = SyncSettings::new(&format!("http://127.0.0.1:{dead_port}"), "laptop");
    let err = sync_once(&UreqClient::new(), &settings, &db, &client_dir.0)
        .expect_err("round against a dead hub must fail");
    assert_eq!(err.class(), activity_sync::RetryClass::Backoff);

    // no ack anywhere: file retained, nothing marked synced
    assert!(client_file.exists(), "without an ack the file must stay");
    assert_eq!(db.unsynced_count().expect("count"), 2);
}
