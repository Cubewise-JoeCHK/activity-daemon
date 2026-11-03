//! Screen frame watcher for activity-daemon (issue #5).
//!
//! Captures the primary monitor as JPEG frames on a configurable interval
//! (ADR: 5-10 s), **paused while the user is AFK or the workstation is
//! locked** (no input means idle time grows past the AFK threshold), with
//! content dedup (`sha16`: identical frames are not written at all) and a
//! rolling retention budget of `screen.max_age_days` / `screen.max_total_mb`
//! enforced by an hourly oldest-first sweep (ADR rev 2 defaults: 90 days /
//! 20 GB).
//!
//! # Layout (ADR media contract)
//!
//! ```text
//! <data_dir>/media/frames/<YYYY>/<MM>/<DD>/<HHMMSS>_<sha16>.jpg
//! ```
//!
//! Every written file gets one index row in the `media` bucket via core:
//! `{ kind: "frame", path, bytes, sha16, producer: "watcher-screen" }`.
//! Index rows are never deleted by the sweep - they are the historical
//! record; the central hub is the keep-forever archive (docs/needs.md).
//!
//! # Data dir resolution
//!
//! The `ACTIVITY_DATA_DIR` environment variable, defaulting to the current
//! directory - the same convention as the CLI's `ACTIVITY_DB`. (Wish-listed
//! for core: an explicit `data_dir` config field.)
//!
//! # Daemon usage
//!
//! Blocking API for a daemon thread; stop it via a shared `AtomicBool`:
//!
//! ```no_run
//! use std::sync::atomic::AtomicBool;
//! use std::sync::Arc;
//!
//! let cfg = activity_core::Config::default();
//! let db = activity_core::Db::open_db(std::path::Path::new("activity.db"))?;
//! let shutdown = Arc::new(AtomicBool::new(false));
//! let stop = Arc::clone(&shutdown);
//! let worker = std::thread::spawn(move || watcher_screen::run(&cfg, &db, &stop));
//! // ... later, on Ctrl-C or service stop:
//! shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
//! worker.join().expect("screen watcher thread")?;
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! Any capture failure logs a warning and continues; it never kills the
//! daemon. The pipeline is hidden behind the [`FrameSource`] trait with a
//! [`FakeFrameSource`] double, so encode, dedup, file layout, index rows
//! and retention are tested on all platforms; only the GDI capture itself
//! is Windows-only.

pub mod capture;
pub mod retention;
pub mod store;
pub mod watcher;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use activity_core::{Config, Db};
use chrono::Local;
use log::{info, warn};

#[cfg(windows)]
pub use capture::gdi::GdiFrameSource;
pub use capture::{FakeFrameSource, Frame, FrameSource};
pub use retention::{
    plan_deletions, sweep_frames, FileInfo, RetentionBudget, SweepReport, MB_BYTES, SWEEP_INTERVAL,
};
pub use store::{encode_jpeg, sha16, FrameStore, StoredFrame, DEFAULT_JPEG_QUALITY, PRODUCER};
pub use watcher::{sleep_until, FrameStats, TickOutcome, Watcher};

/// Resolve the data dir (media files live under `<data_dir>/media`).
///
/// `ACTIVITY_DATA_DIR` environment variable, defaulting to the current
/// directory so a daemon started next to `activity.db` keeps both together.
pub fn data_dir() -> std::path::PathBuf {
    std::env::var_os("ACTIVITY_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// The production frame source: GDI primary-monitor capture on Windows.
///
/// Other platforms have no capture backend in v0.1; [`run`] reports this
/// as an immediate error there.
#[cfg(windows)]
fn default_frame_source() -> anyhow::Result<Box<dyn FrameSource>> {
    Ok(Box::new(capture::gdi::GdiFrameSource::new()))
}

/// The production input-idle probe (seconds since last keyboard/mouse
/// input). On Windows: `GetLastInputInfo`; the same probe covers the lock
/// screen, because input idle keeps growing while the session is locked.
/// Non-Windows reports 0 (never AFK) and `run` errors out anyway.
#[cfg(windows)]
fn input_idle_s() -> f64 {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    // SAFETY: plain Win32 queries; GetLastInputInfo fills a caller-owned
    // struct and GetTickCount takes no arguments.
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            ..Default::default()
        };
        if GetLastInputInfo(&mut info).as_bool() {
            // u32 arithmetic makes tick-count wraparound harmless.
            let idle_ms = GetTickCount().wrapping_sub(info.dwTime);
            return idle_ms as f64 / 1000.0;
        }
    }
    0.0
}

#[cfg(not(windows))]
fn default_frame_source() -> anyhow::Result<Box<dyn FrameSource>> {
    anyhow::bail!("watcher-screen capture is only implemented on Windows (GDI); retention and the encode/dedup pipeline remain testable everywhere")
}

#[cfg(not(windows))]
fn input_idle_s() -> f64 {
    0.0
}

/// Run the screen frame watcher until `shutdown` is set (blocking).
///
/// Daemon loop (one iteration per `screen.interval_s`):
///
/// 1. if input idle >= `afk_threshold_min`, skip this tick (AFK/lock pause);
/// 2. capture the primary monitor via GDI;
/// 3. JPEG-encode (quality `screen.quality`, default
///    [`DEFAULT_JPEG_QUALITY`]) and skip the write if the frame's `sha16`
///    matches the previous frame;
/// 4. otherwise write `media/frames/YYYY/MM/DD/HHMMSS_<sha16>.jpg` and
///    insert the `media` index row via `Db::insert_event`;
/// 5. once an hour (and once at startup), sweep old/oversized frames
///    oldest-first within `screen.max_age_days` + `screen.max_total_mb`.
///
/// Returns when `shutdown` becomes true. Capture or storage failures log a
/// warning and continue. Non-Windows platforms return an immediate error
/// (no capture backend; see [`default_frame_source`]).
pub fn run(cfg: &Config, db: &Db, shutdown: &AtomicBool) -> anyhow::Result<()> {
    let media_root = data_dir().join("media");
    let quality = cfg.screen.quality;
    let mut watcher = Watcher::new(
        default_frame_source()?,
        FrameStore::new(media_root.clone(), quality),
        cfg.afk_threshold_min.saturating_mul(60),
    );
    let budget = RetentionBudget {
        max_age_days: cfg.screen.max_age_days,
        max_total_mb: cfg.screen.max_total_mb,
    };

    info!(
        "watcher-screen: interval {}s, quality {}, retention {}/{} MB, media root {}",
        cfg.screen.interval_s,
        quality,
        budget.max_age_days,
        budget.max_total_mb,
        media_root.display()
    );

    sweep_and_log(&media_root, &budget);
    let mut last_sweep = Instant::now();
    let interval = std::time::Duration::from_secs(cfg.screen.interval_s.max(1));

    while !shutdown.load(Ordering::Relaxed) {
        watcher.tick(Local::now(), input_idle_s(), db, &cfg.sync.device_name);

        if last_sweep.elapsed() >= SWEEP_INTERVAL {
            sweep_and_log(&media_root, &budget);
            last_sweep = Instant::now();
        }

        sleep_until(shutdown, Instant::now() + interval);
    }

    let stats = watcher.stats();
    info!(
        "watcher-screen stopped: {} tick(s), {} written, {} deduped, {} AFK-skipped, {} failed",
        stats.ticks, stats.written, stats.deduped, stats.afk_skipped, stats.failed
    );
    Ok(())
}

/// One retention pass; a failing sweep is logged and retried next hour.
fn sweep_and_log(media_root: &std::path::Path, budget: &RetentionBudget) {
    match sweep_frames(media_root, budget) {
        Ok(report) if report.errors > 0 => {
            warn!(
                "retention sweep completed with {} error(s): deleted {}, scanned {}",
                report.errors, report.deleted, report.scanned
            );
        }
        Ok(_) => {}
        Err(e) => warn!("retention sweep failed, will retry next hour: {e}"),
    }
}
