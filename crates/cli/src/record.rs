//! `activity-daemon record start|stop` (issue #7): manual meeting-audio
//! control through the watcher-audio `record.json` control file.
//!
//! The file contract lives in `watcher_audio::control`: the payload is
//! exactly `{"action":"start"|"stop","trigger":"cli"}`, written **atomically**
//! (write `record.json.tmp`, then rename to `record.json`) so the watcher's
//! 500 ms poll never sees a half-written file. The watcher consumes the
//! file as soon as it has read it - file gone = command accepted.
//!
//! This module delegates serialization to `watcher_audio::control::to_json`,
//! so the CLI side cannot drift from the watcher side; a test re-parses the
//! written file with the watcher's parser.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use watcher_audio::control::{to_json, ControlAction};

use crate::usage_log;

/// `record` subcommand entry point.
pub fn cmd_record(action: RecordAction, config_override: Option<&Path>) -> Result<()> {
    let resolved = crate::config::resolve(config_override)?;
    let data_dir = resolved.config.data_dir;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;
    let result = write_control_file(&data_dir, action);
    // Issue #27: a successful manual start/stop counts as
    // {meeting-recording, manual-start|manual-stop}; fire-and-forget.
    if result.is_ok() {
        let act = match action {
            RecordAction::Start => usage_log::ACTION_MANUAL_START,
            RecordAction::Stop => usage_log::ACTION_MANUAL_STOP,
        };
        usage_log::record(
            &data_dir,
            &resolved.config.sync.device_name,
            usage_log::FEATURE_MEETING_RECORDING,
            act,
        );
    }
    result
}

/// What the CLI should write (mirrors the clap subcommand).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordAction {
    Start,
    Stop,
}

/// The control file location for a data dir.
pub fn control_path(data_dir: &Path) -> PathBuf {
    data_dir.join("record.json")
}

/// Atomically write the control file and print the one-line confirmation.
pub fn write_control_file(data_dir: &Path, action: RecordAction) -> Result<()> {
    let path = control_path(data_dir);
    let tmp = data_dir.join("record.json.tmp");
    let json = to_json(
        match action {
            RecordAction::Start => ControlAction::Start,
            RecordAction::Stop => ControlAction::Stop,
        },
        "cli",
    );
    std::fs::write(&tmp, &json).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("cannot move {} into place", tmp.display()))?;
    match action {
        RecordAction::Start => println!(
            "start requested: {} written; the audio watcher picks it up within ~500 ms (mic only)",
            path.display()
        ),
        RecordAction::Stop => println!(
            "stop requested: {} written; the audio watcher picks it up within ~500 ms",
            path.display()
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use watcher_audio::control::{parse, ControlAction};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activity-cli-record-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn writes_the_exact_watcher_audio_contract() {
        let dir = temp_dir("contract");
        write_control_file(&dir, RecordAction::Start).expect("write start");
        let path = control_path(&dir);
        let content = std::fs::read_to_string(&path).expect("read control file");
        assert_eq!(content, r#"{"action":"start","trigger":"cli"}"#);

        // The watcher's own parser must accept what we wrote.
        let cmd = parse(&content).expect("watcher-audio parses the CLI file");
        assert_eq!(cmd.action, ControlAction::Start);
        assert_eq!(cmd.trigger.as_deref(), Some("cli"));

        // No temp file left behind by the atomic write.
        assert!(!dir.join("record.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_overwrites_start_atomically() {
        let dir = temp_dir("overwrite");
        write_control_file(&dir, RecordAction::Start).expect("write start");
        write_control_file(&dir, RecordAction::Stop).expect("write stop");
        let content = std::fs::read_to_string(control_path(&dir)).expect("read");
        assert_eq!(content, r#"{"action":"stop","trigger":"cli"}"#);
        let cmd = parse(&content).expect("watcher-audio parses");
        assert_eq!(cmd.action, ControlAction::Stop);
        assert!(!dir.join("record.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
