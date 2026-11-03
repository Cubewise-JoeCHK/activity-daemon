//! File-only logging for watcher-audio (issue #6: no visible UI ever).
//!
//! Everything this watcher reports goes to `{data_dir}/logs/audio.log`.
//! If the log file cannot be opened, lines fall back to stderr so the
//! watcher still stays observable when redirected; nothing is ever shown
//! on screen.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Cloneable file logger. Safe to share with worker threads.
#[derive(Clone)]
pub struct Log {
    inner: Arc<Mutex<Option<File>>>,
}

impl Log {
    /// Open (append) the log file. Never fails: on error the logger runs in
    /// stderr-only mode.
    pub fn open(path: &Path) -> Log {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new().create(true).append(true).open(path).ok();
        Log {
            inner: Arc::new(Mutex::new(file)),
        }
    }

    /// A logger that only writes to stderr (tests).
    pub fn stderr_only() -> Log {
        Log {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    pub fn info(&self, msg: &str) {
        self.write_line("INFO", msg);
    }

    pub fn warn(&self, msg: &str) {
        self.write_line("WARN", msg);
    }

    pub fn error(&self, msg: &str) {
        self.write_line("ERROR", msg);
    }

    fn write_line(&self, level: &str, msg: &str) {
        let ts = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let line = format!("{ts} {level} watcher-audio: {msg}");
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.as_mut() {
            Some(file) => {
                use std::io::Write;
                let _ = writeln!(file, "{line}");
            }
            None => {
                eprintln!("{line}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_file_receives_lines() {
        let dir = std::env::temp_dir().join(format!("watcher-audio-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("logs").join("audio.log");
        let log = Log::open(&path);
        log.info("started");
        log.warn("hotkey unavailable");
        log.error("device error");
        drop(log);
        let content = std::fs::read_to_string(&path).expect("read log");
        assert!(content.contains("INFO watcher-audio: started"));
        assert!(content.contains("WARN watcher-audio: hotkey unavailable"));
        assert!(content.contains("ERROR watcher-audio: device error"));
        assert!(content.contains("watcher-audio"), "every line is tagged");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clone_shares_the_same_file() {
        let dir = std::env::temp_dir().join(format!("watcher-audio-logc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("audio.log");
        let log = Log::open(&path);
        let clone = log.clone();
        clone.info("from worker thread");
        drop(log);
        drop(clone);
        let content = std::fs::read_to_string(&path).expect("read log");
        assert!(content.contains("from worker thread"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
