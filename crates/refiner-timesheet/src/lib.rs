//! Hub-side Timesheet AI pipeline (ADR addendum on issue #16, 2026-11-06).
//!
//! This crate carries the pipeline's dedicated hub tables and the
//! month-end export:
//!
//! - [`storage`] - the question queue (`ts_questions`, issue #44): when
//!   the frame analysis (issue #43) is unsure, it leaves a question item
//!   with frame evidence, which the owner answers asynchronously through
//!   the hub API. The analysis worker creates questions through
//!   [`storage::TsQuestionStore::create_question`].
//! - [`export`] - the month-end xlsx export (issue #46): a month's
//!   approved activities (bucket `timesheet-approved`) assembled into the
//!   Cubewise offline-template upload format, refusals reported as
//!   warnings.
//! - [`vocab`] - the Cubewise vocabulary (projects + categories) loaded
//!   from `assets/cubewise_vocab.toml` (embedded; optional config path
//!   override), the source of truth for what the export accepts.
//!
//! The store opens the same SQLite file as the core event store (the hub
//! store) and follows the core `Db` pattern: WAL mode, idempotent schema,
//! blocking rusqlite calls under a mutex, `spawn_blocking` discipline at
//! the call site. Analysis events, drafts and approved activities stay in
//! the core `events` table (buckets `frame-analysis` / `timesheet-draft` /
//! `timesheet-approved` per the ADR); only the question queue gets its own
//! table.

pub mod export;
pub mod storage;
pub mod vocab;

pub use export::{
    build_month_grid, export_month_bytes, parse_month, write_workbook, ExportCell, ExportOutcome,
    BUCKET_TIMESHEET_APPROVED, UPLOAD_HEADERS,
};
pub use storage::{EvidenceFrame, StatusFilter, TsQuestion, TsQuestionStatus, TsQuestionStore};
pub use vocab::CubewiseVocab;
