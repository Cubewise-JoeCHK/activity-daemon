//! The Cubewise offline-template vocabulary (ADR addendum decision 4 on
//! issue #16): the exact project strings (`<code> - <client> - <name>`) and
//! the seven activity categories the template's upload sheet accepts.
//!
//! One canonical source serves both consumers of the vocabulary:
//!
//! - the frame-analysis worker (issue #43) builds its Lemonade prompt from
//!   it and vocabulary-validates the model's reply (exact match first,
//!   case-insensitive fallback to the canonical spelling), and
//! - the month-end xlsx export (issue #46) accepts only approved rows whose
//!   project string and category are exact template strings, and mirrors
//!   the vocabulary into the workbook's `Sheet1`.
//!
//! The vocabulary lives in `assets/cubewise_vocabulary.toml` (extracted
//! from `timsheet_offline_2026_08.xlsx`, Sheet1 A2:A8 / C2:C190) and is
//! embedded into the binary at compile time - no vocabulary strings are
//! hardcoded in .rs source. A deployment may point either consumer at a
//! different TOML file of the same shape through the server's
//! `[timesheet] vocab_path` config; a configured file that cannot be read
//! or parsed is a hard error, matching the server-config philosophy (a
//! typo must not silently change what the worker prompts for or what the
//! export produces).

use std::path::Path;
use std::sync::OnceLock;

use activity_core::CoreError;
use serde::Deserialize;

/// The embedded template vocabulary (compile-time asset; the default).
const EMBEDDED_VOCAB_TOML: &str = include_str!("../assets/cubewise_vocabulary.toml");

/// Project strings and categories the Cubewise upload sheet accepts.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CubewiseVocab {
    /// The 7 activity categories (template Sheet1 A2:A8).
    pub categories: Vec<String>,
    /// The 189 exact project strings (template Sheet1 C2:C190).
    pub projects: Vec<String>,
}

impl CubewiseVocab {
    /// Parse vocabulary TOML (shape: `categories = [...]`, `projects =
    /// [...]`). Entries are trimmed; empty lists, blank entries or
    /// duplicates are config errors - a vocabulary the owner cannot trust
    /// must fail loudly, not silently narrow the prompt or shrink the
    /// export.
    pub fn parse_toml(text: &str) -> Result<CubewiseVocab, CoreError> {
        let file: CubewiseVocab = toml::from_str(text)
            .map_err(|e| CoreError::Config(format!("cannot parse vocabulary TOML: {e}")))?;
        let clean = |label: &str, items: Vec<String>| -> Result<Vec<String>, CoreError> {
            if items.is_empty() {
                return Err(CoreError::Config(format!(
                    "vocabulary {label} list is empty"
                )));
            }
            let mut out: Vec<String> = Vec::with_capacity(items.len());
            for (n, item) in items.iter().enumerate() {
                let item = item.trim();
                if item.is_empty() {
                    return Err(CoreError::Config(format!(
                        "vocabulary {label} entry {n} is blank"
                    )));
                }
                let item = item.to_string();
                if out.contains(&item) {
                    return Err(CoreError::Config(format!(
                        "vocabulary {label} list contains duplicates ({item})"
                    )));
                }
                out.push(item);
            }
            Ok(out)
        };
        let categories = clean("categories", file.categories)?;
        let projects = clean("projects", file.projects)?;
        Ok(CubewiseVocab {
            categories,
            projects,
        })
    }

    /// The embedded template vocabulary (parsed once per process).
    pub fn embedded() -> &'static CubewiseVocab {
        static EMBEDDED: OnceLock<CubewiseVocab> = OnceLock::new();
        EMBEDDED.get_or_init(|| {
            CubewiseVocab::parse_toml(EMBEDDED_VOCAB_TOML)
                .expect("embedded cubewise_vocabulary.toml is valid")
        })
    }

    /// Load the vocabulary: an explicit path wins (read errors and parse
    /// errors are hard errors carrying the path), otherwise the embedded
    /// template asset is used.
    pub fn load(path: Option<&Path>) -> Result<CubewiseVocab, CoreError> {
        match path {
            None => Ok(CubewiseVocab::embedded().clone()),
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|e| {
                    CoreError::Config(format!("cannot read vocabulary {}: {e}", path.display()))
                })?;
                CubewiseVocab::parse_toml(&text).map_err(|e| {
                    CoreError::Config(format!("invalid vocabulary {}: {e}", path.display()))
                })
            }
        }
    }

    /// Whether `project` is one of the exact template project strings.
    pub fn has_project(&self, project: &str) -> bool {
        self.projects.iter().any(|known| known == project)
    }

    /// Whether `category` is one of the 7 template categories.
    pub fn has_category(&self, category: &str) -> bool {
        self.categories.iter().any(|known| known == category)
    }

    /// Canonical spelling of a model-returned category: exact match first,
    /// then case-insensitive fallback; `None` when out of vocabulary.
    pub fn canonical_category(&self, answer: &str) -> Option<&str> {
        Self::canonical(&self.categories, answer)
    }

    /// Canonical spelling of a model-returned project string: exact match
    /// first, then case-insensitive fallback; `None` when out of vocabulary.
    pub fn canonical_project(&self, answer: &str) -> Option<&str> {
        Self::canonical(&self.projects, answer)
    }

    fn canonical<'a>(items: &'a [String], answer: &str) -> Option<&'a str> {
        if let Some(exact) = items.iter().find(|item| item.as_str() == answer) {
            return Some(exact.as_str());
        }
        items
            .iter()
            .find(|item| item.eq_ignore_ascii_case(answer))
            .map(|item| item.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_vocab_matches_the_template_extraction() {
        // Locks the committed seed against silent truncation: 7 categories
        // and 189 projects straight from timsheet_offline_2026_08.xlsx.
        let vocab = CubewiseVocab::embedded();
        assert_eq!(
            vocab.categories,
            [
                "Consulting",
                "Support",
                "Training",
                "Management",
                "Pre-Sales",
                "QA",
                "Customer Returned",
            ]
        );
        assert_eq!(vocab.projects.len(), 189);
        // first and last extracted rows, spot-checked against the xlsx
        assert!(vocab.projects[0].starts_with("A143P010 - Frasers"));
        assert!(vocab.projects[188].starts_with("C920P020 - AIA HK"));
        // export-style exact lookups (a case variant is NOT in vocabulary)
        assert!(vocab.has_category("Consulting"));
        assert!(vocab.has_category("Customer Returned"));
        assert!(vocab.has_project("C000P000 - Cubewise - Internal"));
        assert!(!vocab.has_category("consulting"));
        assert!(!vocab.has_project("C000P000 - Internal"));
    }

    #[test]
    fn canonical_matching_is_exact_then_case_insensitive() {
        let vocab = CubewiseVocab::embedded();
        assert_eq!(
            vocab.canonical_category("QA"),
            Some("QA"),
            "exact hit keeps canonical spelling"
        );
        assert_eq!(
            vocab.canonical_category("consulting"),
            Some("Consulting"),
            "case-insensitive fallback maps to canonical"
        );
        assert_eq!(vocab.canonical_category("Bogus"), None);
        assert_eq!(
            vocab.canonical_project("A143P010 - Frasers - H2 FH - BU Core"),
            Some("A143P010 - Frasers - H2 FH - BU Core")
        );
        assert_eq!(
            vocab.canonical_project("a143p010 - frasers - h2 fh - bu core"),
            Some("A143P010 - Frasers - H2 FH - BU Core")
        );
        assert_eq!(vocab.canonical_project(""), None);
    }

    #[test]
    fn load_uses_embedded_without_path() {
        let vocab = CubewiseVocab::load(None).expect("embedded load");
        assert_eq!(vocab, *CubewiseVocab::embedded());
    }

    #[test]
    fn load_reads_override_path_and_fails_loudly() {
        let dir = std::env::temp_dir().join(format!("refiner-vocab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("vocab.toml");
        std::fs::write(
            &path,
            "categories = [\"Consulting\"]\nprojects = [\"A1 - X - Y\"]\n",
        )
        .expect("write vocab");

        let vocab = CubewiseVocab::load(Some(&path)).expect("override load");
        assert_eq!(vocab.projects, vec!["A1 - X - Y".to_string()]);

        std::fs::write(
            &path,
            "categories = [\"Consulting\", \"Consulting\"]\nprojects = [\"A1 - X - Y\"]\n",
        )
        .expect("write dup vocab");
        let err = CubewiseVocab::load(Some(&path)).expect_err("duplicate rejected");
        assert!(matches!(err, CoreError::Config(_)), "{err:?}");

        let missing = CubewiseVocab::load(Some(&dir.join("absent.toml")));
        assert!(missing.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_rejects_empty_and_blank_entries() {
        let empty = CubewiseVocab::parse_toml("categories = []\nprojects = [\"A1 - X\"]\n");
        assert!(empty.is_err());
        let blank = CubewiseVocab::parse_toml(
            "categories = [\"Consulting\", \" \"]\nprojects = [\"A1 - X\"]\n",
        );
        assert!(blank.is_err());
        let garbage = CubewiseVocab::parse_toml("categories = \"nope\"\n");
        assert!(garbage.is_err());
        // a missing list is a parse error too
        assert!(CubewiseVocab::parse_toml("projects = [\"P1\"]\n").is_err());
    }

    #[test]
    fn parse_trims_padded_entries() {
        let vocab = CubewiseVocab::parse_toml(
            "categories = [\" Alpha \", \"Beta\"]\nprojects = [\" P1 \", \"P2\"]\n",
        )
        .expect("parse padded vocabulary");
        assert_eq!(vocab.categories, ["Alpha".to_string(), "Beta".to_string()]);
        assert_eq!(vocab.projects, ["P1".to_string(), "P2".to_string()]);
    }
}
