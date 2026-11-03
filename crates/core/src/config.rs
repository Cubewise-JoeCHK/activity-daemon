//! TOML configuration for the daemon and its watchers.
//!
//! Every field has a sensible default (see `Default` impls, matching the ADR
//! decisions): screen frames keep a rolling budget of 90 days / 20 GB on the
//! device, meeting audio 60 days / 10 GB. Partial config files are valid:
//! missing fields fall back to defaults.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::storage::Result;

/// Top-level daemon configuration, deserialized from `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Root directory of the local archive (ADR rev 5): `activity.db` lives
    /// at `<data_dir>/activity.db` and media files under
    /// `<data_dir>/media/<kind>/<YYYY>/<MM>/<DD>/`. The central hub mirrors
    /// this layout and keeps its copy forever by default.
    pub data_dir: PathBuf,
    /// How often the window watcher polls the foreground window (seconds).
    /// Design intent is 1 s so short interactions stay attributable
    /// (issue #7 wired the default down from the earlier 5).
    pub poll_interval_s: u64,
    /// Input idle time after which the user counts as AFK (minutes).
    pub afk_threshold_min: u64,
    /// Screen frame watcher settings (#5).
    pub screen: ScreenConfig,
    /// Meeting audio watcher settings (#6).
    pub audio: AudioConfig,
    /// Sync / identity settings for the central hub (#18, #17).
    pub sync: SyncConfig,
}

/// Screen frame capture and retention (#5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenConfig {
    /// Seconds between frame captures (ADR: 5-10 s, content-hash deduped).
    pub interval_s: u64,
    /// JPEG quality (1-100) for captured frames. Was hardcoded 70 in
    /// watcher-screen until issue #7 lifted it into the config.
    pub quality: u8,
    /// Rolling retention: oldest-first deletion beyond this age (days).
    pub max_age_days: u64,
    /// Rolling retention: total frame budget on the device (MB).
    pub max_total_mb: u64,
}

/// Meeting audio capture (#6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Global hotkey that starts/stops manual (in-person, mic-only)
    /// recording. Syntax is defined by #6; this is documentation-first.
    pub hotkey: String,
    /// Conferencing process names that may auto-trigger recording when they
    /// hold an active audio session (ADR rev 4 addendum).
    pub process_list: Vec<String>,
    /// Grace period between the conferencing session ending and the auto
    /// recording stopping (seconds). Was hardcoded 10 s in watcher-audio
    /// until issue #7 lifted it into the config.
    pub grace_stop_s: u64,
    /// Rolling retention: oldest-first deletion beyond this age (days).
    pub max_age_days: u64,
    /// Rolling retention: total audio budget on the device (MB).
    pub max_total_mb: u64,
}

/// Sync and identity (#18 sync, #17 auto-update).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Base URL of the WSL central hub over Tailscale (plain HTTP).
    pub server_url: String,
    /// Device name used as identity in the central store.
    pub device_name: String,
    /// Whether the client auto-updates from GitHub Releases (#17): while
    /// `run` is active the daemon checks `releases/latest` every
    /// `update_interval_s` and self-updates (sha256-verified, one `.old`
    /// rollback copy, graceful restart). The manual `update` subcommand
    /// works regardless of this toggle - it only gates the automatic check.
    pub auto_update: bool,
    /// Seconds between automatic update checks (#17). Default 86 400 s =
    /// once a day, well inside the unauthenticated GitHub API rate limit
    /// (60 requests/hour per IP).
    pub update_interval_s: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: PathBuf::from("data"),
            poll_interval_s: 1,
            afk_threshold_min: 5,
            screen: ScreenConfig::default(),
            audio: AudioConfig::default(),
            sync: SyncConfig::default(),
        }
    }
}

impl Default for ScreenConfig {
    fn default() -> Self {
        // ADR rev 2: rolling budget default max 90 days / 20 GB.
        ScreenConfig {
            interval_s: 5,
            quality: 70,
            max_age_days: 90,
            max_total_mb: 20 * 1024,
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        // ADR rev 4: rolling retention default 60 days / 10 GB.
        AudioConfig {
            hotkey: "ctrl+shift+alt+m".to_string(),
            process_list: vec![
                "Teams.exe".to_string(),
                "ms-teams.exe".to_string(),
                "Zoom.exe".to_string(),
                "webex.exe".to_string(),
            ],
            grace_stop_s: 10,
            max_age_days: 60,
            max_total_mb: 10 * 1024,
        }
    }
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            server_url: "http://activity-hub:8080".to_string(),
            device_name: "windows-client".to_string(),
            auto_update: true,
            update_interval_s: 86_400,
        }
    }
}

impl Config {
    /// Parse a config from a TOML string. Missing fields use defaults.
    pub fn from_toml_str(s: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(s)?;
        Ok(cfg)
    }

    /// Load and parse a config file.
    pub fn load(path: &Path) -> Result<Config> {
        let s = std::fs::read_to_string(path)?;
        Config::from_toml_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::CoreError;

    #[test]
    fn partial_toml_fills_defaults() {
        let cfg = Config::from_toml_str(
            "[screen]\ninterval_s = 10\n\n[sync]\nserver_url = \"http://hub:8080\"\ndevice_name = \"laptop\"\n",
        )
        .expect("parse partial config");
        assert_eq!(cfg.screen.interval_s, 10);
        assert_eq!(cfg.screen.max_age_days, 90);
        assert_eq!(cfg.screen.max_total_mb, 20 * 1024);
        assert_eq!(cfg.data_dir, PathBuf::from("data"));
        assert_eq!(cfg.poll_interval_s, 1);
        assert_eq!(cfg.afk_threshold_min, 5);
        assert_eq!(cfg.sync.device_name, "laptop");
        assert!(cfg.sync.auto_update);
        assert_eq!(cfg.screen.quality, 70);
        assert_eq!(cfg.audio.grace_stop_s, 10);
        assert_eq!(cfg.audio.max_age_days, 60);
        assert_eq!(cfg.audio.max_total_mb, 10 * 1024);
    }

    #[test]
    fn new_fields_carry_serde_defaults() {
        // Old config files written before issue #7 lack screen.quality and
        // audio.grace_stop_s; they must still parse with the defaults.
        let cfg = Config::from_toml_str(
            "[screen]\ninterval_s = 5\n\n[audio]\nhotkey = \"ctrl+shift+alt+m\"\n",
        )
        .expect("parse legacy config");
        assert_eq!(cfg.screen.quality, 70);
        assert_eq!(cfg.audio.grace_stop_s, 10);
    }

    #[test]
    fn default_roundtrips_through_toml() {
        let text = toml::to_string(&Config::default()).expect("serialize");
        let parsed = Config::from_toml_str(&text).expect("parse");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn auto_update_fields_default_and_override() {
        // Defaults: auto-update on, checked once a day (#17).
        let cfg = Config::default();
        assert!(cfg.sync.auto_update);
        assert_eq!(cfg.sync.update_interval_s, 86_400);

        // Both fields are overridable, and old config files without them
        // still parse (serde defaults).
        let cfg = Config::from_toml_str(
            "[sync]\nserver_url = \"http://hub:8080\"\ndevice_name = \"laptop\"\nauto_update = false\nupdate_interval_s = 3600\n",
        )
        .expect("parse update config");
        assert!(!cfg.sync.auto_update);
        assert_eq!(cfg.sync.update_interval_s, 3600);
    }

    #[test]
    fn data_dir_can_be_overridden() {
        let cfg = Config::from_toml_str("data_dir = \"D:\\\\archive\"\n").expect("parse");
        assert_eq!(cfg.data_dir, PathBuf::from("D:\\archive"));
    }

    #[test]
    fn invalid_toml_is_a_config_error() {
        let err = Config::from_toml_str("[screen\ninterval_s = ").expect_err("must fail");
        assert!(matches!(err, CoreError::Config(_)));
    }
}
