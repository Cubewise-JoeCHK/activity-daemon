//! `activity-daemon run` (issue #7): the daemon wiring.
//!
//! Architecture:
//!
//! - One config resolve (template created on first use, see [`crate::config`]).
//! - One process-wide shutdown flag ([`SHUTDOWN`]).
//! - A single-instance lock in `{data_dir}/daemon.lock` (pid-checked, stale
//!   locks are taken over).
//! - Four worker threads, each opening its **own** `Db` connection via
//!   `activity_core::Db::open_db` - core `Db` is not `Sync`/`Send`, this is
//!   the documented threading contract:
//!     1. `watcher_window::run`   (window/AFK sessions, flushes on shutdown)
//!     2. `watcher_screen::run`   (JPEG frames, rolling retention)
//!     3. `watcher_audio::run`    (meeting audio: hotkey + record.json + auto)
//!     4. `activity_sync::run`    (forwarder: events + media to the hub)
//! - A heartbeat thread rewriting `{data_dir}/heartbeat` every 5 s.
//! - Logs to `{data_dir}/daemon.log` via the `log` crate.
//! - Ctrl+C (Windows console handler) sets [`SHUTDOWN`]; the main thread
//!   joins every worker and logs a clean exit.
//! - Auto-update (#17): with `sync.auto_update` on, the main loop checks
//!   GitHub Releases every `sync.update_interval_s` (default daily); an
//!   applied update - or a restart request written by a manual `update` -
//!   runs the same graceful shutdown (workers join, session flush) and then
//!   relaunches the updated exe.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use activity_core::Db;
use activity_sync::SyncSettings;
use anyhow::{Context, Result};

use crate::config;
use crate::heartbeat;
use crate::lock::{DaemonLock, LockOutcome};
use crate::logging::FileLogger;
use crate::update;

/// The process-wide daemon shutdown flag. One daemon per process (the
/// single-instance lock enforces it), so a static is the simplest correct
/// shape; the Windows console handler writes it from its own thread and
/// every worker reads it as a `&'static AtomicBool`.
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Poll cadence of the main wait loop.
const WAIT_SLICE: Duration = Duration::from_millis(200);

/// What the main loop decided: plain stop, or a graceful restart onto the
/// updated exe (relaunch happens after the single-instance lock is
/// released, so the new instance can acquire it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitAction {
    /// Ctrl+C or service stop.
    Stop,
    /// An update was applied (or `update/restart.request` was written by a
    /// manual `update`): relaunch after the workers are joined.
    Restart,
}

/// `run` subcommand entry point.
pub fn cmd_run(config_override: Option<&Path>) -> Result<()> {
    let resolved = config::resolve(config_override)?;
    if resolved.created {
        println!(
            "no config found - wrote a template with defaults to {}",
            resolved.path.display()
        );
    }
    let cfg = resolved.config;
    let data_dir = cfg.data_dir.clone();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;

    // Single instance: claim the lock or explain who owns it.
    let (lock, outcome) = DaemonLock::acquire(&data_dir)
        .with_context(|| format!("cannot create lock in {}", data_dir.display()))?;
    match outcome {
        LockOutcome::Acquired => {}
        LockOutcome::TookOver { stale_pid } => {
            println!("took over a stale lock (dead pid {stale_pid}); continuing");
        }
        LockOutcome::Blocked { pid } => {
            anyhow::bail!(
                "another daemon instance is already running (pid {pid}); not starting. \
                 Check `activity-daemon status` or delete {}/daemon.lock if that pid is wrong.",
                data_dir.display()
            );
        }
    }
    let run_result = run_locked(&data_dir, &cfg);
    lock.release();
    match run_result? {
        ExitAction::Stop => {
            println!("daemon stopped cleanly.");
            Ok(())
        }
        ExitAction::Restart => {
            println!("daemon stopped cleanly; relaunching on the updated version...");
            update::relaunch_after_update()
        }
    }
}

/// The daemon body while holding the lock.
fn run_locked(data_dir: &Path, cfg: &activity_core::Config) -> Result<ExitAction> {
    // The plugins resolve their media roots from ACTIVITY_DATA_DIR (the
    // documented env override); pin it to the configured data dir before
    // any worker thread starts so every watcher lands in the same tree.
    std::env::set_var("ACTIVITY_DATA_DIR", data_dir);

    FileLogger::init_global(&data_dir.join("daemon.log"), true);
    log::info!(
        "activity-daemon starting: data_dir={} device={} poll={}s auto_update={} interval={}s",
        data_dir.display(),
        cfg.sync.device_name,
        cfg.poll_interval_s,
        cfg.sync.auto_update,
        cfg.sync.update_interval_s
    );

    let handles = spawn_workers(cfg, data_dir);
    let heartbeat_handle = heartbeat::spawn_writer(data_dir, &SHUTDOWN);
    install_ctrl_handler();

    println!(
        "daemon running: {} worker thread(s), data dir {}, Ctrl+C to stop",
        handles.len(),
        data_dir.display()
    );

    let exit_action = wait_for_shutdown_or_update(cfg, data_dir);
    log::info!(
        "{}; joining workers",
        match exit_action {
            ExitAction::Stop => "shutdown requested",
            ExitAction::Restart => "update applied - restarting gracefully",
        }
    );

    let mut failed = 0usize;
    for handle in handles {
        if handle.join().is_err() {
            failed += 1;
        }
    }
    let _ = heartbeat_handle.join();
    if failed > 0 {
        log::warn!("{failed} worker thread(s) panicked during shutdown");
    }
    log::info!("activity-daemon stopped cleanly");
    Ok(exit_action)
}

/// The main wait loop. Returns when
/// - Ctrl+C sets [`SHUTDOWN`] (plain stop), or
/// - a manual `update` wrote `update/restart.request` (graceful restart on
///   the already-swapped exe), or
/// - `sync.auto_update` is on and a due GitHub Releases check applied an
///   update (graceful restart on the new exe).
///
/// Network/API failures of the update check are logged and ignored - the
/// daemon keeps running and the next check happens after the interval.
/// The check runs synchronously here (one update in flight, ever).
fn wait_for_shutdown_or_update(cfg: &activity_core::Config, data_dir: &Path) -> ExitAction {
    let check_interval = Duration::from_secs(cfg.sync.update_interval_s.max(1));
    let mut last_check = Instant::now();
    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            return ExitAction::Stop;
        }
        // A manual `update` asked the daemon to restart on the new exe.
        if update::take_restart_request(data_dir) {
            log::info!(
                "restart requested via update/{}",
                update::RESTART_REQUEST_FILE
            );
            return ExitAction::Restart;
        }
        if cfg.sync.auto_update && last_check.elapsed() >= check_interval {
            last_check = Instant::now();
            log::info!("auto-update: checking for a new release");
            match check_for_update(data_dir) {
                Ok(true) => return ExitAction::Restart,
                Ok(false) => {}
                Err(e) => log::warn!("auto-update check failed (continuing): {e:#}"),
            }
        }
        std::thread::sleep(WAIT_SLICE);
    }
}

/// One update check + apply against GitHub Releases. `Ok(true)` = the new
/// exe is in place, the daemon should restart gracefully.
fn check_for_update(data_dir: &Path) -> Result<bool> {
    let fetcher = update::GitHubFetcher::new();
    let downloader = update::HttpDownloader::new();
    let updater = update::Updater::real(data_dir, &fetcher, &downloader)?;
    Ok(updater.check_and_apply()?.is_some())
}

/// Spawn the four watcher/sync workers. Each opens its own `Db` connection
/// (`Db` is neither `Sync` nor `Send` - the documented threading contract)
/// and never panics: startup or runtime errors are logged and the worker
/// exits, leaving the rest of the daemon alive.
pub(crate) fn spawn_workers(cfg: &activity_core::Config, data_dir: &Path) -> Vec<JoinHandle<()>> {
    let db_path: PathBuf = data_dir.join("activity.db");
    let settings = SyncSettings::from_config(cfg);
    let mut handles = Vec::with_capacity(4);

    // 1. Window/AFK watcher (flushes its open session on shutdown).
    handles.push(spawn_worker("window", {
        let cfg = cfg.clone();
        let db_path = db_path.clone();
        move || {
            let db = open_db_with_retry(&db_path)?;
            watcher_window::run(&cfg, &db, &SHUTDOWN)
        }
    }));

    // 2. Screen frame watcher.
    handles.push(spawn_worker("screen", {
        let cfg = cfg.clone();
        let db_path = db_path.clone();
        move || {
            let db = open_db_with_retry(&db_path)?;
            watcher_screen::run(&cfg, &db, &SHUTDOWN)
        }
    }));

    // 3. Meeting audio watcher (hotkey + record.json + conferencing auto).
    handles.push(spawn_worker("audio", {
        let cfg = cfg.clone();
        let db_path = db_path.clone();
        move || {
            let db = open_db_with_retry(&db_path)?;
            watcher_audio::run(&cfg, &db, &SHUTDOWN)
        }
    }));

    // 4. Sync forwarder (local-first: the watchers never wait on it).
    handles.push(spawn_worker("sync", {
        let db_path = db_path.clone();
        let data_dir = data_dir.to_path_buf();
        move || {
            let db = open_db_with_retry(&db_path)?;
            activity_sync::run(&settings, &db, &data_dir, &SHUTDOWN)?;
            Ok(())
        }
    }));

    handles
}

/// Open the worker `Db` with a short retry loop: on first boot all four
/// workers race to create the WAL database, and SQLite rejects concurrent
/// `journal_mode` switches with SQLITE_BUSY. A few quick retries absorb
/// that window; a persistent failure (broken dir, corrupted file) still
/// surfaces as an error after ~2 s.
fn open_db_with_retry(db_path: &Path) -> Result<Db> {
    const ATTEMPTS: u32 = 20;
    const BACKOFF: Duration = Duration::from_millis(100);
    let mut last_err = None;
    for _ in 0..ATTEMPTS {
        match Db::open_db(db_path) {
            Ok(db) => return Ok(db),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(BACKOFF);
            }
        }
    }
    Err(last_err.expect("at least one attempt made")).context("open activity.db")
}

/// Spawn one named worker thread that logs (never propagates) its outcome.
fn spawn_worker<F>(name: &'static str, job: F) -> JoinHandle<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    std::thread::spawn(move || match job() {
        Ok(()) => log::info!("worker {name} exited cleanly"),
        // {e:#} prints the full anyhow chain (context + root cause).
        Err(e) => log::warn!("worker {name} stopped: {e:#}"),
    })
}

#[cfg(windows)]
fn install_ctrl_handler() {
    // SAFETY: registers a plain callback that only touches the atomic
    // shutdown flag; the handler itself does no allocation.
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleCtrlHandler(
            Some(console_ctrl_handler),
            true,
        );
    }
}

#[cfg(not(windows))]
fn install_ctrl_handler() {
    // Non-Windows keeps the default SIGINT behaviour (the process is killed
    // by the OS); daemon autostart is a Windows concern in v0.1.
}

#[cfg(windows)]
extern "system" fn console_ctrl_handler(ctrl_type: u32) -> windows::core::BOOL {
    log::info!("console ctrl event {ctrl_type}: shutting down");
    SHUTDOWN.store(true, Ordering::Relaxed);
    windows::core::BOOL(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use activity_core::Config;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activity-cli-run-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// Run-loop smoke test: with the shutdown flag already set, all four
    /// workers must exit immediately on every platform (window/screen
    /// report their platform gap as a logged error, audio and sync stop on
    /// the flag), each from its own Db connection.
    #[test]
    fn workers_exit_immediately_when_shutdown_is_already_set() {
        let dir = temp_dir("smoke");
        // cfg.data_dir must point into the temp dir: the audio watcher
        // resolves its data dir from the config (not from the data_dir
        // argument) and must not litter the package directory.
        let cfg = Config {
            data_dir: dir.clone(),
            ..Config::default()
        };
        SHUTDOWN.store(true, Ordering::Relaxed);
        let handles = spawn_workers(&cfg, &dir);
        assert_eq!(handles.len(), 4);
        for handle in handles {
            handle.join().expect("no worker panics");
        }
        SHUTDOWN.store(false, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
