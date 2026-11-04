//! `activity-daemon usage` (issue #27): per-feature usage counts.
//!
//! Every **user-initiated** action writes one event into the `usage` bucket
//! through the normal core insert (`data = {"feature": ..., "action": ...}`,
//! `duration_s = 0`, `device = sync.device_name`). The rows live in the
//! local-first SQLite store like any event and reach the WSL hub through the
//! regular sync forwarder, so server-side aggregation stays possible later.
//!
//! The taxonomy is fixed and documented in README ("Usage log"); the counts
//! answer one question: which features earn their keep. Automatic features
//! (auto-update scheduled checks, sync, capture, retention) NEVER log -
//! otherwise they would fake usage. The `usage` report logs itself
//! (`{usage, view}`) as an honest meta-signal, inserted after reading so the
//! report just printed does not include its own view.
//!
//! Instrumentation is fire-and-forget: a usage-insert failure prints a
//! warning and must never fail the command the user actually ran.

use std::collections::BTreeMap;
use std::path::Path;

use activity_core::{Db, EventEnvelope};
use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local, SecondsFormat, TimeZone};
use serde::{Deserialize, Serialize};

/// Bucket for usage events. ADR buckets are open-ended, so this lives in
/// the CLI (no core change): plugins may invent their own bucket names.
pub const BUCKET_USAGE: &str = "usage";

// The fixed taxonomy (feature constants; actions below). Documented in the
// README table; a unit test pins every string.
pub const FEATURE_DAEMON: &str = "daemon";
pub const FEATURE_STATUS: &str = "status";
pub const FEATURE_MEETING_RECORDING: &str = "meeting-recording";
pub const FEATURE_STARTUP: &str = "startup";
pub const FEATURE_UPDATE: &str = "update";
pub const FEATURE_USAGE: &str = "usage";

pub const ACTION_START: &str = "start";
pub const ACTION_VIEW: &str = "view";
pub const ACTION_MANUAL_START: &str = "manual-start";
pub const ACTION_MANUAL_STOP: &str = "manual-stop";
pub const ACTION_INSTALL: &str = "install";
pub const ACTION_UNINSTALL: &str = "uninstall";
pub const ACTION_APPLY: &str = "apply";
pub const ACTION_CHECK: &str = "check";

/// Insert one usage event (bucket `usage`, `{"feature", "action"}`,
/// duration 0) for `device`. Timestamped now (RFC3339 with the local
/// offset). Errors are returned, never swallowed - callers choose the
/// fire-and-forget wrapper when they want warn-and-continue semantics.
pub fn log_usage(db: &Db, feature: &str, action: &str, device: &str) -> Result<i64> {
    let envelope = EventEnvelope {
        bucket: BUCKET_USAGE.to_string(),
        timestamp: Local::now().to_rfc3339_opts(SecondsFormat::Secs, false),
        duration_s: 0.0,
        data: serde_json::json!({ "feature": feature, "action": action }),
    };
    Ok(db.insert_event(&envelope, device)?)
}

/// Fire-and-forget contract (issue #27): a usage-insert failure must never
/// fail the user's actual command - warn on stderr and continue. The insert
/// is passed as a closure so the contract itself is unit-testable.
pub fn log_usage_quiet<F, E>(feature: &str, action: &str, insert: F)
where
    F: FnOnce() -> std::result::Result<i64, E>,
    E: std::fmt::Display,
{
    if let Err(e) = insert() {
        eprintln!("warning: usage log {feature}/{action} failed (continuing): {e}");
    }
}

/// Convenience for call sites that have no `Db` open: best-effort open (or
/// create) `{data_dir}/activity.db` and log one usage event. Every failure
/// (missing dir, broken db) is a warning, never an error - fire-and-forget.
pub fn record(data_dir: &Path, device: &str, feature: &str, action: &str) {
    if let Err(e) = try_record(data_dir, device, feature, action) {
        eprintln!("warning: usage log {feature}/{action} failed (continuing): {e:#}");
    }
}

fn try_record(data_dir: &Path, device: &str, feature: &str, action: &str) -> Result<()> {
    // Best effort: the data dir usually exists already; if it cannot be
    // created the open below fails and `record` warns instead.
    let _ = std::fs::create_dir_all(data_dir);
    let db_path = data_dir.join("activity.db");
    let db = Db::open_db(&db_path).with_context(|| format!("open {}", db_path.display()))?;
    log_usage(&db, feature, action, device).context("insert usage event")?;
    Ok(())
}

/// Per feature+action counts over the four windows (also the `--json`
/// payload rows).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageRow {
    pub feature: String,
    pub action: String,
    /// Events since local midnight of the report day.
    pub today: u64,
    /// Events in the last 7 days (inclusive lower bound, `now - 7d`).
    pub last7d: u64,
    /// Events in the last 30 days (inclusive lower bound, `now - 30d`).
    pub last30d: u64,
    /// All usage events in the local store.
    pub all_time: u64,
}

/// The full usage report (also the `--json` payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    /// Device identity the report was generated for (`sync.device_name`).
    pub device: String,
    /// Report time, RFC3339 with the local offset.
    pub generated_at: String,
    /// One row per seen feature+action pair, ordered by feature then action.
    pub rows: Vec<UsageRow>,
}

/// `usage` subcommand entry point: read the local store, print the report
/// (human table or `--json`), then log the view itself. The local rows also
/// sync to the WSL hub via the normal forwarder, so server-side aggregation
/// stays possible later.
pub fn cmd_usage(json: bool, config_override: Option<&Path>) -> Result<()> {
    let resolved = crate::config::resolve(config_override)?;
    let cfg = resolved.config;
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("cannot create data dir {}", cfg.data_dir.display()))?;
    let db_path = cfg.data_dir.join("activity.db");
    let db = Db::open_db(&db_path).with_context(|| format!("cannot open {}", db_path.display()))?;

    let report = collect(&db, Local::now(), &cfg.sync.device_name)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_human(&report));
    }

    // Honest meta-signal: the view counts itself, but it is inserted after
    // reading, so the report just printed does not include it.
    log_usage_quiet(FEATURE_USAGE, ACTION_VIEW, || {
        log_usage(&db, FEATURE_USAGE, ACTION_VIEW, &cfg.sync.device_name)
    });
    Ok(())
}

/// Aggregate usage events over the four windows. `now` anchors "today"
/// (local midnight boundary) and the 7d/30d lookbacks (injected for tests;
/// production passes `Local::now()`). Read path: one `query_events` call on
/// the `usage` bucket, filtered and counted in memory.
pub fn collect(db: &Db, now: DateTime<Local>, device: &str) -> Result<UsageReport> {
    let events = db.query_events(BUCKET_USAGE, None, None)?;
    let mut report = UsageReport {
        device: device.to_string(),
        generated_at: now.to_rfc3339_opts(SecondsFormat::Secs, false),
        rows: Vec::new(),
    };
    if events.is_empty() {
        return Ok(report);
    }

    let midnight = local_midnight(now);
    let week = (now - chrono::Duration::days(7)).fixed_offset();
    let month = (now - chrono::Duration::days(30)).fixed_offset();

    // BTreeMap keeps the output stable (feature, then action).
    let mut counts: BTreeMap<(String, String), [u64; 4]> = BTreeMap::new();
    for event in &events {
        // Each window is checked independently, so a today event also counts
        // in the 7d/30d/all-time columns (cumulative windows).
        let ts = match DateTime::parse_from_rfc3339(&event.timestamp) {
            Ok(ts) => ts,
            // Impossible through insert_event (it validates), but a corrupt
            // row must not fail the whole report - skip it.
            Err(_) => continue,
        };
        let feature = event.data["feature"].as_str().unwrap_or("(unknown)");
        let action = event.data["action"].as_str().unwrap_or("(unknown)");
        let entry = counts
            .entry((feature.to_string(), action.to_string()))
            .or_insert([0, 0, 0, 0]);
        if ts >= midnight {
            entry[0] += 1;
        }
        if ts >= week {
            entry[1] += 1;
        }
        if ts >= month {
            entry[2] += 1;
        }
        entry[3] += 1;
    }
    report.rows = counts
        .into_iter()
        .map(|((feature, action), c)| UsageRow {
            feature,
            action,
            today: c[0],
            last7d: c[1],
            last30d: c[2],
            all_time: c[3],
        })
        .collect();
    Ok(report)
}

/// Local midnight of `now`'s day as a fixed-offset instant (inclusive lower
/// bound for the "today" window). Same convention as `status`'s today line.
fn local_midnight(now: DateTime<Local>) -> DateTime<FixedOffset> {
    let naive = now.date_naive().and_hms_opt(0, 0, 0).expect("midnight");
    Local
        .from_local_datetime(&naive)
        .earliest()
        .unwrap_or(now)
        .fixed_offset()
}

/// Plain-text rendering (exactly what `usage` prints without --json).
pub(crate) fn render_human(report: &UsageReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "activity-daemon usage (device: {}, generated {})\n",
        report.device, report.generated_at
    ));
    if report.rows.is_empty() {
        out.push_str("\n  no usage events yet - counts appear once you use features\n");
        return out;
    }

    let feature_w = report
        .rows
        .iter()
        .map(|r| r.feature.len())
        .max()
        .unwrap_or(0)
        .max("feature".len());
    let action_w = report
        .rows
        .iter()
        .map(|r| r.action.len())
        .max()
        .unwrap_or(0)
        .max("action".len());
    let today_w = num_width(report.rows.iter().map(|r| r.today), "today");
    let week_w = num_width(report.rows.iter().map(|r| r.last7d), "7d");
    let month_w = num_width(report.rows.iter().map(|r| r.last30d), "30d");
    let all_w = num_width(report.rows.iter().map(|r| r.all_time), "all-time");

    out.push_str(&format!(
        "\n{:<feature_w$}  {:<action_w$}  {:>today_w$}  {:>week_w$}  {:>month_w$}  {:>all_w$}\n",
        "feature", "action", "today", "7d", "30d", "all-time"
    ));
    for row in &report.rows {
        out.push_str(&format!(
            "{:<feature_w$}  {:<action_w$}  {:>today_w$}  {:>week_w$}  {:>month_w$}  {:>all_w$}\n",
            row.feature, row.action, row.today, row.last7d, row.last30d, row.all_time
        ));
    }
    out
}

/// Column width for a numeric column: widest value or the header length.
fn num_width(values: impl Iterator<Item = u64>, header: &str) -> usize {
    values
        .map(|v| v.to_string().len())
        .max()
        .unwrap_or(0)
        .max(header.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use activity_core::BUCKET_WINDOW;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activity-cli-usage-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// Local wall-clock instant, so seeded timestamps round-trip through the
    /// RFC3339 parsing on any machine / timezone.
    fn local_ts(y: i32, m: u32, d: u32, h: u32, min: u32) -> String {
        Local
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("valid timestamp")
            .to_rfc3339_opts(SecondsFormat::Secs, false)
    }

    /// Seed one usage-shaped event at an explicit timestamp (log_usage always
    /// stamps "now", so aggregation tests insert the envelope directly).
    fn seed(db: &Db, feature: &str, action: &str, ts: &str) {
        db.insert_event(
            &EventEnvelope {
                bucket: BUCKET_USAGE.to_string(),
                timestamp: ts.to_string(),
                duration_s: 0.0,
                data: serde_json::json!({"feature": feature, "action": action}),
            },
            "dev-laptop",
        )
        .expect("seed usage event");
    }

    #[test]
    fn taxonomy_matches_the_documented_readme_table() {
        assert_eq!(BUCKET_USAGE, "usage");
        assert_eq!(FEATURE_DAEMON, "daemon");
        assert_eq!(FEATURE_STATUS, "status");
        assert_eq!(FEATURE_MEETING_RECORDING, "meeting-recording");
        assert_eq!(FEATURE_STARTUP, "startup");
        assert_eq!(FEATURE_UPDATE, "update");
        assert_eq!(FEATURE_USAGE, "usage");
        assert_eq!(ACTION_START, "start");
        assert_eq!(ACTION_VIEW, "view");
        assert_eq!(ACTION_MANUAL_START, "manual-start");
        assert_eq!(ACTION_MANUAL_STOP, "manual-stop");
        assert_eq!(ACTION_INSTALL, "install");
        assert_eq!(ACTION_UNINSTALL, "uninstall");
        assert_eq!(ACTION_APPLY, "apply");
        assert_eq!(ACTION_CHECK, "check");
    }

    #[test]
    fn log_usage_writes_expected_rows() {
        let db = Db::open_in_memory().expect("in-memory db");
        log_usage(&db, FEATURE_DAEMON, ACTION_START, "dev-laptop").expect("insert daemon start");
        log_usage(&db, FEATURE_STATUS, ACTION_VIEW, "dev-laptop").expect("insert status view");
        log_usage(&db, FEATURE_DAEMON, ACTION_START, "dev-other").expect("insert other device");

        let events = db.query_events(BUCKET_USAGE, None, None).expect("query");
        assert_eq!(events.len(), 3);
        for e in &events {
            assert_eq!(e.bucket, "usage");
            assert_eq!(e.duration_s, 0.0);
            // RFC3339 with a timezone offset, like every stored event.
            assert!(DateTime::parse_from_rfc3339(&e.timestamp).is_ok());
        }
        assert_eq!(events[0].data["feature"], "daemon");
        assert_eq!(events[0].data["action"], "start");
        assert_eq!(events[0].device, "dev-laptop");
        assert_eq!(events[1].data["feature"], "status");
        assert_eq!(events[1].data["action"], "view");
        assert_eq!(events[2].device, "dev-other");

        // Usage stays in its own bucket; window events are untouched.
        assert_eq!(db.counts_by_bucket().expect("counts").len(), 1);
        assert!(db
            .query_events(BUCKET_WINDOW, None, None)
            .expect("window query")
            .is_empty());
    }

    #[test]
    fn windows_aggregate_around_local_midnight() {
        let db = Db::open_in_memory().expect("in-memory db");
        // Anchor: 2026-11-04 15:00 local. Boundaries: midnight 11-04 00:00,
        // 7d = 10-28 15:00, 30d = 10-05 15:00.
        let now = Local
            .with_ymd_and_hms(2026, 11, 4, 15, 0, 0)
            .single()
            .expect("now");

        seed(&db, "daemon", "start", &local_ts(2026, 11, 4, 14, 30)); // today
        seed(&db, "daemon", "start", &local_ts(2026, 11, 4, 0, 0)); // exactly at local midnight: today (inclusive)
        seed(&db, "daemon", "start", &local_ts(2026, 11, 3, 12, 0)); // yesterday: 7d + 30d
        seed(&db, "daemon", "start", &local_ts(2026, 10, 28, 14, 0)); // just past the 7d edge: 30d only
        seed(&db, "daemon", "start", &local_ts(2026, 10, 4, 15, 0)); // just past the 30d edge: all-time only
        seed(&db, "status", "view", &local_ts(2026, 11, 2, 9, 0)); // other row: 7d + 30d
                                                                   // Non-usage activity must never leak into usage counts.
        db.insert_event(
            &EventEnvelope {
                bucket: BUCKET_WINDOW.to_string(),
                timestamp: local_ts(2026, 11, 4, 10, 0),
                duration_s: 60.0,
                data: serde_json::json!({"app": "Code.exe", "title": "t"}),
            },
            "dev-laptop",
        )
        .expect("seed window event");

        let report = collect(&db, now, "dev-laptop").expect("collect");
        assert_eq!(report.device, "dev-laptop");
        assert_eq!(
            report.generated_at,
            now.to_rfc3339_opts(SecondsFormat::Secs, false)
        );
        assert_eq!(
            report.rows,
            vec![
                UsageRow {
                    feature: "daemon".to_string(),
                    action: "start".to_string(),
                    today: 2,
                    last7d: 3,
                    last30d: 4,
                    all_time: 5,
                },
                UsageRow {
                    feature: "status".to_string(),
                    action: "view".to_string(),
                    today: 0,
                    last7d: 1,
                    last30d: 1,
                    all_time: 1,
                },
            ]
        );
    }

    #[test]
    fn json_payload_roundtrips_and_carries_the_windows() {
        let report = UsageReport {
            device: "dev-laptop".to_string(),
            generated_at: "2026-11-04T15:00:00+08:00".to_string(),
            rows: vec![UsageRow {
                feature: "daemon".to_string(),
                action: "start".to_string(),
                today: 2,
                last7d: 5,
                last30d: 9,
                all_time: 12,
            }],
        };
        let json = serde_json::to_string_pretty(&report).expect("serialize");
        for field in [
            "device",
            "generated_at",
            "rows",
            "feature",
            "action",
            "today",
            "last7d",
            "last30d",
            "all_time",
        ] {
            assert!(json.contains(field), "json mentions {field}: {json}");
        }
        let back: UsageReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, report);
    }

    #[test]
    fn quiet_wrapper_never_propagates_instrumentation_failures() {
        let mut calls = 0usize;
        log_usage_quiet("daemon", "start", || {
            calls += 1;
            Err::<i64, _>(anyhow::anyhow!("boom"))
        });
        assert_eq!(calls, 1, "the insert was attempted exactly once");
        // The Ok path is a no-fuss pass-through.
        log_usage_quiet("daemon", "start", || Ok::<i64, anyhow::Error>(1));
    }

    #[test]
    fn record_writes_one_row_and_survives_an_unusable_data_dir() {
        let dir = temp_dir("record");
        record(&dir, "dev-laptop", FEATURE_STATUS, ACTION_VIEW);
        let db = Db::open_db(&dir.join("activity.db")).expect("db created by record");
        let events = db.query_events(BUCKET_USAGE, None, None).expect("query");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["feature"], "status");
        assert_eq!(events[0].data["action"], "view");
        assert_eq!(events[0].device, "dev-laptop");
        assert_eq!(events[0].duration_s, 0.0);

        // A data_dir that cannot host a db (a regular file) must only warn,
        // never panic and never write anything.
        let not_a_dir = dir.join("not-a-dir");
        std::fs::write(&not_a_dir, b"x").expect("write plain file");
        record(&not_a_dir, "dev-laptop", FEATURE_DAEMON, ACTION_START);
        assert_eq!(
            db.query_events(BUCKET_USAGE, None, None)
                .expect("query")
                .len(),
            1,
            "the failed record must not have touched the db"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn human_rendering_shows_header_counts_and_empty_state() {
        let report = UsageReport {
            device: "dev-laptop".to_string(),
            generated_at: "2026-11-04T15:00:00+08:00".to_string(),
            rows: vec![
                UsageRow {
                    feature: "daemon".to_string(),
                    action: "start".to_string(),
                    today: 2,
                    last7d: 5,
                    last30d: 9,
                    all_time: 12,
                },
                UsageRow {
                    feature: "meeting-recording".to_string(),
                    action: "manual-stop".to_string(),
                    today: 0,
                    last7d: 1,
                    last30d: 3,
                    all_time: 100,
                },
            ],
        };
        let text = render_human(&report);
        for needle in [
            "dev-laptop",
            "feature",
            "action",
            "today",
            "7d",
            "30d",
            "all-time",
            "daemon",
            "manual-stop",
        ] {
            assert!(text.contains(needle), "renders {needle}: {text}");
        }
        // Columns are aligned: the header width of the feature column fits
        // the longest feature name.
        assert!(
            text.contains("meeting-recording  "),
            "feature column padded: {text}"
        );

        let empty = render_human(&UsageReport {
            device: "dev".to_string(),
            generated_at: "t".to_string(),
            rows: Vec::new(),
        });
        assert!(empty.contains("no usage events yet"), "{empty}");
    }
}
