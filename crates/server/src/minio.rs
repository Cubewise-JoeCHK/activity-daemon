//! MinIO permanent tier for retention bundles (issue #39): verified
//! bundles move to an S3-compatible object store (`blob.joechowhk.internal`,
//! MinIO) once their month is cold, and the hub-local copy is deleted only
//! after the round trip (size + sha256 of the fetched object) verifies.
//!
//! Contract (owner decision v1.2, 2026-11-06):
//! - Nothing is ever deleted from MinIO by this feature. The only overwrite
//!   is the idempotent re-upload of the same month's key on retry. Aborting
//!   an *in-progress* multipart upload on failure drops orphaned parts of an
//!   object that never completed - it never touches a stored object.
//! - Upload failure / verify failure keep the hub-local bundle, record the
//!   failure in the move state, and retry on the next daily tick (the tick
//!   cadence is the backoff; there is no tight retry loop).
//! - Move state lives in `bundles/minio-state.json` (atomic temp+rename,
//!   same pattern as the bundles themselves). A month's state is written
//!   *before* its hub-local copy is removed, so a crash anywhere in between
//!   is completed by the next pass ([`move_pass`] finalizes interrupted
//!   moves). If the state file is unreadable, both stages pause and keep
//!   everything local: the state file is what proves the remote copy
//!   exists, so it must never be guessed at. Recovery: fix or delete the
//!   file (deleting it makes the next pass re-upload every local bundle;
//!   overwrites are idempotent and verified).
//! - Credentials come from the environment (`ACTIVITY_MINIO_ACCESS_KEY` /
//!   `ACTIVITY_MINIO_SECRET_KEY`), never from the config file or the repo.
//!   Anything user-facing ([`MinioStats`], log lines) carries endpoint and
//!   bucket names only.
//! - Layout: `s3://<bucket>/<prefix>/<YYYY-MM>.tar.zst` plus the `.sha256`
//!   sidecar alongside. The app performs the move (no MinIO ILM/lifecycle
//!   rule) so the audit trail stays in one place; decision recorded on the
//!   issue.
//!
//! The move pass runs right after the bundling pass on the daily tick
//! ([`crate::retention`]), is idempotent, and its outcome surfaces through
//! `GET /api/retention` (per-bundle tier + last move result).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::future::Future;
use std::io;
use std::io::Write as _;
use std::path::Path;
use std::pin::Pin;

use bytes::Bytes;
use chrono::{Local, NaiveDate};
use futures::{Stream, StreamExt};
use object_store::{ObjectStore as _, ObjectStoreExt as _};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::retention::{
    self, is_cold, parse_month, RetentionConfig, RetentionStore, SHA256_SUFFIX,
};

/// Move state file name inside the bundles directory.
const STATE_FILE: &str = "minio-state.json";
/// Chunk size for multipart uploads (S3 minimum part size is 5 MiB; 16 MiB
/// keeps the part count low for multi-GB bundles while bounding memory).
const UPLOAD_PART_BYTES: usize = 16 * 1024 * 1024;

// ------------------------------------------------------------------- config

/// `[retention.minio]` section of the server config file. Defaults keep the
/// stage disabled: bundles stay hub-local until the owner opts in. The
/// credentials are never here - they come from the environment
/// (`ACTIVITY_MINIO_ACCESS_KEY` / `ACTIVITY_MINIO_SECRET_KEY`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MinioConfig {
    /// Master switch for the move stage.
    pub enabled: bool,
    /// S3 endpoint host[:port] (e.g. `blob.joechowhk.internal`). Path-style
    /// addressing is always used (MinIO convention).
    pub endpoint: String,
    /// Destination bucket (e.g. `activity-archive`).
    pub bucket: String,
    /// Key prefix under the bucket (default `bundles`; empty means no
    /// prefix). Segment of the object key, not a filesystem path.
    pub prefix: String,
    /// TLS on/off (default true). The devbox `*.joechowhk.internal` vhosts
    /// are plain HTTP by design today, so production starts with
    /// `secure = false` inside the WARP boundary.
    pub secure: bool,
}

impl Default for MinioConfig {
    fn default() -> Self {
        MinioConfig {
            enabled: false,
            endpoint: String::new(),
            bucket: String::new(),
            prefix: "bundles".to_string(),
            secure: true,
        }
    }
}

impl MinioConfig {
    /// Semantic validation beyond serde: an enabled section must name an
    /// endpoint and a bucket, and a non-empty prefix must be usable as an
    /// S3 key path (only non-empty name segments, no `.`/`..` traversal).
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.endpoint.trim().is_empty() {
            return Err("retention.minio: enabled but endpoint is empty".to_string());
        }
        if self.bucket.trim().is_empty() {
            return Err("retention.minio: enabled but bucket is empty".to_string());
        }
        if !self.prefix.is_empty() {
            for segment in self.prefix.split('/') {
                if segment.is_empty() || segment == "." || segment == ".." {
                    return Err(format!(
                        "retention.minio: invalid prefix {:?} (use slash-separated name segments)",
                        self.prefix
                    ));
                }
            }
        }
        Ok(())
    }

    /// Object key for a bundle file name (`<YYYY-MM>.tar.zst` or its
    /// `.sha256` sidecar) under the configured prefix.
    fn object_key(&self, file_name: &str) -> String {
        if self.prefix.is_empty() {
            file_name.to_string()
        } else {
            format!("{}/{}", self.prefix, file_name)
        }
    }
}

// -------------------------------------------------------------- tier client

/// A streamed object body: chunks of raw bytes, or an IO error mid-stream.
pub type ObjectStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

/// One object in the permanent tier, as [`move_pass`] needs it: upload a
/// local file under `key`, then fetch the stored body back for round-trip
/// verification. Implementations must not offer deletion - the contract is
/// that this feature never deletes from the tier.
pub trait BundleTier: Send + Sync {
    /// Upload `source` as `key` (overwrite allowed - the same month's key
    /// is re-uploaded idempotently on retry).
    fn put_object(
        &self,
        key: &str,
        source: &Path,
    ) -> impl Future<Output = Result<(), String>> + Send;
    /// Fetch the stored body of `key` for verification.
    fn get_object(&self, key: &str) -> impl Future<Output = Result<ObjectStream, String>> + Send;
}

/// Production tier: MinIO (or any S3-compatible store) via `object_store`,
/// path-style addressing. Constructed per move pass from config + env
/// credentials; connection errors are move failures (local copies kept).
pub struct MinioTier {
    store: object_store::aws::AmazonS3,
}

impl MinioTier {
    /// Build the tier from config. Credentials are read from the
    /// environment here - missing ones are an error naming the variables
    /// (fail-safe: the move stage skips the tick, local copies stay).
    pub fn from_config(cfg: &MinioConfig) -> Result<MinioTier, String> {
        let access_key = env_credential("ACTIVITY_MINIO_ACCESS_KEY")?;
        let secret_key = env_credential("ACTIVITY_MINIO_SECRET_KEY")?;
        let scheme = if cfg.secure { "https" } else { "http" };
        let store = object_store::aws::AmazonS3Builder::new()
            .with_endpoint(format!("{}://{}", scheme, cfg.endpoint.trim()))
            .with_bucket_name(cfg.bucket.trim())
            .with_access_key_id(access_key)
            .with_secret_access_key(secret_key)
            .with_allow_http(!cfg.secure)
            .with_region("us-east-1")
            .build()
            .map_err(|e| format!("retention.minio: bad config: {e}"))?;
        Ok(MinioTier { store })
    }
}

/// Read one credential variable; empty/unset is an error naming it (the
/// value itself never appears anywhere).
fn env_credential(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(format!(
            "retention.minio: environment variable {name} is not set - cannot authenticate"
        )),
    }
}

impl BundleTier for MinioTier {
    async fn put_object(&self, key: &str, source: &Path) -> Result<(), String> {
        let location = object_store::path::Path::from(key.to_string());
        let mut upload = self
            .store
            .put_multipart_opts(&location, object_store::PutMultipartOptions::default())
            .await
            .map_err(|e| format!("minio: begin upload {key}: {e}"))?;
        let outcome = upload_parts(&mut upload, key, source).await;
        if let Err(reason) = outcome {
            // Drop the orphaned parts of the incomplete upload; a completed
            // object is never touched by this.
            let _ = upload.abort().await;
            return Err(reason);
        }
        Ok(())
    }

    async fn get_object(&self, key: &str) -> Result<ObjectStream, String> {
        let location = object_store::path::Path::from(key.to_string());
        let result = self
            .store
            .get(&location)
            .await
            .map_err(|e| format!("minio: fetch {key}: {e}"))?;
        let stream = result
            .into_stream()
            .map(|chunk| chunk.map_err(|e| io::Error::other(e.to_string())));
        Ok(Box::pin(stream))
    }
}

/// Stream `source` into the multipart upload in fixed-size parts. Filling
/// each buffer fully (except the last) keeps every part above the S3
/// 5 MiB minimum.
async fn upload_parts(
    upload: &mut impl object_store::MultipartUpload,
    key: &str,
    source: &Path,
) -> Result<(), String> {
    let mut file = tokio::fs::File::open(source)
        .await
        .map_err(|e| format!("minio: open {}: {e}", source.display()))?;
    let mut buffer = vec![0u8; UPLOAD_PART_BYTES];
    loop {
        let filled = read_up_to(&mut file, &mut buffer)
            .await
            .map_err(|e| format!("minio: read {}: {e}", source.display()))?;
        if filled == 0 {
            break;
        }
        upload
            .put_part(object_store::PutPayload::from(buffer[..filled].to_vec()))
            .await
            .map_err(|e| format!("minio: upload part of {key}: {e}"))?;
    }
    upload
        .complete()
        .await
        .map_err(|e| format!("minio: complete upload {key}: {e}"))?;
    Ok(())
}

/// Fill `buffer` as far as the file allows; returns 0 only at EOF.
async fn read_up_to(file: &mut tokio::fs::File, buffer: &mut [u8]) -> io::Result<usize> {
    use tokio::io::AsyncReadExt;
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]).await? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

// ---------------------------------------------------------------- move state

/// What the last move attempt for a month did. `moved` proves the remote
/// copy exists (the sha256 was verified by round trip); `hot` with an
/// error means the bundle is still hub-only and will be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MoveStatus {
    Hot,
    Moved,
}

/// Per-month move state (one entry per month the stage ever attempted).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonthMoveState {
    pub status: MoveStatus,
    /// Object key of the uploaded bundle (when moved).
    #[serde(default)]
    pub key: String,
    /// sha256 of the verified bundle bytes (when moved).
    #[serde(default)]
    pub sha256: String,
    /// Bundle size in bytes (when moved).
    #[serde(default)]
    pub bytes: u64,
    /// RFC3339 instant of the verified move.
    #[serde(default)]
    pub moved_at: Option<String>,
    /// RFC3339 instant of the last attempt (success or failure).
    #[serde(default)]
    pub last_attempt: Option<String>,
    /// Failure reason of the last failed attempt; cleared on success.
    #[serde(default)]
    pub last_error: Option<String>,
    /// How many attempts this month has consumed (observability).
    #[serde(default)]
    pub attempts: u64,
}

/// `bundles/minio-state.json`: the move ledger. This file is the proof a
/// remote copy exists; see the module docs for the failure policy.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MoveState {
    pub version: u32,
    pub months: BTreeMap<String, MonthMoveState>,
}

impl MoveState {
    fn record_attempt(&mut self, month: &str, now: &str) -> &mut MonthMoveState {
        let entry = self
            .months
            .entry(month.to_string())
            .or_insert_with(|| MonthMoveState {
                status: MoveStatus::Hot,
                key: String::new(),
                sha256: String::new(),
                bytes: 0,
                moved_at: None,
                last_attempt: None,
                last_error: None,
                attempts: 0,
            });
        entry.attempts += 1;
        entry.last_attempt = Some(now.to_string());
        entry
    }
}

/// Load the move state. A missing file means "no moves yet"; an unreadable
/// or unparsable file is an error - callers must pause the stage instead of
/// guessing (see module docs).
pub fn read_move_state(bundles_dir: &Path) -> Result<MoveState, String> {
    let path = bundles_dir.join(STATE_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(MoveState::default()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    serde_json::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

/// Write the move state atomically (temp file + rename), mirroring the
/// bundle write pattern.
fn write_move_state(bundles_dir: &Path, state: &MoveState) -> Result<(), String> {
    let state = MoveState {
        version: 1,
        ..state.clone()
    };
    let path = bundles_dir.join(STATE_FILE);
    let part = bundles_dir.join(format!("{STATE_FILE}{PART_SUFFIX_PLACEHOLDER}"));
    let text = serde_json::to_string_pretty(&state)
        .map_err(|e| format!("cannot serialize move state: {e}"))?;
    std::fs::create_dir_all(bundles_dir)
        .map_err(|e| format!("cannot create {}: {e}", bundles_dir.display()))?;
    std::fs::write(&part, text).map_err(|e| format!("cannot write {}: {e}", part.display()))?;
    std::fs::rename(&part, &path).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        format!("cannot finalize {}: {e}", path.display())
    })
}

/// Suffix of the in-progress state file; mirrors retention's `.part`.
const PART_SUFFIX_PLACEHOLDER: &str = ".part";

/// Months with a verified remote copy, sorted ascending.
fn moved_months(state: &MoveState) -> Vec<&str> {
    state
        .months
        .iter()
        .filter(|(_, entry)| entry.status == MoveStatus::Moved)
        .map(|(month, _)| month.as_str())
        .collect()
}

/// Months the bundling stage must not touch (their archive already lives in
/// the permanent tier). `Err` means the state file is unreadable: the caller
/// pauses rather than guessing.
pub fn moved_months_guard(bundles_dir: &Path) -> Result<BTreeSet<String>, String> {
    read_move_state(bundles_dir).map(|state| {
        moved_months(&state)
            .into_iter()
            .map(str::to_string)
            .collect()
    })
}

// ------------------------------------------------------------------ move pass

/// What one move pass did, for logging and tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MoveReport {
    /// Months newly uploaded + round-trip verified (local copy removed).
    pub moved: Vec<(String, u64)>,
    /// Months whose verified upload had already been recorded and whose
    /// leftover hub-local copy was now removed (crash completion).
    pub finalized: Vec<String>,
    /// (month, reason) - bundle kept hub-local, retried next tick.
    pub failed: Vec<(String, String)>,
    /// When set, the stage paused entirely this tick (unreadable state).
    pub paused: Option<String>,
}

impl MoveReport {
    /// Log lines for the daily tick.
    pub fn log_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(reason) = &self.paused {
            lines.push(format!("retention.minio: paused - {reason}"));
        }
        for (month, bytes) in &self.moved {
            lines.push(format!(
                "retention.minio: moved {} to the permanent tier ({}, verified sha256)",
                month,
                retention::human_bytes(*bytes)
            ));
        }
        for month in &self.finalized {
            lines.push(format!(
                "retention.minio: finalized {month} (verified upload was already recorded; removed leftover hub copy)"
            ));
        }
        for (month, reason) in &self.failed {
            lines.push(format!(
                "retention.minio: kept {month} hub-local - {reason} (retry next tick)"
            ));
        }
        lines
    }
}

/// Bundle file names in the bundles directory shaped `<YYYY-MM>.tar.zst`,
/// sorted ascending. Anything else (state file, sidecars, `.part`) is
/// ignored.
fn local_bundle_months(bundles_dir: &Path) -> Vec<String> {
    let mut months = Vec::new();
    let Ok(entries) = std::fs::read_dir(bundles_dir) else {
        return months;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".tar.zst") else {
            continue;
        };
        if parse_month(stem).is_some() {
            months.push(stem.to_string());
        }
    }
    months.sort();
    months
}

/// Execute one move pass: upload every cold month's verified bundle to the
/// permanent tier, round-trip-verify (size + sha256 of the fetched object),
/// and only then remove the hub-local copy. Idempotent and crash-safe.
pub async fn move_pass<T: BundleTier>(
    store: &RetentionStore,
    cfg: &RetentionConfig,
    tier: &T,
    today: NaiveDate,
) -> MoveReport {
    let mut report = MoveReport::default();
    if !cfg.minio.enabled {
        return report;
    }
    let mut state = match read_move_state(&store.bundles_dir) {
        Ok(state) => state,
        Err(reason) => {
            report.paused = Some(reason);
            return report;
        }
    };

    // 1. Complete interrupted moves: the verified upload is already
    //    recorded, only the local copy removal is missing. When the local
    //    digest differs from the recorded one the local bundle holds
    //    unexpected content - keep it and flag instead of deleting blindly.
    for month in moved_months(&state) {
        let bundle = store.bundle_path(month);
        if !bundle.exists() {
            continue;
        }
        let recorded = state
            .months
            .get(month)
            .and_then(|entry| (!entry.sha256.is_empty()).then_some(entry.sha256.as_str()));
        let mismatch = match recorded {
            Some(expected) => match retention::sha256_file(&bundle) {
                Ok(actual) if actual != expected => Some(actual),
                Ok(_) => None,
                Err(reason) => return paused_reason(report, reason),
            },
            None => None,
        };
        if mismatch.is_some() {
            report.failed.push((
                month.to_string(),
                "verified upload is recorded but the hub copy's digest differs - kept for inspection"
                    .to_string(),
            ));
            continue;
        }
        remove_local_copy(store, month);
        report.finalized.push(month.to_string());
    }

    // 2. Move cold months whose verified bundle is still hub-local.
    let already_moved: BTreeSet<String> = moved_months(&state)
        .into_iter()
        .map(str::to_string)
        .collect();
    for month in local_bundle_months(&store.bundles_dir) {
        if !is_cold(&month, today, cfg.hot_days) || already_moved.contains(month.as_str()) {
            continue;
        }
        let now = Local::now().to_rfc3339();
        let bundle = store.bundle_path(&month);

        // Never upload an archive that does not verify structurally.
        if let Err(reason) = retention::verify_bundle(store, &month) {
            let entry = state.record_attempt(&month, &now);
            entry.last_error = Some(format!("bundle verify failed: {reason}"));
            report
                .failed
                .push((month, format!("bundle verify failed: {reason}")));
            continue;
        }
        let digest = match retention::sha256_file(&bundle) {
            Ok(digest) => digest,
            Err(reason) => return paused_reason(report, reason),
        };
        let bytes = match bundle.metadata() {
            Ok(meta) => meta.len(),
            Err(reason) => return paused_reason(report, reason.to_string()),
        };
        let key = cfg.minio.object_key(&format!("{month}.tar.zst"));

        if let Err(reason) = tier.put_object(&key, &bundle).await {
            let entry = state.record_attempt(&month, &now);
            entry.last_error = Some(reason.clone());
            report.failed.push((month, reason));
            continue;
        }
        // The .sha256 sidecar goes alongside (regenerated if missing) so
        // the tier copy can be re-checked without unpacking. A sidecar
        // failure fails the month like any other - all or nothing.
        if let Err(reason) = write_digest_sidecar(&bundle, &digest) {
            let entry = state.record_attempt(&month, &now);
            entry.last_error = Some(reason.clone());
            report.failed.push((month, reason));
            continue;
        }
        let sidecar_name = format!("{month}.tar.zst{SHA256_SUFFIX}");
        let sidecar = bundle.with_file_name(&sidecar_name);
        if let Err(reason) = tier
            .put_object(&cfg.minio.object_key(&sidecar_name), &sidecar)
            .await
        {
            let reason = format!("sidecar upload failed: {reason}");
            let entry = state.record_attempt(&month, &now);
            entry.last_error = Some(reason.clone());
            report.failed.push((month, reason));
            continue;
        }
        if let Err(reason) = verify_round_trip(tier, &key, &digest, bytes).await {
            // The stored object may be wrong, but deleting from the tier is
            // out of contract: the next successful retry overwrites the key
            // with the correct bytes. The hub copy stays authoritative.
            let entry = state.record_attempt(&month, &now);
            entry.last_error = Some(reason.clone());
            report.failed.push((month, reason));
            continue;
        }

        let entry = state.record_attempt(&month, &now);
        entry.status = MoveStatus::Moved;
        entry.key = key;
        entry.sha256 = digest;
        entry.bytes = bytes;
        entry.moved_at = Some(now);
        entry.last_error = None;
        if let Err(reason) = write_move_state(&store.bundles_dir, &state) {
            return paused_reason(report, reason);
        }
        remove_local_copy(store, &month);
        report.moved.push((month, bytes));
    }

    // Persist failure bookkeeping (attempts / last_error) from this pass.
    if let Err(reason) = write_move_state(&store.bundles_dir, &state) {
        return paused_reason(report, reason);
    }
    report
}

/// Record a local-IO failure as a full pause of the stage (the bundle state
/// on disk could not be read or written; keeping everything is fail-safe).
fn paused_reason(mut report: MoveReport, reason: String) -> MoveReport {
    report.paused = Some(reason);
    report
}

/// Stream the stored object back and check size + sha256 against the local
/// bundle's verified values.
async fn verify_round_trip<T: BundleTier>(
    tier: &T,
    key: &str,
    expected_sha256: &str,
    expected_bytes: u64,
) -> Result<(), String> {
    let mut stream = tier.get_object(key).await?;
    let mut hasher = sha2::Sha256::new();
    let mut bytes = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("minio: read back {key}: {e}"))?;
        hasher.update(&chunk);
        bytes += chunk.len() as u64;
    }
    if bytes != expected_bytes {
        return Err(format!(
            "round trip of {key}: {bytes} bytes stored, {expected_bytes} local"
        ));
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha256 {
        return Err(format!("round trip of {key}: sha256 mismatch"));
    }
    Ok(())
}

/// Regenerate (or refresh) the bundle's `.sha256` sidecar with the verified
/// digest, so the hub copy and the tier copy carry the same sidecar.
fn write_digest_sidecar(bundle: &Path, digest: &str) -> Result<(), String> {
    let name = bundle
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("bundle file name is not valid UTF-8")?;
    let sidecar = bundle.with_file_name(format!("{name}{SHA256_SUFFIX}"));
    let mut file =
        File::create(&sidecar).map_err(|e| format!("cannot create {}: {e}", sidecar.display()))?;
    writeln!(file, "{digest}  {name}")
        .map_err(|e| format!("cannot write {}: {e}", sidecar.display()))
}

/// Remove the hub-local bundle and its digest sidecar (targeted per-file
/// removal; missing files are fine).
fn remove_local_copy(store: &RetentionStore, month: &str) {
    let bundle = store.bundle_path(month);
    let _ = std::fs::remove_file(&bundle);
    let _ = std::fs::remove_file(bundle.with_file_name(format!("{month}.tar.zst{SHA256_SUFFIX}")));
}

// --------------------------------------------------------------------- stats

/// Per-bundle tier info for `GET /api/retention` (one bundle per month).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleInfo {
    /// `YYYY-MM`.
    pub month: String,
    /// Bundle size in bytes (local size for hot, recorded size for moved).
    pub bytes: u64,
    /// `hot` (hub-local) or `minio` (permanent tier).
    pub tier: String,
}

/// The most recent move attempt outcome, for the API and `status --json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MoveOutcome {
    pub month: String,
    /// `moved` | `failed`.
    pub status: String,
    /// Failure reason when the last attempt failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// RFC3339 instant of that attempt.
    pub at: String,
    pub attempts: u64,
}

/// `[retention.minio]` view for `GET /api/retention`. Names only - no
/// credentials ever appear here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MinioStats {
    pub enabled: bool,
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
    pub secure: bool,
    /// Months with a verified remote copy, oldest first.
    pub moved_months: Vec<String>,
    /// Sum of moved bundle sizes.
    pub moved_bytes: u64,
    /// The most recent attempt (any month), by time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_move: Option<MoveOutcome>,
    /// Present when the state file was unreadable (stage is paused).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_warning: Option<String>,
}

/// Per-bundle tier for every bundle the store knows about (hot + moved),
/// sorted ascending by month. Local bundle size is read from disk; moved
/// sizes come from the move state. `state_warning` reports an unreadable
/// state file: tiers degrade to "hot" for that case (fail-safe display),
/// and the move stage itself pauses (see [`move_pass`]).
pub fn bundle_tiers(
    store: &RetentionStore,
    cfg: &RetentionConfig,
) -> (Vec<BundleInfo>, MinioStats) {
    let state = match read_move_state(&store.bundles_dir) {
        Ok(state) => state,
        Err(reason) => {
            // Local bundles only; the minio block carries the warning.
            let mut tiers = local_bundle_months(&store.bundles_dir)
                .into_iter()
                .map(|month| bundle_info_hot(store, &month))
                .collect::<Vec<_>>();
            tiers.sort_by(|a, b| a.month.cmp(&b.month));
            let stats = MinioStats {
                enabled: cfg.minio.enabled,
                endpoint: cfg.minio.endpoint.clone(),
                bucket: cfg.minio.bucket.clone(),
                prefix: cfg.minio.prefix.clone(),
                secure: cfg.minio.secure,
                moved_months: Vec::new(),
                moved_bytes: 0,
                last_move: None,
                state_warning: Some(reason),
            };
            return (tiers, stats);
        }
    };

    let mut tiers: Vec<BundleInfo> = local_bundle_months(&store.bundles_dir)
        .into_iter()
        .map(|month| bundle_info_hot(store, &month))
        .collect();
    let mut moved_months: Vec<String> = Vec::new();
    let mut moved_bytes = 0u64;
    for (month, entry) in &state.months {
        if entry.status != MoveStatus::Moved {
            continue;
        }
        moved_months.push(month.clone());
        moved_bytes += entry.bytes;
        if !store.bundle_path(month).exists() {
            tiers.push(BundleInfo {
                month: month.clone(),
                bytes: entry.bytes,
                tier: "minio".to_string(),
            });
        }
    }
    tiers.sort_by(|a, b| a.month.cmp(&b.month));

    let last_move = state
        .months
        .iter()
        .filter_map(|(month, entry)| {
            entry.last_attempt.clone().map(|at| MoveOutcome {
                month: month.clone(),
                status: match entry.status {
                    MoveStatus::Moved => "moved".to_string(),
                    MoveStatus::Hot => "failed".to_string(),
                },
                error: entry.last_error.clone(),
                at,
                attempts: entry.attempts,
            })
        })
        .max_by(|a, b| a.at.cmp(&b.at));

    let stats = MinioStats {
        enabled: cfg.minio.enabled,
        endpoint: cfg.minio.endpoint.clone(),
        bucket: cfg.minio.bucket.clone(),
        prefix: cfg.minio.prefix.clone(),
        secure: cfg.minio.secure,
        moved_months,
        moved_bytes,
        last_move,
        state_warning: None,
    };
    (tiers, stats)
}

/// Tier info for a month whose bundle is hub-local right now.
fn bundle_info_hot(store: &RetentionStore, month: &str) -> BundleInfo {
    let bundle = store.bundle_path(month);
    let bytes = bundle.metadata().map(|meta| meta.len()).unwrap_or(0);
    BundleInfo {
        month: month.to_string(),
        bytes,
        tier: "hot".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::NaiveDate;

    /// Self-cleaning temp directory (same pattern as the retention tests).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("activity-server-minio-{tag}-{nanos}"));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// In-memory tier: stores whole objects (test bundles are tiny).
    #[derive(Default)]
    struct InMemoryTier {
        objects: std::sync::Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl InMemoryTier {
        fn stored(&self, key: &str) -> Option<Vec<u8>> {
            self.objects.lock().expect("lock").get(key).cloned()
        }
    }

    impl BundleTier for InMemoryTier {
        async fn put_object(&self, key: &str, source: &Path) -> Result<(), String> {
            let data = std::fs::read(source).map_err(|e| format!("read: {e}"))?;
            self.objects
                .lock()
                .expect("lock")
                .insert(key.to_string(), data);
            Ok(())
        }

        async fn get_object(&self, key: &str) -> Result<ObjectStream, String> {
            match self.objects.lock().expect("lock").get(key).cloned() {
                Some(data) => {
                    let stream: ObjectStream =
                        Box::pin(futures::stream::once(async move { Ok(Bytes::from(data)) }));
                    Ok(stream)
                }
                None => Err(format!("minio: object {key} not found")),
            }
        }
    }

    /// Wraps a tier and corrupts fetched bodies (one byte flipped), so the
    /// round-trip verification must fail: the verify-failure path against a
    /// misbehaving store.
    struct CorruptingTier<'a>(&'a InMemoryTier);

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
                .expect("one chunk")
                .expect("chunk ok")
                .to_vec();
            data[0] ^= 0xff;
            let stream: ObjectStream =
                Box::pin(futures::stream::once(async move { Ok(Bytes::from(data)) }));
            Ok(stream)
        }
    }

    /// A hub store with one cold month bundled the retention way.
    fn store_with_bundle(tag: &str) -> (TempDir, RetentionStore, String) {
        let dir = TempDir::new(tag);
        let media_dir = dir.0.join("data").join("media");
        let store = RetentionStore::from_media_dir(&media_dir);
        let raw = media_dir.join("frames").join("2025").join("10").join("01");
        std::fs::create_dir_all(&raw).expect("mkdir");
        std::fs::write(raw.join("120000_D1_a.jpg"), b"frame-bytes").expect("write raw");
        let cfg = RetentionConfig {
            enabled: true,
            hot_days: 365,
            ..RetentionConfig::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 11, 7).expect("today");
        let report = retention::run_pass(&store, &cfg, today);
        assert_eq!(report.bundled, vec!["2025-10".to_string()], "{report:?}");
        (dir, store, "2025-10".to_string())
    }

    fn minio_cfg() -> MinioConfig {
        MinioConfig {
            enabled: true,
            endpoint: "blob.joechowhk.internal".to_string(),
            bucket: "activity-archive".to_string(),
            prefix: "bundles".to_string(),
            secure: false,
        }
    }

    fn retention_cfg(minio: MinioConfig) -> RetentionConfig {
        RetentionConfig {
            enabled: true,
            hot_days: 365,
            minio,
            ..RetentionConfig::default()
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 7).expect("today")
    }

    // -------------------------------------------------------------- config

    #[test]
    fn minio_config_defaults_and_validation() {
        let cfg: MinioConfig = toml::from_str("").expect("empty section");
        assert!(!cfg.enabled);
        assert_eq!(cfg.prefix, "bundles");
        assert!(cfg.secure, "TLS defaults on");

        // The section lives under `[retention]` in the real config file, so
        // validation is exercised through ServerConfig (the way
        // load_server_config sees it).
        let good: retention::ServerConfig = toml::from_str(
            "[retention.minio]\nenabled = true\nendpoint = \"blob.joechowhk.internal\"\nbucket = \"activity-archive\"\nsecure = false\n",
        )
        .expect("full section");
        assert!(good.retention.minio.validate().is_ok());

        // An empty prefix is legitimate (objects at the bucket root).
        let root: retention::ServerConfig = toml::from_str(
            "[retention.minio]\nenabled = true\nendpoint = \"e\"\nbucket = \"b\"\nprefix = \"\"\n",
        )
        .expect("parse");
        assert!(root.retention.minio.validate().is_ok());
        assert_eq!(root.retention.minio.object_key("a.tar.zst"), "a.tar.zst");

        for bad in [
            "[retention.minio]\nenabled = true\nbucket = \"b\"\n",
            "[retention.minio]\nenabled = true\nendpoint = \"blob.joechowhk.internal\"\n",
            "[retention.minio]\nenabled = true\nendpoint = \"e\"\nbucket = \"b\"\nprefix = \"a//b\"\n",
            "[retention.minio]\nenabled = true\nendpoint = \"e\"\nbucket = \"b\"\nprefix = \"../escape\"\n",
        ] {
            let cfg: retention::ServerConfig = toml::from_str(bad).expect("parse");
            assert!(cfg.retention.minio.validate().is_err(), "{bad}");
        }
        // Disabled sections skip semantic validation.
        let off: retention::ServerConfig = toml::from_str("[retention.minio]\n").expect("parse");
        assert!(off.retention.minio.validate().is_ok());
    }

    #[test]
    fn server_config_toml_carries_the_minio_section() {
        let cfg: retention::ServerConfig = toml::from_str(
            "[retention]\nenabled = true\n\n[retention.minio]\nenabled = true\nendpoint = \"blob.joechowhk.internal\"\nbucket = \"activity-archive\"\nsecure = false\n",
        )
        .expect("parse");
        assert!(cfg.retention.minio.enabled);
        assert_eq!(cfg.retention.minio.bucket, "activity-archive");
        // Old config files without the section keep parsing (defaults off).
        let cfg: retention::ServerConfig =
            toml::from_str("[retention]\nenabled = true\n").expect("parse");
        assert!(!cfg.retention.minio.enabled);
    }

    #[test]
    fn missing_env_credentials_fail_naming_the_variable() {
        // Credentials must come from the environment; an unset variable
        // fails with the variable named (never a panic, never a config-file
        // secret). The real from_config path is covered by the env-gated
        // integration tests.
        let message = env_credential("ACTIVITY_MINIO_DEFINITELY_UNSET_CHECK").unwrap_err();
        assert!(
            message.contains("ACTIVITY_MINIO_DEFINITELY_UNSET_CHECK"),
            "{message}"
        );
        // A set-but-empty variable is just as missing.
        // (Checked through a name that cannot exist rather than mutating
        // the process environment - tests run in parallel.)
        let message = env_credential("").unwrap_err();
        assert!(message.contains("is not set"), "{message}");
    }

    // -------------------------------------------------------- state file

    #[test]
    fn move_state_roundtrip_and_missing_file() {
        let dir = TempDir::new("state");
        let state = read_move_state(&dir.0).expect("missing file means empty state");
        assert!(state.months.is_empty());

        let mut state = MoveState::default();
        let entry = state.record_attempt("2025-10", "2026-11-07T00:00:00Z");
        entry.status = MoveStatus::Moved;
        entry.key = "bundles/2025-10.tar.zst".to_string();
        entry.sha256 = "abc".to_string();
        entry.bytes = 7;
        entry.moved_at = Some("2026-11-07T00:00:00Z".to_string());
        entry.last_error = None;
        write_move_state(&dir.0, &state).expect("write");

        let loaded = read_move_state(&dir.0).expect("reload");
        assert_eq!(loaded.version, 1, "the written ledger is pinned to v1");
        assert_eq!(loaded.months, state.months);

        std::fs::write(dir.0.join(STATE_FILE), "{not json").expect("corrupt");
        assert!(
            read_move_state(&dir.0).is_err(),
            "corrupt state is an error"
        );
        assert!(!dir.0.join(format!("{STATE_FILE}.part")).exists());
    }

    // ------------------------------------------------------- move pass

    #[tokio::test]
    async fn move_pass_disabled_is_a_noop() {
        let (_dir, store, _month) = store_with_bundle("noop");
        let cfg = retention_cfg(MinioConfig::default());
        let report = move_pass(&store, &cfg, &InMemoryTier::default(), today()).await;
        assert_eq!(report, MoveReport::default());
        assert!(store.bundle_path("2025-10").exists());
    }

    #[tokio::test]
    async fn verified_move_uploads_removes_local_and_records_state() {
        let (_dir, store, month) = store_with_bundle("move-ok");
        let tier = InMemoryTier::default();
        let cfg = retention_cfg(minio_cfg());

        let report = move_pass(&store, &cfg, &tier, today()).await;
        assert_eq!(report.moved.len(), 1, "{report:?}");
        assert!(report.failed.is_empty());
        // Local copy (bundle + sidecar) gone, object stored under the
        // proposed layout key.
        assert!(!store.bundle_path(&month).exists());
        assert!(!store
            .bundles_dir
            .join(format!("{month}.tar.zst{SHA256_SUFFIX}"))
            .exists());
        let key = "bundles/2025-10.tar.zst";
        let stored = tier.stored(key).expect("bundle object stored");
        assert!(
            tier.stored(&format!("{key}{SHA256_SUFFIX}")).is_some(),
            "sidecar stored"
        );

        let state = read_move_state(&store.bundles_dir).expect("state");
        let entry = &state.months["2025-10"];
        assert_eq!(entry.status, MoveStatus::Moved);
        assert_eq!(entry.key, key);
        assert_eq!(entry.sha256, retention::sha256_bytes(&stored));
        assert_eq!(entry.bytes, stored.len() as u64);
        assert_eq!(entry.attempts, 1);
        assert_eq!(entry.last_error, None);

        // Second pass: nothing left to move, nothing to finalize.
        let report = move_pass(&store, &cfg, &tier, today()).await;
        assert!(report.moved.is_empty() && report.finalized.is_empty());
    }

    #[tokio::test]
    async fn verify_failure_keeps_the_hub_copy_and_records_the_error() {
        let (_dir, store, month) = store_with_bundle("verify-fail");
        let tier = InMemoryTier::default();
        let corrupting = CorruptingTier(&tier);
        let cfg = retention_cfg(minio_cfg());

        let report = move_pass(&store, &cfg, &corrupting, today()).await;
        assert!(report.moved.is_empty());
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(report.failed[0].1.contains("sha256 mismatch"));
        // The hub copy survives (it is the authoritative copy).
        assert!(store.bundle_path(&month).exists());
        // State records the failed attempt for the API.
        let state = read_move_state(&store.bundles_dir).expect("state");
        let entry = &state.months["2025-10"];
        assert_eq!(entry.status, MoveStatus::Hot);
        assert_eq!(entry.attempts, 1);
        assert!(entry.last_error.is_some());

        // Retry on the restored (non-corrupting) tier succeeds.
        let report = move_pass(&store, &cfg, &tier, today()).await;
        assert_eq!(report.moved.len(), 1, "{report:?}");
        assert!(!store.bundle_path(&month).exists());
        let state = read_move_state(&store.bundles_dir).expect("state");
        assert_eq!(state.months["2025-10"].status, MoveStatus::Moved);
        assert_eq!(state.months["2025-10"].attempts, 2);
    }

    #[tokio::test]
    async fn failed_upload_is_retried_after_the_endpoint_is_restored() {
        let (_dir, store, month) = store_with_bundle("retry");
        // An unreachable endpoint: every put fails, local copy kept.
        let down = UnreachableTier;
        let cfg = retention_cfg(minio_cfg());
        let report = move_pass(&store, &cfg, &down, today()).await;
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(store.bundle_path(&month).exists());

        // Endpoint restored: the retry moves the month.
        let tier = InMemoryTier::default();
        let report = move_pass(&store, &cfg, &tier, today()).await;
        assert_eq!(report.moved.len(), 1, "{report:?}");
        assert!(!store.bundle_path(&month).exists());
        let state = read_move_state(&store.bundles_dir).expect("state");
        assert_eq!(state.months["2025-10"].status, MoveStatus::Moved);
    }

    /// Tier whose uploads always fail (simulates an unreachable endpoint).
    struct UnreachableTier;

    impl BundleTier for UnreachableTier {
        async fn put_object(&self, key: &str, _source: &Path) -> Result<(), String> {
            Err(format!("minio: begin upload {key}: connection refused"))
        }

        async fn get_object(&self, key: &str) -> Result<ObjectStream, String> {
            Err(format!("minio: fetch {key}: connection refused"))
        }
    }

    #[tokio::test]
    async fn interrupted_move_is_finalized_by_the_next_pass() {
        let (_dir, store, month) = store_with_bundle("finalize");
        let tier = InMemoryTier::default();
        let cfg = retention_cfg(minio_cfg());

        // Crash simulation: verified upload recorded in state, but the
        // local copy is still on disk.
        let bundle = store.bundle_path(&month);
        let digest = retention::sha256_file(&bundle).expect("digest");
        let mut state = MoveState::default();
        let entry = state.record_attempt("2025-10", "2026-11-07T00:00:00Z");
        entry.status = MoveStatus::Moved;
        entry.key = format!("bundles/{month}.tar.zst");
        entry.sha256 = digest;
        entry.bytes = bundle.metadata().expect("meta").len();
        entry.moved_at = Some("2026-11-07T00:00:00Z".to_string());
        write_move_state(&store.bundles_dir, &state).expect("write state");

        let report = move_pass(&store, &cfg, &tier, today()).await;
        assert_eq!(report.finalized, vec![month.clone()]);
        assert!(report.moved.is_empty(), "no duplicate upload: {report:?}");
        assert!(!bundle.exists());
    }

    #[tokio::test]
    async fn moved_month_with_changed_local_bundle_is_kept_for_inspection() {
        let (_dir, store, month) = store_with_bundle("mismatch");
        let tier = InMemoryTier::default();
        let cfg = retention_cfg(minio_cfg());

        // Verified upload recorded, then the local bundle is replaced by a
        // rebuilt partial archive (e.g. a buggy stage-1 rebuild). The next
        // pass must NOT blindly delete it.
        let bundle = store.bundle_path(&month);
        let digest = retention::sha256_file(&bundle).expect("digest");
        let mut state = MoveState::default();
        let entry = state.record_attempt("2025-10", "2026-11-07T00:00:00Z");
        entry.status = MoveStatus::Moved;
        entry.sha256 = digest;
        entry.bytes = bundle.metadata().expect("meta").len();
        write_move_state(&store.bundles_dir, &state).expect("write state");
        std::fs::write(&bundle, b"rebuilt-partial").expect("replace local bundle");

        let report = move_pass(&store, &cfg, &tier, today()).await;
        assert!(report.moved.is_empty() && report.finalized.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert!(bundle.exists(), "mismatching local bundle kept");
    }

    #[tokio::test]
    async fn unreadable_state_pauses_the_stage_and_keeps_everything() {
        let (_dir, store, month) = store_with_bundle("pause");
        std::fs::write(store.bundles_dir.join(STATE_FILE), "{broken").expect("corrupt state");
        let cfg = retention_cfg(minio_cfg());
        let tier = InMemoryTier::default();

        let report = move_pass(&store, &cfg, &tier, today()).await;
        assert!(
            report
                .paused
                .as_ref()
                .is_some_and(|reason| reason.starts_with("cannot parse")),
            "paused with a reason: {report:?}"
        );
        assert!(store.bundle_path(&month).exists(), "nothing moved");
        assert!(tier.stored("bundles/2025-10.tar.zst").is_none());
    }

    #[tokio::test]
    async fn late_raws_of_a_moved_month_are_kept_never_rebundled() {
        let (_dir, store, month) = store_with_bundle("late-raws");
        let tier = InMemoryTier::default();
        let cfg = retention_cfg(minio_cfg());

        // The month moves to the tier.
        move_pass(&store, &cfg, &tier, today()).await;
        assert!(!store.bundle_path(&month).exists());

        // A device syncs an old frame late: the raw must stay on the hub
        // and stage 1 must NOT rebuild a partial bundle (which could later
        // overwrite the full remote archive).
        let late = store
            .media_dir
            .join("frames")
            .join("2025")
            .join("10")
            .join("02")
            .join("120000_D1_late.jpg");
        std::fs::create_dir_all(late.parent().expect("parent")).expect("mkdir");
        std::fs::write(&late, b"late-frame-bytes").expect("write late raw");

        let report = retention::run_pass(&store, &cfg, today());
        assert!(
            report.bundled.is_empty() && report.pruned.is_empty(),
            "moved month must not be rebundled: {report:?}"
        );
        assert_eq!(report.skipped.len(), 1, "{report:?}");
        assert!(report.skipped[0].0 == "2025-10");
        assert!(report.skipped[0].1.contains("minio"));
        assert!(late.exists(), "late raw stays hot on the hub");
        assert!(!store.bundle_path(&month).exists(), "no partial rebuild");
    }

    #[tokio::test]
    async fn non_cold_and_recent_months_stay_local() {
        let dir = TempDir::new("warm");
        let media_dir = dir.0.join("data").join("media");
        let store = RetentionStore::from_media_dir(&media_dir);
        // A bundle for a month that is NOT cold under today's window (the
        // hot window was tightened after it was built).
        std::fs::create_dir_all(&store.bundles_dir).expect("bundles dir");
        std::fs::write(store.bundle_path("2026-10"), b"bundle").expect("bundle");
        let cfg = retention_cfg(minio_cfg());
        let tier = InMemoryTier::default();

        let report = move_pass(&store, &cfg, &tier, today()).await;
        assert!(report.moved.is_empty(), "{report:?}");
        assert!(store.bundle_path("2026-10").exists());
    }

    #[tokio::test]
    async fn bundle_tiers_reflect_hot_and_moved() {
        let (_dir, store, _month) = store_with_bundle("tiers");
        let tier = InMemoryTier::default();
        let cfg = retention_cfg(minio_cfg());

        // Before the move: one hot bundle, minio block enabled but empty.
        let (tiers, stats) = bundle_tiers(&store, &cfg);
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].tier, "hot");
        assert!(stats.enabled);
        assert_eq!(stats.bucket, "activity-archive");
        assert!(stats.moved_months.is_empty());
        assert_eq!(stats.state_warning, None);

        move_pass(&store, &cfg, &tier, today()).await;
        let (tiers, stats) = bundle_tiers(&store, &cfg);
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].tier, "minio");
        assert_eq!(stats.moved_months, vec!["2025-10".to_string()]);
        assert_eq!(stats.moved_bytes, tiers[0].bytes);
        let last = stats.last_move.expect("last move");
        assert_eq!(last.status, "moved");
        assert_eq!(last.month, "2025-10");
    }

    #[tokio::test]
    async fn log_lines_are_human() {
        let report = MoveReport {
            moved: vec![("2025-10".to_string(), 1536)],
            finalized: vec!["2025-09".to_string()],
            failed: vec![(
                "2025-08".to_string(),
                "round trip of bundles/2025-08.tar.zst: sha256 mismatch".to_string(),
            )],
            paused: Some("cannot parse minio-state.json".to_string()),
        };
        let lines = report.log_lines();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("paused"), "{}", lines[0]);
        assert!(lines[1].contains("moved 2025-10"), "{}", lines[1]);
        assert!(lines[1].contains("1.5 KB"), "{}", lines[1]);
        assert!(lines[2].contains("finalized 2025-09"), "{}", lines[2]);
        assert!(lines[3].contains("kept 2025-08 hub-local"), "{}", lines[3]);
    }
}
