//! Global hotkey specification: parsing is cross-platform and tested
//! everywhere; registration (Windows) lives in `win_hotkey`.
//!
//! # Syntax (config `audio.hotkey`)
//!
//! `+`-separated tokens, exactly one key plus at least one modifier:
//!
//! ```text
//! ctrl+shift+alt+m      (core default)
//! Ctrl+Alt+M            (case-insensitive, `control` accepted for `ctrl`)
//! win+f5                (F1-F24 supported)
//! ```
//!
//! Supported keys: single letters `a`-`z`, digits `0`-`9`, `f1`-`f24`.
//! A bare key without modifiers is rejected (it would hijack normal typing).

/// A parsed hotkey combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotKeySpec {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    /// Normalized key token: `a`-`z`, `0`-`9`, or `f1`-`f24`.
    pub key: String,
}

impl HotKeySpec {
    /// Parse a hotkey string per the module syntax. Returns Err with a
    /// human-readable reason.
    pub fn parse(spec: &str) -> Result<HotKeySpec, String> {
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut win = false;
        let mut key: Option<String> = None;

        for token in spec.split('+') {
            let token = token.trim().to_ascii_lowercase();
            if token.is_empty() {
                return Err(format!("empty token in hotkey '{spec}'"));
            }
            match token.as_str() {
                "ctrl" | "control" => ctrl = true,
                "shift" => shift = true,
                "alt" => alt = true,
                "win" | "super" | "meta" => win = true,
                _ => {
                    if key.is_some() {
                        return Err(format!("multiple keys in hotkey '{spec}'"));
                    }
                    Self::validate_key(&token, spec)?;
                    key = Some(token);
                }
            }
        }

        let key = match key {
            Some(k) => k,
            None => return Err(format!("no key in hotkey '{spec}'")),
        };
        if !ctrl && !shift && !alt && !win {
            return Err(format!("hotkey '{spec}' needs at least one modifier"));
        }
        Ok(HotKeySpec {
            ctrl,
            shift,
            alt,
            win,
            key,
        })
    }

    fn validate_key(token: &str, spec: &str) -> Result<(), String> {
        let chars: Vec<char> = token.chars().collect();
        if chars.len() == 1 && (chars[0].is_ascii_lowercase() || chars[0].is_ascii_digit()) {
            return Ok(());
        }
        if token.len() >= 2
            && token.starts_with('f')
            && token[1..]
                .parse::<u8>()
                .map(|n| (1..=24).contains(&n))
                .unwrap_or(false)
        {
            return Ok(());
        }
        Err(format!(
            "unsupported key '{token}' in hotkey '{spec}' (use a-z, 0-9, f1-f24)"
        ))
    }

    /// Windows virtual-key code for the parsed key.
    pub fn vk_code(&self) -> u32 {
        let t = self.key.as_str();
        let mut chars = t.chars();
        let first = chars.next().unwrap_or('a');
        match (first, t.len()) {
            (c, 1) if c.is_ascii_lowercase() => u32::from(c) - u32::from('a') + 0x41,
            (c, 1) if c.is_ascii_digit() => u32::from(c) - u32::from('0') + 0x30,
            ('f', n) if n >= 2 => 0x70 + t[1..].parse::<u32>().unwrap_or(1) - 1,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_core_default_and_common_forms() {
        let hk = HotKeySpec::parse("ctrl+shift+alt+m").expect("core default");
        assert!(hk.ctrl && hk.shift && hk.alt && !hk.win);
        assert_eq!(hk.key, "m");

        let hk = HotKeySpec::parse("Ctrl+Alt+M").expect("issue text form");
        assert!(hk.ctrl && hk.alt && !hk.shift);
        assert_eq!(hk.key, "m");

        let hk = HotKeySpec::parse("control+alt+m").expect("control alias");
        assert!(hk.ctrl && hk.alt);

        let hk = HotKeySpec::parse("win+f5").expect("f-key");
        assert!(hk.win && !hk.ctrl);
        assert_eq!(hk.key, "f5");
    }

    #[test]
    fn rejects_bad_specs() {
        assert!(HotKeySpec::parse("m").is_err(), "no modifier");
        assert!(HotKeySpec::parse("ctrl+alt").is_err(), "no key");
        assert!(HotKeySpec::parse("ctrl+alt+shift+m+n").is_err(), "two keys");
        assert!(HotKeySpec::parse("ctrl+alt+bogus").is_err());
        assert!(HotKeySpec::parse("ctrl+alt++").is_err(), "empty token");
        assert!(
            HotKeySpec::parse("ctrl+alt+f99").is_err(),
            "out of range f-key"
        );
        assert!(HotKeySpec::parse("").is_err());
    }

    #[test]
    fn vk_codes_match_windows_constants() {
        // 'M' = 0x4D, '1' = 0x31, F1 = 0x70, F12 = 0x7B.
        assert_eq!(HotKeySpec::parse("ctrl+m").unwrap().vk_code(), 0x4D);
        assert_eq!(HotKeySpec::parse("ctrl+alt+1").unwrap().vk_code(), 0x31);
        assert_eq!(HotKeySpec::parse("win+f1").unwrap().vk_code(), 0x70);
        assert_eq!(HotKeySpec::parse("win+f12").unwrap().vk_code(), 0x7B);
    }
}
