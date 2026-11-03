//! Pure session-coalescing state machine: samples in, ADR envelope events
//! out. No OS access and no persistence here; [`run`](crate::run) wires it to
//! a source and the core `Db`.
//!
//! # Semantics
//!
//! - Consecutive samples with the same app + title coalesce into one
//!   `window-watcher` event `{ app, title }`, emitted when the session
//!   closes: `timestamp` = session start, `duration_s` = session length.
//! - Entering AFK (input idle >= threshold) closes the open window session
//!   at the moment the user actually went idle and emits an `afk-watcher`
//!   start marker `{ state: "afk" }`.
//! - Leaving AFK emits an `afk-watcher` stop event `{ state: "active" }`
//!   spanning the whole AFK episode (`timestamp` = episode start,
//!   `duration_s` = episode length). The pair of afk events brackets one
//!   AFK episode.
//! - A wall-clock jump between polls in either direction (sleep / lid close /
//!   manual clock change) closes whatever is open at the LAST POLL time, so
//!   the gap is never attributed to an app.

use std::time::{Duration, SystemTime};

use activity_core::{EventEnvelope, BUCKET_AFK, BUCKET_WINDOW};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde_json::json;

use crate::sample::Sample;

/// Tunables for the session coalescer.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Input idle seconds beyond which the user counts as AFK.
    pub afk_threshold_s: f64,
    /// Wall-clock gap between polls (seconds, absolute) treated as sleep /
    /// suspend / clock change.
    pub gap_threshold_s: f64,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        WatcherConfig {
            afk_threshold_s: 5.0 * 60.0,
            gap_threshold_s: 120.0,
        }
    }
}

#[derive(Debug)]
enum Phase {
    /// Nothing observed yet (first poll pending).
    Unstarted,
    /// A window session is open.
    Window {
        app: String,
        title: String,
        start: SystemTime,
    },
    /// The user is AFK since `start`.
    Afk { start: SystemTime },
}

/// Pure session-coalescing state machine: samples in, event envelopes out.
pub struct SessionCoalescer {
    config: WatcherConfig,
    last_poll: Option<SystemTime>,
    phase: Phase,
}

impl SessionCoalescer {
    pub fn new(config: WatcherConfig) -> Self {
        SessionCoalescer {
            config,
            last_poll: None,
            phase: Phase::Unstarted,
        }
    }

    /// Feed one poll sample; returns the events this sample produced
    /// (usually zero or one, two when AFK starts right after activity).
    pub fn process(&mut self, sample: &Sample) -> Vec<EventEnvelope> {
        let mut events = Vec::new();

        if let Some(last) = self.last_poll {
            let gap_s = seconds_between(last, sample.ts);
            if gap_s.abs() > self.config.gap_threshold_s {
                // Sleep / suspend / clock jump: close open sessions at the
                // last poll time; the gap belongs to nothing.
                events.extend(self.close_phase_at(last));
                self.open_window(&sample.app, &sample.title, sample.ts);
                self.last_poll = Some(sample.ts);
                return events;
            }
        }

        let is_afk = sample.input_idle_s as f64 >= self.config.afk_threshold_s;
        match std::mem::replace(&mut self.phase, Phase::Unstarted) {
            Phase::Unstarted => {
                if is_afk {
                    // AFK began before the first poll; we only know "now".
                    let start = sample.ts;
                    events.push(afk_event("afk", start, 0.0));
                    self.phase = Phase::Afk { start };
                } else {
                    self.open_window(&sample.app, &sample.title, sample.ts);
                }
            }
            Phase::Window { app, title, start } => {
                if is_afk {
                    let afk_start = afk_begin(sample, start);
                    events.push(window_event(&app, &title, start, afk_start));
                    events.push(afk_event("afk", afk_start, 0.0));
                    self.phase = Phase::Afk { start: afk_start };
                } else if sample.app != app || sample.title != title {
                    events.push(window_event(&app, &title, start, sample.ts));
                    self.open_window(&sample.app, &sample.title, sample.ts);
                } else {
                    self.phase = Phase::Window { app, title, start };
                }
            }
            Phase::Afk { start } => {
                if is_afk {
                    self.phase = Phase::Afk { start };
                } else {
                    events.push(afk_event(
                        "active",
                        start,
                        seconds_between(start, sample.ts),
                    ));
                    self.open_window(&sample.app, &sample.title, sample.ts);
                }
            }
        }

        self.last_poll = Some(sample.ts);
        events
    }

    /// Close whatever session is open at the last poll time; called on
    /// daemon shutdown so the in-progress session is persisted instead of
    /// lost. Empty when nothing is open. Resets the coalescer to the
    /// unstarted state.
    pub fn flush(&mut self) -> Vec<EventEnvelope> {
        let at = self.last_poll.unwrap_or_else(SystemTime::now);
        self.close_phase_at(at)
    }

    fn open_window(&mut self, app: &str, title: &str, start: SystemTime) {
        self.phase = Phase::Window {
            app: app.to_string(),
            title: title.to_string(),
            start,
        };
    }

    /// Close whatever phase is open, anchoring the end at `at` (used for
    /// sleep gaps and shutdown flush so the gap is not attributed to the
    /// open session).
    fn close_phase_at(&mut self, at: SystemTime) -> Vec<EventEnvelope> {
        match std::mem::replace(&mut self.phase, Phase::Unstarted) {
            Phase::Unstarted => Vec::new(),
            Phase::Window { app, title, start } => {
                let end = if start < at { at } else { start };
                vec![window_event(&app, &title, start, end)]
            }
            Phase::Afk { start } => {
                let end = if start < at { at } else { start };
                vec![afk_event("active", start, seconds_between(start, end))]
            }
        }
    }
}

/// Moment the user actually went idle, clamped so sessions stay ordered.
fn afk_begin(sample: &Sample, session_start: SystemTime) -> SystemTime {
    match sample
        .ts
        .checked_sub(Duration::from_secs(sample.input_idle_s))
    {
        Some(candidate) if candidate >= session_start => candidate,
        _ => session_start,
    }
}

/// `b - a` in seconds; negative when the clock moved backwards between polls.
fn seconds_between(a: SystemTime, b: SystemTime) -> f64 {
    match b.duration_since(a) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

/// RFC3339 with the local offset (ADR envelope contract), second precision.
pub(crate) fn format_ts(ts: SystemTime) -> String {
    DateTime::<Utc>::from(ts)
        .with_timezone(&Local)
        .to_rfc3339_opts(SecondsFormat::Secs, false)
}

fn window_event(app: &str, title: &str, start: SystemTime, end: SystemTime) -> EventEnvelope {
    EventEnvelope {
        bucket: BUCKET_WINDOW.to_string(),
        timestamp: format_ts(start),
        duration_s: seconds_between(start, end),
        data: json!({ "app": app, "title": title }),
    }
}

fn afk_event(state: &str, at: SystemTime, duration_s: f64) -> EventEnvelope {
    EventEnvelope {
        bucket: BUCKET_AFK.to_string(),
        timestamp: format_ts(at),
        duration_s,
        data: json!({ "state": state }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    /// Local wall-clock 2026-11-04 h:m:s as an absolute instant. Built via
    /// the local offset so the RFC3339 assertions round-trip through
    /// `format_ts` on any machine / timezone (CI runs UTC).
    fn ts(hour: u32, minute: u32, second: u32) -> SystemTime {
        let dt = Local
            .with_ymd_and_hms(2026, 11, 4, hour, minute, second)
            .single()
            .expect("valid test timestamp");
        SystemTime::UNIX_EPOCH + Duration::from_secs(dt.timestamp().max(0) as u64)
    }

    fn sample(at: SystemTime, app: &str, title: &str, idle_s: u64) -> Sample {
        Sample {
            ts: at,
            app: app.to_string(),
            title: title.to_string(),
            input_idle_s: idle_s,
        }
    }

    #[test]
    fn app_switch_closes_previous_session_with_correct_duration() {
        let mut coalescer = SessionCoalescer::new(WatcherConfig::default());
        assert!(coalescer
            .process(&sample(ts(9, 0, 0), "Code.exe", "main.rs", 0))
            .is_empty());

        let events = coalescer.process(&sample(ts(9, 1, 30), "chrome.exe", "Docs", 0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].bucket, BUCKET_WINDOW);
        assert_eq!(events[0].timestamp, format_ts(ts(9, 0, 0)));
        assert!((events[0].duration_s - 90.0).abs() < 1e-9);
        assert_eq!(events[0].data["app"], "Code.exe");
        assert_eq!(events[0].data["title"], "main.rs");

        // Same app + title coalesces silently.
        assert!(coalescer
            .process(&sample(ts(9, 2, 0), "chrome.exe", "Docs", 0))
            .is_empty());
    }

    #[test]
    fn afk_start_and_stop_produce_afk_events() {
        let mut coalescer = SessionCoalescer::new(WatcherConfig::default());
        // Polls every 5 s from 09:02:00; the user works until 09:04:00 and
        // then goes idle (input idle grows from there).
        let start = ts(9, 2, 0);
        let work_until = ts(9, 4, 0);
        let mut events = Vec::new();
        for n in 0..85i64 {
            let t = start + Duration::from_secs((n * 5) as u64);
            let idle = if t > work_until {
                seconds_between(work_until, t) as u64
            } else {
                0
            };
            events.extend(coalescer.process(&sample(t, "Code.exe", "main.rs", idle)));
        }

        // Idle crosses the 300 s threshold at 09:09:00: the window session
        // closes where idleness began (09:04:00) and an afk start marker is
        // emitted.
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].bucket, BUCKET_WINDOW);
        assert_eq!(events[0].timestamp, format_ts(start));
        assert!((events[0].duration_s - 120.0).abs() < 1e-9);
        assert_eq!(events[1].bucket, BUCKET_AFK);
        assert_eq!(events[1].data["state"], "afk");
        assert_eq!(events[1].duration_s, 0.0);
        assert_eq!(events[1].timestamp, format_ts(work_until));

        // Still AFK: nothing new.
        assert!(coalescer
            .process(&sample(ts(9, 9, 5), "Code.exe", "main.rs", 305))
            .is_empty());

        // Back at the keyboard: afk stop event spans the whole episode.
        let events = coalescer.process(&sample(ts(9, 9, 10), "Code.exe", "main.rs", 1));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].bucket, BUCKET_AFK);
        assert_eq!(events[0].data["state"], "active");
        assert_eq!(events[0].timestamp, format_ts(work_until));
        assert!((events[0].duration_s - 310.0).abs() < 1e-9);
    }

    #[test]
    fn sleep_gap_closes_session_at_last_poll_time() {
        let mut coalescer = SessionCoalescer::new(WatcherConfig::default());
        assert!(coalescer
            .process(&sample(ts(9, 0, 0), "Code.exe", "main.rs", 0))
            .is_empty());
        assert!(coalescer
            .process(&sample(ts(9, 0, 5), "Code.exe", "main.rs", 0))
            .is_empty());

        // Laptop asleep for 90 minutes: the gap belongs to nobody.
        let events = coalescer.process(&sample(ts(10, 30, 0), "Code.exe", "main.rs", 0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].bucket, BUCKET_WINDOW);
        assert_eq!(events[0].timestamp, format_ts(ts(9, 0, 0)));
        assert!((events[0].duration_s - 5.0).abs() < 1e-9);
        assert_eq!(events[0].data["app"], "Code.exe");
    }

    #[test]
    fn sleep_gap_while_afk_closes_afk_at_last_poll() {
        let mut coalescer = SessionCoalescer::new(WatcherConfig::default());
        // First poll is already AFK: only the start marker is emitted.
        let events = coalescer.process(&sample(ts(9, 0, 0), "Code.exe", "main.rs", 400));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].bucket, BUCKET_AFK);
        assert_eq!(events[0].data["state"], "afk");

        assert!(coalescer
            .process(&sample(ts(9, 0, 5), "Code.exe", "main.rs", 405))
            .is_empty());

        // Went to sleep right after 09:00:05; next poll is two hours later.
        let events = coalescer.process(&sample(ts(11, 0, 0), "Code.exe", "main.rs", 0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].bucket, BUCKET_AFK);
        assert_eq!(events[0].data["state"], "active");
        // Closed at the last poll time (09:00:05), not at resume time.
        assert_eq!(events[0].timestamp, format_ts(ts(9, 0, 0)));
        assert!((events[0].duration_s - 5.0).abs() < 1e-9);
    }

    #[test]
    fn backwards_clock_jump_closes_session_at_last_poll() {
        let mut coalescer = SessionCoalescer::new(WatcherConfig::default());
        assert!(coalescer
            .process(&sample(ts(10, 0, 0), "Code.exe", "main.rs", 0))
            .is_empty());

        // Clock manually moved back three hours: also a wall-clock jump.
        // The old session closes with zero length (nothing was really
        // observed inside it).
        let events = coalescer.process(&sample(ts(7, 0, 0), "chrome.exe", "Docs", 0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].bucket, BUCKET_WINDOW);
        assert_eq!(events[0].timestamp, format_ts(ts(10, 0, 0)));
        assert!((events[0].duration_s - 0.0).abs() < 1e-9);
    }

    #[test]
    fn flush_closes_open_session_at_last_poll() {
        let mut coalescer = SessionCoalescer::new(WatcherConfig::default());
        // Nothing open yet: flush is a no-op.
        assert!(coalescer.flush().is_empty());

        assert!(coalescer
            .process(&sample(ts(9, 0, 0), "Code.exe", "main.rs", 0))
            .is_empty());
        assert!(coalescer
            .process(&sample(ts(9, 0, 10), "Code.exe", "main.rs", 0))
            .is_empty());

        // Daemon shutdown: the in-progress session is persisted at last-poll
        // time instead of being lost.
        let events = coalescer.flush();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].bucket, BUCKET_WINDOW);
        assert_eq!(events[0].timestamp, format_ts(ts(9, 0, 0)));
        assert!((events[0].duration_s - 10.0).abs() < 1e-9);

        // After the flush the coalescer is unstarted again.
        assert!(coalescer.flush().is_empty());
    }
}
