//! Stderr logging for the hub updater: under the systemd user service
//! stderr lands in the journal (`journalctl --user -u
//! activity-server-update`); standalone runs keep a visible console.
//! Same `log` facade as the client (crates/cli/src/logging.rs), minus the
//! file - the hub has no data-dir log contract.

use std::io::Write;

/// Minimal stderr logger at `INFO` level implementing `log::Log`.
pub struct StderrLogger;

impl StderrLogger {
    /// Install as the process-global `log` logger. Returns false when a
    /// logger was already installed (repeated calls in tests); the existing
    /// logger then stays active.
    pub fn init_global() -> bool {
        match log::set_boxed_logger(Box::new(StderrLogger)) {
            Ok(()) => {
                log::set_max_level(log::LevelFilter::Info);
                true
            }
            Err(_) => false,
        }
    }
}

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ts = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let _ = writeln!(
            std::io::stderr(),
            "{ts} {:<5} {}",
            record.level().to_string(),
            record.args()
        );
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;

    #[test]
    fn logger_writes_lines_and_drops_debug() {
        // No panic, and the level filter contract holds.
        let logger = StderrLogger;
        log::set_max_level(log::LevelFilter::Info);
        assert!(logger.enabled(&log::Metadata::builder().level(log::Level::Info).build()));
        assert!(!logger.enabled(&log::Metadata::builder().level(log::Level::Debug).build()));
        logger.log(
            &log::Record::builder()
                .level(log::Level::Info)
                .target("updater")
                .args(format_args!("line to stderr"))
                .build(),
        );
    }
}
