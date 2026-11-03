//! Sync settings: hub URL, device identity and pacing. `interval_s` and
//! `batch_size` live here (core's `SyncConfig` currently carries only the
//! URL and device name); `SyncSettings::from_config` fills them with
//! defaults so the daemon (issue #7) can wire this straight from
//! `Config::sync`.

use std::time::Duration;

use activity_core::Config;

/// Forward at most every 30 seconds by default.
pub const DEFAULT_INTERVAL_S: u64 = 30;
/// Post at most 500 events per batch by default.
pub const DEFAULT_BATCH_SIZE: u32 = 500;

/// Everything the forwarder needs besides the database handle.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncSettings {
    /// Base URL of the central hub over Tailscale
    /// (e.g. `http://activity-hub:8080`).
    pub server_url: String,
    /// Device name used as identity in the central store (from
    /// `Config::sync.device_name`).
    pub device_name: String,
    /// Seconds between forwarder rounds.
    pub interval_s: u64,
    /// Maximum number of unsynced events fetched per round.
    pub batch_size: u32,
}

impl SyncSettings {
    /// Settings pointing at `server_url` as `device_name`, otherwise
    /// defaults.
    pub fn new(server_url: &str, device_name: &str) -> SyncSettings {
        SyncSettings {
            server_url: server_url.to_string(),
            device_name: device_name.to_string(),
            interval_s: DEFAULT_INTERVAL_S,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    /// Derive settings from the daemon config: `sync.server_url` and
    /// `sync.device_name` come through, pacing uses the defaults.
    pub fn from_config(cfg: &Config) -> SyncSettings {
        SyncSettings {
            server_url: cfg.sync.server_url.clone(),
            device_name: cfg.sync.device_name.clone(),
            interval_s: DEFAULT_INTERVAL_S,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    /// Interval as a `Duration`, at least one second.
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_s.max(1))
    }
}

impl Default for SyncSettings {
    fn default() -> Self {
        SyncSettings {
            server_url: "http://activity-hub:8080".to_string(),
            device_name: "local".to_string(),
            interval_s: DEFAULT_INTERVAL_S,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let s = SyncSettings::default();
        assert_eq!(s.interval_s, 30);
        assert_eq!(s.batch_size, 500);
        assert_eq!(s.interval(), Duration::from_secs(30));
        assert_eq!(s.device_name, "local");
    }

    #[test]
    fn from_config_pulls_sync_section() {
        let cfg = Config::from_toml_str(
            "[sync]\nserver_url = \"http://hub:9999\"\ndevice_name = \"laptop-x\"\n",
        )
        .expect("config");
        let s = SyncSettings::from_config(&cfg);
        assert_eq!(s.server_url, "http://hub:9999");
        assert_eq!(s.device_name, "laptop-x");
        // pacing comes from defaults, not the config file
        assert_eq!(s.interval_s, DEFAULT_INTERVAL_S);
        assert_eq!(s.batch_size, DEFAULT_BATCH_SIZE);
    }

    #[test]
    fn interval_floors_at_one_second() {
        let s = SyncSettings {
            interval_s: 0,
            ..SyncSettings::default()
        };
        assert_eq!(s.interval(), Duration::from_secs(1));
    }
}
