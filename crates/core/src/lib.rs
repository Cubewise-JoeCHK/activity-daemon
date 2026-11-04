//! Core platform for activity-daemon: the SQLite event store and TOML
//! configuration shared by all plugins (ADR, issue #16).
//!
//! The core stores raw event envelopes; it never inspects `data`. Plugins
//! write through this crate, and later read through the query API like
//! everyone else. Media files are indexed as ordinary events in the `media`
//! bucket (ADR convention); the files themselves stay owned by their
//! producer plugin and the central archive.

pub mod config;
pub mod storage;

pub use config::{AudioConfig, Config, HideConsole, RunConfig, ScreenConfig, SyncConfig};
pub use storage::{
    CoreError, Db, Event, EventEnvelope, EventRow, BUCKET_AFK, BUCKET_MEDIA, BUCKET_WINDOW,
};
