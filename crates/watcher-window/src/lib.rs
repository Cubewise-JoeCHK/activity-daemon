//! watcher-window plugin for activity-daemon (issue #4): foreground window
//! and user presence capture.
//!
//! Layers:
//!
//! - [`Sample`]: one poll (wall-clock time, foreground app + title, input
//!   idle seconds).
//! - [`SessionCoalescer`]: pure state machine, samples in -> ADR envelope
//!   events out.
//! - [`ForegroundSource`]: the OS seam. [`WindowsSource`] implements it with
//!   real Win32 calls (GetForegroundWindow, GetWindowTextW,
//!   GetWindowThreadProcessId + QueryFullProcessImageNameW, GetLastInputInfo);
//!   every Win32 failure degrades the sample and logs a warning, it never
//!   kills the capture loop.
//! - [`run`]: the blocking capture loop the daemon binary (activity-cli,
//!   issue #7) calls. It paces the source at `config.poll_interval_s`,
//!   feeds samples through the coalescer and persists the resulting events
//!   via core's `Db` with the buckets from the ADR.
//!
//! # Semantics (ADR, issue #16)
//!
//! - Consecutive samples with the same app + title coalesce into one
//!   `window-watcher` event `{ app, title }`, emitted when the session
//!   closes: `timestamp` = session start, `duration_s` = session length.
//! - Entering AFK (input idle >= `afk_threshold_min`) closes the open window
//!   session at the moment the user actually went idle and emits an
//!   `afk-watcher` start marker `{ state: "afk" }`.
//! - Leaving AFK emits `afk-watcher` `{ state: "active" }` spanning the whole
//!   AFK episode; the pair of afk events brackets one AFK episode.
//! - A wall-clock jump between polls (sleep / hibernate / lid close) closes
//!   whatever is open at the LAST POLL time, so the gap is never attributed
//!   to an app. Samples carry real [`std::time::SystemTime`] for exactly
//!   this reason.
//!
//! # Notes
//!
//! - UIPI may truncate titles of elevated windows; accepted, traced at log
//!   level `trace!`. Exe names fall back to the empty string on failure.
//! - All Win32 code lives behind `#[cfg(windows)]`; the rest (coalescer,
//!   loop, persistence, tests) is platform-neutral and compiles on Linux CI.
//! - `run_with_source` is the injection seam: the end-to-end test drives the
//!   production loop with a scripted source on every platform.

pub mod coalescer;
pub mod run;
pub mod sample;
pub mod source;

pub use coalescer::{SessionCoalescer, WatcherConfig};
pub use run::{run, run_with_source};
pub use sample::Sample;
pub use source::ForegroundSource;

#[cfg(windows)]
pub use source::WindowsSource;
