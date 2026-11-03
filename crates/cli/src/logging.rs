//! File logging for the daemon run loop (issue #7): everything the workers
//! report through the `log` crate goes to `{data_dir}/daemon.log`, warnings
//! and errors are echoed to stderr so a console run stays observable.
//!
//! watcher-audio keeps its own `{data_dir}/logs/audio.log` per its #6
//! contract; this logger only covers the daemon itself plus the other
//! watchers and the sync forwarder.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// Append-only file logger implementing `log::Log`.
pub struct FileLogger {
    file: Mutex<Option<File>>,
    /// Echo `WARN`+ to stderr (console runs; the hidden scheduled task has
    /// no console and the echo is a no-op there).
    echo_stderr: bool,
}

impl FileLogger {
    /// Open (append) the log file, creating parent directories. Never
    /// fails: a file that cannot be opened degrades to stderr-only mode.
    pub fn open(path: &Path, echo_stderr: bool) -> FileLogger {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new().create(true).append(true).open(path).ok();
        FileLogger {
            file: Mutex::new(file),
            echo_stderr,
        }
    }

    /// Install as the process-global `log` logger at `INFO` level. Returns
    /// false when a logger was already installed (repeated `run` calls in
    /// tests); the existing logger then stays active.
    pub fn init_global(path: &Path, echo_stderr: bool) -> bool {
        match log::set_boxed_logger(Box::new(FileLogger::open(path, echo_stderr))) {
            Ok(()) => {
                log::set_max_level(log::LevelFilter::Info);
                true
            }
            Err(_) => false,
        }
    }
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ts = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let line = format!(
            "{ts} {:<5} {}: {}",
            record.level().to_string(),
            record.target(),
            record.args()
        );
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = writeln!(file, "{line}");
            }
        }
        if self.echo_stderr && record.level() <= log::Level::Warn {
            eprintln!("{line}");
        }
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;

    #[test]
    fn logger_writes_lines_and_drops_debug() {
        let dir = std::env::temp_dir().join(format!("activity-cli-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("daemon.log");
        let logger = FileLogger::open(&path, false);
        log::set_max_level(log::LevelFilter::Info);
        logger.log(
            &log::Record::builder()
                .level(log::Level::Info)
                .target("daemon")
                .args(format_args!("started"))
                .build(),
        );
        logger.log(
            &log::Record::builder()
                .level(log::Level::Debug)
                .target("daemon")
                .args(format_args!("too chatty"))
                .build(),
        );
        logger.log(
            &log::Record::builder()
                .level(log::Level::Error)
                .target("worker")
                .args(format_args!("device broke"))
                .build(),
        );
        drop(logger);
        let content = std::fs::read_to_string(&path).expect("read log");
        assert!(content.contains("INFO  daemon: started"), "{content}");
        assert!(content.contains("ERROR worker: device broke"));
        assert!(!content.contains("too chatty"), "debug filtered out");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
