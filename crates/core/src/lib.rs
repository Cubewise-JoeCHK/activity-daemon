//! Core platform for activity-daemon: the SQLite event store and TOML
//! configuration shared by all plugins (ADR, issue #16).
//!
//! The core stores raw event envelopes; it never inspects `data`. Plugins
//! write through this crate, and later read through the query API like
//! everyone else.

pub mod config;
pub mod storage;

pub use config::{AudioConfig, Config, ScreenConfig, SyncConfig};
pub use storage::{
    CoreError, Db, EventEnvelope, EventRow, BUCKET_AFK, BUCKET_MEDIA, BUCKET_WINDOW,
};
