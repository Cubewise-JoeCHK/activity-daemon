//! Hub-side Timesheet AI pipeline (ADR addendum on issue #16, 2026-11-06).
//!
//! This crate carries the pipeline's hub-side pieces that are not plain
//! event rows:
//!
//! - The **frame analysis worker** (issue #43, [`worker`]): new arrivals in
//!   the `media` bucket with `data.kind == "frame"` -> configurable
//!   sampling policy ([`sampler`]) -> Lemonade analysis ([`lemonade`],
//!   OpenAI-compatible chat completions over the tailnet) -> one
//!   `frame-analysis` event per analysed frame via the normal core insert.
//!   Low-confidence answers are handed off to the question queue
//!   ([`storage`], `TsQuestionStore::create_question`).
//! - The **question queue store** (issue #44): the owner answers created
//!   questions asynchronously through the hub API.
//!
//! Failure contract (issue #43): a dead or slow Lemonade costs analysis,
//! not capture. The worker never blocks ingestion (it runs in its own
//! tokio task, all SQLite and HTTP work inside `spawn_blocking`) and
//! retries with capped exponential backoff, re-deciding the same frame
//! until it succeeds.
//!
//! Naming discipline (ADR addendum on #16): bucket `frame-analysis`, config
//! section `[timesheet]` (part of the server config file, resolved by the
//! server binary). Sampling keeps its skip markers in memory only; raw
//! media events are never mutated. The arrival cursor is the one durable
//! piece of worker state (core `refiner_cursors` table) so a hub restart
//! resumes instead of re-analysing history.
//!
//! Storage split: analysis events stay in the core `events` table; only
//! the question queue gets its own table (`ts_questions`, issue #44). The
//! store opens the same SQLite file as the core event store (the hub
//! store) and follows the core `Db` pattern: WAL mode, idempotent schema,
//! blocking rusqlite calls under a mutex, `spawn_blocking` discipline at
//! the call site.

pub mod config;
pub mod lemonade;
pub mod sampler;
pub mod storage;
pub mod vocab;
pub mod worker;

pub use config::{SamplingMode, StyleConfig, StyleExample, TimesheetConfig};
pub use lemonade::{FrameAnalysis, LemonadeClient, LemonadeError};
pub use sampler::{SampleDecision, SamplerState};
pub use storage::{EvidenceFrame, StatusFilter, TsQuestion, TsQuestionStatus, TsQuestionStore};
pub use vocab::Vocabulary;
pub use worker::{run_worker, BatchOutcome, WorkerContext};

#[cfg(test)]
mod tests {
    use super::*;

    /// The template seed that ships with the crate must always parse and
    /// keep the full extracted vocabulary (7 categories / 189 projects,
    /// issue #43) - a truncated seed would silently narrow every analysis.
    #[test]
    fn embedded_vocabulary_seed_is_complete() {
        let vocab = Vocabulary::embedded().expect("embedded seed parses");
        assert_eq!(vocab.categories().len(), 7);
        assert_eq!(vocab.projects().len(), 189);
        assert_eq!(vocab.categories()[0], "Consulting");
        assert_eq!(vocab.categories()[6], "Customer Returned");
    }
}
