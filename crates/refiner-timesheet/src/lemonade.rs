//! Lemonade client (issue #43): OpenAI-compatible chat completions with a
//! vision-capable model (default `Qwen3.6-35B-A3B-MTP-GGUF` + mmproj,
//! images as base64 data URLs in message content parts), running on the
//! owner's machine and reached from the hub over the tailnet.
//!
//! The prompt demands one strict JSON object
//! `{activity, project_guess, category, confidence, evidence}`; the reply is
//! parsed defensively (models still wrap JSON in markdown fences sometimes)
//! and validated against the template vocabulary ([`crate::vocab`]):
//! out-of-vocabulary values are stored empty and zero the confidence so the
//! low-confidence hand-off catches them.

use std::time::Duration;

use base64::Engine as _;
use serde_json::json;

use crate::config::{StyleConfig, TimesheetConfig, DEFAULT_STYLE_PROMPT};
use crate::vocab::CubewiseVocab;

/// A validated model answer.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FrameAnalysis {
    /// One short sentence, present tense.
    pub activity: String,
    /// Template project string, empty when none identifiable / out of vocab.
    pub project_guess: String,
    /// Template category, empty when out of vocabulary.
    pub category: String,
    /// Model confidence clamped to 0.0..=1.0; 0.0 for unusable answers.
    pub confidence: f64,
    /// What the model saw that justifies the answer.
    pub evidence: String,
}

#[derive(Debug)]
pub enum LemonadeError {
    /// Transport failure, timeout, or non-2xx status.
    Http(String),
    /// 2xx but the body held no usable answer.
    Response(String),
}

impl std::fmt::Display for LemonadeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LemonadeError::Http(msg) => write!(f, "lemonade request failed: {msg}"),
            LemonadeError::Response(msg) => write!(f, "lemonade reply unusable: {msg}"),
        }
    }
}

impl std::error::Error for LemonadeError {}

/// Blocking OpenAI-compatible client (call from `spawn_blocking`).
#[derive(Debug, Clone)]
pub struct LemonadeClient {
    agent: ureq::Agent,
    base_url: String,
    model: String,
    api_key: String,
}

impl LemonadeClient {
    pub fn new(cfg: &TimesheetConfig) -> LemonadeClient {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(cfg.request_timeout_s.max(1)))
            .build();
        LemonadeClient {
            agent,
            base_url: cfg
                .lemonade_base_url
                .trim()
                .trim_end_matches('/')
                .to_string(),
            model: cfg.lemonade_model.trim().to_string(),
            api_key: cfg.lemonade_api_key.trim().to_string(),
        }
    }

    /// Model id stamped into `frame-analysis` events.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Analyse one JPEG frame: build the strict-JSON prompt (internal
    /// instructions, then the owner style block, issue #53), POST it, parse
    /// and vocabulary-validate the reply.
    pub fn analyze_jpeg(
        &self,
        jpeg: &[u8],
        vocab: &CubewiseVocab,
        style: &StyleConfig,
    ) -> Result<FrameAnalysis, LemonadeError> {
        let messages = build_messages(jpeg, vocab, style);
        let content = self.chat_content(&messages)?;
        parse_analysis(&content, vocab)
    }

    /// POST /chat/completions and return `choices[0].message.content`.
    fn chat_content(&self, messages: &[serde_json::Value]) -> Result<String, LemonadeError> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut request = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json");
        if !self.api_key.is_empty() {
            request = request.set("Authorization", &format!("Bearer {}", self.api_key));
        }
        let response = request
            .send_json(json!({
                "model": self.model,
                "messages": messages,
                // Deterministic-ish output for structured extraction.
                "temperature": 0.2,
                "stream": false,
                "max_tokens": 512,
            }))
            .map_err(|e| LemonadeError::Http(e.to_string()))?;
        let body: serde_json::Value = response
            .into_json()
            .map_err(|e| LemonadeError::Http(format!("body read failed: {e}")))?;
        let content = body
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_str())
            .ok_or_else(|| {
                LemonadeError::Response(format!(
                    "no choices[0].message.content string in {}",
                    truncate(body.to_string().as_str(), 400)
                ))
            })?;
        Ok(content.to_string())
    }
}

/// Assemble the chat messages for one frame: system + user(text, image).
pub fn build_messages(
    jpeg: &[u8],
    vocab: &CubewiseVocab,
    style: &StyleConfig,
) -> Vec<serde_json::Value> {
    let data_url = format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(jpeg)
    );
    vec![
        json!({
            "role": "system",
            "content": "You are a strict JSON API. Reply with exactly one JSON object and nothing else - no markdown, no commentary.",
        }),
        json!({
            "role": "user",
            "content": [
                {"type": "text", "text": build_text_prompt(vocab, style)},
                {"type": "image_url", "image_url": {"url": data_url}},
            ],
        }),
    ]
}

/// The user-message text: internal instructions first, then the owner style
/// block appended after them (injection contract, issue #53).
pub fn build_text_prompt(vocab: &CubewiseVocab, style: &StyleConfig) -> String {
    let categories = vocab.categories.join(" | ");
    let mut projects = String::new();
    for project in &vocab.projects {
        projects.push_str("  - ");
        projects.push_str(project);
        projects.push('\n');
    }
    let mut prompt = format!(
        "Analyse this desktop screenshot for a work timesheet entry.\n\n\
         Reply with EXACTLY this JSON object (raw JSON, no markdown fences):\n\
         {{\"activity\": \"<one short sentence, present tense>\", \
         \"project_guess\": \"<project string or empty>\", \
         \"category\": \"<one category>\", \
         \"confidence\": <0.0-1.0>, \
         \"evidence\": \"<what you saw that justifies the answer>\"}}\n\n\
         Rules:\n\
         - \"category\" must be exactly one of: {categories}\n\
         - \"project_guess\" must be exactly one of the project strings below, \
         or \"\" if no project is identifiable:\n\
         {projects}\
         - \"confidence\" reflects how sure you are, 0.0 to 1.0.\n\
         Never invent activity that is not visible on screen.\n"
    );
    prompt.push_str(&build_style_block(style));
    prompt
}

/// The owner style block appended after the internal instructions (issue
/// #53 injection contract). Empty config falls back to the documented
/// built-in default.
pub fn build_style_block(style: &StyleConfig) -> String {
    let style_prompt = if style.style_prompt.trim().is_empty() {
        DEFAULT_STYLE_PROMPT
    } else {
        style.style_prompt.trim()
    };
    let mut block =
        format!("\nWriting style (owner-configured, follow exactly):\n{style_prompt}\n");
    if !style.style_examples.is_empty() {
        block.push_str(
            "\nStyle examples - phrase \"activity\" like task and \"evidence\" like comment:\n",
        );
        for (index, example) in style.style_examples.iter().enumerate() {
            block.push_str(&format!(
                "{}. task: {}\n   comment: {}\n",
                index + 1,
                example.task.trim(),
                example.comment.trim()
            ));
        }
    }
    block
}

/// Extract the outermost `{...}` block, tolerating markdown fences or any
/// prose around the JSON.
fn extract_json_object(content: &str) -> Option<&str> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    if end < start {
        return None;
    }
    Some(&content[start..=end])
}

/// Lenient number read: JSON numbers preferred, quoted numbers accepted.
fn as_f64_lenient(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn as_string_field(body: &serde_json::Value, key: &str) -> String {
    body.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// Parse and vocabulary-validate the model reply. Out-of-vocabulary
/// `category`/`project_guess` are stored empty; an empty category also
/// zeroes the confidence (the answer cannot be routed anywhere, so the
/// low-confidence hand-off must catch it).
pub fn parse_analysis(
    content: &str,
    vocab: &CubewiseVocab,
) -> Result<FrameAnalysis, LemonadeError> {
    let json_object = extract_json_object(content).ok_or_else(|| {
        LemonadeError::Response(format!(
            "no JSON object in reply: {}",
            truncate(content, 400)
        ))
    })?;
    let body: serde_json::Value = serde_json::from_str(json_object).map_err(|e| {
        LemonadeError::Response(format!(
            "reply JSON parse failed ({e}): {}",
            truncate(json_object, 400)
        ))
    })?;

    let confidence = body
        .get("confidence")
        .and_then(as_f64_lenient)
        .unwrap_or(0.0);
    let confidence = if confidence.is_finite() {
        confidence.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let category = vocab
        .canonical_category(&as_string_field(&body, "category"))
        .unwrap_or("")
        .to_string();
    let project_guess = vocab
        .canonical_project(&as_string_field(&body, "project_guess"))
        .unwrap_or("")
        .to_string();
    let confidence = if category.is_empty() { 0.0 } else { confidence };
    Ok(FrameAnalysis {
        activity: as_string_field(&body, "activity"),
        project_guess,
        category,
        confidence,
        evidence: as_string_field(&body, "evidence"),
    })
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max_chars).collect();
        format!("{cut}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StyleExample;

    fn vocab() -> CubewiseVocab {
        CubewiseVocab::parse_toml(
            "categories = [\"Consulting\", \"Support\"]\nprojects = [\"A1P01 - Demo\", \"C2P02 - Internal\"]\n",
        )
        .expect("test vocabulary")
    }

    fn style(prompt: &str) -> StyleConfig {
        StyleConfig {
            style_prompt: prompt.to_string(),
            style_examples: Vec::new(),
        }
    }

    #[test]
    fn prompt_carries_schema_vocabulary_and_image() {
        let jpeg = b"fakejpeg-bytes";
        let messages = build_messages(jpeg, &vocab(), &style(""));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        let parts = messages[1]["content"].as_array().expect("content parts");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        let url = parts[1]["image_url"]["url"].as_str().expect("data url");
        assert!(url.starts_with("data:image/jpeg;base64,"));
        assert_eq!(
            url,
            format!(
                "data:image/jpeg;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(jpeg)
            ),
            "image rides as a base64 data URL"
        );

        let text = parts[0]["text"].as_str().expect("prompt text");
        assert!(text.contains("\"activity\""));
        assert!(text.contains("\"confidence\""));
        assert!(text.contains("Consulting | Support"));
        assert!(text.contains("A1P01 - Demo"));
        assert!(text.contains("C2P02 - Internal"));
    }

    #[test]
    fn custom_style_changes_the_assembled_prompt_after_internal_instructions() {
        // Issue #53 injection contract: internal instructions first, owner
        // style appended after; custom style must actually appear.
        let custom = StyleConfig {
            style_prompt: "Write in Traditional Chinese, end with a full stop.".to_string(),
            style_examples: vec![StyleExample {
                task: "Resolved TM1 rules".to_string(),
                comment: "Fixed three rules".to_string(),
            }],
        };
        let with_default = build_text_prompt(&vocab(), &style(""));
        let with_custom = build_text_prompt(&vocab(), &custom);

        assert!(with_default.contains(DEFAULT_STYLE_PROMPT));
        assert!(!with_default.contains("Traditional Chinese"));
        assert!(!with_custom.contains(DEFAULT_STYLE_PROMPT));
        assert!(with_custom.contains("Traditional Chinese"));
        assert!(with_custom.contains("Resolved TM1 rules"));
        assert!(with_custom.contains("Fixed three rules"));
        // ordering: style block comes after the internal schema/rules
        let internal_pos = with_custom
            .find("must be exactly one of")
            .expect("internal instructions present");
        let style_pos = with_custom
            .find("Writing style")
            .expect("style block present");
        assert!(style_pos > internal_pos, "style injected after internals");
    }

    #[test]
    fn empty_style_prompt_falls_back_to_builtin_default() {
        let block = build_style_block(&StyleConfig::default());
        assert!(block.contains(DEFAULT_STYLE_PROMPT));
        assert!(!block.contains("Style examples"));
        let whitespace_only = build_style_block(&style("   \n  "));
        assert!(whitespace_only.contains(DEFAULT_STYLE_PROMPT));
    }

    #[test]
    fn parse_accepts_clean_json() {
        let content = r#"{"activity": "Editing main.rs", "project_guess": "A1P01 - Demo",
                          "category": "Consulting", "confidence": 0.87, "evidence": "editor visible"}"#;
        let analysis = parse_analysis(content, &vocab()).expect("clean parse");
        assert_eq!(analysis.activity, "Editing main.rs");
        assert_eq!(analysis.project_guess, "A1P01 - Demo");
        assert_eq!(analysis.category, "Consulting");
        assert!((analysis.confidence - 0.87).abs() < 1e-9);
        assert_eq!(analysis.evidence, "editor visible");
    }

    #[test]
    fn parse_tolerates_fenced_json_and_prose() {
        let content = "Here you go:\n```json\n{\"activity\": \"Calls\", \
                       \"project_guess\": \"c2p02 - internal\", \"category\": \"support\", \
                       \"confidence\": \"0.4\", \"evidence\": \"Teams\"}\n```\nHope that helps.";
        let analysis = parse_analysis(content, &vocab()).expect("fenced parse");
        // case-insensitive vocabulary fallback maps to canonical spellings
        assert_eq!(analysis.project_guess, "C2P02 - Internal");
        assert_eq!(analysis.category, "Support");
        // quoted number accepted leniently
        assert!((analysis.confidence - 0.4).abs() < 1e-9);
    }

    #[test]
    fn out_of_vocab_answers_are_empty_and_zeroed() {
        let content = r#"{"activity": "Browsing", "project_guess": "P999 - nope",
                          "category": "Bogus", "confidence": 0.95, "evidence": "web"}"#;
        let analysis = parse_analysis(content, &vocab()).expect("parses");
        assert_eq!(analysis.category, "");
        assert_eq!(analysis.project_guess, "");
        assert_eq!(analysis.confidence, 0.0, "unroutable answer must be low");
        assert_eq!(analysis.activity, "Browsing", "activity text is kept");
    }

    #[test]
    fn confidence_is_clamped_and_missing_fields_default() {
        let content = r#"{"activity": "Somewhere", "category": "Consulting",
                          "confidence": 1.7, "project_guess": ""}"#;
        let analysis = parse_analysis(content, &vocab()).expect("parses");
        assert_eq!(analysis.confidence, 1.0);
        assert_eq!(analysis.evidence, "");

        let negative = parse_analysis(r#"{"confidence": -3, "category": "Support"}"#, &vocab())
            .expect("parses");
        assert_eq!(negative.confidence, 0.0);
        assert_eq!(negative.category, "Support");
        assert_eq!(negative.activity, "");
    }

    #[test]
    fn parse_rejects_non_json_replies() {
        assert!(matches!(
            parse_analysis("no json at all", &vocab()),
            Err(LemonadeError::Response(_))
        ));
        assert!(matches!(
            parse_analysis("{\"activity\": oops}", &vocab()),
            Err(LemonadeError::Response(_))
        ));
    }
}
