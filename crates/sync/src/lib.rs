//! Client-side sync forwarder for activity-daemon (issue #18, ADR rev 5).
//!
//! `activity-sync` is the library the daemon binary (issue #7) will call on
//! a dedicated thread: it forwards locally-stored events and media files to
//! the WSL central hub over plain HTTP on the tailnet.
//!
//! Contract:
//! - Local-first: nothing is ever deleted locally here; the daemon's rolling
//!   retention (elsewhere) only prunes files whose index rows are already
//!   synced.
//! - File first, index row after (ADR rev 5): for each unsynced `media`
//!   event the referenced file is uploaded via
//!   `PUT /ingest/media/{kind}/{date}/{filename}` BEFORE the index row is
//!   posted, so the central index never points at a missing file. A media
//!   event whose file is missing is skipped and retried later; it is never
//!   posted and never marked synced.
//! - Plain events are posted in batches to `POST /ingest/events`; only the
//!   ids the server reports as accepted are marked synced. Rejected rows
//!   stay unsynced (visible in the logs; nothing is silently dropped).
//! - When the hub is unreachable the forwarder retries with exponential
//!   backoff plus jitter (cap 5 minutes); local writes keep flowing the
//!   whole time.
//!
//! HTTP client: `ureq` - a small blocking client with rustls and JSON
//! support; the forwarder is a plain loop, so no async runtime is needed.
//!
//! Testability: every decision the loop makes (media path parsing, batch
//! building, file-before-index ordering, backoff schedule, retry
//! classification, accepted-id mapping) lives in pure functions in [`plan`],
//! unit-tested against a fake [`HttpClient`].

pub mod forwarder;
pub mod http;
pub mod plan;
pub mod settings;

pub type Result<T> = std::result::Result<T, forwarder::SyncError>;

pub use forwarder::{run, run_with_client, sync_once, SyncError, SyncStats};
pub use http::{
    HttpClient, HttpError, RetryClass, UreqClient, CONNECT_TIMEOUT_S, REQUEST_TIMEOUT_S,
};
pub use plan::{
    accepted_row_ids, apply_jitter, backoff_delay, build_post_batch, join_url, media_put_url,
    parse_media_rel_path, plan_rows, sha256_file_hex, IngestAck, MediaRef, PlannedRow,
    RejectedItem, BACKOFF_BASE, MAX_BACKOFF,
};
pub use settings::{SyncSettings, DEFAULT_BATCH_SIZE, DEFAULT_INTERVAL_S};
