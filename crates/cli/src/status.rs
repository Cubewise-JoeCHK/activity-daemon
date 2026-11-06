//! `activity-daemon status` (issue #7): a read-only snapshot of the daemon
//! and its data.
//!
//! Fields (human text, or `--json` for machines):
//! - daemon liveness from the heartbeat file (alive < 15 s, stale, gone)
//! - current foreground app + title (last `window-watcher` event, 30-day
//!   lookback - enough for "what was I doing", never a full-table scan of a
//!   years-old database)
//! - today active time: sum of `window-watcher` session durations since
//!   local midnight; the coalescer closes sessions at AFK onset, so the sum
//!   already excludes AFK gaps
//! - unsynced event count (the forwarder backlog)
//! - database size on disk (main file + WAL + SHM)
//! - frames and audio media bytes vs their rolling budgets
//!   (`screen.max_total_mb` / `audio.max_total_mb`)
//! - hub media retention stats (issue #38, best effort): `GET
//!   /api/retention` on the configured hub - hot bytes by kind, bundle
//!   count/bytes, months pending, next scheduled action. Absent when the
//!   hub is unreachable; the local report never depends on the network.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use activity_core::{Config, Db, BUCKET_WINDOW};
use anyhow::Result;
use chrono::{DateTime, Local, SecondsFormat, TimeZone};
use serde::{Deserialize, Serialize};

use crate::heartbeat::{self, HeartbeatState};
use crate::usage_log;

/// Lookback for the "current foreground" probe.
const FOREGROUND_LOOKBACK_DAYS: i64 = 30;

/// `status` subcommand entry point.
pub fn cmd_status(json: bool, config_override: Option<&Path>) -> Result<()> {
    let resolved = crate::config::resolve(config_override)?;
    let mut report = collect(&resolved.config, Local::now());
    // Best effort: retention stats live on the hub; offline or older hubs
    // simply leave the field absent.
    report.retention = fetch_retention(&resolved.config.sync.server_url);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_human(&report));
    }
    // Issue #27: every status invocation counts as {status, view}; the
    // usage insert is fire-and-forget and never fails the status command.
    usage_log::record(
        &resolved.config.data_dir,
        &resolved.config.sync.device_name,
        usage_log::FEATURE_STATUS,
        usage_log::ACTION_VIEW,
    );
    Ok(())
}

/// Media directory usage against its rolling budget.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct MediaUsage {
    /// Total bytes of regular files below the media directory.
    pub bytes: u64,
    /// Rolling budget in MB from the config.
    pub budget_mb: u64,
}

/// Last observed foreground window (from the last window-watcher event).
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct Foreground {
    pub app: String,
    pub title: String,
    /// RFC3339 timestamp of the session start.
    pub timestamp: String,
}

/// Daemon liveness from the heartbeat file.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct DaemonState {
    /// "running" | "stale" | "not-running".
    pub state: String,
    /// Pid of the heartbeat writer, when a heartbeat exists.
    pub pid: Option<u32>,
    /// Seconds since the last heartbeat beat, when one exists.
    pub heartbeat_age_s: Option<f64>,
    /// RFC3339 timestamp of the last beat, when one exists.
    pub last_seen: Option<String>,
}

/// The full status snapshot (also the `--json` payload).
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct StatusReport {
    pub data_dir: String,
    pub daemon: DaemonState,
    pub foreground: Option<Foreground>,
    /// Active seconds today (window sessions since local midnight).
    pub today_active_s: f64,
    pub total_events: i64,
    pub unsynced_events: i64,
    /// False when `activity.db` does not exist yet.
    pub db_present: bool,
    /// activity.db + WAL + SHM bytes.
    pub db_bytes: u64,
    pub frames: MediaUsage,
    pub audio: MediaUsage,
    /// Hub media retention stats (issue #38); absent when the hub is
    /// unreachable or predates #38.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionStats>,
}

/// Hub-side media retention snapshot (issue #38), fetched from
/// `GET /api/retention` on the configured hub. Mirror of the server
/// payload; deserializes loosely so additive server fields stay harmless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionStats {
    pub enabled: bool,
    pub hot_days: u64,
    /// Raw bytes per media kind still hot on the hub.
    pub hot_bytes: BTreeMap<String, u64>,
    pub bundle_count: u64,
    pub bundle_bytes: u64,
    /// Cold months still holding raw files, oldest first.
    pub months_pending: Vec<HubPendingMonth>,
    /// What the hub's next daily sweeper pass would do.
    pub next_action: String,
}

/// One pending month on the hub ([`RetentionStats`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HubPendingMonth {
    pub month: String,
    pub files: u64,
    pub bytes: u64,
}

/// Best-effort fetch of the hub's retention stats. `None` on any failure
/// (hub down, old hub without the endpoint, timeout) - status is a local
/// command and must never fail because the network is away.
fn fetch_retention(server_url: &str) -> Option<RetentionStats> {
    let url = format!("{}/api/retention", server_url.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(3))
        .build();
    let response = agent.get(&url).call().ok()?;
    response.into_json::<RetentionStats>().ok()
}

/// Collect the status snapshot. `now` anchors the "today" window (injected
/// for tests; production passes `Local::now()`).
pub fn collect(cfg: &Config, now: DateTime<Local>) -> StatusReport {
    let data_dir = &cfg.data_dir;
    let daemon = match heartbeat::status(data_dir) {
        HeartbeatState::Running { heartbeat } => DaemonState {
            state: "running".to_string(),
            pid: Some(heartbeat.pid),
            heartbeat_age_s: Some(heartbeat.age_s),
            last_seen: Some(
                heartbeat
                    .timestamp
                    .to_rfc3339_opts(SecondsFormat::Secs, false),
            ),
        },
        HeartbeatState::Stale { heartbeat } => DaemonState {
            state: "stale".to_string(),
            pid: Some(heartbeat.pid),
            heartbeat_age_s: Some(heartbeat.age_s),
            last_seen: Some(
                heartbeat
                    .timestamp
                    .to_rfc3339_opts(SecondsFormat::Secs, false),
            ),
        },
        HeartbeatState::NotRunning => DaemonState {
            state: "not-running".to_string(),
            pid: None,
            heartbeat_age_s: None,
            last_seen: None,
        },
    };

    let db_path = data_dir.join("activity.db");
    let db_present = db_path.exists();
    let mut report = StatusReport {
        data_dir: data_dir.display().to_string(),
        daemon,
        foreground: None,
        today_active_s: 0.0,
        total_events: 0,
        unsynced_events: 0,
        db_present,
        db_bytes: 0,
        frames: MediaUsage {
            bytes: dir_size(&data_dir.join("media").join("frames")),
            budget_mb: cfg.screen.max_total_mb,
        },
        audio: MediaUsage {
            bytes: dir_size(&data_dir.join("media").join("audio")),
            budget_mb: cfg.audio.max_total_mb,
        },
        retention: None,
    };

    if db_present {
        if let Ok(db) = Db::open_db(&db_path) {
            report.total_events = db.event_count().unwrap_or(0);
            report.unsynced_events = db.unsynced_count().unwrap_or(0);
            report.db_bytes = db_disk_bytes(data_dir);
            if let Ok(events) = db.query_events(BUCKET_WINDOW, Some(&midnight_rfc3339(now)), None) {
                // Explicit fold: std's `sum()` yields -0.0 on an empty
                // iterator, which would leak into the JSON as "-0.0".
                report.today_active_s = events.iter().map(|e| e.duration_s).fold(0.0, |a, b| a + b);
            }
            report.foreground = last_foreground(&db, now);
        }
    }
    report
}

/// Local midnight of `now`'s day as RFC3339 with offset.
fn midnight_rfc3339(now: DateTime<Local>) -> String {
    let naive = now.date_naive().and_hms_opt(0, 0, 0).expect("midnight");
    let midnight = Local.from_local_datetime(&naive).earliest().unwrap_or(now);
    midnight.to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// Last window-watcher event within the lookback window.
fn last_foreground(db: &Db, now: DateTime<Local>) -> Option<Foreground> {
    let from = (now - chrono::Duration::days(FOREGROUND_LOOKBACK_DAYS))
        .to_rfc3339_opts(SecondsFormat::Secs, false);
    let events = db.query_events(BUCKET_WINDOW, Some(&from), None).ok()?;
    let last = events.last()?;
    let app = last.data["app"].as_str().unwrap_or_default().to_string();
    let title = last.data["title"].as_str().unwrap_or_default().to_string();
    Some(Foreground {
        app,
        title,
        timestamp: last.timestamp.clone(),
    })
}

/// Sum of activity.db + activity.db-wal + activity.db-shm sizes.
fn db_disk_bytes(data_dir: &Path) -> u64 {
    ["activity.db", "activity.db-wal", "activity.db-shm"]
        .iter()
        .map(|name| file_size(&data_dir.join(name)))
        .sum()
}

/// Size of one regular file (0 if missing or unreadable).
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Total bytes of all regular files below `dir` (0 when the dir is absent).
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += dir_size(&path);
        } else if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// Human binary size: "0 B", "512 B", "1.5 MB", "3.2 GB", ...
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

/// Human duration: "45s", "12m 30s", "3h 24m", "1d 2h".
pub(crate) fn human_duration(total_s: u64) -> String {
    let days = total_s / 86_400;
    let hours = (total_s % 86_400) / 3600;
    let minutes = (total_s % 3600) / 60;
    let seconds = total_s % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Plain-text rendering (exactly what `status` prints without --json).
pub(crate) fn render_human(report: &StatusReport) -> String {
    let mut out = String::new();
    let db_line = if report.db_present {
        format!(
            "{} events, {} unsynced, db {}",
            report.total_events,
            report.unsynced_events,
            human_bytes(report.db_bytes)
        )
    } else {
        "no database yet - run `activity-daemon run` first".to_string()
    };
    let daemon_line = match (
        report.daemon.state.as_str(),
        report.daemon.pid,
        report.daemon.heartbeat_age_s,
    ) {
        ("running", Some(pid), Some(age)) => {
            format!("running (pid {pid}, heartbeat {:.0}s old)", age)
        }
        ("stale", Some(pid), Some(age)) => {
            format!("stale (pid {pid}, last heartbeat {:.0}s ago)", age)
        }
        _ => "not running".to_string(),
    };
    let foreground_line = match &report.foreground {
        Some(fg) if !fg.app.is_empty() && !fg.title.is_empty() => {
            format!("{} - {} (since {})", fg.app, fg.title, fg.timestamp)
        }
        Some(fg) if !fg.app.is_empty() => format!("{} (since {})", fg.app, fg.timestamp),
        _ => "unknown".to_string(),
    };

    out.push_str("activity-daemon status\n");
    out.push_str(&format!("  daemon:   {daemon_line}\n"));
    out.push_str(&format!("  data dir: {}\n", report.data_dir));
    out.push_str(&format!("  current:  {foreground_line}\n"));
    out.push_str(&format!(
        "  today:    {} active (window sessions since local midnight)\n",
        human_duration(report.today_active_s.round() as u64)
    ));
    out.push_str(&format!("  db:       {db_line}\n"));
    out.push_str(&format!(
        "  frames:   {} / {} MB budget\n",
        human_bytes(report.frames.bytes),
        report.frames.budget_mb
    ));
    out.push_str(&format!(
        "  audio:    {} / {} MB budget\n",
        human_bytes(report.audio.bytes),
        report.audio.budget_mb
    ));
    if let Some(retention) = &report.retention {
        let state = if retention.enabled {
            format!("on (hot {}d)", retention.hot_days)
        } else {
            "off".to_string()
        };
        let pending = if retention.months_pending.is_empty() {
            "nothing pending".to_string()
        } else {
            format!(
                "{} month(s) pending, next: {}",
                retention.months_pending.len(),
                retention.next_action
            )
        };
        out.push_str(&format!(
            "  hub:      {} bundle(s) ({}), {pending}, retention {state}\n",
            retention.bundle_count,
            human_bytes(retention.bundle_bytes)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use activity_core::{EventEnvelope, SyncConfig};
    use chrono::TimeZone;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activity-cli-status-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// Local wall-clock instant, so event timestamps round-trip through the
    /// RFC3339 query bounds on any machine / timezone.
    fn local_ts(day: u32, hour: u32, min: u32) -> String {
        let dt = Local
            .with_ymd_and_hms(2026, 11, day, hour, min, 0)
            .single()
            .expect("valid timestamp");
        dt.to_rfc3339_opts(SecondsFormat::Secs, false)
    }

    fn insert_window(db: &Db, ts: &str, duration_s: f64, app: &str, title: &str) {
        db.insert_event(
            &EventEnvelope {
                bucket: BUCKET_WINDOW.to_string(),
                timestamp: ts.to_string(),
                duration_s,
                data: serde_json::json!({"app": app, "title": title}),
            },
            "test-device",
        )
        .expect("insert window event");
    }

    #[test]
    fn human_formats() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536 * 1024), "1.5 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
        assert_eq!(human_bytes(5 * 1024_u64.pow(4)), "5.0 TB");
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(750), "12m 30s");
        assert_eq!(human_duration(12_240), "3h 24m");
        assert_eq!(human_duration(93_600), "1d 2h");
    }

    #[test]
    fn aggregates_from_seeded_temp_db() {
        let dir = temp_dir("seeded");
        let db = Db::open_db(&dir.join("activity.db")).expect("open db");
        // Today (2026-11-04 local): two active sessions and an AFK bracket.
        insert_window(&db, &local_ts(4, 9, 0), 3_600.0, "Code.exe", "main.rs");
        insert_window(&db, &local_ts(4, 10, 0), 1_800.0, "chrome.exe", "Docs");
        db.insert_event(
            &EventEnvelope {
                bucket: activity_core::BUCKET_AFK.to_string(),
                timestamp: local_ts(4, 10, 30),
                duration_s: 0.0,
                data: serde_json::json!({"state": "afk"}),
            },
            "test-device",
        )
        .expect("insert afk");
        // Yesterday: must not leak into today's numbers.
        insert_window(&db, &local_ts(3, 9, 0), 9_999.0, "OldApp.exe", "old");

        let cfg = Config {
            data_dir: dir.clone(),
            sync: SyncConfig {
                device_name: "test-device".to_string(),
                ..SyncConfig::default()
            },
            ..Config::default()
        };
        let now = Local
            .with_ymd_and_hms(2026, 11, 4, 11, 0, 0)
            .single()
            .expect("valid now");
        let report = collect(&cfg, now);

        assert_eq!(report.today_active_s, 5_400.0, "60m + 30m, afk excluded");
        assert_eq!(
            report.foreground,
            Some(Foreground {
                app: "chrome.exe".to_string(),
                title: "Docs".to_string(),
                timestamp: local_ts(4, 10, 0),
            })
        );
        assert_eq!(report.total_events, 4);
        assert_eq!(report.unsynced_events, 4);
        assert!(report.db_present);
        assert!(report.db_bytes > 0);
        assert_eq!(report.frames.bytes, 0);
        assert_eq!(report.frames.budget_mb, cfg.screen.max_total_mb);
        assert_eq!(report.audio.budget_mb, cfg.audio.max_total_mb);

        // Human rendering carries the essentials.
        let text = render_human(&report);
        assert!(text.contains("chrome.exe - Docs"), "{text}");
        assert!(text.contains("1h 30m"), "{text}");
        assert!(text.contains("unsynced"), "{text}");
        assert!(text.contains("daemon:"), "{text}");

        // JSON payload round-trips through serde.
        let json = serde_json::to_string(&report).expect("serialize");
        let back: StatusReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, report);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_db_and_missing_heartbeat_render_not_running() {
        let dir = temp_dir("empty");
        let cfg = Config {
            data_dir: dir.clone(),
            ..Config::default()
        };
        let now = Local
            .with_ymd_and_hms(2026, 11, 4, 11, 0, 0)
            .single()
            .expect("now");
        let report = collect(&cfg, now);
        assert!(!report.db_present);
        assert_eq!(report.daemon.state, "not-running");
        assert_eq!(report.today_active_s, 0.0);
        let text = render_human(&report);
        assert!(text.contains("not running"), "{text}");
        assert!(text.contains("no database yet"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: a present-but-empty db must yield a clean "+0.0" (std's
    /// `sum()` returns -0.0 on empty iterators, which serde_json would print
    /// as "-0.0").
    #[test]
    fn empty_db_today_active_serializes_as_zero() {
        let dir = temp_dir("zerodb");
        drop(Db::open_db(&dir.join("activity.db")).expect("create empty db"));
        let cfg = Config {
            data_dir: dir.clone(),
            ..Config::default()
        };
        let now = Local
            .with_ymd_and_hms(2026, 11, 4, 11, 0, 0)
            .single()
            .expect("now");
        let report = collect(&cfg, now);
        assert!(report.db_present);
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(json.contains("\"today_active_s\":0.0"), "{json}");
        assert!(!json.contains("-0.0"), "{json}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #38: the retention block is absent while the hub is
    /// unreachable (field skipped in JSON, still parseable), and a fetched
    /// hub payload round-trips through the report.
    #[test]
    fn retention_field_is_optional_and_roundtrips() {
        let cfg = Config {
            data_dir: temp_dir("retention-absent"),
            ..Config::default()
        };
        let now = Local
            .with_ymd_and_hms(2026, 11, 4, 11, 0, 0)
            .single()
            .expect("now");
        let report = collect(&cfg, now);
        assert_eq!(report.retention, None);
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(
            !json.contains("\"retention\""),
            "absent field must be skipped: {json}"
        );
        let back: StatusReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, report);

        // A hub payload (as GET /api/retention returns) round-trips.
        let mut with_hub = report.clone();
        with_hub.retention = Some(RetentionStats {
            enabled: true,
            hot_days: 365,
            hot_bytes: BTreeMap::from([("frames".to_string(), 1024), ("audio".to_string(), 512)]),
            bundle_count: 2,
            bundle_bytes: 4096,
            months_pending: vec![HubPendingMonth {
                month: "2025-10".to_string(),
                files: 12,
                bytes: 2048,
            }],
            next_action:
                "bundle 2025-10 into bundles/2025-10.tar.zst (12 files, 2.0 KB) then delete raws"
                    .to_string(),
        });
        let json = serde_json::to_string(&with_hub).expect("serialize");
        assert!(json.contains("\"retention\""), "{json}");
        let back: StatusReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, with_hub);

        // Human rendering carries the essentials.
        let text = render_human(&with_hub);
        assert!(text.contains("hub:"), "{text}");
        assert!(text.contains("2 bundle(s)"), "{text}");
        assert!(text.contains("1 month(s) pending"), "{text}");
        assert!(text.contains("on (hot 365d)"), "{text}");
        let _ = std::fs::remove_dir_all(&cfg.data_dir);
    }
}
