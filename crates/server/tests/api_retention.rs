//! Integration test for `GET /api/retention` (issue #38): the endpoint
//! reports the hub's bundling state - enabled flag, hot bytes by kind,
//! bundle count/bytes, cold months still pending, next scheduled action.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Datelike, Months, NaiveDate};
use serde_json::Value;
use server::retention::RetentionConfig;
use server::{build_router, AppState, AuthConfig};
use tower::ServiceExt;

/// Self-cleaning temp directory (same pattern as the ingest tests).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("activity-server-apiretention-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn state_in(dir: &TempDir, retention: RetentionConfig) -> AppState {
    AppState::open_with_retention(
        &dir.0.join("activity.db"),
        dir.0.join("media"),
        AuthConfig::disabled(),
        retention,
    )
    .expect("test app state")
}

/// Seed one raw frame file into a month of the store under `dir`.
fn seed_raw(dir: &TempDir, month: &str, day: &str, name: &str) -> u64 {
    let (year, rest) = month.split_once('-').expect("month shape");
    let path = dir
        .0
        .join("media")
        .join("frames")
        .join(year)
        .join(rest)
        .join(day)
        .join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let content = format!("{month}/{day}/{name}\n");
    std::fs::write(&path, &content).expect("write");
    content.len() as u64
}

/// `YYYY-MM` of the month `months_back` months before `today`'s month.
/// That month is guaranteed cold for any `hot_days` below the distance
/// (its last day always lies more than `months_back - 1` months behind).
fn month_key_of(today: NaiveDate, months_back: u32) -> String {
    let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
        .expect("month start")
        .checked_sub_months(Months::new(months_back))
        .expect("month shift");
    format!("{:04}-{:02}", first.year(), first.month())
}

async fn get_retention(state: AppState) -> (StatusCode, Value) {
    let app = build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/retention")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let payload = serde_json::from_slice(&bytes).expect("json body");
    (status, payload)
}

#[tokio::test]
async fn retention_endpoint_reports_disabled_default_state() {
    let dir = TempDir::new("disabled");
    // One cold month (13 months back: cold for the 365d default on any run
    // date) plus one current-month file (never cold).
    let today = chrono::Local::now().date_naive();
    let cold_month = month_key_of(today, 13);
    let current_month = month_key_of(today, 0);
    seed_raw(&dir, &cold_month, "15", "120000_D1_a.jpg");
    seed_raw(&dir, &current_month, "01", "120000_D1_now.jpg");

    let (status, payload) = get_retention(state_in(&dir, RetentionConfig::default())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["enabled"], false);
    assert_eq!(payload["hot_days"], 365);
    assert!(payload["hot_bytes"]["frames"].as_u64().expect("frames") > 0);
    assert_eq!(payload["bundle_count"], 0);
    // Issue #39: per-bundle tiers and the minio block are always present;
    // with the stage disabled nothing is moved.
    assert_eq!(payload["bundles"], serde_json::json!([]));
    assert_eq!(payload["minio"]["enabled"], false);
    assert_eq!(payload["minio"]["moved_months"], serde_json::json!([]));
    // Months are still reported as pending even while disabled: the stats
    // describe the store, the flag describes the sweeper. The current month
    // is never pending.
    let pending: Vec<&str> = payload["months_pending"]
        .as_array()
        .expect("months_pending")
        .iter()
        .map(|m| m["month"].as_str().expect("month"))
        .collect();
    assert_eq!(pending, vec![cold_month.as_str()]);
    assert!(payload["next_action"]
        .as_str()
        .expect("next_action")
        .contains(&format!("bundle {cold_month}")));
}

#[tokio::test]
async fn retention_endpoint_reflects_enabled_config() {
    let dir = TempDir::new("enabled");
    let today = chrono::Local::now().date_naive();
    // hot_days = 30: the month 2 months back is cold on any run date; the
    // current month stays hot forever.
    let cold_month = month_key_of(today, 2);
    let current_month = month_key_of(today, 0);
    seed_raw(&dir, &cold_month, "15", "120000_D1_old.jpg");
    seed_raw(&dir, &current_month, "01", "120000_D1_now.jpg");

    let cfg = RetentionConfig {
        enabled: true,
        hot_days: 30,
        ..RetentionConfig::default()
    };
    let (status, payload) = get_retention(state_in(&dir, cfg)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["enabled"], true);
    assert_eq!(payload["hot_days"], 30);
    let pending: Vec<&str> = payload["months_pending"]
        .as_array()
        .expect("months_pending")
        .iter()
        .map(|m| m["month"].as_str().expect("month"))
        .collect();
    assert_eq!(pending, vec![cold_month.as_str()]);
}
