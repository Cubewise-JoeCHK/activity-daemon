//! Shared recording types exchanged between the run loop (`daemon`) and the
//! capture worker (WASAPI on Windows). Cross-platform definitions only.

use std::path::PathBuf;

/// Which stream of a recording a sink/file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// Microphone (manual + auto trigger).
    Mic,
    /// System loopback / speakers (auto trigger only: remote voices arrive
    /// via the speakers).
    Loopback,
}

impl StreamKind {
    pub fn label(&self) -> &'static str {
        match self {
            StreamKind::Mic => "mic",
            StreamKind::Loopback => "loopback",
        }
    }
}

/// One command for the capture worker.
#[derive(Debug, Clone)]
pub enum CaptureCommand {
    Start(StartSpec),
    Stop,
}

/// What and how to record. Produced by the run loop from a state-machine
/// transition.
#[derive(Debug, Clone)]
pub struct StartSpec {
    /// Trigger label for logs and events (`manual`, `auto_ms-teams`).
    pub trigger_label: String,
    /// Matched conferencing process (auto trigger only).
    pub process: Option<String>,
    /// Manual source (`hotkey` | `cli`), empty string for auto.
    pub source: &'static str,
    /// Detection mode label for auto (`audio-session` | `process-running`).
    pub detect_mode: Option<&'static str>,
    /// Base path without stream suffix/extension (see `naming::base_path`).
    pub base_path: PathBuf,
    /// Whether to capture the system loopback in addition to the mic.
    pub with_loopback: bool,
    /// Recording start time, RFC3339 with local offset.
    pub started_rfc3339: String,
}

/// One finished (renamed into place) audio file.
#[derive(Debug, Clone, PartialEq)]
pub struct FinalizedFile {
    /// Final path on disk.
    pub path: PathBuf,
    /// Path relative to the data dir with forward slashes (index payload).
    pub rel_path: String,
    /// Size in bytes (incl. WAV header).
    pub bytes: u64,
    /// First 16 hex chars of the SHA-256 over the file content (`sha16`).
    pub sha16: String,
    /// Which stream this file holds.
    pub stream: StreamKind,
}

/// Reported by the capture worker on the done channel after a recording
/// finishes (successfully or not). Exactly one per `CaptureCommand::Start`.
#[derive(Debug, Clone)]
pub struct FinishedRecording {
    /// The spec the recording was started with.
    pub spec: StartSpec,
    /// Actual capture duration in seconds.
    pub duration_s: f64,
    /// Finalized files (may be empty if capture failed outright).
    pub files: Vec<FinalizedFile>,
    /// Non-empty when capture failed outright (no data recorded).
    pub error: Option<String>,
}
