//! The watcher tick: AFK pause -> capture -> dedup/store, with counters
//! (issue #5). One [`Watcher::tick`] is one capture attempt covering every
//! selected monitor; the daemon loop in [`crate::run`] simply calls it
//! every `screen.interval_s`.

use std::time::Duration;

use activity_core::Db;
use chrono::{DateTime, Local};
use log::{debug, warn};

use crate::capture::FrameSource;
use crate::store::{FrameStore, StoredFrame};

/// Cumulative counters for the screen watcher (skipped vs written).
/// Counters aggregate across monitors: one tick with two new frames
/// counts `written += 2`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// Ticks processed (capture attempts + AFK-skipped ticks).
    pub ticks: u64,
    /// Frames written to disk and indexed (all monitors summed).
    pub written: u64,
    /// Frames skipped: identical to that monitor's previous frame.
    pub deduped: u64,
    /// Ticks skipped: user AFK (input idle >= threshold).
    pub afk_skipped: u64,
    /// Per-monitor capture/storage failures (daemon keeps running).
    pub failed: u64,
}

impl FrameStats {
    /// Total ticks that produced no frame (AFK, duplicate, or failure).
    pub fn skipped(&self) -> u64 {
        self.deduped + self.afk_skipped + self.failed
    }
}

/// What one captured monitor did within a tick.
#[derive(Debug, Clone, PartialEq)]
pub enum TickOutcome {
    /// New frame written and indexed.
    Written(StoredFrame),
    /// Frame identical to that monitor's previous one; nothing written.
    Deduped,
    /// User is AFK; capture skipped this tick (whole tick, all monitors).
    AfkPaused,
    /// Capture or storage failed; the daemon continues.
    Failed(String),
}

/// Screen watcher pipeline around any [`FrameSource`].
pub struct Watcher<S: FrameSource> {
    source: S,
    store: FrameStore,
    afk_threshold_s: f64,
    stats: FrameStats,
}

impl<S: FrameSource> Watcher<S> {
    /// `afk_threshold_s`: input idle seconds that pause capturing
    /// (`Config::afk_threshold_min` times 60 in the daemon).
    pub fn new(source: S, store: FrameStore, afk_threshold_s: u64) -> Watcher<S> {
        Watcher {
            source,
            store,
            afk_threshold_s: afk_threshold_s as f64,
            stats: FrameStats::default(),
        }
    }

    pub fn stats(&self) -> &FrameStats {
        &self.stats
    }

    pub fn store(&self) -> &FrameStore {
        &self.store
    }

    /// One capture tick over every selected monitor. Infallible: failures
    /// become [`TickOutcome::Failed`] entries plus a warning - never an
    /// error that could stop the daemon. AFK yields `vec![AfkPaused]`;
    /// otherwise the vec holds one outcome per captured monitor.
    pub fn tick(
        &mut self,
        at: DateTime<Local>,
        input_idle_s: f64,
        db: &Db,
        device: &str,
    ) -> Vec<TickOutcome> {
        self.stats.ticks += 1;
        if input_idle_s >= self.afk_threshold_s {
            self.stats.afk_skipped += 1;
            debug!(
                "AFK for {input_idle_s:.0}s (>= {}s): skipping capture",
                self.afk_threshold_s
            );
            return vec![TickOutcome::AfkPaused];
        }

        let captured = match self.source.capture() {
            Ok(frames) => frames,
            Err(e) => {
                self.stats.failed += 1;
                warn!("screen capture failed, continuing: {e:#}");
                return vec![TickOutcome::Failed(e.to_string())];
            }
        };

        let mut outcomes = Vec::with_capacity(captured.len());
        for mf in captured {
            match self
                .store
                .store_frame(&mf.monitor, &mf.frame, at, db, device)
            {
                Ok(Some(stored)) => {
                    self.stats.written += 1;
                    outcomes.push(TickOutcome::Written(stored));
                }
                Ok(None) => {
                    self.stats.deduped += 1;
                    debug!(
                        "duplicate frame on {} (same sha16): nothing written",
                        mf.monitor
                    );
                    outcomes.push(TickOutcome::Deduped);
                }
                Err(e) => {
                    self.stats.failed += 1;
                    warn!("frame storage failed, continuing: {e:#}");
                    outcomes.push(TickOutcome::Failed(e.to_string()));
                }
            }
        }
        outcomes
    }
}

/// Sleep until `deadline` in small slices, waking early on shutdown.
pub fn sleep_until(shutdown: &std::sync::atomic::AtomicBool, deadline: std::time::Instant) {
    const SLICE: Duration = Duration::from_millis(200);
    while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep(SLICE.min(deadline - now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::FakeFrameSource;
    use activity_core::Db;
    use chrono::TimeZone;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    fn at() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 11, 4, 9, 15, 0)
            .single()
            .expect("valid test time")
    }

    fn temp_media(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("watcher-screen-watcher-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn watcher(
        tag: &str,
        source: FakeFrameSource,
        afk_threshold_s: u64,
    ) -> Watcher<FakeFrameSource> {
        let media = temp_media(tag);
        Watcher::new(
            source,
            FrameStore::new(media, crate::store::DEFAULT_JPEG_QUALITY),
            afk_threshold_s,
        )
    }

    #[test]
    fn afk_tick_skips_capture_entirely() {
        let mut w = watcher("afk", FakeFrameSource::new(4, 4), 300);
        let outcomes = w.tick(at(), 400.0, &Db::open_in_memory().expect("db"), "dev");
        assert_eq!(outcomes, vec![TickOutcome::AfkPaused]);
        assert_eq!(w.source.calls(), 0, "AFK must not even poll the source");
        let stats = w.stats();
        assert_eq!((stats.afk_skipped, stats.written), (1, 0));
        assert_eq!(stats.skipped(), 1);
    }

    #[test]
    fn active_tick_writes_frame_and_indexes_it() {
        let db = Db::open_in_memory().expect("db");
        let mut w = watcher("active", FakeFrameSource::new(4, 4), 300);
        let outcomes = w.tick(at(), 0.0, &db, "dev");
        let [TickOutcome::Written(stored)] = outcomes.as_slice() else {
            panic!("expected one written frame, got {outcomes:?}");
        };
        assert_eq!(stored.sha16.len(), 16);
        assert!(stored.bytes > 0);
        let full = w
            .store()
            .media_root()
            .join("frames")
            .join("2026")
            .join("11")
            .join("04")
            .join(format!("091500_DISPLAY1_{}.jpg", stored.sha16));
        assert!(full.exists(), "frame file must exist at {}", full.display());
        assert_eq!(db.event_count().expect("count"), 1);
        assert_eq!(w.stats().written, 1);
    }

    #[test]
    fn identical_frame_counts_as_deduped() {
        let db = Db::open_in_memory().expect("db");
        let source = FakeFrameSource::new(4, 4);
        let mut w = watcher("dedup", source, 300);
        assert!(matches!(
            w.tick(at(), 0.0, &db, "dev").as_slice(),
            [TickOutcome::Written(_)]
        ));
        // Next capture returns the byte-identical previous frame.
        w.source.repeat_last();
        assert_eq!(w.tick(at(), 0.0, &db, "dev"), vec![TickOutcome::Deduped]);
        let stats = w.stats();
        assert_eq!((stats.written, stats.deduped), (1, 1));
    }

    #[test]
    fn capture_failure_is_contained_and_next_tick_recovers() {
        let db = Db::open_in_memory().expect("db");
        let mut source = FakeFrameSource::new(4, 4);
        source.fail_next();
        let mut w = watcher("failure", source, 300);

        let outcomes = w.tick(at(), 0.0, &db, "dev");
        assert_eq!(
            outcomes,
            vec![TickOutcome::Failed("synthetic capture failure".to_string())]
        );
        assert_eq!(w.stats().failed, 1);
        assert_eq!(db.event_count().expect("count"), 0);

        // The daemon keeps running: the next tick writes normally.
        assert!(matches!(
            w.tick(at(), 0.0, &db, "dev").as_slice(),
            [TickOutcome::Written(_)]
        ));
        assert_eq!(w.stats().written, 1);
    }

    #[test]
    fn multi_monitor_tick_writes_one_frame_per_monitor() {
        let db = Db::open_in_memory().expect("db");
        let source = FakeFrameSource::with_monitors(["DISPLAY1", "DISPLAY2"], 4, 4);
        let mut w = watcher("multi", source, 300);

        let outcomes = w.tick(at(), 0.0, &db, "dev");
        assert_eq!(outcomes.len(), 2, "one outcome per monitor");
        let mut labels = Vec::new();
        for outcome in &outcomes {
            let TickOutcome::Written(stored) = outcome else {
                panic!("expected a written frame, got {outcome:?}");
            };
            labels.push(stored.monitor.clone());
        }
        labels.sort();
        assert_eq!(labels, ["DISPLAY1", "DISPLAY2"]);

        // Two files, two index rows, distinct paths (labels keep them
        // apart even within the same second).
        assert_eq!(db.event_count().expect("count"), 2);
        let frames_dir = w
            .store()
            .media_root()
            .join("frames")
            .join("2026")
            .join("11")
            .join("04");
        let names: Vec<String> = std::fs::read_dir(&frames_dir)
            .expect("day dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(
            names
                .iter()
                .all(|n| n.starts_with("091500_DISPLAY1_") || n.starts_with("091500_DISPLAY2_")),
            "both file names must carry their monitor: {names:?}"
        );
        assert_eq!(w.stats().written, 2);
        assert_eq!(w.stats().ticks, 1);
    }

    #[test]
    fn multi_monitor_repeat_last_dedups_every_monitor() {
        let db = Db::open_in_memory().expect("db");
        let source = FakeFrameSource::with_monitors(["DISPLAY1", "DISPLAY2"], 4, 4);
        let mut w = watcher("multi-dedup", source, 300);

        assert!(matches!(
            w.tick(at(), 0.0, &db, "dev").as_slice(),
            [TickOutcome::Written(_), TickOutcome::Written(_)]
        ));
        // Both monitors replay their own previous frame: both deduped.
        w.source.repeat_last();
        assert_eq!(
            w.tick(at(), 0.0, &db, "dev"),
            vec![TickOutcome::Deduped, TickOutcome::Deduped]
        );
        assert_eq!(db.event_count().expect("count"), 2, "nothing new written");
        let stats = w.stats();
        assert_eq!((stats.written, stats.deduped), (2, 2));
    }

    #[test]
    fn sleep_until_returns_at_deadline_and_on_shutdown() {
        let shutdown = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_millis(300);
        sleep_until(&shutdown, deadline);
        assert!(Instant::now() >= deadline);

        // A pre-set shutdown flag must return immediately.
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(60);
        let t0 = Instant::now();
        sleep_until(&shutdown, deadline);
        assert!(t0.elapsed() < Duration::from_secs(1));
    }
}
