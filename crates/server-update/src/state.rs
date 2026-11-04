//! Durable updater state (`<state dir>/activity-server/state.json`,
//! default `~/.local/state/activity-server/state.json`): enough to
//!
//! - self-heal a container that vanished mid-swap (saved run args +
//!   rollback image id - a `systemctl stop` that lands between the
//!   `podman stop` and `podman run` steps kills the swap; the next `watch`
//!   tick recreates the container from this state),
//! - keep exactly one rollback image between successful updates, and
//! - answer `status` without podman or network.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Where the updater keeps its state (XDG_STATE_HOME honored, tests pass
/// temp dirs instead - never call this from library code).
pub fn default_state_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("activity-server"));
        }
    }
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("cannot resolve a state dir (no XDG_STATE_HOME, no HOME)");
    home.map(|h| h.join(".local/state/activity-server"))
}

/// The persisted state. Every field optional: a fresh hub has none of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateState {
    /// RFC3339 timestamp of the last finished check (success or failure).
    pub last_check: Option<String>,
    /// Release tag successfully swapped in, e.g. `"v0.1.3"`.
    pub last_applied_tag: Option<String>,
    /// Image id that was running before the last swap: the rollback target
    /// and the self-heal source when the container is missing.
    pub rollback_image_id: Option<String>,
    /// `podman run` args (without the image) that recreate the container.
    pub run_args: Vec<String>,
    /// Human-readable outcome of the last check/apply, for `status`.
    pub last_result: Option<String>,
}

/// State file path below `state_dir`.
pub fn state_path(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("state.json")
}

/// Load the state; a missing file is the default (empty) state. A corrupt
/// file is an error - silently resetting it would drop the rollback info.
pub fn load(state_dir: &std::path::Path) -> Result<UpdateState> {
    let path = state_path(state_dir);
    if !path.exists() {
        return Ok(UpdateState::default());
    }
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("corrupt {}", path.display()))
}

/// Save the state (creating the dir); atomic enough for a single-writer
/// updater (write + rename).
pub fn save(state_dir: &std::path::Path, state: &UpdateState) -> Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("cannot create {}", state_dir.display()))?;
    let path = state_path(state_dir);
    let tmp = state_dir.join("state.json.tmp");
    let body = serde_json::to_string_pretty(state).context("cannot serialize state")?;
    std::fs::write(&tmp, body).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot rename into {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activity-server-update-state-{tag}-{}-{}",
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
    fn missing_file_is_the_default_state() {
        let dir = temp_dir("missing");
        let state = load(&dir).expect("load missing");
        assert_eq!(state, UpdateState::default());
        assert!(state.last_applied_tag.is_none());
        assert!(state.rollback_image_id.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_load_roundtrip_and_tmp_cleanup() {
        let dir = temp_dir("roundtrip");
        let state = UpdateState {
            last_check: Some("2026-11-04T12:00:00+08:00".to_string()),
            last_applied_tag: Some("v0.2.0".to_string()),
            rollback_image_id: Some("a1b2c3".to_string()),
            run_args: vec![
                "--name".into(),
                "activity-server".into(),
                "-p".into(),
                "8080:8080".into(),
            ],
            last_result: Some("applied v0.2.0".to_string()),
        };
        save(&dir, &state).expect("save");
        assert!(!dir.join("state.json.tmp").exists(), "tmp consumed");
        let back = load(&dir).expect("load");
        assert_eq!(back, state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_is_an_error_never_a_silent_reset() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join("state.json"), "{not json").expect("write garbage");
        let err = load(&dir).expect_err("corrupt state must error");
        assert!(err.to_string().contains("corrupt"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
