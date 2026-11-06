//! Binary entry point for the central server. Configuration via environment
//! variables plus an optional TOML config file on the data volume (see
//! docs/sync.md for the deployment runbook):
//!
//! | var               | default                 | meaning                       |
//! |-------------------|-------------------------|-------------------------------|
//! | `ACTIVITY_PORT`   | `8080`                  | bind port on `0.0.0.0`        |
//! | `ACTIVITY_DB`     | `./data/activity.db`    | central SQLite store          |
//! | `ACTIVITY_DATA`   | `./data`                | media root (`$DATA/media`)    |
//! | `ACTIVITY_TOKEN`  | unset                   | require `X-Activity-Token`    |
//! | `ACTIVITY_CONFIG` | `$ACTIVITY_DATA/config.toml` | server config (`[retention]`, `[timesheet]`) |
//!
//! Flags: `--version` (print and exit), `--dry-run` (print the retention
//! action plan for the store and exit - nothing is written or deleted,
//! works whether or not retention is enabled).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use chrono::Local;
use server::retention::{self, RetentionStore};
use server::{AppState, AuthConfig};

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn main() {
    // `--version`: print and exit (no clap here - these are the only flags).
    // The hub updater's smoke test (issue #32) runs the new image with it
    // before swapping the container.
    if std::env::args().any(|a| a == "--version") {
        println!("activity-server {}", server::VERSION);
        return;
    }
    let dry_run = std::env::args().any(|a| a == "--dry-run");

    let port: u16 = env_or("ACTIVITY_PORT", "8080").parse().unwrap_or_else(|_| {
        eprintln!("ACTIVITY_PORT is not a valid port number, using 8080");
        8080
    });
    let db_path = PathBuf::from(env_or("ACTIVITY_DB", "./data/activity.db"));
    let data_dir = PathBuf::from(env_or("ACTIVITY_DATA", "./data"));

    // Server config: missing file means all defaults (retention disabled);
    // an unparsable file must not start the hub (a typo must not silently
    // run with different settings than the owner wrote down).
    let config_path = retention::config_path_or_default(&data_dir);
    let server_config = match retention::load_server_config(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    if !config_path.exists() {
        println!(
            "no server config at {} - defaults apply (retention disabled)",
            config_path.display()
        );
    }

    let store = RetentionStore::from_media_dir(&data_dir.join("media"));
    if dry_run {
        // Preview only: reads the store, prints the plan, exits.
        print!(
            "{}",
            retention::render_plan(&store, &server_config.retention, Local::now().date_naive())
        );
        return;
    }

    if let Some(parent) = db_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "failed to create database directory {}: {e}",
                parent.display()
            );
            std::process::exit(1);
        }
    }
    if let Err(e) = std::fs::create_dir_all(data_dir.join("media")) {
        eprintln!(
            "failed to create media root {}: {e}",
            data_dir.join("media").display()
        );
        std::process::exit(1);
    }

    let auth = AuthConfig::detect();
    println!("auth mode: {}", auth.describe());

    let state = AppState::open_with_retention(
        &db_path,
        data_dir.join("media"),
        auth,
        server_config.retention.clone(),
    )
    .map(|state| state.with_timesheet_vocab_path(server_config.timesheet.vocab_path.clone()))
    .unwrap_or_else(|e| {
        eprintln!("failed to open central store at {}: {e}", db_path.display());
        std::process::exit(1);
    });
    let app = server::build_router(state);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async move {
        // Retention sweeper (issue #38): one idempotent pass at startup,
        // then a daily tick. Disabled by default; nothing is scheduled
        // until the owner opts in via the config file.
        if server_config.retention.enabled {
            println!(
                "retention: enabled (hot_days {}, bundle monthly) - daily sweeper scheduled",
                server_config.retention.hot_days
            );
            tokio::spawn(retention_tick(store, server_config.retention));
        } else {
            println!("retention: disabled - raw media is never bundled or deleted");
        }

        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| {
                eprintln!("failed to bind {addr}: {e}");
                std::process::exit(1);
            });
        println!(
            "activity-daemon central hub listening on http://{addr} (version {})",
            server::VERSION
        );
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
    });
}

/// Daily retention sweeper: run one pass now (blocking fs work on the
/// blocking pool), log the outcome, then wait 24 h. Each pass is
/// idempotent and crash-safe; failures keep the raw files and retry on
/// the next tick.
async fn retention_tick(store: RetentionStore, cfg: retention::RetentionConfig) {
    loop {
        let report = tokio::task::spawn_blocking({
            let store = store.clone();
            let cfg = cfg.clone();
            move || retention::run_pass(&store, &cfg, Local::now().date_naive())
        })
        .await
        .unwrap_or_else(|e| {
            eprintln!("retention: pass task failed: {e}");
            Default::default()
        });
        for line in report.log_lines() {
            println!("{line}");
        }
        tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
    }
}

/// Resolve when the process is asked to stop: Ctrl+C (SIGINT) everywhere,
/// plus SIGTERM on unix - `podman stop` sends SIGTERM, so the hub update
/// swap (issue #32) drains in-flight requests gracefully instead of
/// running into the runtime's SIGKILL timeout.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("cannot install the SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// SIGTERM must resolve the shutdown future (the podman stop path).
    #[tokio::test]
    async fn sigterm_resolves_the_shutdown_signal() {
        use std::process::Command;
        use std::time::Duration;
        tokio::spawn(async {
            let pid = std::process::id().to_string();
            let _ = Command::new("kill").args(["-TERM", &pid]).status();
        });
        tokio::time::timeout(Duration::from_secs(10), shutdown_signal())
            .await
            .expect("SIGTERM must resolve the shutdown future within 10 s");
    }
}
