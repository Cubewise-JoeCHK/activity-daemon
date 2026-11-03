//! Pure trigger state machine for meeting recording (#6).
//!
//! Cross-platform and unit-tested everywhere. States:
//!
//! - `idle -> recording(manual)`: hotkey toggle or CLI control file start.
//! - `idle -> recording(auto_<process>)`: conferencing detector reports an
//!   active audio session of a configured process.
//! - `recording -> idle`: explicit stop (manual always wins), auto session
//!   lost for longer than the grace period, or daemon shutdown.
//! - **No double start**: any start request while recording is ignored.
//! - **Manual stop wins**: a manual stop request stops even an auto-triggered
//!   recording.
//! - **Auto restart allowed after grace**: once the machine is idle again,
//!   a new conference session triggers a fresh recording.
//! - **AFK never stops recording**: the user may sit idle-listening in a
//!   call; `TriggerMachine::afk_tick` is a documented no-op so no caller can
//!   wire AFK to a stop.

/// What started (or labels) a recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// Manual trigger: hotkey or CLI; records mic only (in-person meetings).
    Manual,
    /// Auto trigger from a conferencing process; records mic + loopback.
    Auto(String),
}

impl Trigger {
    /// Label used in file names and event payloads (`manual`, `auto_teams`).
    pub fn label(&self) -> String {
        match self {
            Trigger::Manual => "manual".to_string(),
            Trigger::Auto(process) => format!("auto_{process}"),
        }
    }

    /// Auto-trigger records mic + system loopback; manual records mic only.
    pub fn uses_loopback(&self) -> bool {
        matches!(self, Trigger::Auto(_))
    }
}

/// Where a manual request came from (recorded in events).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Global hotkey.
    Hotkey,
    /// `record.json` control file (CLI).
    Cli,
}

impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Source::Hotkey => "hotkey",
            Source::Cli => "cli",
        }
    }
}

/// Why a recording stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Explicit manual stop (hotkey toggle or CLI stop command).
    Manual,
    /// The conferencing audio session ended and stayed ended through the
    /// grace period.
    AutoSessionLost,
    /// The capture engine reported an unrecoverable error.
    CaptureFailed,
    /// The daemon is shutting down.
    Shutdown,
}

impl StopReason {
    pub fn label(&self) -> &'static str {
        match self {
            StopReason::Manual => "manual",
            StopReason::AutoSessionLost => "auto_session_lost",
            StopReason::CaptureFailed => "capture_failed",
            StopReason::Shutdown => "shutdown",
        }
    }
}

/// A currently active recording.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveRecording {
    pub trigger: Trigger,
    pub source: Source,
    /// Unix seconds when the recording started.
    pub started_at_unix: u64,
    /// Matched conferencing process name (auto trigger only).
    pub process: Option<String>,
    /// Detection mode label (`audio-session` | `process-running`), auto only.
    pub detect_mode: Option<&'static str>,
    /// Unix seconds when the conferencing session was last seen active
    /// (grace window anchor; auto trigger only).
    pub last_auto_seen_unix: u64,
}

/// A state change emitted by the machine.
#[derive(Debug, Clone, PartialEq)]
pub enum Transition {
    Started(ActiveRecording),
    Stopped {
        recording: ActiveRecording,
        reason: StopReason,
        stopped_at_unix: u64,
    },
}

/// Pure trigger state machine. `idle <-> recording`.
#[derive(Debug, Default)]
pub struct TriggerMachine {
    active: Option<ActiveRecording>,
}

impl TriggerMachine {
    pub fn new() -> Self {
        TriggerMachine { active: None }
    }

    /// The active recording, if any.
    pub fn active(&self) -> Option<&ActiveRecording> {
        self.active.as_ref()
    }

    /// Whether the machine currently holds any recording.
    pub fn is_recording(&self) -> bool {
        self.active.is_some()
    }

    /// Request a manual recording start. Returns `None` (no change) if a
    /// recording is already active - no double start.
    pub fn start_manual(&mut self, source: Source, now_unix: u64) -> Option<Transition> {
        if self.active.is_some() {
            return None;
        }
        let recording = ActiveRecording {
            trigger: Trigger::Manual,
            source,
            started_at_unix: now_unix,
            process: None,
            detect_mode: None,
            last_auto_seen_unix: now_unix,
        };
        self.active = Some(recording.clone());
        Some(Transition::Started(recording))
    }

    /// Request an auto recording start for the given conferencing process.
    /// Returns `None` (no change) if a recording is already active - no
    /// double start (a manual in-progress recording is never preempted).
    pub fn start_auto(
        &mut self,
        process: String,
        detect_mode: &'static str,
        now_unix: u64,
    ) -> Option<Transition> {
        if self.active.is_some() {
            return None;
        }
        let recording = ActiveRecording {
            trigger: Trigger::Auto(process.clone()),
            source: Source::Hotkey, // unused for auto; kept for struct totality
            started_at_unix: now_unix,
            process: Some(process),
            detect_mode: Some(detect_mode),
            last_auto_seen_unix: now_unix,
        };
        self.active = Some(recording.clone());
        Some(Transition::Started(recording))
    }

    /// Stop the active recording for `reason`. Returns `None` if idle.
    /// Manual stop requests win over auto state: callers pass
    /// [`StopReason::Manual`] for hotkey/CLI stops regardless of what
    /// started the current recording.
    pub fn stop(&mut self, reason: StopReason, now_unix: u64) -> Option<Transition> {
        let recording = self.active.take()?;
        Some(Transition::Stopped {
            recording,
            reason,
            stopped_at_unix: now_unix,
        })
    }

    /// Report that the conferencing session is still active. Refreshes the
    /// grace window of an active auto recording; no-op otherwise.
    pub fn auto_seen(&mut self, now_unix: u64) {
        if let Some(rec) = self.active.as_mut() {
            if matches!(rec.trigger, Trigger::Auto(_)) {
                rec.last_auto_seen_unix = now_unix;
            }
        }
    }

    /// Whether an auto recording has been without a seen conference session
    /// for at least `grace_s` seconds (the stop should then be issued).
    pub fn auto_grace_expired(&self, now_unix: u64, grace_s: u64) -> bool {
        match self.active.as_ref() {
            Some(rec) if matches!(rec.trigger, Trigger::Auto(_)) => {
                now_unix.saturating_sub(rec.last_auto_seen_unix) >= grace_s
            }
            _ => false,
        }
    }

    /// AFK input: deliberately ignored. The user may be idle-listening in a
    /// call; AFK must never stop a recording (ADR rev 4 / issue #6).
    pub fn afk_tick(&mut self) {
        // Intentionally does nothing.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_manual(machine: &mut TriggerMachine) {
        let t = machine.start_manual(Source::Hotkey, 1000);
        assert!(t.is_some(), "manual start from idle must transition");
    }

    #[test]
    fn manual_start_stop_roundtrip() {
        let mut m = TriggerMachine::new();
        assert!(!m.is_recording());
        start_manual(&mut m);
        assert!(m.is_recording());
        assert_eq!(m.active().map(|a| a.trigger.clone()), Some(Trigger::Manual));
        let stopped = m.stop(StopReason::Manual, 1060).expect("stop");
        match stopped {
            Transition::Stopped {
                recording,
                reason,
                stopped_at_unix,
            } => {
                assert_eq!(recording.trigger, Trigger::Manual);
                assert_eq!(reason, StopReason::Manual);
                assert_eq!(stopped_at_unix, 1060);
                assert_eq!(recording.started_at_unix, 1000);
            }
            other => panic!("expected Stopped, got {other:?}"),
        }
        assert!(!m.is_recording());
    }

    #[test]
    fn no_double_start_manual_or_auto() {
        let mut m = TriggerMachine::new();
        start_manual(&mut m);
        assert!(m.start_manual(Source::Cli, 1001).is_none());
        assert!(m
            .start_auto("Teams".into(), "audio-session", 1002)
            .is_none());
        // And the other way around.
        let mut m2 = TriggerMachine::new();
        assert!(
            m2.start_auto("Teams".into(), "audio-session", 1000)
                .is_some(),
            "auto start from idle must transition"
        );
        assert!(m2
            .start_auto("Zoom".into(), "audio-session", 1001)
            .is_none());
        assert!(m2.start_manual(Source::Hotkey, 1002).is_none());
    }

    #[test]
    fn manual_stop_wins_over_auto_recording() {
        let mut m = TriggerMachine::new();
        assert!(m
            .start_auto("ms-teams".into(), "audio-session", 1000)
            .is_some());
        // A manual stop request stops the auto recording.
        let stopped = m.stop(StopReason::Manual, 1100).expect("manual stop wins");
        let Transition::Stopped {
            recording, reason, ..
        } = stopped
        else {
            panic!("expected Stopped");
        };
        assert_eq!(recording.trigger, Trigger::Auto("ms-teams".into()));
        assert_eq!(reason, StopReason::Manual);
        assert!(!m.is_recording());
    }

    #[test]
    fn auto_grace_expiry_stops_only_after_grace() {
        let mut m = TriggerMachine::new();
        assert!(m
            .start_auto("zoom".into(), "process-running", 1000)
            .is_some());
        // While the session is active, polls refresh the window.
        m.auto_seen(1008);
        // Session lost after t=1008; grace 10s.
        assert!(!m.auto_grace_expired(1017, 10), "within grace");
        assert!(m.is_recording());
        assert!(m.auto_grace_expired(1018, 10), "grace expired");
        // The stop itself is issued by the caller after this check.
        assert!(m.stop(StopReason::AutoSessionLost, 1018).is_some());
    }

    #[test]
    fn auto_seen_refresh_prevents_grace_stop() {
        let mut m = TriggerMachine::new();
        assert!(m.start_auto("zoom".into(), "audio-session", 1000).is_some());
        m.auto_seen(1012);
        assert!(!m.auto_grace_expired(1019, 10));
        m.auto_seen(1020);
        assert!(!m.auto_grace_expired(1029, 10));
        // Session truly gone now.
        assert!(m.auto_grace_expired(1031, 10));
    }

    #[test]
    fn auto_restart_allowed_after_grace_stop() {
        let mut m = TriggerMachine::new();
        assert!(m
            .start_auto("teams".into(), "audio-session", 1000)
            .is_some());
        assert!(m.stop(StopReason::AutoSessionLost, 1011).is_some());
        // Same conference comes back later: a fresh recording starts.
        assert!(m
            .start_auto("teams".into(), "audio-session", 3600)
            .is_some());
        assert_eq!(
            m.active().map(|a| a.started_at_unix),
            Some(3600),
            "restart must be a new recording"
        );
    }

    #[test]
    fn grace_zero_stops_immediately_after_loss() {
        let mut m = TriggerMachine::new();
        assert!(m.start_auto("zoom".into(), "audio-session", 1000).is_some());
        assert!(
            m.auto_grace_expired(1000, 0),
            "grace 0 means stop on first check"
        );
    }

    #[test]
    fn manual_recording_is_not_grace_stopped() {
        let mut m = TriggerMachine::new();
        start_manual(&mut m);
        assert!(!m.auto_grace_expired(u64::MAX, 10));
        m.auto_seen(2000);
        assert!(m.is_recording());
    }

    #[test]
    fn afk_tick_never_stops_recording() {
        let mut m = TriggerMachine::new();
        start_manual(&mut m);
        for now in 1000..2000 {
            m.afk_tick();
            assert!(m.is_recording(), "AFK at {now} must not stop recording");
        }
        assert!(m
            .start_auto("teams".into(), "audio-session", 3000)
            .is_none());
        for _ in 0..100 {
            m.afk_tick();
        }
        assert!(m.is_recording(), "AFK must not stop auto recording either");
        // Only an explicit stop ends it.
        assert!(m.stop(StopReason::Manual, 4000).is_some());
    }

    #[test]
    fn shutdown_stops_any_recording() {
        let mut m = TriggerMachine::new();
        start_manual(&mut m);
        assert!(m.stop(StopReason::Shutdown, 5000).is_some());
        assert!(!m.is_recording());
        let mut m2 = TriggerMachine::new();
        assert!(
            m2.stop(StopReason::Shutdown, 5000).is_none(),
            "stop when idle is None"
        );
    }

    #[test]
    fn trigger_labels() {
        assert_eq!(Trigger::Manual.label(), "manual");
        assert_eq!(Trigger::Auto("ms-teams".into()).label(), "auto_ms-teams");
        assert!(!Trigger::Manual.uses_loopback());
        assert!(Trigger::Auto("zoom".into()).uses_loopback());
        assert_eq!(Source::Cli.label(), "cli");
        assert_eq!(Source::Hotkey.label(), "hotkey");
        assert_eq!(StopReason::AutoSessionLost.label(), "auto_session_lost");
    }
}
