//! Integration tests for the MinIO permanent tier (issue #39) against a
//! real MinIO (or any S3-compatible endpoint).
//!
//! Gated by environment variables - when they are unset the tests print a
//! skip note and pass, so plain `cargo test` stays green everywhere:
//!
//! | var                          | meaning                          |
//! |------------------------------|----------------------------------|
//! | `ACTIVITY_TEST_MINIO_ENDPOINT` | host[:port] of the S3 endpoint |
//! | `ACTIVITY_TEST_MINIO_BUCKET`   | existing test bucket           |
//! | `ACTIVITY_MINIO_ACCESS_KEY`    | access key (production name)   |
//! | `ACTIVITY_MINIO_SECRET_KEY`    | secret key (production name)   |
//!
//! The tests write only under the `test/` prefix and only ever upload -
//! nothing is deleted from the tier (issue #39 contract). Repeated runs
//! overwrite the same fixed keys (`test/2025-01.tar.zst`, ~1 KB) - the
//! idempotent-retry path the sweeper itself uses. The bucket must already
//! exist (`mc mb local/<bucket>`); the app never creates buckets.
//!
//! Coverage here is the real-network path the unit tests cannot reach:
//! move succeeds end to end, the verify-failure path keeps the hub copy
//! (corrupted fetches through a real upload), and a retry after an
//! unreachable endpoint succeeds.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use chrono::NaiveDate;
use futures::StreamExt;
use server::minio::{
    read_move_state, BundleTier, MinioConfig, MinioTier, MoveStatus, ObjectStream,
};
use server::retention::{self, RetentionConfig, RetentionStore};
use sha2::Digest as _;

/// `test/` prefix so integration runs never touch production keys.
const TEST_PREFIX: &str = "test";
/// Fixed cold month: bundle keys are deterministic across runs (overwrite =
/// the idempotent retry path).
const COLD_MONTH: &str = "2025-01";

struct TestEnv {
    tier: MinioTier,
    cfg: RetentionConfig,
}

impl TestEnv {
    fn from_env() -> Option<TestEnv> {
        let endpoint = std::env::var("ACTIVITY_TEST_MINIO_ENDPOINT").ok()?;
        let bucket = std::env::var("ACTIVITY_TEST_MINIO_BUCKET").ok()?;
        let config = MinioConfig {
            enabled: true,
            endpoint,
            bucket,
            prefix: TEST_PREFIX.to_string(),
            secure: false,
        };
        let tier = MinioTier::from_config(&config).ok()?;
        Some(TestEnv {
            tier,
            cfg: RetentionConfig {
                enabled: true,
                hot_days: 365,
                minio: config,
                ..RetentionConfig::default()
            },
        })
    }
}

/// Wraps a tier and corrupts one byte of every fetched body, so the
/// round-trip verification must fail against the real store.
struct CorruptingTier<'a>(&'a MinioTier);

impl BundleTier for CorruptingTier<'_> {
    async fn put_object(&self, key: &str, source: &Path) -> Result<(), String> {
        self.0.put_object(key, source).await
    }

    async fn get_object(&self, key: &str) -> Result<ObjectStream, String> {
        let mut data = self
            .0
            .get_object(key)
            .await?
            .next()
            .await
            .expect("at least one chunk")
            .expect("chunk is ok")
            .to_vec();
        data[0] ^= 0xff;
        let stream: ObjectStream =
            Box::pin(futures::stream::once(async move { Ok(Bytes::from(data)) }));
        Ok(stream)
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("activity-minio-it-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A store with `2025-01` bundled the retention way (raw + verified
/// archive on disk, raws pruned).
fn store_with_bundle(tag: &str) -> (TempDir, RetentionStore) {
    let dir = TempDir::new(tag);
    let media_dir = dir.0.join("data").join("media");
    let store = RetentionStore::from_media_dir(&media_dir);
    let raw = media_dir.join("frames").join("2025").join("01").join("15");
    std::fs::create_dir_all(&raw).expect("mkdir");
    std::fs::write(raw.join("120000_D1_a.jpg"), b"integration-frame-bytes").expect("write raw");
    let cfg = RetentionConfig {
        enabled: true,
        hot_days: 365,
        ..RetentionConfig::default()
    };
    let today = NaiveDate::from_ymd_opt(2026, 11, 7).expect("today");
    let report = retention::run_pass(&store, &cfg, today);
    assert_eq!(report.bundled, vec![COLD_MONTH.to_string()]);
    (dir, store)
}

fn object_key(name: &str) -> String {
    format!("{TEST_PREFIX}/{name}")
}

async fn fetched_sha256(tier: &MinioTier, key: &str) -> Result<String, String> {
    let mut stream = tier.get_object(key).await?;
    let mut hasher = sha2::Sha256::new();
    while let Some(chunk) = stream.next().await {
        hasher.update(&chunk.map_err(|e| e.to_string())?);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn skip_note() {
    eprintln!(
        "skipping minio tier integration tests - set ACTIVITY_TEST_MINIO_ENDPOINT, \
         ACTIVITY_TEST_MINIO_BUCKET, ACTIVITY_MINIO_ACCESS_KEY and ACTIVITY_MINIO_SECRET_KEY \
         to run against a real endpoint"
    );
}

#[tokio::test]
async fn real_move_roundtrip_verifies_and_removes_the_hub_copy() {
    let Some(env) = TestEnv::from_env() else {
        skip_note();
        return;
    };
    let (_dir, store) = store_with_bundle("roundtrip");

    let today = NaiveDate::from_ymd_opt(2026, 11, 7).expect("today");
    let report = server::minio::move_pass(&store, &env.cfg, &env.tier, today).await;
    assert!(
        report.failed.is_empty() && report.paused.is_none(),
        "unexpected failures: {report:?}"
    );
    assert_eq!(report.moved.len(), 1, "{report:?}");
    assert_eq!(report.moved[0].0, COLD_MONTH);

    // Hub copy (bundle + sidecar) removed only after the verified upload.
    assert!(!store.bundle_path(COLD_MONTH).exists());
    assert!(!store
        .bundles_dir
        .join(format!("{COLD_MONTH}.tar.zst{}", retention::SHA256_SUFFIX))
        .exists());

    // The stored object round-trips: its sha256 equals the state record.
    let state = read_move_state(&store.bundles_dir).expect("state");
    let entry = &state.months[COLD_MONTH];
    assert_eq!(entry.status, MoveStatus::Moved);
    assert_eq!(entry.key, object_key(&format!("{COLD_MONTH}.tar.zst")));
    assert_eq!(entry.bytes, report.moved[0].1);
    let remote_sha = fetched_sha256(&env.tier, &entry.key)
        .await
        .expect("fetch stored bundle");
    assert_eq!(
        remote_sha, entry.sha256,
        "stored bytes match the verified digest"
    );
    fetched_sha256(
        &env.tier,
        &object_key(&format!("{COLD_MONTH}.tar.zst{}", retention::SHA256_SUFFIX)),
    )
    .await
    .expect("sidecar stored alongside");
}

#[tokio::test]
async fn real_verify_failure_keeps_the_hub_copy_then_retry_moves() {
    let Some(env) = TestEnv::from_env() else {
        skip_note();
        return;
    };
    let (_dir, store) = store_with_bundle("verify-failure");

    // Corrupted fetches: the round trip must fail and the hub copy stays.
    let corrupting = CorruptingTier(&env.tier);
    let report = server::minio::move_pass(
        &store,
        &env.cfg,
        &corrupting,
        NaiveDate::from_ymd_opt(2026, 11, 7).expect("today"),
    )
    .await;
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(report.failed[0].1.contains("sha256 mismatch"));
    assert!(store.bundle_path(COLD_MONTH).exists(), "hub copy kept");

    // Restored fetches: the retry moves the month.
    let report = server::minio::move_pass(
        &store,
        &env.cfg,
        &env.tier,
        NaiveDate::from_ymd_opt(2026, 11, 7).expect("today"),
    )
    .await;
    assert_eq!(report.moved.len(), 1, "{report:?}");
    assert!(!store.bundle_path(COLD_MONTH).exists());
    let state = read_move_state(&store.bundles_dir).expect("state");
    assert_eq!(state.months[COLD_MONTH].status, MoveStatus::Moved);
    assert_eq!(state.months[COLD_MONTH].attempts, 2);
}

#[tokio::test]
async fn real_unreachable_endpoint_fails_then_a_restored_one_moves() {
    let Some(env) = TestEnv::from_env() else {
        skip_note();
        return;
    };
    let (_dir, store) = store_with_bundle("unreachable");

    // An endpoint nothing listens on: every upload fails (fast, local),
    // the hub copy stays, and the failure is recorded.
    let down_cfg = RetentionConfig {
        enabled: true,
        hot_days: 365,
        minio: MinioConfig {
            enabled: true,
            endpoint: "127.0.0.1:1".to_string(),
            bucket: env.cfg.minio.bucket.clone(),
            prefix: TEST_PREFIX.to_string(),
            secure: false,
        },
        ..RetentionConfig::default()
    };
    let down_tier = MinioTier::from_config(&down_cfg.minio)
        .expect("tier config (endpoint is unreachable, not invalid)");
    let report = server::minio::move_pass(
        &store,
        &down_cfg,
        &down_tier,
        NaiveDate::from_ymd_opt(2026, 11, 7).expect("today"),
    )
    .await;
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(store.bundle_path(COLD_MONTH).exists(), "hub copy kept");

    // The endpoint is restored: the retry (same keys, idempotent
    // overwrite) moves the month.
    let report = server::minio::move_pass(
        &store,
        &env.cfg,
        &env.tier,
        NaiveDate::from_ymd_opt(2026, 11, 7).expect("today"),
    )
    .await;
    assert_eq!(report.moved.len(), 1, "{report:?}");
    assert!(!store.bundle_path(COLD_MONTH).exists());
    let state = read_move_state(&store.bundles_dir).expect("state");
    assert_eq!(state.months[COLD_MONTH].status, MoveStatus::Moved);
}
