//! Template vocabulary (issue #43, template contract in the ADR addendum
//! on #16): the 7 categories and 189 project strings from the Cubewise
//! offline timesheet template, seeded once into a committed config file -
//! never hardcoded in Rust source.

use std::path::Path;

use serde::Deserialize;

/// The committed seed, compiled in so the hub container needs no extra
/// volume. Override with `[timesheet] vocab_path` for a custom vocabulary.
pub const EMBEDDED_VOCAB_TOML: &str = include_str!("../assets/cubewise_vocabulary.toml");

#[derive(Debug, Deserialize)]
struct VocabFile {
    categories: Vec<String>,
    projects: Vec<String>,
}

/// The closed vocabularies the model's answer must match: `category` is one
/// of the template categories, `project_guess` one of the project strings.
#[derive(Debug, Clone, PartialEq)]
pub struct Vocabulary {
    categories: Vec<String>,
    projects: Vec<String>,
}

impl Vocabulary {
    /// Parse a vocabulary TOML (`categories = [...]`, `projects = [...]`).
    /// Entries are trimmed; empty and duplicate entries are dropped. An
    /// empty result is an error: a vocabulary the model cannot match makes
    /// every analysis useless, so it must be loud.
    pub fn parse(toml_str: &str) -> Result<Vocabulary, String> {
        let file: VocabFile =
            toml::from_str(toml_str).map_err(|e| format!("vocabulary TOML: {e}"))?;
        let clean = |items: Vec<String>| -> Vec<String> {
            let mut out: Vec<String> = Vec::new();
            for item in items {
                let item = item.trim().to_string();
                if !item.is_empty() && !out.contains(&item) {
                    out.push(item);
                }
            }
            out
        };
        let categories = clean(file.categories);
        let projects = clean(file.projects);
        if categories.is_empty() {
            return Err("vocabulary has no categories".to_string());
        }
        if projects.is_empty() {
            return Err("vocabulary has no projects".to_string());
        }
        Ok(Vocabulary {
            categories,
            projects,
        })
    }

    /// The embedded template seed.
    pub fn embedded() -> Result<Vocabulary, String> {
        Vocabulary::parse(EMBEDDED_VOCAB_TOML)
    }

    /// The embedded seed, unless `override_path` is configured.
    pub fn load(override_path: Option<&Path>) -> Result<Vocabulary, String> {
        match override_path {
            None => Vocabulary::embedded(),
            Some(path) => {
                let s = std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read vocabulary file {}: {e}", path.display()))?;
                Vocabulary::parse(&s)
            }
        }
    }

    /// Template categories, in template order.
    pub fn categories(&self) -> &[String] {
        &self.categories
    }

    /// Template project strings, in template order.
    pub fn projects(&self) -> &[String] {
        &self.projects
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
    fn embedded_seed_matches_the_template_extraction() {
        // Locks the committed seed against silent truncation: 7 categories
        // and 189 projects straight from timsheet_offline_2026_08.xlsx.
        let vocab = Vocabulary::embedded().expect("embedded seed parses");
        assert_eq!(
            vocab.categories(),
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
        assert_eq!(vocab.projects().len(), 189);
        // first and last extracted rows, spot-checked against the xlsx
        assert!(vocab.projects()[0].starts_with("A143P010 - Frasers"));
        assert!(vocab.projects()[188].starts_with("C920P020 - AIA HK"));
        // the seed file itself must not carry blanks or duplicates
        let mut sorted = vocab.projects().to_vec();
        sorted.sort();
        let unique = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), unique);
    }

    #[test]
    fn canonical_matching_is_exact_then_case_insensitive() {
        let vocab = Vocabulary::embedded().expect("embedded seed parses");
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
    fn parse_trims_drops_empty_and_duplicate_entries() {
        let vocab = Vocabulary::parse(
            "categories = [\"Alpha\", \"  \", \"alpha\", \"Alpha\"]\nprojects = [\"P1\"]\n",
        )
        .expect("parse noisy vocabulary");
        assert_eq!(vocab.categories(), ["Alpha", "alpha"]);
        assert_eq!(vocab.projects(), ["P1"]);
    }

    #[test]
    fn parse_rejects_missing_or_empty_lists() {
        assert!(Vocabulary::parse("projects = [\"P1\"]\n").is_err());
        assert!(Vocabulary::parse("categories = [\"A\"]\n").is_err());
        assert!(Vocabulary::parse("nope = 1\n").is_err());
    }

    #[test]
    fn load_honours_override_path() {
        let dir = std::env::temp_dir().join(format!(
            "refiner-vocab-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("custom_vocab.toml");
        std::fs::write(
            &path,
            "categories = [\"Internal\"]\nprojects = [\"P1\", \"P2\"]\n",
        )
        .expect("write vocab");
        let vocab = Vocabulary::load(Some(&path)).expect("override loads");
        assert_eq!(vocab.categories(), ["Internal"]);
        assert_eq!(vocab.projects(), ["P1", "P2"]);

        let missing = Vocabulary::load(Some(&dir.join("gone.toml")))
            .expect_err("missing override must be an error");
        assert!(missing.contains("cannot read vocabulary"));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
