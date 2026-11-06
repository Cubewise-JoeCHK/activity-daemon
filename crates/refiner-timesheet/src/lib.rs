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
//! - [`storage`] - the question queue (`ts_questions`, issue #44): when
//!   the frame analysis is unsure, it leaves a question item with frame
//!   evidence, which the owner answers asynchronously through the hub API.
//! - [`export`] - the month-end xlsx export (issue #46): a month's
//!   approved activities (bucket `timesheet-approved`) assembled into the
//!   Cubewise offline-template upload format, refusals reported as
//!   warnings.
//! - [`vocab`] - the Cubewise vocabulary (projects + categories) loaded
//!   from `assets/cubewise_vocabulary.toml` (embedded; optional config
//!   path override), the shared source of truth for what the worker's
//!   model may answer (issue #43) and what the export accepts (issue #46).
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
//! Storage split: analysis events, drafts and approved activities stay in
//! the core `events` table (buckets `frame-analysis` / `timesheet-draft` /
//! `timesheet-approved` per the ADR); only the question queue gets its own
//! table (`ts_questions`, issue #44). The store opens the same SQLite file
//! as the core event store (the hub store) and follows the core `Db`
//! pattern: WAL mode, idempotent schema, blocking rusqlite calls under a
//! mutex, `spawn_blocking` discipline at the call site.
//!
//! Daily itinerary compiler (issue #45, [`compiler`]): merges a day's
//! window sessions, frame analyses, AFK spans and answered questions into
//! draft activities in bucket `timesheet-draft` (low-confidence spans
//! become question items instead of activities). Its optional Lemonade
//! titling call ([`llm`], one per day, fail-soft to heuristic titles;
//! owner style injected per the issue #53 contract) shares the
//! `[timesheet]` config section with the worker.

pub mod compiler;
pub mod config;
pub mod export;
pub mod lemonade;
pub mod llm;
pub mod sampler;
pub mod storage;
pub mod vocab;
pub mod worker;

pub use compiler::{
    current_draft_from_row, heuristic_title, parse_day, plan_day, store_day, ActivitySources,
    BlockTitle, BlockTitler, DayInput, DayPlan, DraftActivity, HeuristicTitler, MergedBlock,
    PlannedQuestion, StoredDay, ADJACENT_GAP_MAX_S, BUCKET_FRAME_ANALYSIS, BUCKET_TIMESHEET_DRAFT,
    CATEGORY_UNCLASSIFIED, PROJECT_UNKNOWN, RESOLVE_MIN_CONFIDENCE,
};
pub use config::{SamplingMode, StyleConfig, StyleExample, TimesheetConfig, DEFAULT_STYLE_PROMPT};
pub use export::{
    build_month_grid, export_month_bytes, parse_month, write_workbook, ExportCell, ExportOutcome,
    BUCKET_TIMESHEET_APPROVED, UPLOAD_HEADERS,
};
pub use lemonade::{FrameAnalysis, LemonadeClient, LemonadeError};
pub use llm::{assemble_prompt, build_request_body, parse_titles, LemonadeTitler};
pub use sampler::{SampleDecision, SamplerState};
pub use storage::{EvidenceFrame, StatusFilter, TsQuestion, TsQuestionStatus, TsQuestionStore};
pub use vocab::CubewiseVocab;
pub use worker::{run_worker, BatchOutcome, WorkerContext};

#[cfg(test)]
mod tests {
    use super::*;

    /// The template seed that ships with the crate must always parse and
    /// keep the full extracted vocabulary (7 categories / 189 projects,
    /// issue #43) - a truncated seed would silently narrow every analysis
    /// and every export.
    #[test]
    fn embedded_vocabulary_seed_is_complete() {
        let vocab = CubewiseVocab::embedded();
        assert_eq!(vocab.categories.len(), 7);
        assert_eq!(vocab.projects.len(), 189);
        assert_eq!(vocab.categories[0], "Consulting");
        assert_eq!(vocab.categories[6], "Customer Returned");
    }
}
