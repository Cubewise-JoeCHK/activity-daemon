//! `[timesheet]` configuration section for the hub server (issue #43, ADR
//! addendum on #16).
//!
//! The server binary resolves the config file (default
//! `$ACTIVITY_DATA/config.toml`, `ACTIVITY_CONFIG` overrides) and hands the
//! file text to [`TimesheetConfig::from_toml_str`]; only the `[timesheet]`
//! section is interpreted here - every other section in the file is
//! ignored, so the file can grow other hub settings without touching this
//! crate. Missing fields (or a missing file) fall back to the documented
//! defaults, matching the partial-config convention of core's `Config`.
//! A file that exists but does not parse is a deployment error (the server
//! refuses to start) - a typo must not silently run with different
//! settings than the owner wrote down.
//!
//! Style ([timesheet.style], issue #53): the owner decides HOW generated
//! timesheet text is written. `style_prompt` is free text injected into
//! every prompt after the builder's internal instructions; `style_examples`
//! are few-shot `{task, comment}` pairs. Empty means the documented
//! built-in default. The GET/PUT API and config write-back live in #53 -
//! this crate only reads the section.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_LEMONADE_BASE_URL: &str = "http://127.0.0.1:11434/v1";
pub const DEFAULT_LEMONADE_MODEL: &str = "Qwen3.6-35B-A3B-MTP-GGUF";
/// `device` written on every `frame-analysis` event (the hub's own name in
/// the central store).
pub const DEFAULT_DEVICE_NAME: &str = "activity-hub";

/// Built-in writing style applied when the owner has not configured
/// `[timesheet.style]` (issue #53: concise professional English, verb-first,
/// no personal data beyond what the activity needs).
pub const DEFAULT_STYLE_PROMPT: &str =
    "Concise professional English, verb-first, no personal data beyond what the activity needs.";

/// Top-level `[timesheet]` section (issue #43).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TimesheetConfig {
    /// Worker on/off. Default on per the ADR (event-driven frame analysis):
    /// while Lemonade is unreachable the worker just backs off (fail-soft).
    pub enabled: bool,
    /// Base URL of the Lemonade server's OpenAI-compatible API. From the
    /// hub container this is the Windows host's tailnet address (wired in
    /// deployment, issue #49); the default targets a same-host Lemonade.
    pub lemonade_base_url: String,
    /// Vision-capable model id served by Lemonade.
    pub lemonade_model: String,
    /// Optional bearer token; empty string sends no Authorization header.
    pub lemonade_api_key: String,
    /// Sampling policy: `keyframe` (default; one frame per window-session
    /// change) or `all` (every arrival, cap still applies).
    pub sampling_mode: SamplingMode,
    /// Per-clock-hour cap on analyses (frames of the same wall-clock hour,
    /// in the frame's own timezone offset). 0 disables analysis entirely.
    pub max_per_hour: u32,
    /// Answers below this confidence go to the question queue hand-off
    /// (issue #44; logged until it lands).
    pub confidence_threshold: f64,
    /// Seconds between arrival polls of the store.
    pub poll_interval_s: u64,
    /// Per-request timeout for Lemonade chat completions (seconds).
    pub request_timeout_s: u64,
    /// First retry delay after a failed analysis (seconds; doubles per
    /// consecutive failure up to `backoff_max_s`).
    pub backoff_initial_s: u64,
    /// Retry delay ceiling (seconds).
    pub backoff_max_s: u64,
    /// Device name stamped on `frame-analysis` events.
    pub device_name: String,
    /// Optional path overriding the embedded template vocabulary seed.
    pub vocab_path: Option<PathBuf>,
    /// Owner writing style (issue #53).
    pub style: StyleConfig,
}

/// Sampling policy selector. Lowercase serde spelling matches the config
/// file: `"keyframe" | "all"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SamplingMode {
    /// One frame per window-session change (default), capped per hour.
    #[default]
    Keyframe,
    /// Every arrival, capped per hour.
    All,
}

/// Owner writing style (issue #53).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleConfig {
    /// Free-text style instructions; empty uses [`DEFAULT_STYLE_PROMPT`].
    pub style_prompt: String,
    /// Few-shot `{task, comment}` pairs demonstrating the desired phrasing.
    pub style_examples: Vec<StyleExample>,
}

/// One few-shot style example (issue #53).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StyleExample {
    pub task: String,
    pub comment: String,
}

impl Default for TimesheetConfig {
    fn default() -> Self {
        TimesheetConfig {
            enabled: true,
            lemonade_base_url: DEFAULT_LEMONADE_BASE_URL.to_string(),
            lemonade_model: DEFAULT_LEMONADE_MODEL.to_string(),
            lemonade_api_key: String::new(),
            sampling_mode: SamplingMode::Keyframe,
            max_per_hour: 8,
            confidence_threshold: 0.5,
            poll_interval_s: 5,
            request_timeout_s: 120,
            backoff_initial_s: 5,
            backoff_max_s: 300,
            device_name: DEFAULT_DEVICE_NAME.to_string(),
            vocab_path: None,
            style: StyleConfig::default(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    timesheet: TimesheetConfig,
}

impl TimesheetConfig {
    /// Parse the `[timesheet]` section out of a TOML string. Other sections
    /// are ignored; a missing `[timesheet]` section yields the defaults.
    pub fn from_toml_str(s: &str) -> Result<TimesheetConfig, String> {
        let file: ConfigFile = toml::from_str(s).map_err(|e| format!("config parse error: {e}"))?;
        Ok(file.timesheet)
    }

    /// Load the `[timesheet]` section from a config file.
    pub fn load_file(path: &Path) -> Result<TimesheetConfig, String> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
        TimesheetConfig::from_toml_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_issue_43_contract() {
        let cfg = TimesheetConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.lemonade_base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(cfg.lemonade_model, "Qwen3.6-35B-A3B-MTP-GGUF");
        assert_eq!(cfg.lemonade_api_key, "");
        assert_eq!(cfg.sampling_mode, SamplingMode::Keyframe);
        assert_eq!(cfg.max_per_hour, 8);
        assert_eq!(cfg.confidence_threshold, 0.5);
        assert_eq!(cfg.poll_interval_s, 5);
        assert_eq!(cfg.request_timeout_s, 120);
        assert_eq!(cfg.backoff_initial_s, 5);
        assert_eq!(cfg.backoff_max_s, 300);
        assert_eq!(cfg.device_name, "activity-hub");
        assert_eq!(cfg.vocab_path, None);
        assert_eq!(cfg.style, StyleConfig::default());
    }

    #[test]
    fn partial_section_fills_defaults_and_ignores_other_sections() {
        let cfg = TimesheetConfig::from_toml_str(
            "[timesheet]\nmax_per_hour = 3\ndevice_name = \"hub-x\"\n\n\
             [timesheet.style]\nstyle_prompt = \"Match the owner's tone.\"\n\n\
             [screen]\ninterval_s = 9\n",
        )
        .expect("parse partial config");
        assert_eq!(cfg.max_per_hour, 3);
        assert_eq!(cfg.device_name, "hub-x");
        assert_eq!(cfg.style.style_prompt, "Match the owner's tone.");
        // untouched fields keep defaults, unrelated sections are ignored
        assert!(cfg.enabled);
        assert_eq!(cfg.lemonade_base_url, DEFAULT_LEMONADE_BASE_URL);
    }

    #[test]
    fn missing_section_yields_defaults() {
        let cfg = TimesheetConfig::from_toml_str("[screen]\ninterval_s = 9\n")
            .expect("config without [timesheet]");
        assert_eq!(cfg, TimesheetConfig::default());
    }

    #[test]
    fn style_examples_roundtrip() {
        let cfg = TimesheetConfig::from_toml_str(
            "[timesheet.style]\nstyle_prompt = \"Short present-tense sentences.\"\n\n\
             [[timesheet.style.style_examples]]\ntask = \"Debugged TM1 rule errors\"\n\
             comment = \"Fixed 3 rules in the P&L cube\"\n\n\
             [[timesheet.style.style_examples]]\ntask = \"Prepared client demo\"\ncomment = \"\"\n",
        )
        .expect("parse style config");
        assert_eq!(cfg.style.style_examples.len(), 2);
        assert_eq!(cfg.style.style_examples[0].task, "Debugged TM1 rule errors");
        assert_eq!(
            cfg.style.style_examples[0].comment,
            "Fixed 3 rules in the P&L cube"
        );
        assert_eq!(cfg.style.style_examples[1].comment, "");
    }

    #[test]
    fn sampling_mode_typo_is_rejected() {
        let err = TimesheetConfig::from_toml_str("[timesheet]\nsampling_mode = \"sometimes\"\n")
            .expect_err("unknown sampling mode must be a config error");
        assert!(err.contains("sampling_mode") || err.contains("unknown variant"));
    }
}
