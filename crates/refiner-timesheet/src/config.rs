//! `[timesheet]` section of the server config file (issue #45).
//!
//! Issue #45 owns the `use_llm_titles` flag and the merge-cap `max_block_min`;
//! the Lemonade connection keys (`lemonade_base_url`, `model`, `api_key`) are
//! shared with the frame-analysis worker whose config section (issue #43) is
//! still in flight - they are parsed defensively here (serde defaults), and
//! the two definitions must be reconciled when #43 lands.
//!
//! The `[timesheet.style]` sub-section is the owner style prompt contract
//! from issue #53: the owner decides HOW timesheet entries are written, the
//! prompt builders inject it after their internal instructions. An empty
//! `style_prompt` means the built-in default (concise professional English,
//! verb-first); an empty `style_examples` list means no few-shot pairs.
//!
//! Partial config files are valid: missing fields fall back to defaults,
//! matching the `[retention]` section's additive rules.

use serde::{Deserialize, Serialize};

/// Built-in style used when the owner's `style_prompt` is empty (issue #53:
/// "concise professional English, verb-first, no personal data beyond what
/// the activity needs").
pub const DEFAULT_STYLE_PROMPT: &str = "Write timesheet entries in concise \
professional English. Start each task with a verb. Keep entries short and \
factual. Do not include personal data beyond what the activity needs.";

/// `{task, comment}` few-shot pair from the owner's style examples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleExample {
    pub task: String,
    pub comment: String,
}

/// `[timesheet.style]`: the owner style prompt (issue #53). Empty fields
/// mean "use the built-in default".
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleConfig {
    /// Free-text style instructions injected after every prompt builder's
    /// internal instructions. Empty = [`DEFAULT_STYLE_PROMPT`].
    pub style_prompt: String,
    /// Few-shot `{task, comment}` pairs appended after the style prompt.
    /// Empty list = none.
    pub style_examples: Vec<StyleExample>,
}

impl StyleConfig {
    /// The effective style prompt: the owner's text when set, the built-in
    /// default otherwise.
    pub fn effective_prompt(&self) -> &str {
        let trimmed = self.style_prompt.trim();
        if trimmed.is_empty() {
            DEFAULT_STYLE_PROMPT
        } else {
            trimmed
        }
    }
}

/// `[timesheet]` section of the server config file (issue #45).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TimesheetConfig {
    /// Issue #45: consult Lemonade once per day to title/comment draft
    /// blocks. Fail-soft: any Lemonade error falls back to heuristic titles.
    pub use_llm_titles: bool,
    /// Base URL of the Lemonade OpenAI-compatible endpoint (shared key,
    /// owned by #43's config section; parsed defensively here). The chat
    /// endpoint is `{base}/chat/completions`.
    pub lemonade_base_url: String,
    /// Lemonade model id (shared key, #43). Empty = omit the field and let
    /// the server use its loaded/pinned model.
    pub model: String,
    /// Lemonade API key (shared key, #43). Empty = no Authorization header
    /// (local Lemonade needs none).
    pub api_key: String,
    /// Adjacent sessions with the same project + category merge up to this
    /// many minutes (issue #45: "configurable max block", default 2 h).
    pub max_block_min: u64,
    /// Owner style prompt (issue #53).
    pub style: StyleConfig,
}

impl Default for TimesheetConfig {
    fn default() -> Self {
        TimesheetConfig {
            use_llm_titles: true,
            lemonade_base_url: "http://127.0.0.1:11434/v1".to_string(),
            model: String::new(),
            api_key: String::new(),
            max_block_min: 120,
            style: StyleConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_issue_45_and_53() {
        let cfg = TimesheetConfig::default();
        assert!(cfg.use_llm_titles);
        assert_eq!(cfg.lemonade_base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(cfg.model, "");
        assert_eq!(cfg.api_key, "");
        assert_eq!(cfg.max_block_min, 120);
        assert_eq!(cfg.style, StyleConfig::default());
    }

    #[test]
    fn empty_style_prompt_resolves_to_builtin_default() {
        let style = StyleConfig::default();
        assert_eq!(style.effective_prompt(), DEFAULT_STYLE_PROMPT);
        // whitespace-only counts as empty
        let style = StyleConfig {
            style_prompt: "   ".to_string(),
            style_examples: vec![],
        };
        assert_eq!(style.effective_prompt(), DEFAULT_STYLE_PROMPT);
        // a custom prompt wins
        let style = StyleConfig {
            style_prompt: "German, telegraphic.".to_string(),
            style_examples: vec![],
        };
        assert_eq!(style.effective_prompt(), "German, telegraphic.");
    }

    #[test]
    fn toml_section_parses_with_defaults_for_missing_fields() {
        // Only #45's own key, nothing else: every other field defaults.
        let cfg = parse_timesheet_section("[timesheet]\nuse_llm_titles = false\n");
        assert!(!cfg.use_llm_titles);
        assert_eq!(cfg.lemonade_base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(cfg.max_block_min, 120);

        // Full section incl. the style sub-section (real config spelling:
        // `[timesheet.style]` under the `[timesheet]` table).
        let text = "\
[timesheet]
lemonade_base_url = \"http://lemonade:11434/v1\"
model = \"Qwen3.6-35B-A3B-MTP-GGUF\"
api_key = \"secret\"
max_block_min = 90
[timesheet.style]
style_prompt = \"Bullet style, no prose.\"
[[timesheet.style.style_examples]]
task = \"Refine cube rules\"
comment = \"TM1 rules work in the Admin hub.\"
[[timesheet.style.style_examples]]
task = \"Run support triage\"
comment = \"Cleared the inbound support queue.\"
";
        let cfg = parse_timesheet_section(text);
        assert_eq!(cfg.lemonade_base_url, "http://lemonade:11434/v1");
        assert_eq!(cfg.model, "Qwen3.6-35B-A3B-MTP-GGUF");
        assert_eq!(cfg.api_key, "secret");
        assert_eq!(cfg.max_block_min, 90);
        assert_eq!(cfg.style.effective_prompt(), "Bullet style, no prose.");
        assert_eq!(cfg.style.style_examples.len(), 2);
        assert_eq!(cfg.style.style_examples[0].task, "Refine cube rules");
        assert_eq!(
            cfg.style.style_examples[1].comment,
            "Cleared the inbound support queue."
        );
    }

    #[test]
    fn parses_inside_a_full_server_config_file() {
        // The real shape: the [timesheet] section sits beside other sections
        // in the server config file; unknown keys are ignored (additive).
        let text = "[retention]\nenabled = false\n\n[timesheet]\nuse_llm_titles = true\nsome_future_key = 1\n";
        let value: toml::Value = toml::from_str(text).expect("full file parses");
        let section = value.get("timesheet").expect("timesheet section");
        let cfg: TimesheetConfig = section.clone().try_into().expect("section deserializes");
        assert!(cfg.use_llm_titles);
        assert_eq!(cfg.style.effective_prompt(), DEFAULT_STYLE_PROMPT);
    }

    /// Helper mirroring how the server config file feeds this section:
    /// parse a full `[timesheet]` section text in isolation.
    fn parse_timesheet_section(text: &str) -> TimesheetConfig {
        let value: toml::Value = toml::from_str(text).expect("toml parses");
        value
            .get("timesheet")
            .expect("section present")
            .clone()
            .try_into()
            .expect("section deserializes")
    }
}
