//! `activity-daemon` CLI. The daemon binary for the Windows client.
//! Subcommands are stubs for now: they print exactly what they will do,
//! so the CLI contract is fixed before the watchers land (#3-#6, #18).

use std::path::PathBuf;

use activity_core::Db;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "activity-daemon",
    version,
    about = "Personal activity collection daemon (window/AFK/screen/audio watchers, local-first + sync)",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: watchers + sync forwarder loop, 24/7, local-first.
    Run,
    /// Print event counts from the local database.
    Status,
    /// Register the daemon to start automatically with Windows.
    Startup,
    /// Manual meeting-audio recording (mic only; #6).
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
    match cli.command {
        Command::Run => {
            println!("TODO(#3): run the daemon loop:");
            println!("  - window/AFK watcher polling every config.poll_interval_s s (#4)");
            println!(
                "  - screen frame watcher every config.screen.interval_s s, hash-deduped (#5)"
            );
            println!("  - meeting audio watcher, hotkey + conferencing auto-trigger (#6)");
            println!("  - forwarder loop: batch unsynced events to the WSL hub over Tailscale, then mark synced (#18)");
            println!(
                "  - local SQLite is always written first; collection never waits on the network"
            );
        }
        Command::Status => status(),
        Command::Startup => {
            println!("TODO: register autostart (scheduled task or Run key) so 24/7 collection survives reboots.");
        }
        Command::Record { action } => match action {
            RecordAction::Start => {
                println!("TODO(#6): start meeting-audio recording (mic only; loopback is added by the conferencing auto-trigger).");
            }
            RecordAction::Stop => {
                println!("TODO(#6): stop the active meeting-audio recording.");
            }
        },
    }
}

/// Local database location; overridable for tests and the central server.
fn db_path() -> PathBuf {
    std::env::var_os("ACTIVITY_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("activity.db"))
}

fn status() {
    let path = db_path();
    if !path.exists() {
        println!(
            "no local database at {} yet - run `activity-daemon run` first",
            path.display()
        );
        return;
    }
    let db = match Db::open_db(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("failed to open {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    match db.counts_by_bucket() {
        Ok(rows) => {
            println!("local db: {}", path.display());
            for (bucket, count) in rows {
                println!("  {bucket}: {count}");
            }
            let total = db.event_count().unwrap_or(0);
            let unsynced = db.unsynced_count().unwrap_or(0);
            println!("  total: {total} event(s), unsynced: {unsynced}");
        }
        Err(e) => {
            eprintln!("query failed: {e}");
            std::process::exit(1);
        }
    }
}
