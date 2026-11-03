//! Daemon heartbeat: `{data_dir}/heartbeat`, one line per write:
//!
//! ```text
//! 2026-11-04T09:14:32+08:00 12345
//! ```
//!
//! (RFC3339 with the local offset, a space, the pid.) The run loop rewrites
//! it every 5 seconds on a dedicated thread; `status` considers the daemon
//! alive while the heartbeat is younger than 15 seconds (3 missed beats).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, Local, SecondsFormat};

/// Heartbeat file name inside the data dir.
pub const FILE_NAME: &str = "heartbeat";
/// A heartbeat older than this counts as stale (issue #7).
pub const MAX_AGE_S: f64 = 15.0;
/// Rewrite cadence while the daemon runs.
pub const WRITE_INTERVAL: Duration = Duration::from_secs(5);

/// One parsed heartbeat line.
#[derive(Debug, Clone, PartialEq)]
pub struct Heartbeat {
    pub pid: u32,
    pub timestamp: DateTime<FixedOffset>,
    /// Seconds since the heartbeat was written.
    pub age_s: f64,
}

/// Liveness state as reported by `status`.
#[derive(Debug, Clone, PartialEq)]
pub enum HeartbeatState {
    Running {
        heartbeat: Heartbeat,
    },
    Stale {
        heartbeat: Heartbeat,
    },
    /// No heartbeat file at all.
    NotRunning,
}

/// Path of the heartbeat file for `data_dir`.
pub fn heartbeat_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

/// Write one heartbeat line (RFC3339 + pid).
pub fn write(data_dir: &Path) -> std::io::Result<()> {
    let ts = Local::now().to_rfc3339_opts(SecondsFormat::Secs, false);
    std::fs::write(
        heartbeat_path(data_dir),
        format!("{ts} {}\n", std::process::id()),
    )
}

/// Read and parse the heartbeat, computing its age against the wall clock.
pub fn read(data_dir: &Path) -> Option<Heartbeat> {
    let content = std::fs::read_to_string(heartbeat_path(data_dir)).ok()?;
    parse(&content)
}

/// Parse one heartbeat line (exposed for tests).
pub fn parse(line: &str) -> Option<Heartbeat> {
    let mut parts = line.trim().rsplitn(2, ' ');
    let pid: u32 = parts.next()?.parse().ok()?;
    let ts = DateTime::parse_from_rfc3339(parts.next()?.trim()).ok()?;
    let age_s = (Local::now().signed_duration_since(ts)).num_seconds() as f64;
    Some(Heartbeat {
        pid,
        timestamp: ts,
        age_s: age_s.max(0.0),
    })
}

/// Classify the daemon liveness from the heartbeat file.
pub fn status(data_dir: &Path) -> HeartbeatState {
    match read(data_dir) {
        Some(hb) if hb.age_s < MAX_AGE_S => HeartbeatState::Running { heartbeat: hb },
        Some(hb) => HeartbeatState::Stale { heartbeat: hb },
        None => HeartbeatState::NotRunning,
    }
}

/// Spawn the heartbeat writer thread: writes immediately, then rewrites at
/// most every 5 seconds until `shutdown` is set (a final beat is written on
/// the way out). Takes the static shutdown flag so the thread owns a
/// `'static` reference.
pub fn spawn_writer(data_dir: &Path, shutdown: &'static AtomicBool) -> JoinHandle<()> {
    let path = data_dir.to_path_buf();
    std::thread::spawn(move || {
        let _ = write(&path);
        let mut last = std::time::Instant::now();
        while !shutdown.load(Ordering::Relaxed) {
            // Sleep in slices so shutdown stays responsive.
            std::thread::sleep(Duration::from_millis(250));
            if last.elapsed() >= WRITE_INTERVAL {
                let _ = write(&path);
                last = std::time::Instant::now();
            }
        }
        let _ = write(&path);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activity-cli-hb-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn write_read_roundtrip_matches_pid() {
        let dir = temp_dir("roundtrip");
        write(&dir).expect("write heartbeat");
        let hb = read(&dir).expect("heartbeat readable");
        assert_eq!(hb.pid, std::process::id());
        assert!(hb.age_s < 5.0, "fresh heartbeat, got {}s", hb.age_s);
        let raw = std::fs::read_to_string(heartbeat_path(&dir)).expect("raw");
        assert_eq!(raw.lines().count(), 1, "exactly one line");
        assert!(raw.trim_end().ends_with(&std::process::id().to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_rejects_garbage_and_naive_timestamps() {
        assert!(parse("no pid here").is_none());
        assert!(parse("").is_none());
        assert!(parse("2026-11-04T09:00:00 abc").is_none(), "no offset");
        assert!(parse("2026-11-04T09:00:00+08:00 notapid").is_none());
    }

    #[test]
    fn stale_when_older_than_max_age() {
        let dir = temp_dir("stale");
        let old = Local
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("valid old timestamp")
            .to_rfc3339_opts(SecondsFormat::Secs, false);
        std::fs::write(
            heartbeat_path(&dir),
            format!("{old} {}\n", std::process::id()),
        )
        .expect("write old heartbeat");
        assert_eq!(
            status(&dir),
            HeartbeatState::Stale {
                heartbeat: read(&dir).expect("readable")
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn not_running_when_file_missing() {
        let dir = temp_dir("missing");
        assert_eq!(status(&dir), HeartbeatState::NotRunning);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_thread_beats_and_stops() {
        static STOP: AtomicBool = AtomicBool::new(false);
        let dir = temp_dir("writer");
        let handle = spawn_writer(&dir, &STOP);
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            read(&dir).is_some(),
            "writer wrote the first beat immediately"
        );
        STOP.store(true, Ordering::Relaxed);
        handle.join().expect("writer joins");
        let final_hb = read(&dir).expect("final beat written");
        assert!(final_hb.age_s < 2.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
