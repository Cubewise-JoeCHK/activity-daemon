//! Hub-side Timesheet AI pipeline (ADR addendum on issue #16, 2026-11-06).
//!
//! This crate is the storage home for the pipeline's dedicated hub tables.
//! Today it carries only the question queue (`ts_questions`, issue #44):
//! when the frame analysis (issue #43) is unsure, it leaves a question item
//! with frame evidence, which the owner answers asynchronously through the
//! hub API. The analysis worker creates questions through
//! [`storage::TsQuestionStore::create_question`].
//!
//! The store opens the same SQLite file as the core event store (the hub
//! store) and follows the core `Db` pattern: WAL mode, idempotent schema,
//! blocking rusqlite calls under a mutex, `spawn_blocking` discipline at
//! the call site. Analysis events and drafts stay in the core `events`
//! table (buckets `frame-analysis` / `timesheet-draft` per the ADR); only
//! the question queue gets its own table.

pub mod storage;

pub use storage::{EvidenceFrame, StatusFilter, TsQuestion, TsQuestionStatus, TsQuestionStore};
