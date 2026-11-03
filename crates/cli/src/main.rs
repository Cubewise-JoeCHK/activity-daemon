//! `activity-daemon` CLI: run / status / startup / record (issue #7).
//!
//! - `run` - the daemon: four worker threads (window, screen, audio, sync),
//!   each with its own core `Db` connection, one shared shutdown flag,
//!   single-instance lock, heartbeat and `daemon.log` in the data dir.
//! - `status` - heartbeat liveness, current foreground, today active time,
//!   sync backlog, storage usage vs budgets; `--json` for machines.
//! - `startup install|uninstall|status` - Windows Task Scheduler autostart
//!   (logon trigger, restart on failure, hidden); `--dry-run` previews.
//! - `record start|stop` - manual mic-only recording via the watcher-audio
//!   `record.json` control file (atomic write, `trigger: "cli"`).
//!
//! Global flag: `--config <path>` (default `%APPDATA%\activity-daemon\config.toml`;
//! a missing file is created as a commented template with defaults).

mod config;
mod heartbeat;
mod lock;
mod logging;
mod record;
mod run;
mod startup;
mod status;

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
    /// Manage autostart via the Windows Task Scheduler (task "ActivityDaemon").
    Startup {
        #[command(subcommand)]
        action: startup::StartupAction,
        /// Print the task XML / commands instead of calling schtasks
        /// (accepted before or after the subcommand).
        #[arg(long, global = true)]
        dry_run: bool,
    },
    /// Manual meeting-audio recording (mic only; in-person meetings).
    Record {
        #[command(subcommand)]
        action: RecordAction,
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
        Command::Record { action } => match action {
            RecordAction::Start => {
                record::cmd_record(record::RecordAction::Start, cli.config.as_deref())
            }
            RecordAction::Stop => {
                record::cmd_record(record::RecordAction::Stop, cli.config.as_deref())
            }
        },
    };
    if let Err(e) = result {
        eprintln!("error: {e:?}");
        std::process::exit(1);
    }
}
