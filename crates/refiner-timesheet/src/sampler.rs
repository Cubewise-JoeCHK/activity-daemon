//! Sampling policy (issue #43): one keyframe per window-session change plus
//! a per-clock-hour cap (defaults: keyframe mode, cap 8/hour).
//!
//! The policy state lives in memory only (skip markers are never written to
//! the store; raw media events are never mutated). [`SamplerState::decide`]
//! is a pure peek so a frame whose analysis fails is re-decided identically
//! on retry; [`SamplerState::commit_analysis`] applies the decision only
//! after the `frame-analysis` event is durably written.

use crate::config::SamplingMode;

/// Policy decision for one frame arrival.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleDecision {
    /// Sample the frame (analyse it).
    Analyse {
        /// Why it was selected: `"first-frame"`, `"window-session-change"`
        /// or `"all-mode"`. Informational (logs).
        reason: &'static str,
    },
    /// Skip the frame. Recorded in memory only, never in the store.
    Skip {
        /// Why it was skipped: `"same-window-session"` or
        /// `"hour-cap-reached"`. Informational (logs).
        reason: &'static str,
    },
}

/// Mutable policy state for one worker. Cloned per pass so a blocking pass
/// works on a snapshot (see the worker loop).
#[derive(Debug, Clone)]
pub struct SamplerState {
    mode: SamplingMode,
    max_per_hour: u32,
    /// Wall-clock hour key (in the frame's own offset) that
    /// `analysed_this_hour` counts.
    hour_key: Option<String>,
    analysed_this_hour: u32,
    /// `ts_start` of the last frame whose analysis committed.
    last_analysed_ts: Option<String>,
    /// In-memory-only count of skipped arrivals (never written to the store).
    skipped: u64,
}

impl SamplerState {
    pub fn new(mode: SamplingMode, max_per_hour: u32) -> SamplerState {
        SamplerState {
            mode,
            max_per_hour,
            hour_key: None,
            analysed_this_hour: 0,
            last_analysed_ts: None,
            skipped: 0,
        }
    }

    /// Pure peek at the decision for a frame whose instant falls in
    /// `hour_key` (`YYYY-MM-DDTHH`, computed in the frame's own timezone
    /// offset). `session_changed` says whether a `window-watcher` session
    /// started after the last committed analysis and at or before this
    /// frame; it is ignored unless the mode is keyframe.
    ///
    /// Mutates nothing: retries of a failed frame re-decide identically.
    pub fn decide(&self, hour_key: &str, session_changed: bool) -> SampleDecision {
        let analysed_this_hour = if self.hour_key.as_deref() == Some(hour_key) {
            self.analysed_this_hour
        } else {
            0
        };
        let candidate = match self.mode {
            SamplingMode::All => true,
            // The first frame ever is a session start by definition;
            // afterwards a keyframe needs a fresh window-watcher session.
            SamplingMode::Keyframe => session_changed || self.last_analysed_ts.is_none(),
        };
        if !candidate {
            return SampleDecision::Skip {
                reason: "same-window-session",
            };
        }
        if analysed_this_hour >= self.max_per_hour {
            return SampleDecision::Skip {
                reason: "hour-cap-reached",
            };
        }
        SampleDecision::Analyse {
            reason: match self.mode {
                SamplingMode::All => "all-mode",
                SamplingMode::Keyframe if self.last_analysed_ts.is_none() => "first-frame",
                _ => "window-session-change",
            },
        }
    }

    /// Apply an Analyse decision after the frame-analysis event is durable.
    pub fn commit_analysis(&mut self, hour_key: &str, ts: &str) {
        if self.hour_key.as_deref() != Some(hour_key) {
            self.hour_key = Some(hour_key.to_string());
            self.analysed_this_hour = 0;
        }
        self.analysed_this_hour += 1;
        self.last_analysed_ts = Some(ts.to_string());
    }

    /// Count one skipped arrival (in memory only).
    pub fn note_skipped(&mut self) {
        self.skipped += 1;
    }

    /// Skipped arrivals so far (this process lifetime).
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// `ts_start` of the last committed analysis, if any.
    pub fn last_analysed_ts(&self) -> Option<&str> {
        self.last_analysed_ts.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyframe(max_per_hour: u32) -> SamplerState {
        SamplerState::new(SamplingMode::Keyframe, max_per_hour)
    }

    #[test]
    fn first_frame_is_a_keyframe_then_same_session_skips() {
        let mut sampler = keyframe(8);
        assert_eq!(
            sampler.decide("2026-11-04T09", false),
            SampleDecision::Analyse {
                reason: "first-frame"
            }
        );
        sampler.commit_analysis("2026-11-04T09", "2026-11-04T09:00:05+08:00");
        // no new window session -> skip, in memory only
        assert_eq!(
            sampler.decide("2026-11-04T09", false),
            SampleDecision::Skip {
                reason: "same-window-session"
            }
        );
        assert_eq!(sampler.skipped(), 0, "decide alone must not count skips");
        sampler.note_skipped();
        assert_eq!(sampler.skipped(), 1);
        assert_eq!(
            sampler.last_analysed_ts(),
            Some("2026-11-04T09:00:05+08:00")
        );
    }

    #[test]
    fn session_change_reselects_until_the_hour_cap_hits() {
        let mut sampler = keyframe(2);
        // the very first decision is the first-frame rule...
        assert_eq!(
            sampler.decide("2026-11-04T09", true),
            SampleDecision::Analyse {
                reason: "first-frame"
            }
        );
        sampler.commit_analysis("2026-11-04T09", "2026-11-04T09:00:10+08:00");
        // ...afterwards session changes select keyframes
        assert_eq!(
            sampler.decide("2026-11-04T09", true),
            SampleDecision::Analyse {
                reason: "window-session-change"
            }
        );
        sampler.commit_analysis("2026-11-04T09", "2026-11-04T09:00:20+08:00");
        // third candidate in the same clock hour: cap reached
        assert_eq!(
            sampler.decide("2026-11-04T09", true),
            SampleDecision::Skip {
                reason: "hour-cap-reached"
            }
        );
        // committing skips is not a thing; the cap still holds
        assert_eq!(
            sampler.decide("2026-11-04T09", true),
            SampleDecision::Skip {
                reason: "hour-cap-reached"
            }
        );
    }

    #[test]
    fn hour_rollover_resets_the_cap() {
        let mut sampler = keyframe(1);
        sampler.decide("2026-11-04T09", true);
        sampler.commit_analysis("2026-11-04T09", "2026-11-04T09:59:00+08:00");
        assert_eq!(
            sampler.decide("2026-11-04T09", true),
            SampleDecision::Skip {
                reason: "hour-cap-reached"
            }
        );
        // next wall-clock hour: cap budget is fresh again
        assert_eq!(
            sampler.decide("2026-11-04T10", true),
            SampleDecision::Analyse {
                reason: "window-session-change"
            }
        );
    }

    #[test]
    fn all_mode_analyses_every_arrival_within_the_cap() {
        let mut sampler = SamplerState::new(SamplingMode::All, 1);
        assert_eq!(
            sampler.decide("2026-11-04T09", false),
            SampleDecision::Analyse { reason: "all-mode" }
        );
        sampler.commit_analysis("2026-11-04T09", "2026-11-04T09:00:05+08:00");
        assert_eq!(
            sampler.decide("2026-11-04T09", false),
            SampleDecision::Skip {
                reason: "hour-cap-reached"
            }
        );
    }

    #[test]
    fn zero_cap_disables_analysis() {
        let mut sampler = keyframe(0);
        assert_eq!(
            sampler.decide("2026-11-04T09", true),
            SampleDecision::Skip {
                reason: "hour-cap-reached"
            }
        );
        assert_eq!(sampler.skipped(), 0);
        sampler.note_skipped();
        assert_eq!(sampler.skipped(), 1);
        assert_eq!(sampler.last_analysed_ts(), None);
    }

    #[test]
    fn decide_is_pure_across_hour_rollovers() {
        // crossing into a new hour in decide() alone must not consume the
        // new budget: only commit_analysis moves state
        let mut sampler = keyframe(1);
        sampler.decide("2026-11-04T09", true);
        sampler.commit_analysis("2026-11-04T09", "2026-11-04T09:59:00+08:00");
        assert_eq!(
            sampler.decide("2026-11-04T10", true),
            SampleDecision::Analyse {
                reason: "window-session-change"
            }
        );
        assert_eq!(
            sampler.decide("2026-11-04T10", true),
            SampleDecision::Analyse {
                reason: "window-session-change"
            }
        );
        sampler.commit_analysis("2026-11-04T10", "2026-11-04T10:00:30+08:00");
        assert_eq!(
            sampler.decide("2026-11-04T10", true),
            SampleDecision::Skip {
                reason: "hour-cap-reached"
            }
        );
    }
}
