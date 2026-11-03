//! Meeting audio watcher for activity-daemon - manual + conferencing
//! auto-trigger recorder (issue #6).
//!
//! # Design contract (ADR #16 revisions 4 and 4 addendum)
//!
//! - **Manual trigger** records the **microphone only** (in-person
//!   meetings). Two channels feed it:
//!   1. the global hotkey from `audio.hotkey` (core default
//!      `ctrl+shift+alt+m`; syntax `ctrl+shift+alt+m` / `Ctrl+Alt+M`,
//!      keys a-z, 0-9, f1-f24),
//!   2. the `record.json` control file - **see the file contract below**.
//! - **Auto trigger** (v0.1 requirement): fires when a conferencing app
//!   from `audio.process_list` holds an **active audio session** (actually
//!   in a call, not merely running), and records **mic + system loopback**
//!   (remote voices arrive via the speakers). Preferred detection is WASAPI
//!   per-app session enumeration; a process-running check is the fallback.
//!   Which mode is active is logged at startup and on every mode change,
//!   and recorded in `meeting-audio` start events (`detect_mode`).
//! - **No visible UI ever**: no windows, no tray; the watcher logs only to
//!   `{data_dir}/logs/audio.log`. The Windows mic-in-use indicator and
//!   hardware LEDs cannot be suppressed legitimately (ADR rev 4); no work
//!   is spent on hiding them.
//! - **Retention**: rolling budget, oldest-first, default 60 days / 10 GB
//!   (`audio.max_age_days` / `audio.max_total_mb`), swept hourly. Files
//!   whose `media` index row is not yet synced are never pruned (ADR
//!   rev 5); stale `*.part` crash leftovers are removed after a day.
//! - Files are local-first under `{data_dir}/media/audio/` and sync to the
//!   WSL hub within v0.1 (#18: file first, index row after).
//! - Consent/legal context for recording participants is the owner's
//!   responsibility (ADR rev 4).
//!
//! # FILE CONTRACT: `{data_dir}/record.json` (for issue #7, the CLI)
//!
//! `watcher-audio` polls **`{data_dir}/record.json` every 500 ms**. To
//! toggle a manual (mic-only) recording without the hotkey, write:
//!
//! ```json
//! {"action": "start", "trigger": "cli"}
//! ```
//!
//! or `{"action": "stop", "trigger": "cli"}`.
//!
//! - `action` is required (`"start"` / `"stop"`); `trigger` is optional.
//! - The watcher **consumes (deletes) the file** once read: file gone =
//!   command accepted.
//! - `start` while already recording and `stop` while idle are no-ops (the
//!   file is still consumed; state is visible via the `meeting-audio`
//!   bucket in the DB).
//! - A malformed file is renamed to `record.json.invalid` for diagnosis.
//! - CLI writes must be atomic: write `record.json.tmp`, then rename to
//!   `record.json`.
//! - The stop of a manual recording is always honored, even for an
//!   auto-triggered recording (manual stop wins).
//!
//! See [`control`] for the parsing rules and [`machine`] for the trigger
//! state machine semantics (no double start; manual stop wins; auto
//! restart after grace; AFK never stops a recording).
//!
//! # Files and events
//!
//! - Recordings: `{data_dir}/media/audio/<YYYY>/<MM>/<DD>/<start_ts>_<trigger>_<mic|loopback>.wav`,
//!   16-bit PCM, 48 kHz, mono (mic) / stereo (loopback), written to a
//!   `.part` file and atomically renamed on finish.
//! - `media` bucket index row per finished file:
//!   `{kind:"audio", path, bytes, sha16, producer:"watcher-audio"}`
//!   (`path` is relative to the data dir; `sha16` = first 16 hex chars of
//!   the file's SHA-256).
//! - `meeting-audio` bucket events for the timesheet (v0.2):
//!   `{state:"start"|"stop", trigger, source, process, detect_mode}`.
//!
//! # Daemon usage
//!
//! [`run`] is blocking and meant to be called on a dedicated thread by the
//! daemon (issue #3):
//!
//! ```no_run
//! # use std::sync::atomic::AtomicBool;
//! # fn demo(cfg: &activity_core::Config, db: &activity_core::Db,
//! #         shutdown: &AtomicBool) -> anyhow::Result<()> {
//! watcher_audio::run(cfg, db, shutdown) // blocks until shutdown
//! # }
//! ```
//!
//! # Robustness
//!
//! Device errors, hotkey registration failures and COM failures are logged
//! and absorbed: a flaky audio device can end a single recording (event
//! `capture_failed`), never the daemon. Hotkey failure leaves the control
//! file channel fully functional.
//!
//! # Platform
//!
//! Capture and detection are Windows-only (`wasapi` + `windows` crates,
//! `#[cfg(windows)]`-gated so Linux CI compiles clean). The state machine,
//! naming, retention, control-file, sink and event modules are
//! cross-platform and unit-tested everywhere.

pub mod control;
pub mod daemon;
pub mod detect;
pub mod events;
pub mod hotkey;
pub mod logging;
pub mod machine;
pub mod naming;
pub mod recording;
pub mod retention;
pub mod sink;

#[cfg(windows)]
mod win_capture;
#[cfg(windows)]
pub mod win_detector;
#[cfg(windows)]
mod win_hotkey;

pub use daemon::{
    data_dir, grace_stop_s, run, DEFAULT_GRACE_STOP_S, ENV_DATA_DIR, ENV_GRACE_STOP_S,
};
pub use detect::{ConferenceHit, ConferenceWatcher, DetectionMode};
pub use machine::{ActiveRecording, Source, StopReason, Transition, Trigger, TriggerMachine};
pub use recording::{CaptureCommand, FinalizedFile, FinishedRecording, StartSpec, StreamKind};
pub use retention::{FileInfo, RetentionBudget, MB_BYTES};
pub use sink::{AudioSink, FakeSink, WavSink};

/// Default rolling retention for meeting audio: max age in days (ADR rev 4).
pub const DEFAULT_MAX_AGE_DAYS: u64 = 60;

/// Default rolling retention for meeting audio: max total size in MB.
pub const DEFAULT_MAX_TOTAL_MB: u64 = 10 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_defaults_match_adr() {
        assert_eq!(DEFAULT_MAX_AGE_DAYS, 60);
        assert_eq!(DEFAULT_MAX_TOTAL_MB, 10 * 1024);
        // The core config defaults must agree with this crate's ADR consts.
        let cfg = activity_core::Config::default();
        assert_eq!(cfg.audio.max_age_days, DEFAULT_MAX_AGE_DAYS);
        assert_eq!(cfg.audio.max_total_mb, DEFAULT_MAX_TOTAL_MB);
    }

    #[test]
    fn grace_default_matches_issue() {
        assert_eq!(DEFAULT_GRACE_STOP_S, 10);
    }

    #[test]
    fn public_api_surface_smoke() {
        // Constructor smoke tests for the documented public types.
        let mut machine = TriggerMachine::new();
        assert!(machine.start_manual(Source::Hotkey, 0).is_some());
        assert!(machine.stop(StopReason::Manual, 1).is_some());
        let budget = RetentionBudget {
            max_age_days: DEFAULT_MAX_AGE_DAYS,
            max_total_mb: DEFAULT_MAX_TOTAL_MB,
        };
        assert!(retention::plan_deletions(&[], &budget).is_empty());
        let _fake = FakeSink::new();
        let mut null = detect::NullWatcher;
        assert!(ConferenceWatcher::poll(&mut null).is_none());
    }
}
