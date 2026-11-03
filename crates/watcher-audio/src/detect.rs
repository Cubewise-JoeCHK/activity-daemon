//! Conferencing detection abstraction (cross-platform seam).
//!
//! # Detection strategy (issue #6, ADR rev 4 addendum)
//!
//! - **Preferred** (`audio-session`): enumerate active WASAPI audio
//!   sessions on the render devices and match the owning process name
//!   against `audio.process_list`. This detects "actually in a call",
//!   not merely a running app.
//! - **Fallback** (`process-running`): if session enumeration is not
//!   available for a poll (no audio device, COM failure), fall back to a
//!   running-process check against the same list. This can over-trigger
//!   (app running but idle) - the active mode is logged at startup and on
//!   every change, and is recorded in `meeting-audio` start events.
//!
//! On non-Windows targets a [`NullWatcher`] is used: auto-trigger never
//! fires there (recording itself is Windows-only in v0.1).

/// How a conference was detected for the current poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionMode {
    /// Active WASAPI audio session of a configured process (preferred).
    AudioSession,
    /// Configured process is running (fallback; may over-trigger).
    ProcessFallback,
}

impl DetectionMode {
    pub fn label(&self) -> &'static str {
        match self {
            DetectionMode::AudioSession => "audio-session",
            DetectionMode::ProcessFallback => "process-running",
        }
    }
}

/// A conferencing app holding an active audio session right now.
#[derive(Debug, Clone, PartialEq)]
pub struct ConferenceHit {
    /// Matched process name (as reported by the OS, not normalized).
    pub process: String,
    /// How it was detected.
    pub mode: DetectionMode,
}

/// Conferencing detector seam.
pub trait ConferenceWatcher: Send {
    /// Poll once. `Some(hit)` = a configured conferencing app currently has
    /// an active audio session (preferred mode) or at least is running
    /// (fallback). `None` = no conference activity seen. Must never panic
    /// and must never block for long (poll cadence is 2 s).
    fn poll(&mut self) -> Option<ConferenceHit>;
}

/// Non-Windows stub: auto-trigger unavailable.
pub struct NullWatcher;

impl ConferenceWatcher for NullWatcher {
    fn poll(&mut self) -> Option<ConferenceHit> {
        None
    }
}

/// Normalize a configured process name for matching: lowercase, ensure an
/// `.exe` suffix is not required by the config (both `teams` and
/// `Teams.exe` match `Teams.exe`).
pub fn normalize_config_name(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.ends_with(".exe") {
        lower
    } else {
        format!("{lower}.exe")
    }
}

/// Whether an OS-reported process image name matches the normalized want
/// list. Comparison is on the lowercased basename, exact.
pub fn process_matches(wanted: &[String], image_name: &str) -> bool {
    let basename = image_name
        .replace('/', "\\")
        .rsplit('\\')
        .next()
        .unwrap_or(image_name)
        .to_ascii_lowercase();
    wanted.contains(&basename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_names_normalize_to_lowercase_exe() {
        assert_eq!(normalize_config_name("Teams.exe"), "teams.exe");
        assert_eq!(normalize_config_name("ms-teams"), "ms-teams.exe");
        assert_eq!(normalize_config_name(" ZOOM.EXE "), "zoom.exe");
    }

    #[test]
    fn process_matching_uses_basename_case_insensitive() {
        let wanted = vec!["teams.exe".to_string(), "zoom.exe".to_string()];
        assert!(process_matches(&wanted, "Teams.exe"));
        assert!(process_matches(
            &wanted,
            r"C:\Program Files\Zoom\bin\Zoom.exe"
        ));
        assert!(!process_matches(&wanted, "ms-teams.exe"));
        assert!(
            !process_matches(&wanted, "teamviewer.exe"),
            "no substring matches"
        );
        assert!(!process_matches(&wanted, "zoom_launcher.exe"));
    }

    #[test]
    fn null_watcher_never_fires() {
        let mut w = NullWatcher;
        assert!(w.poll().is_none());
    }

    #[test]
    fn detection_mode_labels() {
        assert_eq!(DetectionMode::AudioSession.label(), "audio-session");
        assert_eq!(DetectionMode::ProcessFallback.label(), "process-running");
    }
}
