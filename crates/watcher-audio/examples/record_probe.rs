//! End-to-end record probe for issue #6: starts `watcher_audio::run` in a
//! background thread against a temp data dir, toggles a manual recording
//! via the `record.json` control file, stops it, shuts down, and prints
//! what landed (files, DB buckets, log tail). Requires a working default
//! microphone to produce actual audio files.
//!
//! Usage (Windows): cargo run -p watcher-audio --example record_probe

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let dir =
        std::env::temp_dir().join(format!("watcher-audio-record-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::env::set_var("ACTIVITY_DATA_DIR", &dir);

    let cfg = activity_core::Config::default();
    let shutdown = Arc::new(AtomicBool::new(false));

    println!(
        "starting run() on a worker thread; data dir: {}",
        dir.display()
    );
    let shutdown_for_run = shutdown.clone();
    let run_dir = dir.clone();
    let run_thread = std::thread::Builder::new()
        .name("probe-run".to_string())
        .spawn(move || {
            // The probe owns its own Db instance (rusqlite connections are
            // !Sync, so each thread opens its own).
            let db = activity_core::Db::open_db(&run_dir.join("activity.db"))?;
            watcher_audio::run(&cfg, &db, &shutdown_for_run)
        })?;

    std::thread::sleep(std::time::Duration::from_secs(1));

    let control = dir.join("record.json");
    std::fs::write(&control, r#"{"action":"start","trigger":"cli"}"#)?;
    println!("control file: start written");
    std::thread::sleep(std::time::Duration::from_secs(4));

    std::fs::write(&control, r#"{"action":"stop","trigger":"cli"}"#)?;
    println!("control file: stop written");
    std::thread::sleep(std::time::Duration::from_secs(2));

    shutdown.store(true, Ordering::Relaxed);
    let result = run_thread.join().expect("run thread panicked");
    result?;

    println!("\nfiles under data dir:");
    print_tree(&dir, 0);

    let db = activity_core::Db::open_db(&dir.join("activity.db"))?;
    println!("\nDB buckets: {:?}", db.counts_by_bucket()?);
    for row in db.fetch_unsynced_events(100)? {
        println!(
            "  [{}] {} {} dur={}s data={}",
            row.id, row.bucket, row.ts_start, row.duration_s, row.data
        );
    }

    let log_path = dir.join("logs").join("audio.log");
    println!("\nlog tail ({}):", log_path.display());
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    for line in log.lines().skip(log.lines().count().saturating_sub(25)) {
        println!("  {line}");
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[cfg(windows)]
fn print_tree(dir: &std::path::Path, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let indent = "  ".repeat(depth + 1);
        if entry.path().is_dir() {
            println!("{}{}/", indent, entry.file_name().to_string_lossy());
            print_tree(&entry.path(), depth + 1);
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            println!(
                "{}{} ({} bytes)",
                indent,
                entry.file_name().to_string_lossy(),
                size
            );
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("record_probe is windows-only (WASAPI capture)");
}
