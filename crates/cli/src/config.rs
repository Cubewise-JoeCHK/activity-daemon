//! Config resolution for the CLI (issue #7).
//!
//! The config file lives at `%APPDATA%\activity-daemon\config.toml` on
//! Windows (`$XDG_CONFIG_HOME/activity-daemon/config.toml` or
//! `~/.config/activity-daemon/config.toml` elsewhere), overridable with
//! `--config <path>`. A missing file is not an error: the CLI writes a fully
//! commented template (every field, its default) and runs with defaults,
//! with `data_dir` anchored at `<config dir>/data` so a stock install keeps
//! database, media and logs together under the app-data folder.

use std::path::{Path, PathBuf};

use activity_core::Config;
use anyhow::{Context, Result};

/// A resolved configuration: where it came from and what it says.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The config file path (default location or the `--config` override).
    pub path: PathBuf,
    /// The parsed configuration. When the file was just created this is the
    /// default config with `data_dir` set to `<config dir>/data`.
    pub config: Config,
    /// True when the file did not exist and a template was written.
    pub created: bool,
}

/// Directory holding the default config and data: `%APPDATA%\activity-daemon`
/// on Windows, `$XDG_CONFIG_HOME/activity-daemon` or
/// `~/.config/activity-daemon` elsewhere. Falls back to a relative
/// `activity-daemon` directory when no user-home environment is set.
pub fn config_root() -> PathBuf {
    #[cfg(windows)]
    {
        match std::env::var_os("APPDATA") {
            Some(appdata) => PathBuf::from(appdata).join("activity-daemon"),
            None => PathBuf::from("activity-daemon"),
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join("activity-daemon");
            }
        }
        match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => {
                PathBuf::from(home).join(".config").join("activity-daemon")
            }
            _ => PathBuf::from("activity-daemon"),
        }
    }
}

/// Default config file path (see [`config_root`]).
pub fn default_config_path() -> PathBuf {
    config_root().join("config.toml")
}

/// Load the config, or create a commented template on first use.
pub fn resolve(config_override: Option<&Path>) -> Result<Resolved> {
    let path = config_override
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);
    if path.exists() {
        let config = Config::load(&path)
            .with_context(|| format!("cannot load config file {}", path.display()))?;
        return Ok(Resolved {
            path,
            config,
            created: false,
        });
    }

    // First use: write a template next to the config, anchor the data dir
    // there too, and run on defaults.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("cannot create config directory {}", parent.display()))?;
    let config = Config {
        data_dir: parent.join("data"),
        ..Config::default()
    };
    std::fs::write(&path, render_template(&config.data_dir))
        .with_context(|| format!("cannot write config template {}", path.display()))?;
    Ok(Resolved {
        path,
        config,
        created: true,
    })
}

/// The commented config template with every field at its default; `data_dir`
/// is the one active line because the CLI picks it per machine.
///
/// The golden test in this module parses the output and asserts it equals
/// [`Config::default`] (with the passed `data_dir`), so the template cannot
/// drift from the core config silently.
pub fn render_template(data_dir: &Path) -> String {
    let cfg = Config::default();
    let data_dir = data_dir.to_string_lossy();
    let processes = cfg
        .audio
        .process_list
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"# activity-daemon configuration (issue #7 template).
# Every field is listed at its default; delete a line to fall back to it.
# Partial files are valid: missing fields use the defaults shown here.
#
# Root directory of the local archive: activity.db, media/, logs/.
# Absolute paths are recommended (the scheduled task runs in this directory).
data_dir = '{data_dir}'
# How often the foreground window is polled (seconds; design intent 1 s).
poll_interval_s = {poll}
# Input idle minutes after which you count as AFK.
afk_threshold_min = {afk}

[screen]
# Seconds between frame captures (content-hash deduped; paused while AFK).
interval_s = {screen_interval}
# JPEG quality 1-100 for captured frames.
quality = {quality}
# Rolling retention: drop frames older than this (days).
max_age_days = {screen_days}
# Rolling retention: total frame budget (MB).
max_total_mb = {screen_mb}

[audio]
# Global hotkey that toggles manual (mic-only) recording.
hotkey = "{hotkey}"
# Conferencing apps that may auto-trigger recording (mic + loopback) while
# they hold an active audio session.
process_list = [{processes}]
# Seconds a meeting keeps recording after the conferencing session ends.
grace_stop_s = {grace}
# Rolling retention: drop audio older than this (days).
max_age_days = {audio_days}
# Rolling retention: total audio budget (MB).
max_total_mb = {audio_mb}

[sync]
# Central hub over Tailscale (see docs/sync.md for the server side).
server_url = "{server_url}"
# Device identity in the central store.
device_name = "{device}"
# Auto-update from GitHub Releases (#17): sha256-verified download, one
# <exe>.old rollback copy, graceful restart. The manual `update` subcommand
# works even when this is off (it only gates the automatic check).
auto_update = {auto_update}
# Seconds between automatic update checks (86400 = daily; the unauthenticated
# GitHub API allows 60 requests/hour).
update_interval_s = {update_interval}
"#,
        data_dir = data_dir,
        poll = cfg.poll_interval_s,
        afk = cfg.afk_threshold_min,
        screen_interval = cfg.screen.interval_s,
        quality = cfg.screen.quality,
        screen_days = cfg.screen.max_age_days,
        screen_mb = cfg.screen.max_total_mb,
        hotkey = cfg.audio.hotkey,
        processes = processes,
        grace = cfg.audio.grace_stop_s,
        audio_days = cfg.audio.max_age_days,
        audio_mb = cfg.audio.max_total_mb,
        server_url = cfg.sync.server_url,
        device = cfg.sync.device_name,
        auto_update = cfg.sync.auto_update,
        update_interval = cfg.sync.update_interval_s,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_path_lands_in_app_config_dir() {
        let path = default_config_path();
        assert!(path.ends_with("config.toml"), "{}", path.display());
        let parent = path.parent().expect("parent").display().to_string();
        assert!(
            parent.contains("activity-daemon"),
            "config dir is activity-daemon specific: {parent}"
        );
    }

    #[test]
    fn template_parses_to_default_config_with_overridden_data_dir() {
        let dir = std::env::temp_dir().join(format!(
            "activity-cli-tpl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let data_dir = dir.join("data");
        let text = render_template(&data_dir);
        let parsed = Config::from_toml_str(&text).expect("the template must parse as core config");
        assert_eq!(
            parsed,
            Config {
                data_dir: data_dir.clone(),
                ..Config::default()
            }
        );
        // And every documented field really is mentioned.
        for field in [
            "data_dir",
            "poll_interval_s",
            "afk_threshold_min",
            "interval_s",
            "quality",
            "grace_stop_s",
            "hotkey",
            "process_list",
            "max_age_days",
            "max_total_mb",
            "server_url",
            "device_name",
            "auto_update",
            "update_interval_s",
        ] {
            assert!(text.contains(field), "template mentions {field}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_creates_template_and_defaults_on_first_use() {
        let dir = std::env::temp_dir().join(format!(
            "activity-cli-res-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let cfg_path = dir.join("config.toml");
        assert!(!cfg_path.exists());

        let first = resolve(Some(&cfg_path)).expect("resolve creates template");
        assert!(first.created);
        assert!(cfg_path.exists(), "template written");
        assert_eq!(first.config.data_dir, dir.join("data"));
        assert_eq!(first.config.poll_interval_s, 1);
        assert_eq!(first.config.screen.quality, 70);
        assert_eq!(first.config.audio.grace_stop_s, 10);

        // Second resolve loads the file; created stays false, values persist.
        let second = resolve(Some(&cfg_path)).expect("resolve loads file");
        assert!(!second.created);
        assert_eq!(second.config, first.config);

        // An override pointing at an existing file wins.
        let other = dir.join("other.toml");
        std::fs::write(&other, "poll_interval_s = 3\n").expect("write other");
        let third = resolve(Some(&other)).expect("resolve override");
        assert!(!third.created);
        assert_eq!(third.config.poll_interval_s, 3);
        assert_eq!(third.config.data_dir, PathBuf::from("data"));

        // Broken config is a hard error, not silent defaults.
        std::fs::write(&other, "[screen\nbroken = ").expect("write broken");
        assert!(resolve(Some(&other)).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
