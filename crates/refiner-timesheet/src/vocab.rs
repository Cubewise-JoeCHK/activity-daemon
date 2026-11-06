//! The Cubewise offline-template vocabulary (ADR addendum decision 4 on
//! issue #16): the exact project strings (`<code> - <client> - <name>`) and
//! the seven activity categories the template's upload sheet accepts.
//!
//! The vocabulary lives in `assets/cubewise_vocab.toml` (extracted from
//! `timsheet_offline_2026_08.xlsx`, Sheet1 A2:A8 / C2:C190) and is embedded
//! into the binary at compile time - no vocabulary strings are hardcoded in
//! .rs source. A deployment may point the exporter at a different TOML file
//! of the same shape through the server's `[timesheet] vocab_path` config;
//! a configured file that cannot be read or parsed is a hard error, matching
//! the server-config philosophy (a typo must not silently change what the
//! export produces).

use std::path::Path;
use std::sync::OnceLock;

use activity_core::CoreError;
use serde::Deserialize;

/// The embedded template vocabulary (compile-time asset; the default).
const EMBEDDED_VOCAB_TOML: &str = include_str!("../assets/cubewise_vocab.toml");

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
    /// [...]`). Empty lists, blank entries or duplicates are config errors -
    /// a vocabulary the owner cannot trust must fail loudly, not shrink the
    /// export silently.
    pub fn parse_toml(text: &str) -> Result<CubewiseVocab, CoreError> {
        let vocab: CubewiseVocab = toml::from_str(text)
            .map_err(|e| CoreError::Config(format!("cannot parse vocabulary TOML: {e}")))?;
        for (label, list) in [
            ("categories", &vocab.categories),
            ("projects", &vocab.projects),
        ] {
            if list.is_empty() {
                return Err(CoreError::Config(format!(
                    "vocabulary {label} list is empty"
                )));
            }
            for (n, entry) in list.iter().enumerate() {
                if entry.trim().is_empty() {
                    return Err(CoreError::Config(format!(
                        "vocabulary {label} entry {n} is blank"
                    )));
                }
            }
            let mut sorted = list.clone();
            sorted.sort();
            sorted.dedup();
            if sorted.len() != list.len() {
                return Err(CoreError::Config(format!(
                    "vocabulary {label} list contains duplicates"
                )));
            }
        }
        Ok(vocab)
    }

    /// The embedded template vocabulary (parsed once per process).
    pub fn embedded() -> &'static CubewiseVocab {
        static EMBEDDED: OnceLock<CubewiseVocab> = OnceLock::new();
        EMBEDDED.get_or_init(|| {
            CubewiseVocab::parse_toml(EMBEDDED_VOCAB_TOML)
                .expect("embedded cubewise_vocab.toml is valid")
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_vocab_has_template_counts() {
        let vocab = CubewiseVocab::embedded();
        assert_eq!(vocab.categories.len(), 7);
        assert_eq!(vocab.projects.len(), 189);
        // Spot checks straight from the template sheet.
        assert!(vocab.has_category("Consulting"));
        assert!(vocab.has_category("Customer Returned"));
        assert!(vocab.has_project("C000P000 - Cubewise - Internal"));
        assert!(vocab.has_project("A143P010 - Frasers - H2 FH - BU Core"));
        assert!(!vocab.has_project("C000P000 - Internal"));
        assert!(!vocab.has_category("consulting"));
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
    }
}
