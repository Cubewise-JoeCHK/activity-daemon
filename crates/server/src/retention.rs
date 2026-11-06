//! Hub-side media retention (issue #38): finished months of screen frames
//! and meeting audio are packed into monthly `bundles/<YYYY-MM>.tar.zst`
//! archives so the raw file sprawl is bounded on the hub.
//!
//! Contract (owner decision v1.2, 2026-11-06):
//! - Scope is raw media only: `media/frames/...` and `media/audio/...`.
//!   Events in SQLite and derived artifacts (transcripts, summaries) are
//!   exempt - this module never opens the database and never leaves the
//!   media tree.
//! - A month is eligible once it is *fully elapsed* AND its last day lies
//!   strictly beyond the hot window (`hot_days`, default 365). The current
//!   month is structurally never eligible.
//! - For each eligible month: build the bundle (tar, zstd-compressed; the
//!   first member `_manifest.json` lists every file with size + sha256),
//!   verify it (manifest vs archive listing, total bytes, spot-check sample
//!   member checksums), and only then delete that month's raw files.
//! - Deletion is targeted per-file removal (`remove_file`); no wildcard or
//!   recursive shell deletion anywhere.
//! - Verify failure at any point keeps the raw files, is reported, and is
//!   retried on the next pass. Bundles are written to a `.part` file and
//!   atomically renamed, so a crash mid-write can never leave a partial
//!   bundle at the final path; a crash mid-prune is completed by the next
//!   pass (bundle re-verified against the manifest, remaining raws removed).
//! - A `.sha256` sidecar records the verified bundle digest (same format as
//!   the release artifacts; the MinIO tier re-checks it cheaply).
//! - Stage 2 (issue #39, [`crate::minio`]): once a month is cold AND its
//!   bundle is verified, the bundle moves to the MinIO permanent tier and
//!   the hub-local copy is removed only after a verified round trip.
//!   Bundling never touches a month whose archive already lives in MinIO.
//!
//! The sweeper runs inside the server on a daily tick ([`run_pass`]) and is
//! idempotent; `--dry-run` prints the plan without writing or deleting
//! anything ([`plan_months`] / [`render_plan`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chrono::{Duration as ChronoDuration, Months, NaiveDate};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use activity_core::CoreError;

use crate::minio;

/// Media kinds in retention scope (issue #38: frames + audio only). Other
/// kinds under `media/` are never bundled and never deleted.
const MEDIA_KINDS: [&str; 2] = ["frames", "audio"];

/// Name of the self-describing first tar member.
const MANIFEST_NAME: &str = "_manifest.json";
/// Suffix of the in-progress bundle (renamed to the final name on success).
const PART_SUFFIX: &str = ".part";
/// Suffix of the verified-digest sidecar (`sha256sum` text format); the
/// MinIO move stage (issue #39) uploads it alongside the bundle. Part of
/// the documented store layout, hence public.
pub const SHA256_SUFFIX: &str = ".sha256";
/// zstd compression level (3 = the library default; the media is already
/// JPEG/OGG compressed, so higher levels buy almost nothing).
const ZSTD_LEVEL: i32 = 3;
/// Member checksums spot-checked per verification (first / middle / last).
const SAMPLE_COUNT: usize = 3;

// ------------------------------------------------------------------ config

/// Which archive granularity the sweeper bundles. Lowercase serde spelling
/// matches the config file: only `"monthly"` exists today; unknown values
/// are a config error (a typo must not silently disable bundling).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleCadence {
    /// One `bundles/<YYYY-MM>.tar.zst` per fully elapsed calendar month.
    #[default]
    Monthly,
}

/// `[retention]` section of the server config file. Defaults keep the
/// pre-#38 behavior: retention disabled, nothing is ever bundled or deleted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    /// Master switch. Stays `false` until the owner opts in.
    pub enabled: bool,
    /// Hot window in days: raw files stay browsable under `media/` for this
    /// long; months that ended strictly more than `hot_days` ago are
    /// bundled and their raws removed.
    pub hot_days: u64,
    /// Archive granularity (only `monthly` exists).
    pub bundle: BundleCadence,
    /// `[retention.minio]` permanent tier (issue #39): verified bundles
    /// move to MinIO once cold; disabled by default.
    pub minio: crate::minio::MinioConfig,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        RetentionConfig {
            enabled: false,
            hot_days: 365,
            bundle: BundleCadence::Monthly,
            minio: crate::minio::MinioConfig::default(),
        }
    }
}

/// Top-level server config file (`config.toml` on the data volume). New
/// sections are additive: unknown keys are ignored, missing fields default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub retention: RetentionConfig,
    /// `[timesheet]` section: month-end export settings (issue #46).
    pub timesheet: crate::timesheet::TimesheetConfig,
}

/// Path of the server config file: `ACTIVITY_CONFIG` when set, otherwise
/// `config.toml` on the data volume (the hub updater re-creates the
/// container from saved run args, so a file on the volume survives hub
/// updates without touching the saved environment).
pub fn config_path_or_default(data_dir: &Path) -> PathBuf {
    match std::env::var("ACTIVITY_CONFIG") {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => data_dir.join("config.toml"),
    }
}

/// Load the server config: a missing file means all defaults (retention
/// disabled); an unparsable file is a hard error (a typo must not silently
/// run with different settings than the owner wrote down).
pub fn load_server_config(path: &Path) -> Result<ServerConfig, CoreError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ServerConfig::default()),
        Err(e) => {
            return Err(CoreError::Config(format!(
                "cannot read {}: {e}",
                path.display()
            )))
        }
    };
    let cfg: ServerConfig = toml::from_str(&text)
        .map_err(|e| CoreError::Config(format!("cannot parse {}: {e}", path.display())))?;
    // Semantic validation beyond serde (issue #39): an enabled minio
    // section must name an endpoint and a bucket - a typo must not silently
    // run the move stage with empty settings.
    if let Err(reason) = cfg.retention.minio.validate() {
        return Err(CoreError::Config(reason));
    }
    Ok(cfg)
}

// ------------------------------------------------------------------- store

/// The hub store paths the sweeper operates on.
#[derive(Debug, Clone, PartialEq)]
pub struct RetentionStore {
    /// `media/` root (`ACTIVITY_DATA/media` in production).
    pub media_dir: PathBuf,
    /// Bundle output directory (`bundles/` next to `media/`).
    pub bundles_dir: PathBuf,
}

impl RetentionStore {
    /// Store paths for a media root: bundles live in a `bundles/` directory
    /// next to `media/` (never inside it - the walker must not see bundles).
    pub fn from_media_dir(media_dir: &Path) -> RetentionStore {
        let bundles_dir = media_dir
            .parent()
            .map(|parent| parent.join("bundles"))
            .unwrap_or_else(|| PathBuf::from("bundles"));
        RetentionStore {
            media_dir: media_dir.to_path_buf(),
            bundles_dir,
        }
    }

    /// Path of a month's bundle file (`<YYYY-MM>.tar.zst`); part of the
    /// documented store layout, shared with the MinIO move stage
    /// (issue #39).
    pub fn bundle_path(&self, month: &str) -> PathBuf {
        self.bundles_dir.join(format!("{month}.tar.zst"))
    }

    fn part_path(&self, month: &str) -> PathBuf {
        self.bundles_dir
            .join(format!("{month}.tar.zst{PART_SUFFIX}"))
    }
}

/// One raw file still on disk for a month.
#[derive(Debug, Clone, PartialEq)]
struct RawFile {
    /// Path relative to the media root, `/`-separated (the tar member name
    /// and the manifest key; extraction restores it under the media root).
    rel: String,
    abs: PathBuf,
    bytes: u64,
}

/// Manifest embedded as the first tar member: makes the bundle
/// self-describing so it can be re-verified even after its raws are pruned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleManifest {
    /// `YYYY-MM`.
    pub month: String,
    /// RFC3339 creation instant.
    pub created: String,
    /// Every member after the manifest itself, sorted by path.
    pub members: Vec<ManifestMember>,
}

/// One archived file in [`BundleManifest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestMember {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

// -------------------------------------------------------------------- walk

/// All raw files of one month, across the in-scope kinds (targeted walk of
/// `media/<kind>/<YYYY>/<MM>/...` - used by the sweeper pass).
fn walk_month(store: &RetentionStore, month: &str) -> Vec<RawFile> {
    let mut files = Vec::new();
    for kind in MEDIA_KINDS {
        let Some((year, rest)) = month.split_once('-') else {
            continue;
        };
        let month_dir = store.media_dir.join(kind).join(year).join(rest);
        files.extend(walk_dir_recursive(&store.media_dir, &month_dir));
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    files
}

/// Every raw file under the media root across the in-scope kinds, grouped
/// by `YYYY-MM` (one full walk for planning and stats).
fn walk_all(store: &RetentionStore) -> BTreeMap<String, Vec<RawFile>> {
    let mut by_month: BTreeMap<String, Vec<RawFile>> = BTreeMap::new();
    for kind in MEDIA_KINDS {
        let kind_dir = store.media_dir.join(kind);
        let Ok(years) = std::fs::read_dir(&kind_dir) else {
            continue;
        };
        for year in years.flatten() {
            let year_name = year.file_name();
            let Some(year_name) = year_name.to_str() else {
                continue;
            };
            if year_name.len() != 4 || !year_name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Ok(months) = std::fs::read_dir(year.path()) else {
                continue;
            };
            for month in months.flatten() {
                let month_name = month.file_name();
                let Some(month_name) = month_name.to_str() else {
                    continue;
                };
                if month_name.len() != 2 || !month_name.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let key = format!("{year_name}-{month_name}");
                let files = walk_dir_recursive(&store.media_dir, &month.path());
                by_month.entry(key).or_default().extend(files);
            }
        }
    }
    for files in by_month.values_mut() {
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
    }
    by_month
}

/// All regular files below `dir` (recursive), as [`RawFile`] with
/// media-root-relative names. Directories that do not parse as years and
/// months are already excluded by the callers' traversal; unreadable
/// entries are skipped. Names are `/`-normalized so tar members and
/// manifest keys are identical on every platform.
fn walk_dir_recursive(media_dir: &Path, dir: &Path) -> Vec<RawFile> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_dir_recursive(media_dir, &path));
        } else if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                let rel = match path.strip_prefix(media_dir) {
                    Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                    Err(_) => continue,
                };
                files.push(RawFile {
                    rel,
                    abs: path,
                    bytes: meta.len(),
                });
            }
        }
    }
    files
}

/// Last calendar day of the month `first` belongs to.
fn last_day_of(first: NaiveDate) -> NaiveDate {
    first
        .checked_add_months(Months::new(1))
        .expect("next month start exists for any NaiveDate in range")
        - ChronoDuration::days(1)
}

/// A month is *cold* (bundle-eligible) when it is fully elapsed AND its
/// last day lies strictly beyond the hot window. The current month never
/// satisfies this: its last day is always >= today >= the cutoff.
pub(crate) fn is_cold(month: &str, today: NaiveDate, hot_days: u64) -> bool {
    let Some(first) = parse_month(month) else {
        return false;
    };
    let cutoff = today - ChronoDuration::days(hot_days as i64);
    last_day_of(first) < cutoff
}

/// `YYYY-MM` -> first day of that month.
pub(crate) fn parse_month(month: &str) -> Option<NaiveDate> {
    let (year, month) = month.split_once('-')?;
    if year.len() != 4 || month.len() != 2 {
        return None;
    }
    NaiveDate::from_ymd_opt(year.parse().ok()?, month.parse().ok()?, 1)
}

// ------------------------------------------------------------------- build

/// Build `bundles/<month>.tar.zst` from the month's raw files (re-walked
/// fresh: the manifest must describe exactly what goes into the archive).
/// The archive is written to a `.part` file and atomically renamed, so the
/// final path only ever holds a complete bundle; the digest sidecar is
/// written after verification passed (see `pass_month`).
fn build_bundle(
    store: &RetentionStore,
    month: &str,
    created: &str,
) -> Result<BundleManifest, String> {
    let files = walk_month(store, month);
    if files.is_empty() {
        return Err("no raw files found for month".to_string());
    }

    let mut members = Vec::with_capacity(files.len());
    for file in &files {
        members.push(ManifestMember {
            path: file.rel.clone(),
            bytes: file.bytes,
            sha256: sha256_file(&file.abs)?,
        });
    }
    members.sort_by(|a, b| a.path.cmp(&b.path));
    let manifest = BundleManifest {
        month: month.to_string(),
        created: created.to_string(),
        members,
    };

    std::fs::create_dir_all(&store.bundles_dir)
        .map_err(|e| format!("cannot create bundles dir: {e}"))?;
    let part = store.part_path(month);
    let file = File::create(&part).map_err(|e| format!("cannot create {}: {e}", part.display()))?;
    let encoder =
        zstd::stream::Encoder::new(file, ZSTD_LEVEL).map_err(|e| format!("zstd encoder: {e}"))?;
    let mut builder = tar::Builder::new(encoder);

    let manifest_json = serde_json::to_vec(&manifest).map_err(|e| format!("manifest json: {e}"))?;
    append_bytes_member(&mut builder, MANIFEST_NAME, &manifest_json)?;
    for file in &files {
        builder
            .append_path_with_name(&file.abs, &file.rel)
            .map_err(|e| format!("cannot append {}: {e}", file.rel))?;
    }
    let encoder = builder
        .into_inner()
        .map_err(|e| format!("tar finish: {e}"))?;
    let file = encoder.finish().map_err(|e| format!("zstd finish: {e}"))?;
    file.sync_all().map_err(|e| format!("fsync bundle: {e}"))?;
    drop(file);

    let bundle = store.bundle_path(month);
    std::fs::rename(&part, &bundle).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        format!("cannot finalize {}: {e}", bundle.display())
    })?;
    Ok(manifest)
}

/// Append an in-memory member (used for the manifest itself).
fn append_bytes_member(
    builder: &mut tar::Builder<zstd::stream::Encoder<File>>,
    name: &str,
    data: &[u8],
) -> Result<(), String> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, name, data)
        .map_err(|e| format!("cannot append {name}: {e}"))
}

/// sha256 of a file, streamed (media files can be large).
pub(crate) fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// sha256 of an in-memory slice.
pub(crate) fn sha256_bytes(data: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(data))
}

// ------------------------------------------------------------------ verify

/// One-pass structural verification of a bundle: the archive must decode
/// cleanly, its first member must be the manifest, and the remaining
/// members must match the manifest exactly (same set, same sizes, same
/// order). Up to [`SAMPLE_COUNT`] member contents are checksummed against
/// the manifest - hashed inline during the same decode pass, so one pass
/// verifies everything, even with the raws already pruned. Also the gate
/// the MinIO move stage passes before uploading (issue #39).
pub(crate) fn verify_bundle(store: &RetentionStore, month: &str) -> Result<BundleManifest, String> {
    let bundle = store.bundle_path(month);
    let file = File::open(&bundle).map_err(|e| format!("cannot open {}: {e}", bundle.display()))?;
    let decoder = zstd::stream::Decoder::new(file).map_err(|e| format!("zstd decoder: {e}"))?;
    let mut archive = tar::Archive::new(decoder);

    let mut manifest: Option<BundleManifest> = None;
    let mut seen: Vec<(String, u64)> = Vec::new();
    // Sample checksums collected on the way: member path -> sha256.
    let mut sampled: BTreeMap<String, String> = BTreeMap::new();
    for entry in archive.entries().map_err(|e| format!("tar read: {e}"))? {
        let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        let name = entry
            .path()
            .map_err(|e| format!("tar path: {e}"))?
            .to_string_lossy()
            .replace('\\', "/");
        if name == MANIFEST_NAME {
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .map_err(|e| format!("cannot read {MANIFEST_NAME}: {e}"))?;
            let parsed: BundleManifest =
                serde_json::from_slice(&data).map_err(|e| format!("manifest parse: {e}"))?;
            if parsed.month != month {
                return Err(format!("manifest is for {} not {month}", parsed.month));
            }
            manifest = Some(parsed);
            continue;
        }
        // The manifest (member 0) already names the sample members, so
        // their checksums are collected inline - no second decode pass.
        let is_sample = match &manifest {
            Some(manifest) => sample_paths(&manifest.members).contains(&name.as_str()),
            None => false,
        };
        let header_size = entry.size();
        if is_sample {
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .map_err(|e| format!("tar read {name}: {e}"))?;
            if data.len() as u64 != header_size {
                return Err(format!(
                    "{name}: header says {header_size} bytes, archive holds {}",
                    data.len()
                ));
            }
            sampled.insert(name.clone(), sha256_bytes(&data));
        } else {
            // Consume the real bytes: the decoder must agree with the header.
            let real_size = std::io::copy(&mut entry, &mut std::io::sink())
                .map_err(|e| format!("tar read {name}: {e}"))?;
            if real_size != header_size {
                return Err(format!(
                    "{name}: header says {header_size} bytes, archive holds {real_size}"
                ));
            }
        }
        seen.push((name, header_size));
    }

    let manifest = manifest.ok_or("bundle has no manifest member")?;
    let expected: Vec<(String, u64)> = manifest
        .members
        .iter()
        .map(|member| (member.path.clone(), member.bytes))
        .collect();
    if expected != seen {
        return Err(format!(
            "archive members do not match manifest ({} expected vs {} in archive)",
            expected.len(),
            seen.len()
        ));
    }

    // Spot-check verdict: every sampled member's inline sha256 must equal
    // the manifest's recorded checksum.
    for member in &manifest.members {
        if let Some(actual) = sampled.get(member.path.as_str()) {
            if actual != &member.sha256 {
                return Err(format!("sample member {} fails checksum", member.path));
            }
        }
    }
    if sampled.len() != sample_indices(manifest.members.len()).len() {
        return Err("bundle is missing sampled members".to_string());
    }
    Ok(manifest)
}

/// Indices of the spot-check samples: first, middle, last (deduplicated,
/// at most [`SAMPLE_COUNT`]).
fn sample_indices(member_count: usize) -> Vec<usize> {
    if member_count == 0 {
        return Vec::new();
    }
    let mut picks = vec![0usize, member_count / 2, member_count - 1];
    picks.sort_unstable();
    picks.dedup();
    picks.truncate(SAMPLE_COUNT);
    picks
}

/// Paths of the spot-check sample members.
fn sample_paths(members: &[ManifestMember]) -> Vec<&str> {
    sample_indices(members.len())
        .into_iter()
        .map(|index| members[index].path.as_str())
        .collect()
}

/// Cross-check remaining raw files against a verified manifest: every raw
/// still on disk must be listed in the manifest (nothing new appeared after
/// bundling), and any spot-check sample still on disk must match its
/// recorded checksum. Files already pruned are absent here by design.
fn raws_match_manifest(manifest: &BundleManifest, raws: &[RawFile]) -> Result<(), String> {
    let listed: BTreeSet<&str> = manifest.members.iter().map(|m| m.path.as_str()).collect();
    for raw in raws {
        if !listed.contains(raw.rel.as_str()) {
            return Err(format!("{} is on disk but not in the bundle", raw.rel));
        }
        if let Ok(actual) = sha256_file(&raw.abs) {
            let expected = manifest
                .members
                .iter()
                .find(|m| m.path == raw.rel)
                .map(|m| m.sha256.as_str())
                .unwrap_or_default();
            if actual != expected {
                return Err(format!("{} changed on disk after bundling", raw.rel));
            }
        }
    }
    Ok(())
}

/// Write the verified-digest sidecar (`<bundle>.sha256`, sha256sum format).
fn write_digest_sidecar(bundle: &Path) -> Result<(), String> {
    let digest = sha256_file(bundle)?;
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

// ------------------------------------------------------------------- prune

/// Delete exactly the given raw files (targeted per-file removal - never a
/// wildcard or recursive shell deletion), then tidy the now-empty day and
/// month directories with non-recursive `remove_dir` (which fails
/// harmlessly on anything still non-empty). Returns (files, bytes) removed.
fn prune_files(media_dir: &Path, raws: &[RawFile]) -> (usize, u64) {
    let mut removed = 0usize;
    let mut bytes = 0u64;
    let mut parent_dirs: Vec<PathBuf> = Vec::new();
    for raw in raws {
        if std::fs::remove_file(&raw.abs).is_ok() {
            removed += 1;
            bytes += raw.bytes;
            if let Some(parent) = raw.abs.parent() {
                parent_dirs.push(parent.to_path_buf());
            }
        }
    }
    // Deepest-first dedup; each remove_dir only succeeds when the directory
    // is already empty.
    parent_dirs.sort_unstable();
    parent_dirs.dedup();
    for dir in parent_dirs.iter().rev() {
        let mut path = dir.clone();
        while path.starts_with(media_dir) && path != media_dir {
            if std::fs::remove_dir(&path).is_err() {
                break;
            }
            match path.parent().map(Path::to_path_buf) {
                Some(parent) => path = parent,
                None => break,
            }
        }
    }
    (removed, bytes)
}

// ------------------------------------------------------------ pass / plan

/// What one sweeper pass did, for logging and tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PassReport {
    /// Months whose bundle was newly built and verified.
    pub bundled: Vec<String>,
    /// (month, files removed, bytes removed).
    pub pruned: Vec<(String, usize, u64)>,
    /// (month, reason) - raw files kept, retried next pass.
    pub skipped: Vec<(String, String)>,
}

impl PassReport {
    /// Log lines for the daily tick.
    pub fn log_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for month in &self.bundled {
            lines.push(format!("retention: bundled {month}"));
        }
        for (month, files, bytes) in &self.pruned {
            lines.push(format!(
                "retention: pruned {month} ({} {}, {})",
                files,
                files_word(*files),
                human_bytes(*bytes)
            ));
        }
        for (month, reason) in &self.skipped {
            lines.push(format!("retention: kept {month} raws - {reason}"));
        }
        lines
    }
}

/// Execute one idempotent sweeper pass over all cold months with raw
/// files. `today` anchors the hot window (injected for tests; production
/// passes the server-local today).
pub fn run_pass(store: &RetentionStore, cfg: &RetentionConfig, today: NaiveDate) -> PassReport {
    let mut report = PassReport::default();
    if !cfg.enabled {
        return report;
    }
    // Issue #39: months whose verified archive already lives in the
    // permanent tier are never re-bundled - a partial rebuild could later
    // overwrite the full remote copy. An unreadable move state pauses
    // bundling entirely: the state is what proves the remote copy exists.
    let moved = moved_month_guard(store, cfg);
    for month in pending_months(store, cfg, today) {
        match &moved {
            Err(reason) => {
                report.skipped.push((
                    month,
                    format!("minio move state unreadable ({reason}) - bundling paused"),
                ));
            }
            Ok(moved) if moved.contains(&month) => {
                report.skipped.push((
                    month,
                    "archive already moved to minio - raw files kept (rebundling is v2 scope)"
                        .to_string(),
                ));
            }
            Ok(_) => match pass_month(store, &month, today) {
                Ok((bundled, files, bytes)) => {
                    if bundled {
                        report.bundled.push(month.clone());
                    }
                    if files > 0 {
                        report.pruned.push((month.clone(), files, bytes));
                    }
                }
                Err(reason) => report.skipped.push((month, reason)),
            },
        }
    }
    report
}

/// The months already moved to the permanent tier (issue #39), or the
/// reason the move state could not be read. When minio is disabled there
/// is no guard: an empty set.
fn moved_month_guard(
    store: &RetentionStore,
    cfg: &RetentionConfig,
) -> Result<BTreeSet<String>, String> {
    if !cfg.minio.enabled {
        return Ok(BTreeSet::new());
    }
    minio::moved_months_guard(&store.bundles_dir)
}

/// Bundle-and-prune one month. Idempotent: every step is re-derived from
/// what is on disk (bundle present? manifest verifies? raws left?).
/// Returns (newly bundled, raw files removed, raw bytes removed).
fn pass_month(
    store: &RetentionStore,
    month: &str,
    today: NaiveDate,
) -> Result<(bool, usize, u64), String> {
    let bundle = store.bundle_path(month);
    let raws = walk_month(store, month);

    if bundle.exists() {
        // A stale .part leftover from a crashed build is safe to drop once
        // a final bundle exists (targeted single-file removal).
        let part = store.part_path(month);
        if part.exists() {
            let _ = std::fs::remove_file(&part);
        }
        let manifest = verify_bundle(store, month)?;
        // A verified month may still have raws (crash before/during prune)
        // or new files (synced late). New files must NOT be deleted: they
        // are not in the bundle anywhere.
        raws_match_manifest(&manifest, &raws)?;
        if raws.is_empty() {
            return Ok((false, 0, 0));
        }
        let (files, bytes) = prune_files(&store.media_dir, &raws);
        return Ok((false, files, bytes));
    }

    // No bundle yet: build, verify, then (only then) prune.
    let created = today.format("%Y-%m-%d").to_string();
    build_bundle(store, month, &created).inspect_err(|_reason| {
        // Targeted cleanup of the failed attempt's .part file.
        let _ = std::fs::remove_file(store.part_path(month));
    })?;
    let manifest = verify_bundle(store, month)?;
    // Re-walk: files may have appeared while the bundle was written; those
    // are not in the manifest and must stay raw.
    let raws = walk_month(store, month);
    raws_match_manifest(&manifest, &raws)?;
    write_digest_sidecar(&bundle)?;
    let (files, bytes) = prune_files(&store.media_dir, &raws);
    Ok((true, files, bytes))
}

/// Cold months (sorted ascending) that still hold raw files.
pub fn pending_months(
    store: &RetentionStore,
    cfg: &RetentionConfig,
    today: NaiveDate,
) -> Vec<String> {
    walk_all(store)
        .into_iter()
        .filter(|(month, files)| !files.is_empty() && is_cold(month, today, cfg.hot_days))
        .map(|(month, _)| month)
        .collect()
}

/// The `--dry-run` plan for one month.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonthPlan {
    /// `YYYY-MM`.
    pub month: String,
    /// Raw files still on disk for the month.
    pub files: usize,
    /// Their total size in bytes.
    pub bytes: u64,
    /// What the next real pass would do.
    pub action: String,
}

/// Build the dry-run plan: months found, sizes, would-bundle /
/// would-delete. Reads the store only - nothing is written or deleted.
pub fn plan_months(
    store: &RetentionStore,
    cfg: &RetentionConfig,
    today: NaiveDate,
) -> Vec<MonthPlan> {
    let by_month = walk_all(store);
    let moved = moved_month_guard(store, cfg).unwrap_or_default();
    let mut plans = Vec::new();
    for (month, raws) in by_month {
        if raws.is_empty() || !is_cold(&month, today, cfg.hot_days) {
            continue;
        }
        let bytes = raws.iter().map(|file| file.bytes).sum();
        let action = if moved.contains(&month) {
            format!(
                "would keep {} raw {} ({}) - archive already moved to minio, rebundling is v2 scope",
                raws.len(),
                files_word(raws.len()),
                human_bytes(bytes)
            )
        } else if store.bundle_path(&month).exists() {
            match verify_bundle(store, &month)
                .and_then(|manifest| raws_match_manifest(&manifest, &raws))
            {
                Ok(()) => format!(
                    "would delete {} raw {} ({}) - bundle already archived and verified",
                    raws.len(),
                    files_word(raws.len()),
                    human_bytes(bytes)
                ),
                Err(reason) => format!("would keep raws - bundle verify failed: {reason}"),
            }
        } else {
            format!(
                "would bundle into bundles/{month}.tar.zst then delete {} raw {} ({})",
                raws.len(),
                files_word(raws.len()),
                human_bytes(bytes)
            )
        };
        plans.push(MonthPlan {
            month,
            files: raws.len(),
            bytes,
            action,
        });
    }
    plans
}

/// Human-readable dry-run report (what `--dry-run` prints).
pub fn render_plan(store: &RetentionStore, cfg: &RetentionConfig, today: NaiveDate) -> String {
    let plans = plan_months(store, cfg, today);
    let mut out = String::new();
    if !cfg.enabled {
        out.push_str("retention: DISABLED in server config - this plan is a preview only\n");
    }
    let cutoff = today - ChronoDuration::days(cfg.hot_days as i64);
    out.push_str(&format!(
        "retention plan (dry run): today {today}, hot_days {}, cold before {cutoff}\n",
        cfg.hot_days
    ));
    if plans.is_empty() {
        out.push_str("  no cold months with raw media - nothing to bundle or delete\n");
    }
    for plan in &plans {
        out.push_str(&format!(
            "  {}: {} {}, {}\n    {}\n",
            plan.month,
            plan.files,
            files_word(plan.files),
            human_bytes(plan.bytes),
            plan.action
        ));
    }

    // The hot window stays raw.
    let mut hot_months = 0usize;
    let mut hot_bytes = 0u64;
    for (month, files) in walk_all(store) {
        if !is_cold(&month, today, cfg.hot_days) {
            hot_months += 1;
            hot_bytes += files.iter().map(|file| file.bytes).sum::<u64>();
        }
    }
    out.push_str(&format!(
        "  hot window kept raw: {hot_months} month entries, {}\n",
        human_bytes(hot_bytes)
    ));
    out
}

// ------------------------------------------------------------------- stats

/// Retention stats for `GET /api/retention` (surfaced through the client's
/// `status --json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionStats {
    pub enabled: bool,
    pub hot_days: u64,
    /// Raw bytes per media kind still hot under `media/`.
    pub hot_bytes: BTreeMap<String, u64>,
    pub bundle_count: u64,
    pub bundle_bytes: u64,
    /// Cold months still holding raw files (bundle missing or partially
    /// pruned), oldest first.
    pub months_pending: Vec<PendingMonth>,
    /// Human description of what the next daily tick would do.
    pub next_action: String,
    /// Every bundle with its tier (`hot` hub-local / `minio` permanent
    /// tier), oldest first (issue #39).
    pub bundles: Vec<minio::BundleInfo>,
    /// MinIO permanent tier block: moved months, last move result (issue
    /// #39).
    pub minio: minio::MinioStats,
}

/// One pending month in [`RetentionStats`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingMonth {
    pub month: String,
    pub files: u64,
    pub bytes: u64,
}

/// Collect retention stats for the API. Read-only.
pub fn collect_stats(
    store: &RetentionStore,
    cfg: &RetentionConfig,
    today: NaiveDate,
) -> RetentionStats {
    let by_month = walk_all(store);
    let mut hot_bytes: BTreeMap<String, u64> = BTreeMap::new();
    for kind in MEDIA_KINDS {
        let bytes = by_month
            .values()
            .flatten()
            .filter(|file| file.rel.starts_with(&format!("{kind}/")))
            .map(|file| file.bytes)
            .sum();
        hot_bytes.insert(kind.to_string(), bytes);
    }

    let mut bundle_count = 0u64;
    let mut bundle_bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(&store.bundles_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("zst") {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                bundle_count += 1;
                bundle_bytes += meta.len();
            }
        }
    }

    let mut months_pending = Vec::new();
    for (month, raws) in by_month {
        if raws.is_empty() || !is_cold(&month, today, cfg.hot_days) {
            continue;
        }
        months_pending.push(PendingMonth {
            files: raws.len() as u64,
            bytes: raws.iter().map(|file| file.bytes).sum(),
            month,
        });
    }

    let moved = moved_month_guard(store, cfg).unwrap_or_default();
    let next_action = match months_pending.first() {
        Some(first) if !moved.contains(&first.month) => {
            if store.bundle_path(&first.month).exists() {
                format!(
                    "delete {} raw {} of {} ({}) - bundle already archived",
                    first.files,
                    files_word(first.files as usize),
                    first.month,
                    human_bytes(first.bytes)
                )
            } else {
                let move_step = if cfg.minio.enabled {
                    " then move the bundle to minio"
                } else {
                    ""
                };
                format!(
                    "bundle {} into bundles/{}.tar.zst ({} {}, {}) then delete raws{move_step}",
                    first.month,
                    first.month,
                    first.files,
                    files_word(first.files as usize),
                    human_bytes(first.bytes)
                )
            }
        }
        Some(first) => format!(
            "late raws of {} stay on the hub - the archive lives in minio",
            first.month
        ),
        None => "none".to_string(),
    };

    let (bundles, minio) = minio::bundle_tiers(store, cfg);

    RetentionStats {
        enabled: cfg.enabled,
        hot_days: cfg.hot_days,
        hot_bytes,
        bundle_count,
        bundle_bytes,
        months_pending,
        next_action,
        bundles,
        minio,
    }
}

/// Singular/plural for human file-count strings ("1 file", "2 files").
fn files_word(count: usize) -> &'static str {
    if count == 1 {
        "file"
    } else {
        "files"
    }
}

/// Human binary size: "0 B", "512 B", "1.5 MB", "3.2 GB" (mirrors the
/// client's status formatter; strings here are log/plan/UI text).
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{SystemTime, UNIX_EPOCH};

    /// Self-cleaning temp directory (same pattern as the ingest tests).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("activity-server-retention-{tag}-{nanos}"));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A synthetic hub store: `media/` plus one SQLite db (retention must
    /// never touch it) holding one window and one AFK event.
    fn test_store(tag: &str) -> (TempDir, RetentionStore) {
        let dir = TempDir::new(tag);
        let store = RetentionStore::from_media_dir(&dir.0.join("media"));
        std::fs::create_dir_all(&store.media_dir).expect("media dir");
        let db = activity_core::Db::open_db(&dir.0.join("activity.db")).expect("db");
        db.insert_event(
            &activity_core::EventEnvelope {
                bucket: activity_core::BUCKET_WINDOW.to_string(),
                timestamp: "2026-11-01T09:00:00+08:00".to_string(),
                duration_s: 60.0,
                data: serde_json::json!({"app": "Code.exe", "title": "main.rs"}),
            },
            "device",
        )
        .expect("insert window event");
        db.insert_event(
            &activity_core::EventEnvelope {
                bucket: activity_core::BUCKET_AFK.to_string(),
                timestamp: "2026-11-01T09:01:00+08:00".to_string(),
                duration_s: 0.0,
                data: serde_json::json!({"state": "afk"}),
            },
            "device",
        )
        .expect("insert afk event");
        (dir, store)
    }

    /// Write a raw media file into a month and return its content (tests
    /// recreate files byte-identically after simulated crashes).
    fn put_raw(
        store: &RetentionStore,
        kind: &str,
        month: &str,
        day: &str,
        name: &str,
        seed: &str,
    ) -> (PathBuf, Vec<u8>) {
        let (year, rest) = month.split_once('-').expect("month shape");
        let path = store
            .media_dir
            .join(kind)
            .join(year)
            .join(rest)
            .join(day)
            .join(name);
        let content = format!("{kind}/{month}/{day}/{name}/{seed}\n").into_bytes();
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, &content).expect("write");
        (path, content)
    }

    fn put_raw_back(_store: &RetentionStore, path: &Path, content: &[u8]) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, content).expect("write back");
    }

    fn enabled(hot_days: u64) -> RetentionConfig {
        RetentionConfig {
            enabled: true,
            hot_days,
            ..RetentionConfig::default()
        }
    }

    /// `2025-10` is fully elapsed + beyond the hot window for this date.
    fn cold_month_today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 6).expect("today")
    }

    // ------------------------------------------------------------- config

    #[test]
    fn retention_config_defaults_and_overrides() {
        let cfg: ServerConfig = toml::from_str("").expect("empty config");
        assert_eq!(cfg.retention, RetentionConfig::default());
        assert!(!cfg.retention.enabled);
        assert_eq!(cfg.retention.hot_days, 365);
        assert_eq!(cfg.retention.bundle, BundleCadence::Monthly);

        let cfg: ServerConfig =
            toml::from_str("[retention]\nenabled = true\nhot_days = 30\nbundle = \"monthly\"\n")
                .expect("full section");
        assert!(cfg.retention.enabled);
        assert_eq!(cfg.retention.hot_days, 30);

        // A typo in the cadence is a config error, not a silent default.
        assert!(toml::from_str::<ServerConfig>("[retention]\nbundle = \"weekly\"\n").is_err());

        // Config files without the section keep parsing.
        let cfg: ServerConfig = toml::from_str("# nothing yet\n").expect("comment only");
        assert_eq!(cfg.retention, RetentionConfig::default());
    }

    #[test]
    fn missing_config_file_means_defaults() {
        let dir = TempDir::new("cfg-missing");
        let path = dir.0.join("config.toml");
        assert!(!path.exists());
        let cfg = load_server_config(&path).expect("defaults on missing file");
        assert_eq!(cfg, ServerConfig::default());

        std::fs::write(&path, "[retention]\nenabled = true\n").expect("write config");
        let cfg = load_server_config(&path).expect("parse");
        assert!(cfg.retention.enabled);
        assert_eq!(cfg.retention.hot_days, 365);

        std::fs::write(&path, "[retention]\nenabled = maybe\n").expect("write broken config");
        assert!(load_server_config(&path).is_err());
    }

    #[test]
    fn store_paths_derive_from_media_dir() {
        let store = RetentionStore::from_media_dir(Path::new("/srv/activity/media"));
        assert_eq!(store.bundles_dir, PathBuf::from("/srv/activity/bundles"));
        assert_eq!(
            store.bundle_path("2025-10"),
            PathBuf::from("/srv/activity/bundles/2025-10.tar.zst")
        );
        assert_eq!(
            store.part_path("2025-10"),
            PathBuf::from("/srv/activity/bundles/2025-10.tar.zst.part")
        );
    }

    // ------------------------------------------------------- month logic

    #[test]
    fn month_parsing_and_coldness() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 6).expect("today");
        // 365d window: months ending 2025-10-31 are cold, 2025-11-30 are hot.
        assert!(is_cold("2025-10", today, 365));
        assert!(!is_cold("2025-11", today, 365));
        // The current month is never cold, whatever the window.
        assert!(!is_cold("2026-11", today, 365));
        assert!(!is_cold("2026-11", today, 0));
        // hot_days = 0: every fully elapsed month is cold.
        assert!(is_cold("2026-10", today, 0));
        // Boundary: a month ending exactly on the cutoff stays hot (strict).
        let late = NaiveDate::from_ymd_opt(2026, 11, 30).expect("late today");
        assert!(!is_cold("2025-11", late, 365));
        assert!(is_cold("2025-10", late, 365));
    }

    #[test]
    fn sample_indices_first_middle_last() {
        assert_eq!(sample_indices(0), Vec::<usize>::new());
        assert_eq!(sample_indices(1), vec![0]);
        assert_eq!(sample_indices(2), vec![0, 1]);
        assert_eq!(sample_indices(3), vec![0, 1, 2]);
        assert_eq!(sample_indices(10), vec![0, 5, 9]);
    }

    // ------------------------------------------------- end-to-end bundle

    #[test]
    fn cold_month_is_bundled_verified_and_pruned() {
        let (dir, store) = test_store("roundtrip");
        let (a, a_content) = put_raw(&store, "frames", "2025-10", "31", "120000_D1_a.jpg", "a");
        let (b, _) = put_raw(&store, "frames", "2025-10", "05", "090000_D2_b.jpg", "b");
        let (c, c_content) = put_raw(&store, "audio", "2025-10", "12", "101500_mic_c.ogg", "c");

        let report = run_pass(&store, &enabled(365), cold_month_today());
        assert_eq!(report.bundled, vec!["2025-10".to_string()]);
        assert_eq!(report.pruned.len(), 1);
        assert_eq!(report.pruned[0].0, "2025-10");
        assert_eq!(report.pruned[0].1, 3, "three files removed");
        assert_eq!(report.skipped, Vec::<(String, String)>::new());

        // Raws gone (targeted removal), bundle + sidecar present.
        assert!(!a.exists() && !b.exists() && !c.exists());
        let bundle = store.bundle_path("2025-10");
        assert!(bundle.exists());
        assert!(bundle.with_file_name("2025-10.tar.zst.sha256").exists());

        // Manifest is a member and lists all three files with checksums.
        let manifest: BundleManifest =
            serde_json::from_slice(&read_member(&bundle, MANIFEST_NAME).expect("manifest member"))
                .expect("manifest parse");
        assert_eq!(manifest.month, "2025-10");
        let paths: Vec<&str> = manifest.members.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "audio/2025/10/12/101500_mic_c.ogg",
                "frames/2025/10/05/090000_D2_b.jpg",
                "frames/2025/10/31/120000_D1_a.jpg",
            ]
        );
        assert_eq!(manifest.members[0].sha256, sha256_bytes(&c_content));
        assert_eq!(manifest.members[2].sha256, sha256_bytes(&a_content));
        for member in &manifest.members {
            let content = read_member(&bundle, &member.path).expect("member content");
            assert_eq!(content.len() as u64, member.bytes);
            assert_eq!(sha256_bytes(&content), member.sha256, "{}", member.path);
        }

        // The SQLite store next to media is untouched (AFK + window events
        // are exempt from retention).
        let db = activity_core::Db::open_db(&dir.0.join("activity.db")).expect("reopen db");
        assert_eq!(db.event_count().expect("count"), 2);

        // Day/month dirs tidied away for pruned months.
        assert!(!store.media_dir.join("frames/2025/10").exists());
        assert!(!store.media_dir.join("audio/2025/10").exists());

        // Second pass: nothing left to do.
        let again = run_pass(&store, &enabled(365), cold_month_today());
        assert!(again.bundled.is_empty() && again.pruned.is_empty() && again.skipped.is_empty());
    }

    #[test]
    fn hot_and_current_months_are_never_touched() {
        let (dir, store) = test_store("hot-window");
        // Hot (inside 365d) and current-month files must stay raw.
        let (hot, _) = put_raw(
            &store,
            "frames",
            "2026-06",
            "20",
            "120000_D1_hot.jpg",
            "hot",
        );
        let (current, _) = put_raw(
            &store,
            "audio",
            "2026-11",
            "06",
            "080000_mic_now.ogg",
            "now",
        );

        // Plus a cold month to prove the pass ran at all.
        put_raw(
            &store,
            "frames",
            "2025-09",
            "01",
            "000000_D1_old.jpg",
            "old",
        );
        let report = run_pass(&store, &enabled(365), cold_month_today());
        assert_eq!(report.bundled, vec!["2025-09".to_string()]);

        assert!(hot.exists(), "hot window file must stay raw");
        assert!(current.exists(), "current month file must never be touched");
        let db = activity_core::Db::open_db(&dir.0.join("activity.db")).expect("reopen db");
        assert_eq!(db.event_count().expect("count"), 2, "events are exempt");
    }

    #[test]
    fn verify_failure_keeps_raws() {
        let (_dir, store) = test_store("verify-fail");
        put_raw(&store, "frames", "2025-08", "10", "120000_D1_x.jpg", "x");
        put_raw(&store, "frames", "2025-08", "11", "120000_D1_y.jpg", "y");
        let (z, z_content) = put_raw(&store, "frames", "2025-07", "02", "120000_D1_z.jpg", "z");

        // First pass bundles and prunes both cold months.
        let report = run_pass(&store, &enabled(365), cold_month_today());
        assert_eq!(report.bundled.len(), 2, "both cold months bundled");
        assert!(report.skipped.is_empty());

        // Corrupt one bundle on disk (simulate a damaged archive), put its
        // month's raw back byte-identically (as if the prune never
        // happened), run again: verification must fail and the raw must
        // stay.
        let bundle = store.bundle_path("2025-07");
        let original = std::fs::read(&bundle).expect("read bundle");
        std::fs::write(&bundle, &original[..original.len() / 2]).expect("truncate bundle");
        put_raw_back(&store, &z, &z_content);

        let report = run_pass(&store, &enabled(365), cold_month_today());
        assert_eq!(report.pruned, Vec::<(String, usize, u64)>::new());
        assert_eq!(
            report.skipped.len(),
            1,
            "verify failure reported: {:?}",
            report.skipped
        );
        assert_eq!(report.skipped[0].0, "2025-07");
        assert!(!report.skipped[0].1.is_empty(), "a reason is logged");
        assert!(bundle.exists(), "bad bundle left for inspection");
        assert!(z.exists(), "raws must survive a verify failure");

        // A retry hits the same safe outcome (idempotent refusal).
        let report = run_pass(&store, &enabled(365), cold_month_today());
        assert_eq!(report.skipped.len(), 1);
        assert!(z.exists());
    }

    #[test]
    fn partial_prune_completes_idempotently() {
        let (_dir, store) = test_store("partial-prune");
        let seeds: Vec<(PathBuf, Vec<u8>)> = (1..=4)
            .map(|n| {
                put_raw(
                    &store,
                    "frames",
                    "2025-05",
                    &format!("{n:02}"),
                    &format!("120000_D1_f{n}.jpg"),
                    &format!("f{n}"),
                )
            })
            .collect();

        let first = run_pass(&store, &enabled(365), cold_month_today());
        assert_eq!(first.bundled, vec!["2025-05".to_string()]);
        let bundle = store.bundle_path("2025-05");
        let digest_before = sha256_file(&bundle).expect("digest");

        // Simulate a crash mid-prune: the first two raws are back on disk
        // (byte-identical, they were never removed), the verified bundle
        // exists. The survivors' content length is the raw byte count the
        // walker will report.
        let survivors: Vec<&(PathBuf, Vec<u8>)> = seeds.iter().take(2).collect();
        let expected_bytes: u64 = survivors
            .iter()
            .map(|(_, content)| content.len() as u64)
            .sum();
        for (path, content) in &survivors {
            put_raw_back(&store, path, content);
        }

        // Next pass completes the prune without touching the bundle.
        let second = run_pass(&store, &enabled(365), cold_month_today());
        assert_eq!(
            second.pruned,
            vec![("2025-05".to_string(), 2, expected_bytes)]
        );
        assert_eq!(
            sha256_file(&bundle).expect("digest"),
            digest_before,
            "bundle must not be rewritten"
        );
        assert!(!survivors[0].0.exists() && !survivors[1].0.exists());

        // And a third pass is a no-op.
        let third = run_pass(&store, &enabled(365), cold_month_today());
        assert!(third.bundled.is_empty() && third.pruned.is_empty() && third.skipped.is_empty());
    }

    #[test]
    fn late_files_after_prune_are_kept_and_reported() {
        let (_dir, store) = test_store("late-files");
        put_raw(&store, "frames", "2025-04", "01", "120000_D1_a.jpg", "a");
        let report = run_pass(&store, &enabled(365), cold_month_today());
        assert_eq!(report.bundled, vec!["2025-04".to_string()]);

        // A device syncs an old frame late (it was offline for ages).
        let (late, _) = put_raw(
            &store,
            "frames",
            "2025-04",
            "02",
            "120000_D1_late.jpg",
            "late",
        );

        let report = run_pass(&store, &enabled(365), cold_month_today());
        assert_eq!(report.pruned, Vec::<(String, usize, u64)>::new());
        assert_eq!(report.skipped.len(), 1, "late file reported, not deleted");
        assert!(late.exists(), "a file that is in no bundle must stay raw");
    }

    // ------------------------------------------------------ dry-run/stats

    #[test]
    fn dry_run_writes_nothing() {
        let (_dir, store) = test_store("dry-run");
        put_raw(&store, "frames", "2025-03", "01", "120000_D1_a.jpg", "a");
        put_raw(&store, "audio", "2026-11", "06", "080000_mic.ogg", "now");

        let cfg = enabled(365);
        let plan = render_plan(&store, &cfg, cold_month_today());
        assert!(plan.contains("dry run"), "{plan}");
        assert!(plan.contains("2025-03"), "{plan}");
        assert!(
            plan.contains("would bundle into bundles/2025-03.tar.zst"),
            "{plan}"
        );
        // Singular for a single-file month ("1 file", not "1 files").
        assert!(plan.contains("2025-03: 1 file, "), "{plan}");
        assert!(plan.contains("then delete 1 raw file ("), "{plan}");
        assert!(plan.contains("hot window kept raw"), "{plan}");

        // Disabled retention prints the preview banner.
        let plan = render_plan(&store, &RetentionConfig::default(), cold_month_today());
        assert!(plan.contains("DISABLED"), "{plan}");

        // Nothing was written: no bundles dir, raws all still present.
        assert!(!store.bundles_dir.exists());
        assert!(store
            .media_dir
            .join("frames/2025/03/01/120000_D1_a.jpg")
            .exists());
        assert!(store
            .media_dir
            .join("audio/2026/11/06/080000_mic.ogg")
            .exists());
    }

    #[test]
    fn stats_reflect_store_state() {
        let (_dir, store) = test_store("stats");
        let (a, _) = put_raw(&store, "frames", "2025-03", "01", "120000_D1_a.jpg", "aaaa");
        let (b, _) = put_raw(&store, "frames", "2026-06", "20", "120000_D1_b.jpg", "bbbb");
        let (c, _) = put_raw(&store, "audio", "2026-11", "06", "080000_mic.ogg", "cccc");
        let cfg = enabled(365);
        let stats = collect_stats(&store, &cfg, cold_month_today());

        assert!(stats.enabled);
        assert_eq!(stats.hot_days, 365);
        assert_eq!(stats.bundle_count, 0);
        assert_eq!(stats.bundle_bytes, 0);
        assert_eq!(
            stats.hot_bytes.get("frames").copied().unwrap_or(0),
            file_len(&a) + file_len(&b)
        );
        assert_eq!(
            stats.hot_bytes.get("audio").copied().unwrap_or(0),
            file_len(&c)
        );
        assert_eq!(stats.months_pending.len(), 1);
        assert_eq!(stats.months_pending[0].month, "2025-03");
        assert!(
            stats.next_action.contains("bundle 2025-03"),
            "{}",
            stats.next_action
        );

        // After the pass: bundle counted, nothing pending, action "none".
        run_pass(&store, &cfg, cold_month_today());
        let stats = collect_stats(&store, &cfg, cold_month_today());
        assert_eq!(stats.bundle_count, 1);
        assert!(stats.bundle_bytes > 0);
        assert_eq!(stats.months_pending, Vec::<PendingMonth>::new());
        assert_eq!(stats.next_action, "none");
    }

    // ------------------------------------------------------------ helpers

    fn file_len(path: &Path) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    /// Read one member's content from a bundle (test-side extractor):
    /// a streaming decode up to that entry.
    fn read_member(bundle: &Path, name: &str) -> Result<Vec<u8>, String> {
        let file =
            File::open(bundle).map_err(|e| format!("cannot open {}: {e}", bundle.display()))?;
        let decoder = zstd::stream::Decoder::new(file).map_err(|e| format!("zstd decoder: {e}"))?;
        let mut archive = tar::Archive::new(decoder);
        for entry in archive.entries().map_err(|e| format!("tar read: {e}"))? {
            let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
            let entry_name = entry
                .path()
                .map_err(|e| format!("tar path: {e}"))?
                .to_string_lossy()
                .replace('\\', "/");
            if entry_name == name {
                let mut data = Vec::new();
                entry
                    .read_to_end(&mut data)
                    .map_err(|e| format!("cannot read {name}: {e}"))?;
                return Ok(data);
            }
        }
        Err(format!("member {name} not found in bundle"))
    }

    #[test]
    fn pass_report_log_lines_are_human() {
        let report = PassReport {
            bundled: vec!["2025-10".to_string()],
            pruned: vec![("2025-10".to_string(), 3, 1536)],
            skipped: vec![(
                "2025-07".to_string(),
                "bundle verify failed: boom".to_string(),
            )],
        };
        let lines = report.log_lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("bundled 2025-10"), "{}", lines[0]);
        assert!(lines[1].contains("3 files, 1.5 KB"), "{}", lines[1]);
        assert!(lines[2].contains("kept 2025-07 raws"), "{}", lines[2]);
    }

    #[test]
    fn disabled_retention_never_runs() {
        let (_dir, store) = test_store("disabled");
        put_raw(&store, "frames", "2025-02", "01", "120000_D1_a.jpg", "a");
        let report = run_pass(&store, &RetentionConfig::default(), cold_month_today());
        assert_eq!(report, PassReport::default());
        assert!(store
            .media_dir
            .join("frames/2025/02/01/120000_D1_a.jpg")
            .exists());
        assert!(!store.bundles_dir.exists());
    }
}
