//! The `record.json` control-file channel (issue #6, for the CLI in #7).
//!
//! # FILE CONTRACT - `activity-daemon record start|stop` (issue #7)
//!
//! To start or stop a manual (mic-only) recording without a hotkey, the CLI
//! writes a single JSON file:
//!
//! ```text
//! {data_dir}/record.json   polled by watcher-audio every 500 ms
//! ```
//!
//! ```json
//! {"action": "start", "trigger": "cli"}
//! ```
//!
//! - `action` is required: `"start"` or `"stop"`.
//! - `trigger` is optional; `"cli"` is the expected value. Any other content
//!   is accepted (extra fields ignored) and logged.
//!
//! Semantics:
//!
//! - The watcher **consumes the file** (deletes it) as soon as it has read
//!   it, whether or not the command changed anything. File gone = command
//!   accepted.
//! - `start` while already recording is a **no-op** (no double start), but
//!   the file is still consumed. Callers can check recording state by
//!   watching the `meeting-audio` bucket in the DB.
//! - `stop` while idle is a no-op; the file is consumed.
//! - A malformed file is renamed to `record.json.invalid` (not deleted) so
//!   the mistake stays diagnosable, and the watcher moves on.
//! - Atomic write on the CLI side: write `record.json.tmp` then rename to
//!   `record.json` so the watcher never reads a half-written file.
//!
//! This channel works even if the hotkey failed to register.

use std::path::Path;

use serde::Deserialize;

/// A parsed control-file command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlCommand {
    pub action: ControlAction,
    /// Raw `trigger` value as written by the caller (informational).
    pub trigger: Option<String>,
}

/// Action requested by the control file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlAction {
    Start,
    Stop,
}

/// Outcome of one control-file poll.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlFileOutcome {
    /// No file present.
    Absent,
    /// A valid command was read (file already consumed).
    Command(ControlCommand),
    /// The file existed but was not valid JSON / not a known action; it was
    /// renamed to `<name>.invalid` for diagnosis.
    Invalid(String),
}

#[derive(Debug, Deserialize)]
struct RawCommand {
    action: String,
    #[serde(default)]
    trigger: Option<String>,
}

/// Parse the content of a control file. Exposed for tests.
pub fn parse(content: &str) -> Option<ControlCommand> {
    let raw: RawCommand = serde_json::from_str(content).ok()?;
    let action = match raw.action.as_str() {
        "start" => ControlAction::Start,
        "stop" => ControlAction::Stop,
        _ => return None,
    };
    Some(ControlCommand {
        action,
        trigger: raw.trigger,
    })
}

/// Read and consume the control file at `path` (see module docs).
/// `consume` controls whether the file is removed after a successful read
/// (tests pass `false`); production passes `true`.
pub fn poll_control_file(path: &Path, consume: bool) -> ControlFileOutcome {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ControlFileOutcome::Absent,
        Err(e) => {
            // Unreadable (permissions/locking): treat as absent this tick.
            let _ = e;
            return ControlFileOutcome::Absent;
        }
    };

    if !consume {
        return match parse(&content) {
            Some(cmd) => ControlFileOutcome::Command(cmd),
            None => ControlFileOutcome::Invalid(content),
        };
    }

    match parse(&content) {
        Some(cmd) => {
            // Consume only after a successful read+parse.
            if let Err(e) = std::fs::remove_file(path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return ControlFileOutcome::Invalid(format!(
                        "command read but file could not be removed: {e}"
                    ));
                }
            }
            ControlFileOutcome::Command(cmd)
        }
        None => {
            // Keep the malformed file for diagnosis under `.invalid`
            // (record.json -> record.json.invalid).
            let invalid = path.with_extension("json.invalid");
            let _ = std::fs::remove_file(&invalid);
            let renamed = std::fs::rename(path, &invalid).is_ok();
            let snippet: String = content.chars().take(200).collect();
            if !renamed {
                let _ = std::fs::remove_file(path);
            }
            ControlFileOutcome::Invalid(snippet)
        }
    }
}

/// Serialize a command for writing (CLI side / tests).
pub fn to_json(action: ControlAction, trigger: &str) -> String {
    let action = match action {
        ControlAction::Start => "start",
        ControlAction::Stop => "stop",
    };
    serde_json::json!({"action": action, "trigger": trigger}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("watcher-audio-ctl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir.join("record.json")
    }

    #[test]
    fn parses_valid_commands() {
        let cmd = parse(r#"{"action":"start","trigger":"cli"}"#).expect("start");
        assert_eq!(cmd.action, ControlAction::Start);
        assert_eq!(cmd.trigger.as_deref(), Some("cli"));

        let cmd = parse(r#"{"action":"stop"}"#).expect("stop without trigger");
        assert_eq!(cmd.action, ControlAction::Stop);
        assert_eq!(cmd.trigger, None);

        // Extra fields ignored, case of values matters (lowercase required).
        let cmd = parse(r#"{"action":"start","trigger":"cli","extra":1}"#).expect("extra ok");
        assert_eq!(cmd.action, ControlAction::Start);
    }

    #[test]
    fn rejects_malformed_or_unknown() {
        assert!(parse("not json").is_none());
        assert!(parse(r#"{"action":"pause"}"#).is_none(), "unknown action");
        assert!(parse(r#"{"trigger":"cli"}"#).is_none(), "action required");
    }

    #[test]
    fn poll_absent_when_no_file() {
        let path = temp_file("absent");
        assert_eq!(poll_control_file(&path, true), ControlFileOutcome::Absent);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn poll_consumes_valid_command_file() {
        let path = temp_file("consume");
        std::fs::write(&path, to_json(ControlAction::Start, "cli")).expect("write");
        let outcome = poll_control_file(&path, true);
        assert_eq!(
            outcome,
            ControlFileOutcome::Command(ControlCommand {
                action: ControlAction::Start,
                trigger: Some("cli".to_string()),
            })
        );
        assert!(!path.exists(), "file must be consumed");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn poll_invalid_file_is_renamed_not_deleted() {
        let path = temp_file("invalid");
        std::fs::write(&path, "{{{").expect("write");
        let outcome = poll_control_file(&path, true);
        assert!(matches!(outcome, ControlFileOutcome::Invalid(_)));
        let invalid = path.with_extension("json.invalid");
        assert!(invalid.exists(), "invalid file kept for diagnosis");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn poll_without_consume_leaves_file_in_place() {
        let path = temp_file("noconsume");
        std::fs::write(&path, to_json(ControlAction::Stop, "cli")).expect("write");
        let outcome = poll_control_file(&path, false);
        assert_eq!(
            outcome,
            ControlFileOutcome::Command(ControlCommand {
                action: ControlAction::Stop,
                trigger: Some("cli".to_string()),
            })
        );
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn to_json_roundtrips() {
        assert_eq!(
            to_json(ControlAction::Start, "cli"),
            r#"{"action":"start","trigger":"cli"}"#
        );
        assert!(parse(&to_json(ControlAction::Stop, "cli")).is_some());
    }
}
