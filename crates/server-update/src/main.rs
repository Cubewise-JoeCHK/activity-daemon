//! `activity-server-update`: hub updater for the activity-daemon central
//! server (issue #32). Runs on the WSL host (not inside the container -
//! a container cannot swap its own image):
//!
//! - `check` - report the running version vs the latest GitHub release.
//! - `apply` (default) - download the sha256-verified release binary, build
//!   a thin image, smoke test it, swap the container (re-derived run args +
//!   `--restart=on-failure`), health-gate with automatic rollback.
//! - `watch` - loop for the systemd user service: self-heal a missing
//!   container from the saved state, then check every `--interval-secs`.
//! - `install` / `status` / `uninstall` - manage the systemd user service.
//! - `rollback` - start the saved rollback image again.
//!
//! Release checking, tag comparison and sha256 verification are shared with
//! the client auto-update (#17) via the `activity-update` crate. See
//! docs/sync.md (runbook) and issue #32 for the full contract.

use std::time::Duration;

use activity_server_update::{logging, podman, state, systemd, updater};
use activity_update::{GitHubFetcher, HttpDownloader};
use anyhow::Result;
use clap::{Parser, Subcommand};

use activity_server_update::podman::RealPodman;
use updater::{HubUpdater, RealHealthProbe};

/// What `activity-server-update <command>` should do (bare invocation = apply).
#[derive(Debug, Parser)]
#[command(
    name = "activity-server-update",
    version,
    about = "Hub updater: keeps the WSL activity-server container current from GitHub Releases (#32)",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report the running version vs the latest release; change nothing.
    Check,
    /// Update now (download, verify, build, smoke test, swap, health-gate).
    Apply,
    /// Watch loop for the systemd user service: self-heal, then check
    /// every interval.
    Watch {
        /// Seconds between checks (default: daily).
        #[arg(long, default_value_t = systemd::DEFAULT_INTERVAL_SECS)]
        interval_secs: u64,
    },
    /// Install + enable the systemd user service (linger included).
    Install {
        /// Seconds between checks baked into the unit (default: daily).
        #[arg(long, default_value_t = systemd::DEFAULT_INTERVAL_SECS)]
        interval_secs: u64,
        /// Print the unit file and commands without touching anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the service state, the saved state and the running version.
    Status,
    /// Disable + remove the systemd user service.
    Uninstall {
        /// Print the commands without touching anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Start the saved rollback image again (the pre-update deployment).
    Rollback,
}

fn main() {
    logging::StderrLogger::init_global();
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Check) => wired().check_only(),
        Some(Command::Apply) | None => cmd_apply(),
        Some(Command::Watch { interval_secs }) => cmd_watch(interval_secs),
        Some(Command::Install {
            interval_secs,
            dry_run,
        }) => systemd::install(interval_secs, dry_run),
        Some(Command::Status) => cmd_status(),
        Some(Command::Uninstall { dry_run }) => systemd::uninstall(dry_run),
        Some(Command::Rollback) => wired().rollback(),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Production wiring: real GitHub fetcher/downloader, podman CLI and
/// localhost health probe against the default state dir.
fn wired() -> HubUpdater<'static> {
    let fetcher: Box<GitHubFetcher> = Box::new(GitHubFetcher::new());
    let downloader: Box<HttpDownloader> = Box::new(HttpDownloader::new());
    let podman: Box<RealPodman> = Box::new(RealPodman);
    let health: Box<RealHealthProbe> = Box::new(RealHealthProbe::new());
    let state_dir = state::default_state_dir().expect("state dir");
    HubUpdater::new(
        Box::leak(fetcher),
        Box::leak(downloader),
        Box::leak(podman),
        Box::leak(health),
        state_dir,
    )
}

fn cmd_apply() -> Result<()> {
    let updater = wired();
    match updater.check_and_apply()? {
        Some(tag) => println!("updated to {tag}: the container now runs the new release"),
        None => println!("already up to date"),
    }
    Ok(())
}

fn cmd_watch(interval_secs: u64) -> Result<()> {
    let updater = wired();
    let interval = Duration::from_secs(interval_secs.max(1));
    log::info!(
        "watch loop started (interval {:?}); self-heals a missing container, then checks GitHub Releases",
        interval
    );
    loop {
        // Tick = resilience first, then the update check. Every failure is
        // logged and carried: the loop outlives network and podman hiccups.
        if let Err(e) = updater.ensure_container() {
            log::warn!("self-heal check: {e}");
        }
        if let Err(e) = updater.check_and_apply() {
            log::warn!("update check failed (retrying after the interval): {e:#}");
        }
        std::thread::sleep(interval);
    }
}

fn cmd_status() -> Result<()> {
    let updater = wired();
    println!("service: {}", systemd_service_line());
    let st = state::load(&updater.state_dir)?;
    println!(
        "state file: {}",
        state::state_path(&updater.state_dir).display()
    );
    println!(
        "last check: {}",
        st.last_check.unwrap_or_else(|| "never".to_string())
    );
    println!(
        "last result: {}",
        st.last_result.unwrap_or_else(|| "-".to_string())
    );
    println!(
        "last applied tag: {}",
        st.last_applied_tag.unwrap_or_else(|| "-".to_string())
    );
    println!(
        "rollback image: {}",
        st.rollback_image_id.unwrap_or_else(|| "-".to_string())
    );
    match updater.podman.container_inspect(podman::CONTAINER_NAME)? {
        Some(inspect) => {
            let version = updater
                .detect_current_version(&inspect)?
                .unwrap_or_else(|| "unknown".to_string());
            println!("running: {} (image {})", version, inspect.image_id);
        }
        None => println!("running: container {} not found", podman::CONTAINER_NAME),
    }
    Ok(())
}

/// `enabled/active` or a hint when systemd is unavailable (WSL without
/// systemd).
fn systemd_service_line() -> String {
    match systemd::service_state() {
        Ok(Some((enabled, active))) => format!("{enabled} / {active}"),
        _ => "systemd user services unavailable (WSL without systemd?)".to_string(),
    }
}
