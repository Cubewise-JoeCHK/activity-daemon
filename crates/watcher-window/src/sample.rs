//! The per-poll data type shared by the sampling source and the coalescer.

use std::time::SystemTime;

/// One foreground/presence poll sample, handed from a
/// [`ForegroundSource`](crate::ForegroundSource) to the
/// [`SessionCoalescer`](crate::SessionCoalescer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// Wall-clock time of the poll. Real `SystemTime` is passed through on
    /// purpose: after sleep / hibernate the jump between polls is what lets
    /// the coalescer close sessions at last-poll time (issue #4).
    pub ts: SystemTime,
    /// Foreground process/app name (empty string when the OS call failed).
    pub app: String,
    /// Foreground window title (empty when unavailable; UIPI truncates
    /// titles of elevated windows).
    pub title: String,
    /// Seconds since the last user input (keyboard/mouse), as reported by
    /// the OS.
    pub input_idle_s: u64,
}
