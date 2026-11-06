//! Lemonade titling for the daily itinerary compiler (issue #45): one
//! OpenAI-compatible chat completion per day titles/comments the merge
//! blocks, honoring the owner style prompt (issue #53 injection contract:
//! owner style + few-shot examples are appended AFTER the builder's
//! internal instructions; empty style means the built-in default).
//!
//! Fail-soft by design: the real HTTP client only runs when
//! `use_llm_titles` is on, and any failure (connection, timeout, bad JSON,
//! non-2xx) falls back to the deterministic heuristic titles. The pure
//! parts (prompt assembly, request body, response parsing) are unit-tested;
//! the network call itself is thin glue around them.

use crate::compiler::{heuristic_title, BlockTitle, BlockTitler, MergedBlock};
use crate::config::{StyleConfig, TimesheetConfig};
use serde_json::{json, Value};

/// The compiler's internal instructions - everything the model needs to
/// produce one task/comment per block. The owner style section is appended
/// after this (issue #53: "appended AFTER the builder's internal
/// instructions").
const INTERNAL_INSTRUCTIONS: &str = "\
You write timesheet entries for draft activities. Each block below is one \
entry: write a short `task` title and a 1-2 sentence factual `comment` \
summarising the evidence. Do not invent tools, projects or events that the \
block does not mention.\n\n\
Blocks (one JSON object per index):\n";

/// Output-format instructions (part of the builder's internals, before the
/// owner style section).
const OUTPUT_INSTRUCTIONS: &str = "\nRespond with ONLY a JSON array like \
[{\"index\": 0, \"task\": \"...\", \"comment\": \"...\"}] - one object per \
block, same `index` as the block list, no markdown fences, no prose.\n";

/// Total request timeout for the single daily Lemonade call. The owner's
/// local GPU endpoint can take a while on larger models; fail-soft keeps
/// even a hang bounded, and the compiler regenerates on demand.
const REQUEST_TIMEOUT_SECS: u64 = 120;
/// Connect timeout: Lemonade runs on the owner's machine (localhost or
/// tailnet), so anything slower than this means it is not running at all.
const CONNECT_TIMEOUT_SECS: u64 = 5;

/// Assemble the full system prompt: internal instructions, the block list,
/// the output format, then the owner style section (issue #53 injection
/// contract). Empty style config -> the built-in default style text.
pub fn assemble_prompt(blocks: &[MergedBlock], style: &StyleConfig) -> String {
    let mut prompt = String::from(INTERNAL_INSTRUCTIONS);
    for (index, block) in blocks.iter().enumerate() {
        prompt.push_str(&format!(
            "[{index}] {} - {} | project={} category={} apps={} frames={} sessions={} confidence={:.2}",
            block.start,
            block.end,
            block.project_guess,
            block.category,
            if block.apps.is_empty() { "-".to_string() } else { block.apps.join(", ") },
            block.frame_count,
            block.session_count,
            block.confidence,
        ));
        if !block.activity.trim().is_empty() {
            prompt.push_str(&format!(" | observed=\"{}\"", block.activity.trim()));
        }
        prompt.push('\n');
    }
    prompt.push_str(OUTPUT_INSTRUCTIONS);
    // Issue #53: the owner's style is a first-class setting, appended after
    // the builder's internal instructions; empty means the built-in default.
    prompt.push_str("\n## Owner style (binding)\n");
    prompt.push_str(style.effective_prompt());
    prompt.push('\n');
    if !style.style_examples.is_empty() {
        prompt.push_str("\n## Style examples\n");
        for example in &style.style_examples {
            prompt.push_str(&format!(
                "- task: {}\n  comment: {}\n",
                example.task, example.comment
            ));
        }
    }
    prompt
}

/// Build the chat-completion request body. `model` empty -> omit the field
/// (Lemonade then uses its loaded/pinned model).
pub fn build_request_body(prompt: &str, model: &str) -> Value {
    let mut body = json!({
        "temperature": 0,
        "messages": [{"role": "system", "content": prompt}],
    });
    if !model.trim().is_empty() {
        body["model"] = json!(model.trim());
    }
    body
}

/// Parse the model's completion content into `(index, task, comment)`
/// triples. Lenient by design: markdown fences and stray prose around the
/// JSON array are tolerated; anything unparsable returns `None` (the
/// caller falls back to heuristics).
pub fn parse_titles(content: &str) -> Option<Vec<(usize, String, String)>> {
    let text = content.trim();
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end < start {
        return None;
    }
    let parsed: Vec<Value> = serde_json::from_str(&text[start..=end]).ok()?;
    let mut out = Vec::with_capacity(parsed.len());
    for item in parsed {
        let index = item.get("index")?.as_u64()? as usize;
        let task = item.get("task")?.as_str()?.trim().to_string();
        let comment = item.get("comment")?.as_str()?.trim().to_string();
        out.push((index, task, comment));
    }
    Some(out)
}

/// The real titler: one Lemonade call per day, heuristic fallback for any
/// block the model did not deliver a usable title for.
pub struct LemonadeTitler {
    agent: ureq::Agent,
    base_url: String,
    model: String,
    api_key: String,
}

impl LemonadeTitler {
    /// Build from the `[timesheet]` config section.
    pub fn from_config(cfg: &TimesheetConfig) -> LemonadeTitler {
        LemonadeTitler {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
                .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .build(),
            base_url: cfg.lemonade_base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            api_key: cfg.api_key.clone(),
        }
    }

    /// One chat completion for the whole day's blocks. `Err` carries a
    /// short reason (for logging); the caller falls back to heuristics.
    fn call(&self, blocks: &[MergedBlock], style: &StyleConfig) -> Result<Vec<BlockTitle>, String> {
        let prompt = assemble_prompt(blocks, style);
        let body = build_request_body(&prompt, &self.model);
        let mut request = self
            .agent
            .post(&format!("{}/chat/completions", self.base_url))
            .set("Content-Type", "application/json");
        if !self.api_key.is_empty() {
            request = request.set("Authorization", &format!("Bearer {}", self.api_key));
        }
        let response = request
            .send_json(&body)
            .map_err(|e| format!("lemonade request failed: {e}"))?;
        let payload: Value = response
            .into_json()
            .map_err(|e| format!("lemonade response is not JSON: {e}"))?;
        let content = payload["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| "lemonade response has no completion content".to_string())?;
        let parsed = parse_titles(content)
            .ok_or_else(|| "lemonade completion is not a JSON title array".to_string())?;
        let mut titles = vec![None; blocks.len()];
        for (index, task, comment) in parsed {
            if index < titles.len() {
                titles[index] = Some(BlockTitle { task, comment });
            }
        }
        Ok(titles
            .into_iter()
            .zip(blocks.iter())
            .map(|(title, block)| title.unwrap_or_else(|| heuristic_title(block)))
            .collect())
    }
}

impl BlockTitler for LemonadeTitler {
    fn title_blocks(&self, blocks: &[MergedBlock], style: &StyleConfig) -> Vec<BlockTitle> {
        if blocks.is_empty() {
            return Vec::new();
        }
        match self.call(blocks, style) {
            Ok(titles) => titles,
            Err(_) => blocks.iter().map(heuristic_title).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::HeuristicTitler;

    fn block(activity: &str, app: &str) -> MergedBlock {
        MergedBlock {
            start: "2026-11-06T09:00:00+08:00".to_string(),
            end: "2026-11-06T11:00:00+08:00".to_string(),
            project_guess: "C1234".to_string(),
            category: "Consulting".to_string(),
            activity: activity.to_string(),
            apps: vec![app.to_string()],
            session_count: 2,
            frame_count: 12,
            confidence: 0.91,
        }
    }

    #[test]
    fn prompt_contains_blocks_then_output_format_then_style() {
        let blocks = vec![block("editing rust sources", "Code.exe")];
        let prompt = assemble_prompt(&blocks, &StyleConfig::default());
        // internal instructions first
        assert!(prompt.starts_with("You write timesheet entries"));
        // block list with the observed activity
        assert!(prompt.contains("[0] 2026-11-06T09:00:00+08:00 - 2026-11-06T11:00:00+08:00"));
        assert!(prompt.contains("project=C1234 category=Consulting"));
        assert!(prompt.contains("observed=\"editing rust sources\""));
        // output format before the style section
        assert!(
            prompt.find("Respond with ONLY a JSON array").unwrap()
                < prompt.find("## Owner style").unwrap()
        );
        // empty style = built-in default text (issue #53)
        assert!(prompt.contains(crate::config::DEFAULT_STYLE_PROMPT));
    }

    #[test]
    fn custom_style_changes_the_assembled_prompt() {
        // Issue #53 acceptance criterion: a custom style actually changes
        // the assembled prompt, and the examples land after it.
        let style = StyleConfig {
            style_prompt: "German, telegraphic. No articles.".to_string(),
            style_examples: vec![crate::config::StyleExample {
                task: "Cube rules verfeinern".to_string(),
                comment: "Regeln im Admin-Hub angepasst.".to_string(),
            }],
        };
        let default_prompt =
            assemble_prompt(&[block("editing", "Code.exe")], &StyleConfig::default());
        let custom_prompt = assemble_prompt(&[block("editing", "Code.exe")], &style);
        assert!(custom_prompt.contains("German, telegraphic. No articles."));
        assert!(!custom_prompt.contains(crate::config::DEFAULT_STYLE_PROMPT));
        assert!(default_prompt != custom_prompt);
        assert!(custom_prompt.contains("task: Cube rules verfeinern"));
        // examples are appended after the style prompt
        assert!(
            custom_prompt
                .find("German, telegraphic. No articles.")
                .unwrap()
                < custom_prompt.find("task: Cube rules verfeinern").unwrap()
        );
    }

    #[test]
    fn request_body_carries_model_only_when_set() {
        let body = build_request_body("prompt", "");
        assert!(body.get("model").is_none());
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "prompt");

        let body = build_request_body("prompt", " Qwen3.6-35B ");
        assert_eq!(body["model"], "Qwen3.6-35B");
    }

    #[test]
    fn parse_titles_handles_clean_fenced_and_noisy_content() {
        let parsed = parse_titles(r#"[{"index": 0, "task": "T", "comment": "C"}]"#);
        assert_eq!(parsed, Some(vec![(0, "T".to_string(), "C".to_string())]));

        let fenced = "```json\n[{\"index\": 1, \"task\": \" B \", \"comment\": \"C\"}]\n```";
        assert_eq!(
            parse_titles(fenced),
            Some(vec![(1, "B".to_string(), "C".to_string())])
        );

        let noisy = "Sure! Here are the titles:\n[{\"index\": 0, \"task\": \"T\", \"comment\": \"C\"}]\nDone.";
        assert!(parse_titles(noisy).is_some());

        assert_eq!(parse_titles("no json at all"), None);
        assert_eq!(parse_titles("]reversed["), None);
        assert_eq!(parse_titles("[{\"task\": \"missing index\"}]"), None);
        assert_eq!(
            parse_titles("[{\"index\": 0, \"task\": 3, \"comment\": \"C\"}]"),
            None
        );
    }

    #[test]
    fn heuristic_fallback_shapes_block_views() {
        let title = heuristic_title(&block("editing rust sources", "Code.exe"));
        assert_eq!(title.task, "Editing rust sources");
        assert_eq!(
            title.comment,
            "2 window session(s) (Code.exe); 12 frame(s) analysed; confidence 0.91"
        );

        // task truncation at a word boundary
        let long = block(
            "write a very long analysis sentence that keeps going and going and going \
             until it exceeds the eighty character task budget for sure and beyond",
            "App",
        );
        let title = heuristic_title(&long);
        assert!(title.task.len() <= 83, "task = {}", title.task);
        assert!(title.task.ends_with("..."));

        // no observed activity: app-based task
        let title = heuristic_title(&block("", "Excel.exe"));
        assert_eq!(title.task, "Work in Excel.exe");

        // the heuristic titler implements the same contract
        let blocks = vec![block("editing", "Code.exe"), block("", "Excel.exe")];
        let titles = HeuristicTitler.title_blocks(&blocks, &StyleConfig::default());
        assert_eq!(titles.len(), 2);
    }
}
