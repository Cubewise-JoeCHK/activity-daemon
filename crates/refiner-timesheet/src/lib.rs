//! Hub-side Timesheet AI pipeline (ADR addendum on issue #16, 2026-11-06).
//!
//! The crate carries the pipeline's hub-side pieces that are not the
//! frame-analysis worker (issue #43):
//!
//! - [`storage`] - the question queue (`ts_questions`, issue #44): when
//!   the analysis is unsure, it leaves a question item with frame
//!   evidence, which the owner answers asynchronously through the hub
//!   API.
//! - [`compiler`] - the daily itinerary compiler (issue #45): merges a
//!   day's window sessions, frame analyses, AFK spans and answered
//!   questions into draft activities in bucket `timesheet-draft`
//!   (low-confidence spans become question items instead of activities).
//! - [`config`] - the `[timesheet]` server-config section (`use_llm_titles`
//!   and the merge cap on issue #45; the Lemonade keys shared with #43 are
//!   parsed defensively; `[timesheet.style]` per issue #53).
//! - [`llm`] - the optional Lemonade titling call (one per day, fail-soft
//!   to heuristic titles; owner style injected per the issue #53
//!   contract).
//!
//! Stores open the same SQLite file as the core event store (the hub
//! store) and follow the core `Db` pattern: WAL mode, idempotent schema,
//! blocking rusqlite calls under a mutex, `spawn_blocking` discipline at
//! the call site. Analysis events and drafts stay in the core `events`
//! table (buckets `frame-analysis` / `timesheet-draft` per the ADR); only
//! the question queue gets its own table.

pub mod compiler;
pub mod config;
pub mod llm;
pub mod storage;

pub use compiler::{
    current_draft_from_row, heuristic_title, parse_day, plan_day, store_day, ActivitySources,
    BlockTitle, BlockTitler, DayInput, DayPlan, DraftActivity, HeuristicTitler, MergedBlock,
    PlannedQuestion, StoredDay, ADJACENT_GAP_MAX_S, BUCKET_FRAME_ANALYSIS, BUCKET_TIMESHEET_DRAFT,
    CATEGORY_UNCLASSIFIED, PROJECT_UNKNOWN, RESOLVE_MIN_CONFIDENCE,
};
pub use config::{StyleConfig, StyleExample, TimesheetConfig, DEFAULT_STYLE_PROMPT};
pub use llm::{assemble_prompt, build_request_body, parse_titles, LemonadeTitler};
pub use storage::{EvidenceFrame, StatusFilter, TsQuestion, TsQuestionStatus, TsQuestionStore};
