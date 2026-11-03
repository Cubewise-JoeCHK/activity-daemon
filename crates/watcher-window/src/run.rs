//! The blocking capture loop: sample -> coalesce -> persist via core's `Db`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use activity_core::{Config, Db, EventEnvelope};
use log::warn;

use crate::coalescer::{SessionCoalescer, WatcherConfig};
use crate::source::ForegroundSource;

/// Blocking capture loop for the watcher-window plugin. The daemon binary
/// (activity-cli) calls this on its daemon run path (issue #7 wires the
/// plugin in); it does not return until `shutdown` is set.
///
/// Each tick it takes one sample from the platform source (1-second cadence
/// by convention: `config.poll_interval_s`), feeds it through the
/// [`SessionCoalescer`] and persists the resulting ADR envelope events
/// (`window-watcher` with `{ app, title }`, `afk-watcher` with `{ state }`)
/// via core's `Db`, tagged with `config.sync.device_name` (empty -> "local").
///
/// On shutdown the still-open session is flushed at last-poll time. Transient
/// errors never abort the loop: failed Win32 calls degrade the sample inside
/// the source, failed inserts log a warning and continue.
pub fn run(cfg: &Config, db: &Db, shutdown: &AtomicBool) -> anyhow::Result<()> {
    #[cfg(windows)]
    return run_with_source(cfg, db, shutdown, crate::WindowsSource::new());

    #[cfg(not(windows))]
    {
        let _ = (cfg, db, shutdown);
        anyhow::bail!("watcher-window: no foreground source for this platform (Windows required)")
    }
}

/// [`run`] with an injected [`ForegroundSource`]: the seam for the
/// cross-platform end-to-end test and for future non-Windows sources.
pub fn run_with_source<S: ForegroundSource>(
    cfg: &Config,
    db: &Db,
    shutdown: &AtomicBool,
    mut source: S,
) -> anyhow::Result<()> {
    let watcher_cfg = WatcherConfig {
        afk_threshold_s: cfg.afk_threshold_min as f64 * 60.0,
        // A poll gap beyond this (absolute) is sleep / hibernate; keep the
        // 120 s default but never trip on a slow poll interval itself.
        gap_threshold_s: ((cfg.poll_interval_s.saturating_mul(30)) as f64).max(120.0),
    };
    let device = if cfg.sync.device_name.is_empty() {
        "local"
    } else {
        cfg.sync.device_name.as_str()
    };
    let mut coalescer = SessionCoalescer::new(watcher_cfg);
    let interval = Duration::from_secs(cfg.poll_interval_s);

    while !shutdown.load(Ordering::Relaxed) {
        let sample = source.sample();
        persist(db, &coalescer.process(&sample), device);
        sleep_check(interval, shutdown);
    }
    // Persist the still-open session so shutdown does not lose the tail.
    persist(db, &coalescer.flush(), device);
    Ok(())
}

fn persist(db: &Db, events: &[EventEnvelope], device: &str) {
    for event in events {
        if let Err(e) = db.insert_event(event, device) {
            warn!("failed to persist {} event, continuing: {e}", event.bucket);
        }
    }
}

/// Sleep for one poll interval, waking early (checked in 100 ms slices) when
/// shutdown is requested so daemon stop stays responsive.
fn sleep_check(interval: Duration, shutdown: &AtomicBool) {
    let slice = Duration::from_millis(100);
    let mut remaining = interval;
    while !shutdown.load(Ordering::Relaxed) && !remaining.is_zero() {
        let step = remaining.min(slice);
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
}

#[cfg(test)]
mod end_to_end_tests {
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, SystemTime};

    use activity_core::{Config, SyncConfig, BUCKET_AFK, BUCKET_WINDOW};
    use chrono::{Local, TimeZone};

    use super::*;
    use crate::coalescer::format_ts;
    use crate::sample::Sample;
    use crate::source::ForegroundSource;

    /// Scripted source: yields the given samples in order, then keeps
    /// repeating the last one (a steady-state desktop, so no new events).
    /// After handing out the final scripted sample it flips the shared
    /// shutdown flag, so `run_with_source` terminates deterministically and
    /// flushes the open session.
    struct FakeSource<'a> {
        shutdown: &'a AtomicBool,
        scripted: VecDeque<Sample>,
        last: Option<Sample>,
    }

    impl<'a> FakeSource<'a> {
        fn new(shutdown: &'a AtomicBool, scripted: Vec<Sample>) -> Self {
            FakeSource {
                shutdown,
                scripted: scripted.into(),
                last: None,
            }
        }
    }

    impl ForegroundSource for FakeSource<'_> {
        fn sample(&mut self) -> Sample {
            if self.scripted.len() <= 1 {
                self.shutdown.store(true, Ordering::Relaxed);
            }
            let sample = self
                .scripted
                .pop_front()
                .or_else(|| self.last.clone())
                .expect("FakeSource needs at least one scripted sample");
            self.last = Some(sample.clone());
            sample
        }
    }

    /// Local wall-clock 2026-11-04 09:00:00 as an absolute instant, so the
    /// RFC3339 assertions round-trip through `format_ts` on any timezone.
    fn base() -> SystemTime {
        let dt = Local
            .with_ymd_and_hms(2026, 11, 4, 9, 0, 0)
            .single()
            .expect("valid base timestamp");
        SystemTime::UNIX_EPOCH + Duration::from_secs(dt.timestamp().max(0) as u64)
    }

    fn at(base: SystemTime, offset_s: u64) -> SystemTime {
        base + Duration::from_secs(offset_s)
    }

    fn sample(base: SystemTime, offset_s: u64, app: &str, title: &str, idle_s: u64) -> Sample {
        Sample {
            ts: at(base, offset_s),
            app: app.to_string(),
            title: title.to_string(),
            input_idle_s: idle_s,
        }
    }

    fn test_config() -> Config {
        Config {
            // No real sleeping between scripted ticks.
            poll_interval_s: 0,
            afk_threshold_min: 5,
            screen: Default::default(),
            audio: Default::default(),
            sync: SyncConfig {
                device_name: "test-laptop".to_string(),
                ..Default::default()
            },
        }
    }

    fn temp_db_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("watcher-window-e2e-{tag}-{nanos}.db"))
    }

    fn cleanup(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    /// The production loop, driven end-to-end by a scripted source, writing
    /// real rows into a temp-file `Db` (runs on every platform). Covers:
    /// app switch, sleep gap, AFK bracket, gap during AFK and the shutdown
    /// flush.
    #[test]
    fn fake_source_run_writes_expected_rows_into_temp_db() {
        let base = base();
        let shutdown = AtomicBool::new(false);
        let scripted = vec![
            sample(base, 0, "Code.exe", "main.rs", 0),
            sample(base, 5, "Code.exe", "main.rs", 0),
            // 295 s jump (lid closed): Code closes at the last poll (+5).
            sample(base, 300, "chrome.exe", "Docs", 0),
            sample(base, 305, "chrome.exe", "Docs", 0),
            // Idle grows; polls stay under the 120 s gap threshold.
            sample(base, 365, "chrome.exe", "Docs", 60),
            sample(base, 425, "chrome.exe", "Docs", 120),
            sample(base, 545, "chrome.exe", "Docs", 240),
            // Idle crosses the 300 s AFK threshold: chrome closes where
            // idleness began (+305) and the afk start marker is written.
            sample(base, 610, "chrome.exe", "Docs", 305),
            // 310 s sleep while AFK: the afk episode closes at last poll.
            sample(base, 920, "chrome.exe", "Docs", 315),
            sample(base, 925, "chrome.exe", "Docs", 0),
            // Final scripted sample; flips shutdown -> loop flushes chrome.
            sample(base, 930, "chrome.exe", "Docs", 0),
        ];
        let source = FakeSource::new(&shutdown, scripted);

        let path = temp_db_path("run");
        let db = Db::open_db(&path).expect("open temp db");
        let cfg = test_config();
        run_with_source(&cfg, &db, &shutdown, source).expect("run loop");

        assert_eq!(db.event_count().expect("count"), 5);
        assert_eq!(db.unsynced_count().expect("unsynced"), 5);
        let rows = db.fetch_unsynced_events(100).expect("fetch");
        assert_eq!(rows.len(), 5);

        let expected = [
            (
                BUCKET_WINDOW,
                0,
                5.0,
                serde_json::json!({"app": "Code.exe", "title": "main.rs"}),
            ),
            (
                BUCKET_WINDOW,
                300,
                5.0,
                serde_json::json!({"app": "chrome.exe", "title": "Docs"}),
            ),
            (BUCKET_AFK, 305, 0.0, serde_json::json!({"state": "afk"})),
            (
                BUCKET_AFK,
                305,
                305.0,
                serde_json::json!({"state": "active"}),
            ),
            (
                BUCKET_WINDOW,
                920,
                10.0,
                serde_json::json!({"app": "chrome.exe", "title": "Docs"}),
            ),
        ];
        for (row, (bucket, offset, duration, data)) in rows.iter().zip(&expected) {
            assert_eq!(&row.bucket, bucket);
            assert_eq!(row.ts_start, format_ts(at(base, *offset)));
            assert!((row.duration_s - duration).abs() < 1e-9, "row {}", row.id);
            let parsed: serde_json::Value = serde_json::from_str(&row.data).expect("data is json");
            assert_eq!(parsed, *data);
            assert_eq!(row.device, "test-laptop");
            assert!(!row.synced);
        }

        drop(db);
        cleanup(&path);
    }
}
