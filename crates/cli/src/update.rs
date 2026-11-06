//! `activity-daemon update` (issue #17): self-update from GitHub Releases.
//!
//! Release checking, tag comparison and sha256 verification live in the
//! shared [`activity_update`] crate (issue #32 - the hub updater uses the
//! same contract). This module is the Windows client application of it:
//!
//! 1. Fetches the latest release through `activity_update` (unauthenticated
//!    `releases/latest`; 60 req/h limit - a daily check is comfortably
//!    inside it).
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

use activity_update::{
    decide_update, sanitize_tag, select_asset_pair, verify_sha256, Asset, AssetDownloader,
    ReleaseFetcher, ReleaseInfo, SelectedAssets, UpdateDecision,
};
// Re-exported: the run loop constructs the real fetcher/downloader via
// `update::GitHubFetcher` / `update::HttpDownloader`.
pub use activity_update::{GitHubFetcher, HttpDownloader};
use anyhow::{bail, Context, Result};
use semver::Version;

use crate::startup::TASK_NAME;
use crate::usage_log;

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
/// Suffix of a rolled-back (failed) new exe, kept next to the exe for the
/// post-mortem. Exactly one exists between incidents (issue #37).
pub const BAD_SUFFIX: &str = ".bad";
/// How long the relaunch waits for the NEW daemon's first heartbeat beat
/// before declaring the update failed and rolling back (issue #37). The
/// new instance beats within ~1s of spawning; the scheduled-task path is
/// slower, so this is generous on purpose.
pub const RELAUNCH_VERIFY: std::time::Duration = std::time::Duration::from_secs(15);

/// Guard so two updates can never be in flight in one process (within the
/// daemon the check runs on the main loop thread only; the guard also
/// covers a future second entry point).
static UPDATE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// The running cli crate version as semver.
pub fn current_version() -> Version {
    activity_update::current_version()
}

/// Find the exe asset + sha256 companion by the exact names the release
/// workflow uploads (`activity-daemon-windows-x86_64.exe[.sha256]`), with a
/// tolerant fallback should the artifact ever be renamed. Linux server
/// assets are not selected here (see the hub updater, issue #32).
pub fn select_assets(assets: &[Asset]) -> Result<SelectedAssets> {
    if let Ok(pair) = select_asset_pair(assets, WINDOWS_EXE_ASSET) {
        return Ok(pair);
    }
    // Tolerant fallback: any windows-x86_64 exe with its `.sha256` companion.
    let fallback = assets
        .iter()
        .find(|a| a.name.ends_with(".exe") && a.name.contains("windows-x86_64"))
        .context("no Windows x86_64 exe asset in the release")?;
    select_asset_pair(assets, &fallback.name)
}

// ---------------------------------------------------------------------------
// The swap: download -> verify -> rotate exe/.old
// ---------------------------------------------------------------------------

/// Rollback copy path: `<exe>.old`.
pub fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or(exe.as_os_str()).to_os_string();
    name.push(OLD_SUFFIX);
    exe.with_file_name(name)
}

/// Failed-update keep-forever-for-the-post-mortem copy: `<exe>.bad`.
pub fn bad_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or(exe.as_os_str()).to_os_string();
    name.push(BAD_SUFFIX);
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
            .download_to(&selected.asset.url, &exe_tmp)
            .with_context(|| format!("download of {} failed", selected.asset.name))?;
        log::info!("downloaded {} ({} bytes)", selected.asset.name, bytes);
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
        log::info!("sha256 verified for {}", selected.asset.name);

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

/// Wait for the NEW daemon's first heartbeat beat (issue #37).
///
/// The exiting process wrote a FINAL beat on shutdown - fresh, but carrying
/// the old pid - so freshness alone proves nothing: only a beat with a
/// DIFFERENT pid proves the new instance is alive (heartbeat.rs has carried
/// the pid for exactly this). Returns `false` on timeout.
pub fn wait_for_relaunch_beat(data_dir: &Path, old_pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(hb) = crate::heartbeat::read(data_dir) {
            if hb.pid != old_pid && hb.age_s < crate::heartbeat::MAX_AGE_S {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Roll a failed update back (issue #37): the new exe (currently at the
/// canonical path) moves to `<exe>.bad` - allowed on Windows even when a
/// wedged new process still runs - and the `.old` copy returns to the
/// canonical path. A previous `.bad` is removed first (exactly one exists
/// between incidents). Restores nothing on failure of the second rename:
/// the error carries the manual-recovery path, mirroring `swap_in_place`.
pub fn rollback_update(exe: &Path) -> Result<()> {
    let bad = bad_path(exe);
    if bad.exists() {
        std::fs::remove_file(&bad).with_context(|| {
            format!(
                "cannot remove the previous failed-update copy {}",
                bad.display()
            )
        })?;
    }
    std::fs::rename(exe, &bad)
        .with_context(|| format!("cannot move the failed exe to {}", bad.display()))?;
    let old = old_path(exe);
    if let Err(e) = std::fs::rename(&old, exe) {
        // Put the failed exe back so the canonical path is not empty.
        match std::fs::rename(&bad, exe) {
            Ok(()) => bail!(
                "cannot restore the rollback copy ({e}); the failed exe is back at {} - \
                 restore {} manually",
                exe.display(),
                old.display()
            ),
            Err(restore_err) => bail!(
                "cannot restore the rollback copy ({e}); the restore failed too ({restore_err}): \
                 the old binary is at {} - move it back manually",
                old.display()
            ),
        }
    }
    log::info!(
        "rollback: {} restored; failed update kept as {}",
        exe.display(),
        bad.display()
    );
    Ok(())
}

/// Relaunch the (updated) daemon: through the "ActivityDaemon" Task
/// Scheduler task when it exists, otherwise detached with `run`.
///
/// Issue #37: the relaunch is then VERIFIED - the exiting daemon waits for
/// the new instance's first heartbeat beat (a beat with a different pid).
/// A relaunch that never beats rolls the update back to the `.old` copy
/// and relaunches the old version loudly. This is the 2026-11-06 incident
/// fix: the new exe was swapped in, never came up (EDR suspect), and the
/// machine silently lost the rest of the day.
pub fn relaunch_after_update(data_dir: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("cannot resolve the current exe path")?;
    let old_pid = std::process::id();
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

    // Verify the new daemon actually beats (issue #37). The lock is
    // already released (run.rs) - the new instance can acquire it and
    // write its first beat immediately.
    if wait_for_relaunch_beat(data_dir, old_pid, RELAUNCH_VERIFY) {
        println!("relaunch verified: the new daemon is beating");
        return Ok(());
    }

    // The new exe never came up. Roll back loudly - this is exactly the
    // failure mode that silently killed capture on 2026-11-06.
    println!(
        "WARNING: the updated exe did not start within {}s - rolling the update back",
        RELAUNCH_VERIFY.as_secs()
    );
    log::warn!("update rollback: the relaunched daemon produced no heartbeat beat");
    rollback_update(&exe)?;
    match relaunch_mode(task_exists()) {
        RelaunchMode::ScheduledTask => {
            schtasks_run().context("rollback: cannot start the scheduled task")?;
        }
        RelaunchMode::DetachedSpawn => {
            spawn_detached(&exe).context("rollback: cannot spawn the restored exe")?;
        }
    }
    if wait_for_relaunch_beat(data_dir, old_pid, RELAUNCH_VERIFY) {
        println!(
            "rolled back: the previous version is running again (failed exe kept as {})",
            bad_path(&exe).display()
        );
        Ok(())
    } else {
        bail!(
            "rolled back to {} but it did not beat either - the watchdog (issue #36) or the \
             next logon must recover; failed exe kept as {}",
            exe.display(),
            bad_path(&exe).display()
        )
    }
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

/// Spawn `exe run` detached so it survives this process exiting. Also used
/// by the #29 Startup-folder fallback to start the daemon right after
/// install (windowless, no console flash).
#[cfg(windows)]
pub(crate) fn spawn_detached(exe: &Path) -> Result<()> {
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
pub(crate) fn spawn_detached(exe: &Path) -> Result<()> {
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
    let (result, logged_action) = match action {
        Some(UpdateAction::Check) => (check_only(&data_dir), usage_log::ACTION_CHECK),
        None => (apply_now(&data_dir), usage_log::ACTION_APPLY),
    };
    // Issue #27: only the MANUAL update paths log ({update, check|apply});
    // the automatic scheduled check in the run loop never reaches this
    // function and never logs. Fire-and-forget.
    if result.is_ok() {
        usage_log::record(
            &data_dir,
            &resolved.config.sync.device_name,
            usage_log::FEATURE_UPDATE,
            logged_action,
        );
    }
    result
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
    use activity_update::{file_sha256_hex, parse_release_json, user_agent};
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
    fn select_assets_finds_the_windows_pair_and_ignores_linux() {
        let info = parse_release_json(SAMPLE_RELEASE_JSON).expect("parse sample");
        let sel = select_assets(&info.assets).expect("select");
        assert_eq!(sel.asset.name, WINDOWS_EXE_ASSET);
        assert_eq!(sel.sha256.name, format!("{WINDOWS_EXE_ASSET}.sha256"));

        // No assets / missing sha256 companion -> errors.
        assert!(select_assets(&[]).is_err());
        let exe_only = vec![Asset {
            name: WINDOWS_EXE_ASSET.to_string(),
            url: "u".to_string(),
        }];
        assert!(select_assets(&exe_only).is_err());
    }

    /// The tolerant fallback still finds the pair when the artifact is
    /// renamed (no exact `activity-daemon-windows-x86_64.exe` asset).
    #[test]
    fn select_assets_fallback_finds_a_renamed_windows_exe() {
        let info = parse_release_json(SAMPLE_RELEASE_JSON).expect("parse sample");
        let renamed = vec![
            Asset {
                name: "activity-daemon-windows-x86_64-v2.exe".to_string(),
                url: "https://example.test/dl/renamed.exe".to_string(),
            },
            Asset {
                name: "activity-daemon-windows-x86_64-v2.exe.sha256".to_string(),
                url: "https://example.test/dl/renamed.exe.sha256".to_string(),
            },
        ];
        let sel = select_assets(&renamed).expect("fallback select");
        assert_eq!(sel.asset.name, "activity-daemon-windows-x86_64-v2.exe");
        // The Linux server pair alone is never selected.
        let linux_only: Vec<Asset> = info
            .assets
            .into_iter()
            .filter(|a| a.name.contains("linux"))
            .collect();
        assert!(select_assets(&linux_only).is_err());
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
        // Digest via the shared helper (the sha2 dependency now lives in
        // activity-update only).
        let body_file = dir.join("new-body-reference.bin");
        std::fs::write(&body_file, new_body).expect("write reference body");
        let digest_hex = file_sha256_hex(&body_file).expect("hash reference");
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

    /// The relaunch verifier (issue #37) keys on the PID CHANGE, not on
    /// freshness alone: the old daemon's final shutdown beat is fresh but
    /// carries the old pid, and only a different pid proves the new
    /// instance is alive.
    #[test]
    fn relaunch_beat_requires_a_pid_change() {
        let dir = temp_dir("relaunch-beat");
        let old_pid = std::process::id();
        let new_pid = 999_999; // cannot exist anywhere

        // No heartbeat at all: timeout (kept tiny so the test is fast).
        assert!(!wait_for_relaunch_beat(
            &dir,
            old_pid,
            std::time::Duration::from_millis(250)
        ));

        // The old daemon's final beat (fresh, but OUR pid): must NOT count
        // as the new instance coming up.
        crate::heartbeat::write(&dir).expect("write old final beat");
        let started = std::time::Instant::now();
        assert!(
            !wait_for_relaunch_beat(&dir, old_pid, std::time::Duration::from_millis(400)),
            "our own fresh final beat is not a new instance"
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(350),
            "waited"
        );

        // A fresh beat from a different pid: the new daemon is alive.
        let ts = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        std::fs::write(
            crate::heartbeat::heartbeat_path(&dir),
            format!("{ts} {new_pid}\n"),
        )
        .expect("write new-instance beat");
        assert!(wait_for_relaunch_beat(
            &dir,
            old_pid,
            std::time::Duration::from_secs(5)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rollback dance (issue #37): the failed exe moves to `<exe>.bad`,
    /// the `.old` copy returns to the canonical path. With no `.old` present
    /// the rollback fails and the failed exe is PUT BACK (the canonical path
    /// is never left empty).
    #[test]
    fn rollback_update_restores_the_old_exe_and_keeps_the_bad_one() {
        let dir = temp_dir("rollback");
        let exe = dir.join("activity-daemon.exe");
        std::fs::write(&exe, "NEW BROKEN BUILD").expect("write new exe");
        std::fs::write(old_path(&exe), "OLD GOOD BUILD").expect("write old exe");

        rollback_update(&exe).expect("rollback");
        assert_eq!(
            std::fs::read_to_string(&exe).expect("canonical path"),
            "OLD GOOD BUILD",
            "the old exe is back at the canonical path"
        );
        assert_eq!(
            std::fs::read_to_string(bad_path(&exe)).expect("bad copy"),
            "NEW BROKEN BUILD",
            "the failed exe is kept for the post-mortem"
        );
        assert!(!old_path(&exe).exists(), "exactly one .old contract");

        // No .old present any more (the first rollback consumed it): the
        // rollback must fail WITHOUT emptying the canonical path.
        std::fs::write(&exe, "SECOND BROKEN BUILD").expect("write new exe");
        assert!(rollback_update(&exe).is_err(), "no .old to restore");
        assert_eq!(
            std::fs::read_to_string(&exe).expect("canonical path intact"),
            "SECOND BROKEN BUILD",
            "the failed exe was put back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
