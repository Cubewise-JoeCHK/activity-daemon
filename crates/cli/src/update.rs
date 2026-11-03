//! `activity-daemon update` (issue #17): self-update from GitHub Releases.
//!
//! While the daemon runs (config `[sync] auto_update`, checked every
//! `sync.update_interval_s`, default daily) or on demand via
//! `activity-daemon update`, the CLI:
//!
//! 1. `GET https://api.github.com/repos/Cubewise-JoeCHK/activity-daemon/releases/latest`
//!    unauthenticated (`User-Agent: activity-daemon/<version>`, `Accept:
//!    application/vnd.github+json`; 60 req/h limit - a daily check is
//!    comfortably inside it).
//! 2. Compares the release tag with the running version (semver, optional
//!    leading `v`; `<=` current = no-op; malformed tag = warn + skip).
//! 3. Downloads the `activity-daemon-windows-x86_64.exe` asset plus its
//!    `.sha256` companion (the exact names the release workflow uploads)
//!    into `{data_dir}/update/<tag>.exe.tmp`.
//! 4. Verifies the download's sha256 (case-insensitive hex); a mismatch
//!    deletes the download and aborts - the old version keeps running.
//! 5. Swaps files: delete a previous `<exe>.old` (exactly one rollback copy
//!    is kept between updates), rename the running exe to `<exe>.old`
//!    (allowed on Windows while it runs), rename the download over the exe
//!    path; if that last rename fails, the `.old` copy is renamed back so
//!    the daemon is never left without a working exe.
//! 6. The caller (run loop, or the manual `update` command via the
//!    `update/restart.request` control file) then performs the same graceful
//!    shutdown as Ctrl+C - workers are joined so the window watcher flushes
//!    its open session - and relaunches: through the "ActivityDaemon" Task
//!    Scheduler task when it exists, otherwise by spawning the new exe
//!    detached with `run`.
//!
//! Every network access goes through the [`ReleaseFetcher`] /
//! [`AssetDownloader`] traits, so each step is unit-tested against fakes on
//! every platform. Robustness contract (issue #17): network down, GitHub
//! 403 rate limit and malformed release JSON all surface as errors that the
//! run loop logs and ignores - the daemon keeps running and the next check
//! happens after the interval.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use semver::{BuildMetadata, Version};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::startup::TASK_NAME;

/// GitHub API endpoint for the latest published release (drafts and
/// pre-releases are excluded by the API itself).
pub const RELEASE_API_URL: &str =
    "https://api.github.com/repos/Cubewise-JoeCHK/activity-daemon/releases/latest";
/// Asset name the release workflow uploads for the Windows x86_64 cli exe.
pub const WINDOWS_EXE_ASSET: &str = "activity-daemon-windows-x86_64.exe";
/// Update work directory below the data dir.
pub const UPDATE_DIR_NAME: &str = "update";
/// Restart request control file: the manual `update` command writes it
/// after a successful swap; the run loop consumes it (same pattern as the
/// watcher-audio `record.json` control file).
pub const RESTART_REQUEST_FILE: &str = "restart.request";
/// Suffix of the rollback copy kept next to the exe.
pub const OLD_SUFFIX: &str = ".old";

/// Connection timeout shared by the API client and the downloader.
const CONNECT_TIMEOUT_S: u64 = 10;
/// Whole-request timeout for the small `releases/latest` JSON call.
const API_REQUEST_TIMEOUT_S: u64 = 30;
/// Whole-request timeout for exe downloads (a few MB over slow links).
const DOWNLOAD_TIMEOUT_S: u64 = 600;

/// Guard so two updates can never be in flight in one process (within the
/// daemon the check runs on the main loop thread only; the guard also
/// covers a future second entry point).
static UPDATE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// User-Agent for all GitHub requests (API + asset downloads).
pub fn user_agent() -> String {
    format!("activity-daemon/{}", env!("CARGO_PKG_VERSION"))
}

/// The running crate version as semver.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is valid semver")
}

// ---------------------------------------------------------------------------
// Release metadata: trait, GitHub implementation, parsing
// ---------------------------------------------------------------------------

/// One downloadable release asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// The latest published release, as the updater sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// Release tag, e.g. `v0.2.0`.
    pub tag: String,
    pub assets: Vec<Asset>,
}

/// Source of release metadata. The real implementation talks to the GitHub
/// API; tests inject fakes (all network is behind this trait).
pub trait ReleaseFetcher {
    fn fetch_latest(&self) -> Result<ReleaseInfo>;
}

/// Moves bytes from a URL into a local file; see [`ReleaseFetcher`].
pub trait AssetDownloader {
    /// Download `url` into `dest` (created/overwritten); returns the byte
    /// count. Errors on HTTP failure or an empty response body.
    fn download_to(&self, url: &str, dest: &Path) -> Result<u64>;
}

/// `releases/latest` JSON wire format (only the fields we need).
#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// Parse the `releases/latest` JSON body into a [`ReleaseInfo`].
pub fn parse_release_json(body: &str) -> Result<ReleaseInfo> {
    let raw: GitHubRelease = serde_json::from_str(body)
        .with_context(|| format!("malformed release JSON: {}", clip(body)))?;
    Ok(ReleaseInfo {
        tag: raw.tag_name,
        assets: raw
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
            })
            .collect(),
    })
}

/// Trim + clip a (possibly huge) body for log/error lines.
fn clip(s: &str) -> String {
    s.trim().chars().take(200).collect()
}

fn http_error(e: ureq::Error, what: &str) -> anyhow::Error {
    match e {
        // 403 = unauthenticated rate limit, 404 = no releases published yet.
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            if code == 404 {
                anyhow!("{what}: HTTP 404 (no releases published yet)")
            } else {
                anyhow!("{what}: HTTP {code}: {}", clip(&body))
            }
        }
        ureq::Error::Transport(t) => anyhow!("{what}: transport error: {t}"),
    }
}

/// Real [`ReleaseFetcher`]: the unauthenticated `releases/latest` call.
pub struct GitHubFetcher {
    agent: ureq::Agent,
}

impl GitHubFetcher {
    pub fn new() -> GitHubFetcher {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(CONNECT_TIMEOUT_S))
            .timeout(Duration::from_secs(API_REQUEST_TIMEOUT_S))
            .build();
        GitHubFetcher { agent }
    }
}

impl Default for GitHubFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ReleaseFetcher for GitHubFetcher {
    fn fetch_latest(&self) -> Result<ReleaseInfo> {
        let resp = self
            .agent
            .get(RELEASE_API_URL)
            .set("User-Agent", &user_agent())
            .set("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| http_error(e, "cannot fetch the latest release"))?;
        let body = resp
            .into_string()
            .context("cannot read the release API response body")?;
        parse_release_json(&body)
    }
}

/// Real [`AssetDownloader`]: streaming GET into the destination file.
pub struct HttpDownloader {
    agent: ureq::Agent,
}

impl HttpDownloader {
    pub fn new() -> HttpDownloader {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(CONNECT_TIMEOUT_S))
            .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_S))
            .build();
        HttpDownloader { agent }
    }
}

impl Default for HttpDownloader {
    fn default() -> Self {
        Self::new()
    }
}

impl AssetDownloader for HttpDownloader {
    fn download_to(&self, url: &str, dest: &Path) -> Result<u64> {
        let mut reader = self
            .agent
            .get(url)
            .set("User-Agent", &user_agent())
            .call()
            .map_err(|e| http_error(e, "cannot download the asset"))?
            .into_reader();
        let mut file = std::fs::File::create(dest)
            .with_context(|| format!("cannot create {}", dest.display()))?;
        let bytes = std::io::copy(&mut reader, &mut file)
            .with_context(|| format!("cannot write {}", dest.display()))?;
        if bytes == 0 {
            bail!(
                "downloaded {} but the response body is empty",
                dest.display()
            );
        }
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// Version compare + asset selection
// ---------------------------------------------------------------------------

/// Outcome of comparing the latest release tag with the running version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDecision {
    /// Remote tag parses to `<=` the running version.
    UpToDate,
    /// Remote tag is not valid semver (with optional leading `v`): skipped.
    MalformedTag(String),
    /// Remote tag parses to a strictly higher version.
    Available(Version),
}

/// Strip an optional leading `v`/`V` and surrounding whitespace, then parse
/// the rest as semver.
pub fn parse_tag(tag: &str) -> Result<Version> {
    let t = tag.trim();
    let t = t.strip_prefix(['v', 'V']).unwrap_or(t);
    Version::parse(t).with_context(|| format!("malformed release tag {tag:?}"))
}

/// Spec precedence comparison: build metadata ignored. (`Version`'s own
/// `Ord` includes build metadata as a final tiebreaker to stay consistent
/// with its derived `Eq`, which would order `0.1.0+build.7 > 0.1.0` - not
/// what semver precedence says. Pre-release ordering itself is spec-correct
/// in the crate: a pre-release orders below its release.)
fn is_newer_than(current: &Version, remote: &Version) -> bool {
    fn precedence(v: &Version) -> Version {
        Version {
            major: v.major,
            minor: v.minor,
            patch: v.patch,
            pre: v.pre.clone(),
            build: BuildMetadata::EMPTY,
        }
    }
    precedence(remote) > precedence(current)
}

/// Compare the running version against the release tag (build metadata is
/// ignored and pre-releases order below their release, per semver).
pub fn decide_update(current: &Version, tag: &str) -> UpdateDecision {
    match parse_tag(tag) {
        Ok(remote) if is_newer_than(current, &remote) => UpdateDecision::Available(remote),
        Ok(_) => UpdateDecision::UpToDate,
        Err(_) => UpdateDecision::MalformedTag(tag.to_string()),
    }
}

/// The Windows x86_64 cli exe asset and its `.sha256` companion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedAssets {
    pub exe: Asset,
    pub sha256: Asset,
}

/// Find the exe asset + sha256 companion by the exact names the release
/// workflow uploads (`activity-daemon-windows-x86_64.exe[.sha256]`), with a
/// tolerant fallback should the artifact ever be renamed. Linux server
/// assets are not selected here.
pub fn select_assets(assets: &[Asset]) -> Result<SelectedAssets> {
    let exe = assets
        .iter()
        .find(|a| a.name == WINDOWS_EXE_ASSET)
        .or_else(|| {
            assets
                .iter()
                .find(|a| a.name.ends_with(".exe") && a.name.contains("windows-x86_64"))
        })
        .context("no Windows x86_64 exe asset in the release")?
        .clone();
    let sha_name = format!("{}.sha256", exe.name);
    let sha256 = assets
        .iter()
        .find(|a| a.name == sha_name)
        .with_context(|| format!("no {sha_name} companion asset in the release"))?
        .clone();
    Ok(SelectedAssets { exe, sha256 })
}

// ---------------------------------------------------------------------------
// sha256 verification
// ---------------------------------------------------------------------------

/// Lowercase hex sha256 of a file (streamed).
pub fn file_sha256_hex(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("cannot open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .with_context(|| format!("cannot read {} for hashing", path.display()))?;
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Verify a file against a `sha256sum`-style digest line (the digest is the
/// first whitespace token; hex compare is case-insensitive).
pub fn verify_sha256(path: &Path, digest_line: &str) -> Result<()> {
    let expected = digest_line
        .split_whitespace()
        .next()
        .context("the .sha256 file is empty")?
        .to_ascii_lowercase();
    let actual = file_sha256_hex(path)?;
    if actual != expected {
        bail!(
            "sha256 mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The swap: download -> verify -> rotate exe/.old
// ---------------------------------------------------------------------------

/// Tag -> safe file name stem (tags are `vX.Y.Z` in practice; anything
/// outside `[A-Za-z0-9._-]` is replaced with `_`).
fn sanitize_tag(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Rollback copy path: `<exe>.old`.
pub fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or(exe.as_os_str()).to_os_string();
    name.push(OLD_SUFFIX);
    exe.with_file_name(name)
}

/// Swap steps (c)-(e): remove a stale rollback copy so exactly one exists
/// between updates, move the running exe aside (`<exe>.old` - allowed on
/// Windows while it runs), move the new exe into place. If the last rename
/// fails, the `.old` copy is renamed back so the daemon is never left
/// without a working exe.
pub fn swap_in_place(exe: &Path, new_exe: &Path) -> Result<()> {
    let old = old_path(exe);
    if old.exists() {
        std::fs::remove_file(&old).with_context(|| {
            format!("cannot remove the previous rollback copy {}", old.display())
        })?;
        log::info!("removed the previous rollback copy {}", old.display());
    }
    std::fs::rename(exe, &old)
        .with_context(|| format!("cannot rename {} to {}", exe.display(), old.display()))?;
    if let Err(e) = std::fs::rename(new_exe, exe) {
        // Restore: put the old exe back at the canonical path.
        match std::fs::rename(&old, exe) {
            Ok(()) => bail!(
                "cannot move the new exe into place ({e}); restored {} from {}, still on the old version",
                exe.display(),
                old.display()
            ),
            Err(restore_err) => bail!(
                "cannot move the new exe into place ({e}); and the rollback restore failed too ({restore_err}): the old binary is at {} - move it back manually",
                old.display()
            ),
        }
    }
    Ok(())
}

/// One applied update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Release tag that was installed, e.g. `v0.2.0`.
    pub tag: String,
}

/// Everything the updater needs. Production wiring is [`Updater::real`];
/// tests construct it with fakes and temp dirs.
pub struct Updater<'a> {
    /// Running version (of the cli crate).
    pub current: Version,
    /// The running exe - the file that gets swapped.
    pub exe: PathBuf,
    /// Data dir; the work dir is `{data_dir}/update`.
    pub data_dir: PathBuf,
    fetcher: &'a dyn ReleaseFetcher,
    downloader: &'a dyn AssetDownloader,
}

impl<'a> Updater<'a> {
    pub fn new(
        current: Version,
        exe: PathBuf,
        data_dir: PathBuf,
        fetcher: &'a dyn ReleaseFetcher,
        downloader: &'a dyn AssetDownloader,
    ) -> Updater<'a> {
        Updater {
            current,
            exe,
            data_dir,
            fetcher,
            downloader,
        }
    }

    /// Production wiring: running version, current exe, real GitHub fetcher
    /// and HTTP downloader.
    pub fn real(
        data_dir: &Path,
        fetcher: &'a dyn ReleaseFetcher,
        downloader: &'a dyn AssetDownloader,
    ) -> Result<Updater<'a>> {
        let exe = std::env::current_exe().context("cannot resolve the current exe path")?;
        Ok(Updater::new(
            current_version(),
            exe,
            data_dir.to_path_buf(),
            fetcher,
            downloader,
        ))
    }

    /// The `{data_dir}/update` work dir.
    pub fn update_dir(&self) -> PathBuf {
        self.data_dir.join(UPDATE_DIR_NAME)
    }

    /// Fetch the latest release and compare it with the running version
    /// (no download, no files touched). Used by `update check`.
    pub fn fetch_decision(&self) -> Result<UpdateDecision> {
        let release = self.fetcher.fetch_latest()?;
        Ok(decide_update(&self.current, &release.tag))
    }

    /// Fetch the latest release and, when a newer version is available,
    /// download, verify and swap it in. `Ok(None)` = up to date or skipped
    /// (a debug/warn line is logged); `Ok(Some(_))` = the new exe is in
    /// place and the caller must restart to run it. Network/API failures
    /// are returned as errors - the run loop logs and ignores them.
    pub fn check_and_apply(&self) -> Result<Option<Applied>> {
        if UPDATE_IN_PROGRESS.swap(true, Ordering::SeqCst) {
            log::warn!("update already in flight; skipping this check");
            return Ok(None);
        }
        let result = self.check_and_apply_inner();
        UPDATE_IN_PROGRESS.store(false, Ordering::SeqCst);
        result
    }

    fn check_and_apply_inner(&self) -> Result<Option<Applied>> {
        let release = self.fetcher.fetch_latest()?;
        match decide_update(&self.current, &release.tag) {
            UpdateDecision::UpToDate => {
                log::debug!(
                    "auto-update: latest release {} <= running {}; up to date",
                    release.tag,
                    self.current
                );
                Ok(None)
            }
            UpdateDecision::MalformedTag(tag) => {
                log::warn!("auto-update: release tag {tag:?} is not valid semver; skipping");
                Ok(None)
            }
            UpdateDecision::Available(remote) => {
                log::info!(
                    "auto-update: update available: {} -> {remote} (tag {})",
                    self.current,
                    release.tag
                );
                self.download_verify_swap(&release)?;
                Ok(Some(Applied { tag: release.tag }))
            }
        }
    }

    /// Steps (a)-(e): download exe + .sha256 into the work dir, verify, swap.
    fn download_verify_swap(&self, release: &ReleaseInfo) -> Result<()> {
        let selected = select_assets(&release.assets)?;
        let dir = self.update_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create the update dir {}", dir.display()))?;
        let stem = sanitize_tag(&release.tag);
        let exe_tmp = dir.join(format!("{stem}.exe.tmp"));
        let sha_tmp = dir.join(format!("{stem}.exe.tmp.sha256"));
        // A leftover tmp from an aborted run must not be mistaken for this
        // release's download.
        let _ = std::fs::remove_file(&exe_tmp);
        let _ = std::fs::remove_file(&sha_tmp);

        // (a) download the exe (HTTP success is implied by ureq's non-2xx
        // error; non-empty is enforced by the downloader).
        let bytes = self
            .downloader
            .download_to(&selected.exe.url, &exe_tmp)
            .with_context(|| format!("download of {} failed", selected.exe.name))?;
        log::info!("downloaded {} ({} bytes)", selected.exe.name, bytes);
        // (b) download the digest and verify the exe against it.
        self.downloader
            .download_to(&selected.sha256.url, &sha_tmp)
            .with_context(|| format!("download of {} failed", selected.sha256.name))?;
        let digest = std::fs::read_to_string(&sha_tmp)
            .with_context(|| format!("cannot read {}", sha_tmp.display()))?;
        if let Err(e) = verify_sha256(&exe_tmp, &digest) {
            let _ = std::fs::remove_file(&exe_tmp);
            let _ = std::fs::remove_file(&sha_tmp);
            log::warn!(
                "auto-update: {e}; deleted the download, staying on {}",
                self.current
            );
            bail!("{e}; the old version keeps running");
        }
        log::info!("sha256 verified for {}", selected.exe.name);

        // (c)-(e) rotate exe/.old with restore-on-failure.
        swap_in_place(&self.exe, &exe_tmp).context("swapping the updated exe into place")?;
        let _ = std::fs::remove_file(&sha_tmp);
        log::info!(
            "update applied: {} -> {} (rollback copy: {})",
            self.current,
            release.tag,
            old_path(&self.exe).display()
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Restart plumbing: control file + relaunch
// ---------------------------------------------------------------------------

/// Path of the restart request control file for a data dir.
pub fn restart_request_path(data_dir: &Path) -> PathBuf {
    data_dir.join(UPDATE_DIR_NAME).join(RESTART_REQUEST_FILE)
}

/// Ask a running daemon to shut down gracefully and relaunch (written by
/// the manual `update` command after a successful swap).
pub fn request_restart(data_dir: &Path) -> Result<()> {
    let path = restart_request_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(&path, "restart\n").with_context(|| format!("cannot write {}", path.display()))
}

/// Poll from the run loop: `true` (exactly once) when a restart is pending;
/// consumes the request file.
pub fn take_restart_request(data_dir: &Path) -> bool {
    let path = restart_request_path(data_dir);
    if path.exists() {
        let _ = std::fs::remove_file(&path);
        true
    } else {
        false
    }
}

/// How the post-update relaunch happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelaunchMode {
    /// `schtasks /run /tn ActivityDaemon` (autostart task installed).
    ScheduledTask,
    /// Spawn the new exe detached with the `run` argument.
    DetachedSpawn,
}

/// Pure decision (unit-tested); the process calls are cfg(windows)-gated.
pub fn relaunch_mode(task_exists: bool) -> RelaunchMode {
    if task_exists {
        RelaunchMode::ScheduledTask
    } else {
        RelaunchMode::DetachedSpawn
    }
}

/// Relaunch the (updated) daemon: through the "ActivityDaemon" Task
/// Scheduler task when it exists, otherwise detached with `run`.
pub fn relaunch_after_update() -> Result<()> {
    let exe = std::env::current_exe().context("cannot resolve the current exe path")?;
    match relaunch_mode(task_exists()) {
        RelaunchMode::ScheduledTask => {
            log::info!("relaunch: starting the scheduled task {TASK_NAME}");
            schtasks_run()?;
            println!("relaunched via the scheduled task {TASK_NAME}");
        }
        RelaunchMode::DetachedSpawn => {
            log::info!("relaunch: spawning {} detached", exe.display());
            spawn_detached(&exe)?;
            println!("relaunched {} detached", exe.display());
        }
    }
    Ok(())
}

/// Whether the "ActivityDaemon" scheduled task exists; any `schtasks
/// /query` failure counts as absent (the fallback is a detached spawn).
#[cfg(windows)]
fn task_exists() -> bool {
    match std::process::Command::new("schtasks")
        .args(["/query", "/tn", TASK_NAME])
        .output()
    {
        Ok(out) => out.status.success(),
        Err(e) => {
            log::debug!("schtasks /query failed ({e}); treating the task as absent");
            false
        }
    }
}

#[cfg(not(windows))]
fn task_exists() -> bool {
    // Task Scheduler is a Windows concern (see startup.rs); non-Windows
    // always uses the detached spawn fallback.
    false
}

#[cfg(windows)]
fn schtasks_run() -> Result<()> {
    let output = std::process::Command::new("schtasks")
        .args(["/run", "/tn", TASK_NAME])
        .output()
        .context("cannot run schtasks.exe")?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("schtasks /run /tn {TASK_NAME} failed: {}", stderr.trim())
    }
}

#[cfg(not(windows))]
fn schtasks_run() -> Result<()> {
    // Unreachable through relaunch_after_update (task_exists() is false on
    // non-Windows); this stub only keeps the code cross-platform-compilable.
    bail!("schtasks is only available on Windows: /run /tn {TASK_NAME}")
}

/// Spawn `exe run` detached so it survives this process exiting.
#[cfg(windows)]
fn spawn_detached(exe: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS: no inherited console window; CREATE_NEW_PROCESS_GROUP:
    // the child does not receive this console's ctrl events.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    std::process::Command::new(exe)
        .arg("run")
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .with_context(|| format!("cannot spawn {}", exe.display()))?;
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached(exe: &Path) -> Result<()> {
    // A POSIX orphan keeps running after the parent exits; no console to detach.
    std::process::Command::new(exe)
        .arg("run")
        .spawn()
        .with_context(|| format!("cannot spawn {}", exe.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// `update` / `update check` subcommands
// ---------------------------------------------------------------------------

/// What `update <action>` should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Subcommand)]
pub enum UpdateAction {
    /// Report the current and latest versions without changing anything.
    Check,
}

/// `update` / `update check` entry point.
pub fn cmd_update(action: Option<UpdateAction>, config_override: Option<&Path>) -> Result<()> {
    let resolved = crate::config::resolve(config_override)?;
    let data_dir = resolved.config.data_dir;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;
    match action {
        Some(UpdateAction::Check) => check_only(&data_dir),
        None => apply_now(&data_dir),
    }
}

/// `update check`: report only; never touches files. `sync.auto_update`
/// gates the automatic check in the run loop, not this manual command.
fn check_only(data_dir: &Path) -> Result<()> {
    let fetcher = GitHubFetcher::new();
    let downloader = HttpDownloader::new();
    let updater = Updater::real(data_dir, &fetcher, &downloader)?;
    let current = updater.current.clone();
    match updater.fetch_decision()? {
        UpdateDecision::UpToDate => println!("up to date: running {current}, no newer release"),
        UpdateDecision::MalformedTag(tag) => {
            println!("running {current}; latest release tag {tag:?} is not valid semver - skipped")
        }
        UpdateDecision::Available(remote) => println!(
            "update available: {current} -> {remote} (run `activity-daemon update` to apply)"
        ),
    }
    Ok(())
}

/// `update`: check now and apply if available. When the daemon is running,
/// the graceful restart is requested via the `update/restart.request`
/// control file; otherwise a hint to start it is printed.
fn apply_now(data_dir: &Path) -> Result<()> {
    let fetcher = GitHubFetcher::new();
    let downloader = HttpDownloader::new();
    let updater = Updater::real(data_dir, &fetcher, &downloader)?;
    match updater.check_and_apply()? {
        None => println!("already up to date (running {})", updater.current),
        Some(applied) => {
            println!(
                "updated to {}: new exe at {}, rollback copy at {}",
                applied.tag,
                updater.exe.display(),
                old_path(&updater.exe).display()
            );
            match crate::heartbeat::status(data_dir) {
                crate::heartbeat::HeartbeatState::Running { heartbeat } => {
                    request_restart(data_dir)?;
                    println!(
                        "the running daemon (pid {}) was asked to restart gracefully; it relaunches on {} within a second",
                        heartbeat.pid, applied.tag
                    );
                }
                _ => println!(
                    "the daemon is not running; start it with `activity-daemon run` (or `startup install`)"
                ),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes tests that exercise [`Updater::check_and_apply`]: the
    /// in-flight guard is process-global, and cargo runs tests in parallel
    /// threads (a concurrent check would spuriously return `Ok(None)`).
    static APPLY_LOCK: Mutex<()> = Mutex::new(());

    fn apply_lock() -> MutexGuard<'static, ()> {
        APPLY_LOCK.lock().expect("apply lock")
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activity-cli-update-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// Sample `releases/latest` body with the exact asset names the release
    /// workflow uploads (plus the Linux server pair, which must be ignored
    /// by the selection).
    const SAMPLE_RELEASE_JSON: &str = r#"{
        "tag_name": "v0.2.0",
        "assets": [
            {"name": "activity-daemon-windows-x86_64.exe",
             "browser_download_url": "https://example.test/dl/activity-daemon-windows-x86_64.exe"},
            {"name": "activity-daemon-windows-x86_64.exe.sha256",
             "browser_download_url": "https://example.test/dl/activity-daemon-windows-x86_64.exe.sha256"},
            {"name": "activity-server-linux-x86_64",
             "browser_download_url": "https://example.test/dl/activity-server-linux-x86_64"},
            {"name": "activity-server-linux-x86_64.sha256",
             "browser_download_url": "https://example.test/dl/activity-server-linux-x86_64.sha256"}
        ]
    }"#;

    struct FakeFetcher {
        release: Option<ReleaseInfo>,
        error: Option<String>,
    }

    impl ReleaseFetcher for FakeFetcher {
        fn fetch_latest(&self) -> Result<ReleaseInfo> {
            match &self.error {
                Some(e) => bail!("{e}"),
                None => self.release.clone().context("no release configured"),
            }
        }
    }

    /// Writes canned bodies per URL.
    struct FakeDownloader {
        bodies: HashMap<String, String>,
    }

    impl AssetDownloader for FakeDownloader {
        fn download_to(&self, url: &str, dest: &Path) -> Result<u64> {
            let body = self
                .bodies
                .get(url)
                .with_context(|| format!("unexpected url {url}"))?;
            std::fs::write(dest, body).with_context(|| format!("write {}", dest.display()))?;
            Ok(body.len() as u64)
        }
    }

    fn release_with_default_assets(tag: &str, exe_url: &str, sha_url: &str) -> ReleaseInfo {
        ReleaseInfo {
            tag: tag.to_string(),
            assets: vec![
                Asset {
                    name: WINDOWS_EXE_ASSET.to_string(),
                    url: exe_url.to_string(),
                },
                Asset {
                    name: format!("{WINDOWS_EXE_ASSET}.sha256"),
                    url: sha_url.to_string(),
                },
            ],
        }
    }

    #[test]
    fn parse_release_json_maps_tag_and_assets() {
        let info = parse_release_json(SAMPLE_RELEASE_JSON).expect("parse sample");
        assert_eq!(info.tag, "v0.2.0");
        assert_eq!(info.assets.len(), 4);
        assert_eq!(info.assets[0].name, WINDOWS_EXE_ASSET);
        assert_eq!(
            info.assets[0].url,
            "https://example.test/dl/activity-daemon-windows-x86_64.exe"
        );
        assert_eq!(
            info.assets[1].url,
            "https://example.test/dl/activity-daemon-windows-x86_64.exe.sha256"
        );
        // Malformed JSON (wrong types) must be an error, not a panic.
        assert!(parse_release_json("{\"tag_name\": 12}").is_err());
        assert!(parse_release_json("not json at all").is_err());
    }

    #[test]
    fn select_assets_finds_the_windows_pair_and_ignores_linux() {
        let info = parse_release_json(SAMPLE_RELEASE_JSON).expect("parse sample");
        let sel = select_assets(&info.assets).expect("select");
        assert_eq!(sel.exe.name, WINDOWS_EXE_ASSET);
        assert_eq!(sel.sha256.name, format!("{WINDOWS_EXE_ASSET}.sha256"));

        // No assets / missing sha256 companion -> errors.
        assert!(select_assets(&[]).is_err());
        let exe_only = vec![Asset {
            name: WINDOWS_EXE_ASSET.to_string(),
            url: "u".to_string(),
        }];
        assert!(select_assets(&exe_only).is_err());
    }

    #[test]
    fn parse_tag_strips_v_and_whitespace() {
        assert_eq!(
            parse_tag("v1.2.3").expect("v prefix"),
            Version::parse("1.2.3").expect("plain")
        );
        assert_eq!(
            parse_tag("  V2.0.0 ").expect("V prefix + spaces"),
            Version::parse("2.0.0").expect("plain")
        );
        assert_eq!(
            parse_tag("0.2.0-alpha.1").expect("bare semver"),
            Version::parse("0.2.0-alpha.1").expect("plain")
        );
        assert!(parse_tag("v1.2").is_err(), "missing patch");
        assert!(parse_tag("latest").is_err());
    }

    #[test]
    fn decide_update_semver_and_prerelease_edges() {
        let cur = Version::parse("0.1.0").expect("current");
        // Strictly newer -> available.
        assert_eq!(
            decide_update(&cur, "v0.2.0"),
            UpdateDecision::Available(Version::parse("0.2.0").expect("remote"))
        );
        assert_eq!(
            decide_update(&cur, "0.2.0-alpha.1"),
            UpdateDecision::Available(Version::parse("0.2.0-alpha.1").expect("remote"))
        );
        // Equal or older -> up to date.
        assert_eq!(decide_update(&cur, "v0.1.0"), UpdateDecision::UpToDate);
        assert_eq!(decide_update(&cur, "0.0.9"), UpdateDecision::UpToDate);
        // Build metadata is ignored: 0.1.0+build == 0.1.0.
        assert_eq!(
            decide_update(&cur, "v0.1.0+build.7"),
            UpdateDecision::UpToDate
        );
        // Pre-release of the current version orders below it.
        assert_eq!(decide_update(&cur, "v0.1.0-rc.1"), UpdateDecision::UpToDate);
        // Malformed tags are reported, never guessed.
        assert_eq!(
            decide_update(&cur, "not-a-version"),
            UpdateDecision::MalformedTag("not-a-version".to_string())
        );
        assert_eq!(
            decide_update(&cur, "v1.2"),
            UpdateDecision::MalformedTag("v1.2".to_string())
        );
    }

    #[test]
    fn sha256_verify_ok_mismatch_and_case_insensitive() {
        let dir = temp_dir("sha");
        let file = dir.join("blob.bin");
        std::fs::write(&file, b"new exe bytes").expect("write blob");
        let digest = file_sha256_hex(&file).expect("hash");
        assert_eq!(digest.len(), 64);
        // sha256sum-style line: digest is the first whitespace token.
        verify_sha256(&file, &format!("{digest}  blob.bin\n")).expect("digest line ok");
        // Case-insensitive hex compare.
        verify_sha256(&file, &digest.to_uppercase()).expect("uppercase ok");
        // Mismatch -> error.
        let err = verify_sha256(&file, "deadbeef").expect_err("mismatch must fail");
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");
        // Canonical empty-file digest (independent constant).
        let empty = dir.join("empty.bin");
        std::fs::write(&empty, b"").expect("write empty");
        assert_eq!(
            file_sha256_hex(&empty).expect("hash empty"),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // Empty digest line -> error.
        assert!(verify_sha256(&file, "   \n").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_tag_and_old_path() {
        assert_eq!(sanitize_tag("v0.2.0"), "v0.2.0");
        assert_eq!(sanitize_tag("v0.2.0-beta.1+build"), "v0.2.0-beta.1_build");
        let exe = Path::new("somewhere").join("activity-daemon.exe");
        assert_eq!(
            old_path(&exe),
            exe.with_file_name("activity-daemon.exe.old")
        );
    }

    #[test]
    fn swap_keeps_exactly_one_rollback_copy() {
        let dir = temp_dir("swap");
        let exe = dir.join("activity-daemon.exe");
        std::fs::write(&exe, b"old").expect("write old exe");
        let new = dir.join("downloaded.exe.tmp");
        std::fs::write(&new, b"new").expect("write new exe");

        swap_in_place(&exe, &new).expect("swap");
        assert_eq!(std::fs::read(&exe).expect("read"), b"new");
        assert_eq!(std::fs::read(old_path(&exe)).expect("read"), b"old");
        assert!(!new.exists(), "tmp consumed");

        // Second swap rotates the rollback copy: still exactly one .old.
        std::fs::write(&new, b"newer").expect("write newer");
        swap_in_place(&exe, &new).expect("second swap");
        assert_eq!(std::fs::read(&exe).expect("read"), b"newer");
        assert_eq!(
            std::fs::read(old_path(&exe)).expect("read"),
            b"new",
            "rollback copy is the previous version"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The failure-restore path (step e): when the new exe cannot be moved
    /// into place, the `.old` copy is renamed back and the canonical exe
    /// path still holds the old working version.
    #[test]
    fn swap_restores_the_old_exe_when_the_new_one_cannot_be_moved() {
        let dir = temp_dir("swapfail");
        let exe = dir.join("activity-daemon.exe");
        std::fs::write(&exe, b"old").expect("write old exe");
        // A missing source makes the second rename fail on every platform.
        let missing = dir.join("does-not-exist.exe");

        let err = swap_in_place(&exe, &missing).expect_err("swap must fail");
        assert!(err.to_string().contains("restored"), "{err}");
        assert_eq!(
            std::fs::read(&exe).expect("read"),
            b"old",
            "old version restored at the canonical path"
        );
        assert!(!old_path(&exe).exists(), "no .old left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_and_apply_full_flow_with_fakes() {
        let _lock = apply_lock();
        let dir = temp_dir("apply");
        let exe = dir.join("activity-daemon.exe");
        std::fs::write(&exe, b"old-exe-bytes").expect("write old exe");

        let new_body = "new-exe-bytes";
        let digest_hex = hex(&Sha256::digest(new_body));
        let release = release_with_default_assets("v0.2.0", "fake://exe", "fake://sha");
        let fetcher = FakeFetcher {
            release: Some(release),
            error: None,
        };
        let downloader = FakeDownloader {
            bodies: HashMap::from([
                ("fake://exe".to_string(), new_body.to_string()),
                (
                    "fake://sha".to_string(),
                    format!("{digest_hex}  activity-daemon-windows-x86_64.exe\n"),
                ),
            ]),
        };
        let updater = Updater::new(
            current_version(),
            exe.clone(),
            dir.clone(),
            &fetcher,
            &downloader,
        );

        let applied = updater
            .check_and_apply()
            .expect("apply succeeds")
            .expect("an update was applied");
        assert_eq!(applied.tag, "v0.2.0");
        assert_eq!(std::fs::read(&exe).expect("read"), new_body.as_bytes());
        assert_eq!(
            std::fs::read(old_path(&exe)).expect("read"),
            b"old-exe-bytes"
        );
        // No leftovers in the work dir.
        let leftovers: Vec<_> = std::fs::read_dir(updater.update_dir())
            .expect("read update dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert!(leftovers.is_empty(), "no tmp leftovers: {leftovers:?}");

        // After the restart the new version re-checks the same release:
        // up to date, nothing changes.
        let updater_new = Updater::new(
            Version::parse("0.2.0").expect("new version"),
            exe.clone(),
            dir.clone(),
            &fetcher,
            &downloader,
        );
        assert_eq!(updater_new.check_and_apply().expect("check"), None);
        assert_eq!(std::fs::read(&exe).expect("read"), new_body.as_bytes());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_and_apply_sha_mismatch_aborts_and_keeps_the_old_exe() {
        let _lock = apply_lock();
        let dir = temp_dir("badsha");
        let exe = dir.join("activity-daemon.exe");
        std::fs::write(&exe, b"old-exe-bytes").expect("write old exe");
        let release = release_with_default_assets("v0.2.0", "fake://exe", "fake://sha");
        let fetcher = FakeFetcher {
            release: Some(release),
            error: None,
        };
        let downloader = FakeDownloader {
            bodies: HashMap::from([
                ("fake://exe".to_string(), "tampered".to_string()),
                ("fake://sha".to_string(), format!("{}  x\n", "0".repeat(64))),
            ]),
        };
        let updater = Updater::new(
            current_version(),
            exe.clone(),
            dir.clone(),
            &fetcher,
            &downloader,
        );

        let err = updater.check_and_apply().expect_err("mismatch must abort");
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");
        // The download was deleted, the old version is untouched.
        assert!(!updater.update_dir().join("v0.2.0.exe.tmp").exists());
        assert_eq!(std::fs::read(&exe).expect("read"), b"old-exe-bytes");
        assert!(!old_path(&exe).exists(), "no swap happened");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_and_apply_skips_up_to_date_and_malformed_tags() {
        let _lock = apply_lock();
        let dir = temp_dir("skip");
        let exe = dir.join("activity-daemon.exe");
        std::fs::write(&exe, b"old").expect("write old exe");
        let downloader = FakeDownloader {
            bodies: HashMap::new(),
        };

        // Up to date.
        let fetcher = FakeFetcher {
            release: Some(release_with_default_assets("v0.1.0", "u", "s")),
            error: None,
        };
        let updater = Updater::new(
            current_version(),
            exe.clone(),
            dir.clone(),
            &fetcher,
            &downloader,
        );
        assert_eq!(updater.check_and_apply().expect("check"), None);

        // Malformed tag: warn + skip, still Ok(None).
        let fetcher = FakeFetcher {
            release: Some(release_with_default_assets("latest", "u", "s")),
            error: None,
        };
        let updater = Updater::new(
            current_version(),
            exe.clone(),
            dir.clone(),
            &fetcher,
            &downloader,
        );
        assert_eq!(updater.check_and_apply().expect("check"), None);
        assert_eq!(std::fs::read(&exe).expect("read"), b"old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The run loop must receive fetch failures as `Err` and carry on (the
    /// daemon never crashes on network / rate limit / malformed JSON).
    #[test]
    fn fetch_failures_surface_as_errors() {
        let _lock = apply_lock();
        let dir = temp_dir("netfail");
        let fetcher = FakeFetcher {
            release: None,
            error: Some("cannot fetch the latest release: HTTP 403: rate limited".to_string()),
        };
        let downloader = FakeDownloader {
            bodies: HashMap::new(),
        };
        let updater = Updater::new(
            current_version(),
            dir.join("activity-daemon.exe"),
            dir.clone(),
            &fetcher,
            &downloader,
        );
        let err = updater.check_and_apply().expect_err("network failure");
        assert!(err.to_string().contains("403"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_request_roundtrip() {
        let dir = temp_dir("restart");
        assert!(!take_restart_request(&dir), "no request yet");
        request_restart(&dir).expect("write request");
        assert!(restart_request_path(&dir).exists());
        assert!(take_restart_request(&dir), "request seen once");
        assert!(!restart_request_path(&dir).exists(), "consumed");
        assert!(!take_restart_request(&dir), "exactly once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// While an update is in flight, a second check in the same process is
    /// a no-op (`Ok(None)`): never two updates in flight.
    #[test]
    fn check_and_apply_never_runs_two_updates_in_flight() {
        let _lock = apply_lock();
        let dir = temp_dir("guard");
        let release = release_with_default_assets("v0.2.0", "u", "s");
        let fetcher = FakeFetcher {
            release: Some(release),
            error: None,
        };
        let downloader = FakeDownloader {
            bodies: HashMap::new(),
        };
        let updater = Updater::new(
            current_version(),
            dir.join("activity-daemon.exe"),
            dir.clone(),
            &fetcher,
            &downloader,
        );
        UPDATE_IN_PROGRESS.store(true, Ordering::SeqCst);
        let result = updater.check_and_apply().expect("skipped, not an error");
        UPDATE_IN_PROGRESS.store(false, Ordering::SeqCst);
        assert_eq!(result, None, "second update skipped");
        assert!(
            !dir.join("activity-daemon.exe").exists(),
            "no download or swap happened"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relaunch_mode_depends_on_task_presence() {
        assert_eq!(relaunch_mode(true), RelaunchMode::ScheduledTask);
        assert_eq!(relaunch_mode(false), RelaunchMode::DetachedSpawn);
        assert!(user_agent().starts_with("activity-daemon/"));
    }
}
