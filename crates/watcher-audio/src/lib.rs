//! Meeting audio watcher for activity-daemon - SKELETON (issue #6).
//!
//! # Contract (ADR revisions 4 and 4 addendum)
//!
//! - **Manual trigger** (global hotkey from `audio.hotkey`, or the CLI
//!   `activity-daemon record start|stop`): records the **microphone only**.
//!   Intended for in-person meetings.
//! - **Auto-trigger** (v0.1 requirement, #6): fires when a conferencing app
//!   from `audio.process_list` holds an **active audio session** - detected
//!   via WASAPI per-app sessions (not merely a running process), with
//!   process-match as config fallback. Auto-trigger records **mic + system
//!   loopback** (online meetings).
//! - **Retention**: recorder output follows rolling retention, default
//!   60 days / 10 GB (see `core`'s `AudioConfig` defaults).
//! - **OS limitation, accepted**: the Windows mic-in-use indicator and
//!   hardware LEDs cannot be suppressed legitimately (ADR rev 4); no work
//!   is spent on hiding them.
//! - Recorded files are local-first and sync to the WSL hub within v0.1
//!   (ADR rev 5: upload file first, index row after).
//! - Consent/legal context for recording is the owner's responsibility
//!   (ADR rev 4).
//!
//! # Implementation TODO (#6)
//!
//! - TODO(#6): WASAPI per-app active-audio-session detection with config
//!   process-list fallback.
//! - TODO(#6): mic and loopback capture into files under
//!   `media/audio/<YYYY>/<MM>/<DD>/`.
//! - TODO(#6): global hotkey listener + CLI record start/stop plumbing
//!   (CLI stubs exist in the `activity-cli` crate).
//! - TODO(#6): rolling-retention integration (mirrors watcher-screen's
//!   planner rules, but this plugin must stay dependency-free per the ADR
//!   "plugins must not depend on other plugins").
//! - TODO(#6): write `media` bucket index events (kind/path/bytes/sha16/
//!   producer) after each finished recording.

/// Default rolling retention for meeting audio: max age in days.
pub const DEFAULT_MAX_AGE_DAYS: u64 = 60;

/// Default rolling retention for meeting audio: max total size in MB.
pub const DEFAULT_MAX_TOTAL_MB: u64 = 10 * 1024;

/// Auto-trigger detector placeholder: watches conferencing processes for
/// active WASAPI audio sessions and reports trigger start/stop.
/// TODO(#6): implement.
pub struct ConferencingDetector;

/// Meeting audio recorder placeholder: captures mic (+ loopback on
/// auto-trigger) into rolling-retained files. TODO(#6): implement.
pub struct MeetingRecorder;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_defaults_match_adr() {
        assert_eq!(DEFAULT_MAX_AGE_DAYS, 60);
        assert_eq!(DEFAULT_MAX_TOTAL_MB, 10 * 1024);
    }
}
