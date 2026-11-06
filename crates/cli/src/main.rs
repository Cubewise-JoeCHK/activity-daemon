//! `activity-daemon` CLI: run / status / startup / install / record /
//! update.
//!
//! - `run` - the daemon: four worker threads (window, screen, audio, sync),
//!   each with its own core `Db` connection, one shared shutdown flag,
//!   single-instance lock, heartbeat and `daemon.log` in the data dir;
//!   optional auto-update from GitHub Releases (issue #17). Windows only
//!   (issue #30): the console window is hidden per `run.hide_console`
//!   (default auto) and a system tray icon shows health (active / recording
//!   / heartbeat stale) with an Update now / Quit menu.
//! - `status` - heartbeat liveness, current foreground, today active time,
//!   sync backlog, storage usage vs budgets; `--json` for machines.
//! - `startup install|uninstall|status` - Windows autostart via a no-admin
//!   fallback chain (issues #7, #29): full Task Scheduler XML -> minimal
//!   task XML -> Startup-folder `activity-daemon.vbs` (windowless);
//!   `status` reports which mechanism is effective, `uninstall` removes
//!   all of them; `--dry-run` previews.
//! - `install` - one command from bare install to fully supervised capture
//!   (issue #36): config patches, the #29 autostart chain, no-sleep-on-AC,
//!   and the watchdog (Windows service `ActivityDaemonWatchdog` checking
//!   the heartbeat every 15s and restarting the daemon into the active
//!   user session, degrading to an every-minute `ensure-run` scheduled
//!   task where service creation is denied). `install status|uninstall`
//!   mirror `startup`; `--dry-run` previews.
//! - `ensure-run` - one supervision decision (heartbeat fresh = leave,
//!   stale + dead lock holder = start, stale + wedged = kill and start);
//!   used by the watchdog task, never logs usage.
//! - `record start|stop` - manual mic-only recording via the watcher-audio
//!   `record.json` control file (atomic write, `trigger: "cli"`).
//! - `update` / `update check` - self-update from GitHub Releases (issue
//!   #17): sha256-verified download, `<exe>.old` rollback copy, graceful
//!   restart, then a verified relaunch - when the new instance produces no
//!   heartbeat beat within 15s the update rolls itself back (issue #37).
//! - `usage` - per-feature usage counts over today / 7d / 30d / all-time
//!   (issue #27); every user-initiated action above logs one `usage` event
//!   (fire-and-forget), automatic features never log.
//!
//! Global flag: `--config <path>` (default `%APPDATA%\activity-daemon\config.toml`;
//! a missing file is created as a commented template with defaults).

mod config;
mod console;
mod heartbeat;
mod install;
mod lock;
mod logging;
mod record;
mod run;
mod startup;
mod status;
mod tray;
mod update;
mod usage_log;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "activity-daemon",
    version,
    about = "Personal activity collection daemon (window/AFK/screen/audio watchers, local-first + sync)",
    long_about = None
)]
struct Cli {
    /// Path to config.toml (default: %APPDATA%\activity-daemon\config.toml;
    /// a missing file is created as a commented template with defaults).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: watchers + sync forwarder, 24/7, local-first.
    Run,
    /// Show daemon liveness, current activity and storage state.
    Status {
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Manage autostart (task "ActivityDaemon" with a Startup-folder
    /// fallback when the Task Scheduler refuses; issue #29).
    Startup {
        #[command(subcommand)]
        action: startup::StartupAction,
        /// Print the task XML / commands instead of calling schtasks
        /// (accepted before or after the subcommand).
        #[arg(long, global = true)]
        dry_run: bool,
    },
    /// One command from bare install to fully supervised capture (issue
    /// #36): config patches, autostart, no-sleep-on-AC, and the watchdog
    /// (service preferred, every-minute task fallback).
    Install {
        /// `status` reports every layer; `uninstall` removes them; bare
        /// `install` applies.
        #[command(subcommand)]
        action: Option<install::InstallAction>,
        /// Patch sync.device_name in config.toml (.bak-install backup).
        #[arg(long, value_name = "NAME")]
        device: Option<String>,
        /// Patch sync.server_url in config.toml (.bak-install backup).
        #[arg(long, value_name = "URL")]
        server_url: Option<String>,
        /// Flip sync.auto_update to false (the daily update swap is the
        /// prime suspect for the 2026-11-06 silent death; off by default).
        #[arg(long)]
        disable_auto_update: bool,
        /// Skip the no-sleep-on-AC power default.
        #[arg(long)]
        keep_power_plan: bool,
        /// Only the supervisor stage (used by the elevated retry).
        #[arg(long)]
        service_only: bool,
        /// Print the plan instead of touching anything.
        #[arg(long, global = true)]
        dry_run: bool,
    },
    /// One supervision check: start or restart the daemon when its
    /// heartbeat is stale (used by the watchdog task; silent when healthy).
    EnsureRun,
    /// Run the watchdog service loop (issue #36). Hidden: the SCM starts
    /// the exe with `service --data-dir <dir>`; not for interactive use.
    #[command(hide = true)]
    Service {
        /// Data dir to supervise (heartbeat + lock live here).
        #[arg(long, value_name = "DIR")]
        data_dir: PathBuf,
    },
    /// Manual meeting-audio recording (mic only; in-person meetings).
    Record {
        #[command(subcommand)]
        action: RecordAction,
    },
    /// Check for and install updates from GitHub Releases (issue #17).
    Update {
        /// `check` reports only; bare `update` applies when a newer release
        /// exists.
        #[command(subcommand)]
        action: Option<update::UpdateAction>,
    },
    /// Per-feature usage counts (today / 7d / 30d / all-time) from the local
    /// store (issue #27). Usage events also sync to the WSL hub via the
    /// normal forwarder, so server-side aggregation stays possible later.
    Usage {
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum RecordAction {
    /// Start recording the microphone (in-person meeting).
    Start,
    /// Stop the active recording.
    Stop,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run => run::cmd_run(cli.config.as_deref()),
        Command::Status { json } => status::cmd_status(json, cli.config.as_deref()),
        Command::Startup { dry_run, action } => {
            startup::cmd_startup(action, dry_run, cli.config.as_deref())
        }
        Command::Install {
            action,
            device,
            server_url,
            disable_auto_update,
            keep_power_plan,
            service_only,
            dry_run,
        } => install::cmd_install(
            action,
            device,
            server_url,
            disable_auto_update,
            keep_power_plan,
            service_only,
            dry_run,
            cli.config.as_deref(),
        ),
        Command::EnsureRun => install::cmd_ensure_run(cli.config.as_deref()),
        Command::Service { data_dir } => install::cmd_service(&data_dir),
        Command::Record { action } => match action {
            RecordAction::Start => {
                record::cmd_record(record::RecordAction::Start, cli.config.as_deref())
            }
            RecordAction::Stop => {
                record::cmd_record(record::RecordAction::Stop, cli.config.as_deref())
            }
        },
        Command::Update { action } => update::cmd_update(action, cli.config.as_deref()),
        Command::Usage { json } => usage_log::cmd_usage(json, cli.config.as_deref()),
    };
    if let Err(e) = result {
        eprintln!("error: {e:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `update` (apply) and `update check` (report only) both parse; the
    /// `--config` global flag is accepted in any position.
    #[test]
    fn update_subcommand_parsing() {
        let cli = Cli::try_parse_from(["activity-daemon", "update"]).expect("parse update");
        assert!(matches!(cli.command, Command::Update { action: None }));

        let cli = Cli::try_parse_from(["activity-daemon", "update", "check"]).expect("parse check");
        assert!(matches!(
            cli.command,
            Command::Update {
                action: Some(update::UpdateAction::Check)
            }
        ));

        let cli = Cli::try_parse_from([
            "activity-daemon",
            "--config",
            "D:\\cfg\\config.toml",
            "update",
            "check",
        ])
        .expect("parse with config override");
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("D:\\cfg\\config.toml"))
        );
    }

    /// `usage` parses with and without `--json`.
    #[test]
    fn usage_subcommand_parsing() {
        let cli = Cli::try_parse_from(["activity-daemon", "usage"]).expect("parse usage");
        assert!(matches!(cli.command, Command::Usage { json: false }));

        let cli =
            Cli::try_parse_from(["activity-daemon", "usage", "--json"]).expect("parse usage json");
        assert!(matches!(cli.command, Command::Usage { json: true }));
    }

    /// `install` (apply), `install status`, `install uninstall` and the
    /// flags parse (issue #36).
    #[test]
    fn install_subcommand_parsing() {
        let cli = Cli::try_parse_from(["activity-daemon", "install"]).expect("parse install");
        let Command::Install {
            action,
            device,
            server_url,
            disable_auto_update,
            keep_power_plan,
            service_only,
            dry_run,
        } = &cli.command
        else {
            panic!("install variant");
        };
        assert!(action.is_none());
        assert!(device.is_none() && server_url.is_none());
        assert!(!disable_auto_update && !keep_power_plan && !service_only && !dry_run);

        let cli = Cli::try_parse_from([
            "activity-daemon",
            "install",
            "--device",
            "cubejoe",
            "--server-url",
            "http://hub:8080",
            "--disable-auto-update",
            "--keep-power-plan",
            "--dry-run",
        ])
        .expect("parse install flags");
        let Command::Install {
            action,
            device,
            server_url,
            disable_auto_update,
            keep_power_plan,
            service_only: _,
            dry_run,
        } = &cli.command
        else {
            panic!("install variant");
        };
        assert!(action.is_none());
        assert_eq!(device.as_deref(), Some("cubejoe"));
        assert_eq!(server_url.as_deref(), Some("http://hub:8080"));
        assert!(*disable_auto_update && *keep_power_plan && *dry_run);

        let cli =
            Cli::try_parse_from(["activity-daemon", "install", "status"]).expect("parse status");
        let Command::Install { action, .. } = &cli.command else {
            panic!("install variant");
        };
        assert!(matches!(action, Some(install::InstallAction::Status)));

        let cli = Cli::try_parse_from(["activity-daemon", "install", "uninstall"])
            .expect("parse uninstall");
        let Command::Install { action, .. } = &cli.command else {
            panic!("install variant");
        };
        assert!(matches!(action, Some(install::InstallAction::Uninstall)));

        // The internal service entrypoint parses with its data dir.
        let cli = Cli::try_parse_from(["activity-daemon", "service", "--data-dir", "C:\\ad\\data"])
            .expect("parse service");
        let Command::Service { data_dir } = &cli.command else {
            panic!("service variant");
        };
        assert_eq!(data_dir, &PathBuf::from("C:\\ad\\data"));

        // ensure-run parses bare.
        let cli = Cli::try_parse_from(["activity-daemon", "ensure-run"]).expect("parse ensure");
        assert!(matches!(cli.command, Command::EnsureRun));
    }
}
