//! The forwarder loop: fetch unsynced rows, upload media files first, post
//! the event batch, mark accepted rows synced. Media pass-through (sync
//! contract v2, issue #40): a media file is deleted from the client as soon
//! as the hub's PUT ack proves a durable, checksum-verified write; no ack,
//! a checksum mismatch, or any other error leaves the file untouched for
//! the retry. Errors are retried with exponential backoff plus jitter; the
//! shutdown flag is polled every 250 ms so the daemon (issue #7) can stop
//! the thread promptly.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use activity_core::Db;

use crate::http::{HttpClient, HttpError, RetryClass, UreqClient};
use crate::plan::{
    accepted_row_ids, ack_verifies_upload, apply_jitter, backoff_delay, build_post_batch, join_url,
    plan_rows, sha256_file_hex, IngestAck, MediaPutAck, PlannedRow, BACKOFF_BASE, MAX_BACKOFF,
};
use crate::settings::SyncSettings;

/// What one forwarder round accomplished.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SyncStats {
    pub uploaded_files: usize,
    pub posted_events: usize,
    pub accepted: usize,
    pub rejected: usize,
}

/// Forwarder failure, classified for the retry loop.
#[derive(Debug)]
pub enum SyncError {
    /// Hub interaction failed; classification lives in [`HttpError`].
    Http(HttpError),
    /// Local storage failed (corrupt row, sqlite error): wait one interval
    /// and retry - the data is still local and intact.
    Storage(String),
}

impl SyncError {
    pub fn class(&self) -> RetryClass {
        match self {
            SyncError::Http(e) => e.class(),
            SyncError::Storage(_) => RetryClass::SkipCycle,
        }
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Http(e) => write!(f, "{e}"),
            SyncError::Storage(msg) => write!(f, "local storage error: {msg}"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<activity_core::CoreError> for SyncError {
    fn from(e: activity_core::CoreError) -> Self {
        SyncError::Storage(e.to_string())
    }
}

/// Run the forwarder until `shutdown` is set, using a real HTTP client.
/// See the crate docs for the delivery contract.
pub fn run(
    settings: &SyncSettings,
    db: &Db,
    data_dir: &Path,
    shutdown: &AtomicBool,
) -> crate::Result<()> {
    let client = UreqClient::new();
    run_with_client(&client, settings, db, data_dir, shutdown)
}

/// [`run`] with an injected client (tests and exotic transports).
pub fn run_with_client(
    client: &dyn HttpClient,
    settings: &SyncSettings,
    db: &Db,
    data_dir: &Path,
    shutdown: &AtomicBool,
) -> crate::Result<()> {
    let mut backoff_attempt: u32 = 0;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        match sync_once(client, settings, db, data_dir) {
            Ok(stats) => {
                backoff_attempt = 0;
                if stats.posted_events < usize::try_from(settings.batch_size).unwrap_or(usize::MAX)
                {
                    // batch not full: wait a normal interval; when it was
                    // full, drain the backlog immediately
                    sleep_interruptible(settings.interval(), shutdown);
                }
            }
            Err(err) => match err.class() {
                RetryClass::SkipCycle => {
                    log::warn!("sync round skipped, will retry next interval: {err}");
                    sleep_interruptible(settings.interval(), shutdown);
                }
                RetryClass::Backoff => {
                    backoff_attempt = backoff_attempt.saturating_add(1);
                    let delay = apply_jitter(
                        backoff_delay(backoff_attempt, BACKOFF_BASE, MAX_BACKOFF),
                        rand01(),
                    );
                    log::warn!(
                        "hub unreachable (round failed: {err}); backing off {delay:?} (attempt {backoff_attempt})"
                    );
                    sleep_interruptible(delay, shutdown);
                }
            },
        }
    }
}

/// One forwarder round. Returns the delivery stats; an `Err` means the
/// round made no progress (and any files already uploaded will simply be
/// re-uploaded and overwritten by the next round).
pub fn sync_once(
    client: &dyn HttpClient,
    settings: &SyncSettings,
    db: &Db,
    data_dir: &Path,
) -> crate::Result<SyncStats> {
    let rows = db.fetch_unsynced_events(settings.batch_size)?;
    if rows.is_empty() {
        return Ok(SyncStats::default());
    }
    let planned = plan_rows(&rows, data_dir).map_err(SyncError::Storage)?;

    // (a) media files first; rows whose file cannot be delivered are
    // skipped this round (stay unsynced, retried later) and excluded from
    // the batch below.
    let mut uploaded_ids: Vec<i64> = Vec::new();
    for p in &planned {
        if let PlannedRow::Media {
            row_id,
            file,
            media,
            ..
        } = p
        {
            match upload_media_file(client, settings, file, media) {
                Ok(()) => uploaded_ids.push(*row_id),
                Err(err) => {
                    log::warn!("media file for row {row_id} not uploaded, will retry later: {err}")
                }
            }
        }
    }

    // (b) post the batch: plain events plus media events with delivered files.
    let posted = build_post_batch(&planned, &uploaded_ids);
    let mut stats = SyncStats {
        uploaded_files: uploaded_ids.len(),
        ..SyncStats::default()
    };
    if posted.is_empty() {
        return Ok(stats);
    }
    let events: Vec<activity_core::EventEnvelope> =
        posted.iter().map(|(_, e)| e.envelope()).collect();
    let url = join_url(&settings.server_url, "/ingest/events");
    let body = serde_json::json!({ "device": settings.device_name, "events": events });
    let resp_text = client.post_events(&url, &body).map_err(SyncError::Http)?;

    // (c) mark exactly what the server accepted; rejected rows stay
    // unsynced (and stay visible in the logs) - never silently dropped.
    let ack: IngestAck = serde_json::from_str(&resp_text).map_err(|e| {
        SyncError::Http(HttpError::transport(format!("unparseable ingest ack: {e}")))
    })?;
    let accepted_ids = accepted_row_ids(&posted, &ack);
    db.mark_synced(&accepted_ids)?;
    for rejected in &ack.rejected {
        let (id, _) = &posted[rejected.index];
        log::warn!(
            "row {id} rejected by hub (index {}): {}",
            rejected.index,
            rejected.reason
        );
    }
    stats.posted_events = posted.len();
    stats.accepted = accepted_ids.len();
    stats.rejected = ack.rejected.len();
    Ok(stats)
}

/// Hash and stream one media file to the hub, then apply the sync v2
/// pass-through (issue #40): on a verified ack - the hub fsynced exactly
/// these bytes and echoes their checksum - the local file is deleted
/// (targeted per-file removal). An ack whose checksum mismatches fails the
/// upload (the row stays unsynced and retries); a pre-v2 ack without a
/// checksum counts as uploaded but keeps the file (deletion requires a
/// verified checksum).
fn upload_media_file(
    client: &dyn HttpClient,
    settings: &SyncSettings,
    file: &Path,
    media: &crate::plan::MediaRef,
) -> std::result::Result<(), String> {
    let (_, sha_hex) =
        sha256_file_hex(file).map_err(|e| format!("cannot hash {}: {e}", file.display()))?;
    let handle =
        std::fs::File::open(file).map_err(|e| format!("cannot open {}: {e}", file.display()))?;
    let url = crate::plan::media_put_url(&settings.server_url, media);
    let body = client
        .put_media(&url, &sha_hex, Box::new(handle))
        .map_err(|e| e.to_string())?;
    let ack: MediaPutAck = serde_json::from_str(&body)
        .map_err(|e| format!("unparseable media ack from {url}: {e}"))?;
    if ack_verifies_upload(&ack, &sha_hex) {
        // Verified: the hub durably holds exactly these bytes - the client
        // stops being a media archive. A failed removal is logged, not
        // fatal: the hub copy exists, so the index row must still flow; the
        // rolling budget backstop will evict the leftover file later.
        if let Err(e) = std::fs::remove_file(file) {
            log::warn!(
                "media uploaded and acked, but {} could not be deleted: {e} - the rolling budget will evict it",
                file.display()
            );
        } else {
            log::info!(
                "media uploaded (ack verified), local copy deleted: {}",
                file.display()
            );
        }
    } else if ack.sha256.is_some() {
        // A checksum that does NOT match is never deletion clearance: fail
        // the upload so the row stays unsynced and retries.
        return Err(format!(
            "hub ack for {url} is not checksum-verified (ack sha {:?} vs local {sha_hex}) - file retained",
            ack.sha256
        ));
    } else {
        // Pre-v2 hub (ack without a checksum): the upload counts, the file
        // stays - an old hub must not stall media sync.
        log::info!(
            "media uploaded; hub ack carries no checksum (pre-v2 hub) - file retained: {}",
            file.display()
        );
    }
    Ok(())
}

/// Sleep in small slices so the shutdown flag takes effect promptly.
fn sleep_interruptible(total: Duration, shutdown: &AtomicBool) {
    let step = Duration::from_millis(250);
    let mut slept = Duration::ZERO;
    while slept < total && !shutdown.load(Ordering::Relaxed) {
        std::thread::sleep(step.min(total - slept));
        slept += step;
    }
}

/// Weak entropy source for jitter (sub-millisecond clock remainder); the
/// schedule itself comes from the pure [`backoff_delay`] function.
fn rand01() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1000) / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{parse_media_rel_path, to_hex, MediaRef};
    use activity_core::Db;
    use sha2::{Digest, Sha256};
    use std::io::Read;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Fake client that records call order and can fail per endpoint. The
    /// media PUT behaves like a sync-v2 hub: it hashes the received bytes
    /// and echoes their sha256 in the ack - unless an explicit ack body is
    /// forced (mismatch / pre-v2 ack cases).
    struct FakeClient {
        calls: Mutex<Vec<String>>,
        put_result: Mutex<Option<Result<String, HttpError>>>,
        post_result: Mutex<Option<Result<String, HttpError>>>,
        last_post_body: Mutex<Option<serde_json::Value>>,
        put_ack_body: Mutex<Option<String>>,
    }

    impl FakeClient {
        fn ok() -> FakeClient {
            FakeClient {
                calls: Mutex::new(Vec::new()),
                put_result: Mutex::new(None),
                post_result: Mutex::new(Some(Ok("{\"accepted\":2,\"rejected\":[]}".to_string()))),
                last_post_body: Mutex::new(None),
                put_ack_body: Mutex::new(None),
            }
        }

        fn with_post_result(res: Result<String, HttpError>) -> FakeClient {
            let c = FakeClient::ok();
            *c.post_result.lock().expect("lock") = Some(res);
            c
        }

        fn with_put_error(res: HttpError) -> FakeClient {
            let c = FakeClient::ok();
            *c.put_result.lock().expect("lock") = Some(Err(res));
            c
        }

        /// Force the media PUT ack body verbatim (wrong checksum, or a
        /// pre-v2 hub ack without a checksum).
        fn with_put_ack_body(body: &str) -> FakeClient {
            let c = FakeClient::ok();
            *c.put_ack_body.lock().expect("lock") = Some(body.to_string());
            c
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl HttpClient for FakeClient {
        fn post_events(&self, url: &str, body: &serde_json::Value) -> Result<String, HttpError> {
            self.calls.lock().expect("lock").push(format!("post {url}"));
            *self.last_post_body.lock().expect("lock") = Some(body.clone());
            self.post_result
                .lock()
                .expect("lock")
                .take()
                .unwrap_or_else(|| Ok("{\"accepted\":0,\"rejected\":[]}".to_string()))
        }

        fn put_media(
            &self,
            url: &str,
            sha256_hex: &str,
            mut reader: Box<dyn std::io::Read + '_>,
        ) -> Result<String, HttpError> {
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .map_err(|e| HttpError::transport(e.to_string()))?;
            self.calls
                .lock()
                .expect("lock")
                .push(format!("put {url} {sha256_hex}"));
            let ack = match &*self.put_ack_body.lock().expect("lock") {
                Some(forced) => forced.clone(),
                None => serde_json::json!({
                    "stored": "x",
                    "bytes": bytes.len(),
                    "sha256": to_hex(&Sha256::digest(&bytes)),
                })
                .to_string(),
            };
            self.put_result
                .lock()
                .expect("lock")
                .take()
                .unwrap_or(Ok(ack))
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activity-sync-fwd-{tag}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn insert_plain(db: &Db, marker: &str) -> i64 {
        db.insert_event(
            &activity_core::EventEnvelope {
                bucket: "window-watcher".to_string(),
                timestamp: "2026-11-04T09:14:32+08:00".to_string(),
                duration_s: 1.0,
                data: serde_json::json!({ "marker": marker }),
            },
            "laptop",
        )
        .expect("insert")
    }

    fn insert_media(db: &Db, rel: &str) -> i64 {
        db.insert_media_index(
            "screen",
            rel,
            5,
            "aa16bb16cc16dd16",
            "watcher-screen",
            "laptop",
        )
        .expect("insert media index")
    }

    fn stop_flag() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn full_round_uploads_file_before_post_and_marks_synced() {
        let dir = temp_dir("full");
        let db = Db::open_in_memory().expect("db");
        let plain = insert_plain(&db, "a");
        let rel = "media/screen/2026/11/03/frame-1.jpg";
        let media = insert_media(&db, rel);
        let file_path = dir.join(rel);
        std::fs::create_dir_all(file_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file_path, b"jpeg-bytes").expect("write file");

        let client = FakeClient::ok();
        let settings = SyncSettings::new("http://hub:8080", "laptop");
        let stats = sync_once(&client, &settings, &db, &dir).expect("sync round");

        // file before index row: the put happened first
        let calls = client.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(
            calls[0].starts_with("put http://hub:8080/ingest/media/screen/2026-11-03/frame-1.jpg")
        );
        assert!(calls[1].starts_with("post http://hub:8080/ingest/events"));
        // both events in one batch, device in the body
        let body = client
            .last_post_body
            .lock()
            .expect("lock")
            .clone()
            .expect("body");
        assert_eq!(body["device"], "laptop");
        assert_eq!(body["events"].as_array().expect("events").len(), 2);
        // both marked synced
        assert_eq!(db.unsynced_count().expect("count"), 0);
        assert_eq!(stats.accepted, 2);
        assert_eq!(stats.uploaded_files, 1);
        // sync contract v2 (#40): the verified ack deletes the local file
        assert!(
            !file_path.exists(),
            "checksum-verified ack must delete the local media file"
        );
        let _ = (plain, media);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn mismatched_checksum_ack_keeps_the_file_and_the_row_unsynced() {
        // A 200 ack carrying the WRONG checksum (broken middlebox / skew):
        // the client must not delete, and the row must stay unsynced for
        // retry - only a matching checksum is deletion clearance.
        let dir = temp_dir("mismatch");
        let db = Db::open_in_memory().expect("db");
        insert_plain(&db, "a");
        let rel = "media/screen/2026/11/03/frame-9.jpg";
        insert_media(&db, rel);
        let file_path = dir.join(rel);
        std::fs::create_dir_all(file_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file_path, b"jpeg-bytes").expect("write file");

        let forced = format!(
            r#"{{"stored":"x","bytes":10,"sha256":"{}"}}"#,
            "0".repeat(64)
        );
        let client = FakeClient::with_put_ack_body(&forced);
        let settings = SyncSettings::new("http://hub:8080", "laptop");
        let stats = sync_once(&client, &settings, &db, &dir).expect("round completes");

        assert!(
            file_path.exists(),
            "mismatched checksum ack must keep the file"
        );
        assert_eq!(stats.uploaded_files, 0, "mismatch counts as not uploaded");
        assert_eq!(
            db.unsynced_count().expect("count"),
            1,
            "media row stays unsynced, plain event delivered"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn v1_ack_without_checksum_delivers_the_event_but_keeps_the_file() {
        // Skew: a pre-v2 hub acks without a checksum. The upload counts and
        // the index row flows (the bytes were accepted), but the local file
        // is retained - deletion requires a verified checksum.
        let dir = temp_dir("v1ack");
        let db = Db::open_in_memory().expect("db");
        insert_plain(&db, "a");
        let rel = "media/screen/2026/11/03/frame-10.jpg";
        insert_media(&db, rel);
        let file_path = dir.join(rel);
        std::fs::create_dir_all(file_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file_path, b"jpeg-bytes").expect("write file");

        let client = FakeClient::with_put_ack_body(r#"{"stored":"x","bytes":10}"#);
        let settings = SyncSettings::new("http://hub:8080", "laptop");
        let stats = sync_once(&client, &settings, &db, &dir).expect("round completes");

        assert!(file_path.exists(), "unverified ack must keep the file");
        assert_eq!(stats.uploaded_files, 1);
        assert_eq!(db.unsynced_count().expect("count"), 0);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn missing_media_file_is_skipped_and_never_marked() {
        let dir = temp_dir("missing");
        let db = Db::open_in_memory().expect("db");
        insert_plain(&db, "a");
        insert_media(&db, "media/screen/2026/11/03/gone.jpg");

        let client = FakeClient::ok();
        let settings = SyncSettings::new("http://hub:8080", "laptop");
        let stats = sync_once(&client, &settings, &db, &dir).expect("sync round");

        // only the plain event went out; the media row stays unsynced
        let calls = client.calls();
        assert_eq!(calls.len(), 1, "no media PUT for a missing file: {calls:?}");
        assert!(calls[0].starts_with("post"));
        let body = client
            .last_post_body
            .lock()
            .expect("lock")
            .clone()
            .expect("body");
        assert_eq!(body["events"].as_array().expect("events").len(), 1);
        assert_eq!(
            db.unsynced_count().expect("count"),
            1,
            "media row stays unsynced"
        );
        assert_eq!(stats.uploaded_files, 0);
        assert_eq!(stats.accepted, 1);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn upload_failure_keeps_row_unsynced_but_batch_still_flows() {
        let dir = temp_dir("putfail");
        let db = Db::open_in_memory().expect("db");
        insert_plain(&db, "a");
        let rel = "media/screen/2026/11/03/broken.jpg";
        insert_media(&db, rel);
        let file_path = dir.join(rel);
        std::fs::create_dir_all(file_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file_path, b"x").expect("write");

        let client = FakeClient::with_put_error(HttpError::http(400, "sha mismatch"));
        let settings = SyncSettings::new("http://hub:8080", "laptop");
        let stats = sync_once(&client, &settings, &db, &dir).expect("sync round");

        assert_eq!(stats.uploaded_files, 0);
        assert_eq!(stats.accepted, 1); // plain event still delivered
        assert_eq!(
            db.unsynced_count().expect("count"),
            1,
            "media row stays unsynced"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn transport_failure_propagates_as_backoff_error() {
        let dir = temp_dir("transport");
        let db = Db::open_in_memory().expect("db");
        insert_plain(&db, "a");
        let client = FakeClient::with_post_result(Result::Err(HttpError::transport("refused")));
        let settings = SyncSettings::new("http://hub:8080", "laptop");
        let err = sync_once(&client, &settings, &db, &dir).expect_err("must fail");
        assert_eq!(err.class(), RetryClass::Backoff);
        assert_eq!(
            db.unsynced_count().expect("count"),
            1,
            "nothing marked on failure"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn rejected_rows_stay_unsynced() {
        let dir = temp_dir("rejected");
        let db = Db::open_in_memory().expect("db");
        insert_plain(&db, "a");
        insert_plain(&db, "b");
        let client = FakeClient::with_post_result(Ok(
            "{\"accepted\":1,\"rejected\":[{\"index\":1,\"reason\":\"bad timestamp\"}]}"
                .to_string(),
        ));
        let settings = SyncSettings::new("http://hub:8080", "laptop");
        let stats = sync_once(&client, &settings, &db, &dir).expect("sync round");
        assert_eq!(stats.accepted, 1);
        assert_eq!(stats.rejected, 1);
        assert_eq!(db.unsynced_count().expect("count"), 1);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn empty_local_queue_is_a_noop() {
        let dir = temp_dir("idle");
        let db = Db::open_in_memory().expect("db");
        let client = FakeClient::ok();
        let settings = SyncSettings::new("http://hub:8080", "laptop");
        let stats = sync_once(&client, &settings, &db, &dir).expect("sync round");
        assert_eq!(stats, SyncStats::default());
        assert!(client.calls().is_empty());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn run_loop_returns_immediately_when_shutdown_is_set() {
        let db = Db::open_in_memory().expect("db");
        let client = FakeClient::ok();
        let settings = SyncSettings::new("http://hub:8080", "laptop");
        let shutdown = stop_flag();
        shutdown.store(true, Ordering::Relaxed);
        let started = std::time::Instant::now();
        run_with_client(&client, &settings, &db, Path::new("."), &shutdown).expect("run");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn media_url_matches_server_route_shape() {
        let m: MediaRef = parse_media_rel_path("media/audio/2026/11/03/meet.m4a").expect("media");
        assert_eq!(
            crate::plan::media_put_url("http://hub:8080/", &m),
            "http://hub:8080/ingest/media/audio/2026-11-03/meet.m4a"
        );
    }
}
