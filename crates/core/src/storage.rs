//! SQLite event storage (WAL mode) per the ADR storage contract.
//!
//! Schema (`events` table): `id INTEGER PRIMARY KEY, bucket TEXT NOT NULL,
//! ts_start TEXT NOT NULL, duration_s REAL NOT NULL, data TEXT NOT NULL,
//! device TEXT NOT NULL, synced INTEGER NOT NULL DEFAULT 0`.
//!
//! `data` holds the JSON payload of the envelope as opaque text. `synced = 0`
//! means "not yet forwarded to the central hub" on a device, and is marked
//! on the central server after append (rows arriving over the wire are by
//! definition synced).
//!
//! Indexes (created idempotently by [`Db::open_db`]): `(bucket, ts_start)`,
//! `(device, ts_start)` and `(synced)`.
//!
//! Media convention (ADR): media files live on disk under
//! `media/<kind>/<YYYY>/<MM>/<DD>/` and are indexed as ordinary events in the
//! `media` bucket via [`Db::insert_media_index`]. A media index row is just an
//! event - the core stores the index, the producer plugin owns the file.

use std::path::Path;

use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};
use rusqlite::{params, params_from_iter, Connection};
use serde::{Deserialize, Serialize};

/// Well-known buckets (ADR v0.1). Buckets are open-ended; plugins may invent
/// their own names without any core change.
pub const BUCKET_WINDOW: &str = "window-watcher";
pub const BUCKET_AFK: &str = "afk-watcher";
pub const BUCKET_MEDIA: &str = "media";

#[derive(Debug)]
pub enum CoreError {
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
    Config(String),
    /// Input validation failure (empty bucket, timestamp without a timezone
    /// offset, non-finite duration, invalid JSON data, ...).
    Validation(String),
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreError::Sqlite(e) => write!(f, "sqlite error: {e}"),
            CoreError::Json(e) => write!(f, "json error: {e}"),
            CoreError::Io(e) => write!(f, "io error: {e}"),
            CoreError::Config(msg) => write!(f, "config error: {msg}"),
            CoreError::Validation(msg) => write!(f, "validation error: {msg}"),
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CoreError::Sqlite(e) => Some(e),
            CoreError::Json(e) => Some(e),
            CoreError::Io(e) => Some(e),
            CoreError::Config(_) => None,
            CoreError::Validation(_) => None,
        }
    }
}

impl From<rusqlite::Error> for CoreError {
    fn from(e: rusqlite::Error) -> Self {
        CoreError::Sqlite(e)
    }
}

impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        CoreError::Json(e)
    }
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        CoreError::Io(e)
    }
}

impl From<toml::de::Error> for CoreError {
    fn from(e: toml::de::Error) -> Self {
        CoreError::Config(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// Event envelope contract (ADR). The timestamp is RFC3339 with the local
/// offset (e.g. `2026-11-03T09:14:32+08:00`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub bucket: String,
    pub timestamp: String,
    pub duration_s: f64,
    pub data: serde_json::Value,
}

/// A stored event row as read back from the database.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRow {
    pub id: i64,
    pub bucket: String,
    pub ts_start: String,
    pub duration_s: f64,
    pub data: String,
    pub device: String,
    pub synced: bool,
}

/// An event envelope plus its owning device: the full unit of storage, query
/// and sync. Serializes to/from JSON for the wire format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub bucket: String,
    /// RFC3339 timestamp with a timezone offset
    /// (e.g. `2026-11-03T09:14:32+08:00` or `...Z`).
    pub timestamp: String,
    pub duration_s: f64,
    pub data: serde_json::Value,
    pub device: String,
}

/// Per-device summary for hub-side status views: how many events a device
/// has contributed and when the hub last heard from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceOverview {
    pub device: String,
    pub events_total: i64,
    /// `ts_start` of the most recently ARRIVED row (`MAX(id)`): when the hub
    /// last heard from this device. This is arrival recency, not the newest
    /// event instant - batches can carry older events.
    pub last_seen: Option<String>,
}

impl EventEnvelope {
    /// Validate the envelope: bucket non-empty, timestamp parses as RFC3339
    /// with a timezone offset, duration finite, and `data` is valid JSON.
    /// Returns [`CoreError::Validation`] on the first failure.
    pub fn validate(&self) -> Result<()> {
        validate_bucket(&self.bucket)?;
        validate_timestamp(&self.timestamp)?;
        validate_duration(self.duration_s)?;
        validate_data_json(&serde_json::to_string(&self.data)?)
    }
}

impl Event {
    /// Same validation rules as [`EventEnvelope::validate`].
    pub fn validate(&self) -> Result<()> {
        validate_bucket(&self.bucket)?;
        validate_timestamp(&self.timestamp)?;
        validate_duration(self.duration_s)?;
        validate_data_json(&serde_json::to_string(&self.data)?)
    }

    /// The envelope part of this event (device dropped), ready for
    /// [`Db::insert_event`].
    pub fn envelope(&self) -> EventEnvelope {
        EventEnvelope {
            bucket: self.bucket.clone(),
            timestamp: self.timestamp.clone(),
            duration_s: self.duration_s,
            data: self.data.clone(),
        }
    }

    /// Rebuild a row-shaped view of this event under the given `id` and sync
    /// state (useful for round-tripping and wire serialization).
    pub fn to_row(&self, id: i64, synced: bool) -> EventRow {
        EventRow {
            id,
            bucket: self.bucket.clone(),
            ts_start: self.timestamp.clone(),
            duration_s: self.duration_s,
            data: serde_json::to_string(&self.data).unwrap_or_else(|_| "null".to_string()),
            device: self.device.clone(),
            synced,
        }
    }
}

impl TryFrom<&EventRow> for Event {
    type Error = CoreError;

    fn try_from(row: &EventRow) -> Result<Self> {
        Ok(Event {
            bucket: row.bucket.clone(),
            timestamp: row.ts_start.clone(),
            duration_s: row.duration_s,
            data: serde_json::from_str(&row.data)?,
            device: row.device.clone(),
        })
    }
}

/// Handle to the local (or central) SQLite event store.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (creating if needed) the database file in WAL mode and ensure
    /// the schema and indexes exist.
    pub fn open_db(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        Db::init(conn)
    }

    /// In-memory database, useful for tests and throwaway queries.
    pub fn open_in_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        Db::init(conn)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY,
                bucket TEXT NOT NULL,
                ts_start TEXT NOT NULL,
                duration_s REAL NOT NULL,
                data TEXT NOT NULL,
                device TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_events_bucket_ts ON events (bucket, ts_start);
            CREATE INDEX IF NOT EXISTS idx_events_device_ts ON events (device, ts_start);
            CREATE INDEX IF NOT EXISTS idx_events_synced ON events (synced);",
        )?;
        Ok(Db { conn })
    }

    /// Append-only insert of one event envelope for the given device.
    /// Returns the new row id.
    ///
    /// The envelope is validated first; invalid input (empty bucket, a
    /// timestamp that is not RFC3339 with a timezone offset, a non-finite
    /// duration, or `data` that is not valid JSON) is rejected with
    /// [`CoreError::Validation`] and nothing is written.
    pub fn insert_event(&self, event: &EventEnvelope, device: &str) -> Result<i64> {
        event.validate()?;
        let data_json = serde_json::to_string(&event.data)?;
        self.conn.execute(
            "INSERT INTO events (bucket, ts_start, duration_s, data, device)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.bucket,
                event.timestamp,
                event.duration_s,
                data_json,
                device
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Index one media file as an event in the `media` bucket (ADR
    /// convention): the payload is `{ kind, path, bytes, sha16, producer }`
    /// and the file itself lives on disk under
    /// `media/<kind>/<YYYY>/<MM>/<DD>/`, written and owned by the producer
    /// plugin (rolling retention on the device per ADR rev 2, keep-forever
    /// archive at the central hub per rev 5). The core never touches the
    /// file - this row is just an event like any other.
    ///
    /// `sha16` is the content-hash prefix that lets producers (e.g.
    /// watcher-screen) skip duplicate frames; `timestamp` is taken at insert
    /// time (UTC, RFC3339 with offset). Returns the new row id.
    pub fn insert_media_index(
        &self,
        kind: &str,
        path: &str,
        bytes: u64,
        sha16: &str,
        producer: &str,
        device: &str,
    ) -> Result<i64> {
        let event = EventEnvelope {
            bucket: BUCKET_MEDIA.to_string(),
            timestamp: now_rfc3339(),
            duration_s: 0.0,
            data: serde_json::json!({
                "kind": kind,
                "path": path,
                "bytes": bytes,
                "sha16": sha16,
                "producer": producer,
            }),
        };
        self.insert_event(&event, device)
    }

    /// Query events in `bucket` whose timestamp instant lies within
    /// `from_ts..=to_ts` (both bounds optional; RFC3339 with a timezone
    /// offset; inclusive). Rows are ordered by timestamp instant, ties keep
    /// insertion order.
    ///
    /// Bounds are compared as parsed instants, so events stored with
    /// different timezone offsets still compare correctly. Events with an
    /// unparseable stored timestamp (impossible through
    /// [`Db::insert_event`]) fail the query rather than being skipped
    /// silently.
    pub fn query_events(
        &self,
        bucket: &str,
        from_ts: Option<&str>,
        to_ts: Option<&str>,
    ) -> Result<Vec<Event>> {
        self.query_events_in(Some(bucket), None, from_ts, to_ts)
    }

    /// Generalized instant-window query: like [`Db::query_events`], but
    /// bucket and device are both optional filters (`None` = all buckets /
    /// all devices). This is the read path for the hub's web APIs, where a
    /// calendar wants "one device, all buckets" and a machine summary wants
    /// "everything since midnight".
    pub fn query_events_in(
        &self,
        bucket: Option<&str>,
        device: Option<&str>,
        from_ts: Option<&str>,
        to_ts: Option<&str>,
    ) -> Result<Vec<Event>> {
        self.query_event_rows_in(bucket, device, from_ts, to_ts)?
            .iter()
            .map(Event::try_from)
            .collect::<Result<Vec<Event>>>()
    }

    /// Row-level variant of [`Db::query_events_in`]: identical filtering and
    /// ordering, but returns raw [`EventRow`]s (carrying row ids) instead of
    /// parsed [`Event`]s. The Timesheet AI compiler (issue #45) cites row ids
    /// as activity evidence sources (`session_ids`), and hub-side refinement
    /// paths rewrite row payloads in place (draft supersession), which the
    /// id-less `Event` view cannot express.
    pub fn query_event_rows_in(
        &self,
        bucket: Option<&str>,
        device: Option<&str>,
        from_ts: Option<&str>,
        to_ts: Option<&str>,
    ) -> Result<Vec<EventRow>> {
        if let Some(bucket) = bucket {
            validate_bucket(bucket)?;
        }
        let from = from_ts.map(parse_rfc3339).transpose()?;
        let to = to_ts.map(parse_rfc3339).transpose()?;
        let mut sql = String::from(
            "SELECT id, bucket, ts_start, duration_s, data, device, synced FROM events",
        );
        let mut clauses: Vec<&str> = Vec::new();
        if bucket.is_some() {
            clauses.push("bucket = ?");
        }
        if device.is_some() {
            clauses.push("device = ?");
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY ts_start ASC, id ASC");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut bind: Vec<String> = Vec::new();
        if let Some(bucket) = bucket {
            bind.push(bucket.to_string());
        }
        if let Some(device) = device {
            bind.push(device.to_string());
        }
        let mut hits: Vec<(DateTime<FixedOffset>, EventRow)> = Vec::new();
        for row in stmt.query_map(params_from_iter(bind.iter()), map_row)? {
            let row = row?;
            let ts = parse_rfc3339(&row.ts_start)?;
            if from.is_some_and(|f| ts < f) || to.is_some_and(|t| ts > t) {
                continue;
            }
            hits.push((ts, row));
        }
        // `ORDER BY ts_start` is lexicographic, which is wrong across
        // timezone offsets; sort by parsed instant instead (stable, so ties
        // keep the insertion order above).
        hits.sort_by_key(|(ts, _)| *ts);
        Ok(hits.into_iter().map(|(_, row)| row).collect())
    }

    /// Replace the stored `data` JSON of one event row in place. Hub-side
    /// refinement writes only: the daily itinerary compiler (issue #45) marks
    /// superseded drafts this way; events stay append-only everywhere else.
    /// The JSON string is validated before the write (same rule as insert).
    /// Returns `false` when the id is unknown, `true` when the row updated.
    pub fn update_event_data(&self, id: i64, data_json: &str) -> Result<bool> {
        validate_data_json(data_json)?;
        let updated = self.conn.execute(
            "UPDATE events SET data = ?2 WHERE id = ?1",
            params![id, data_json],
        )?;
        Ok(updated > 0)
    }

    /// Most recently arrived events, newest first, optionally narrowed to
    /// one device. Ordered by insertion (`id`) - the honest "what did the
    /// hub learn lately" order for a feed - not by event timestamp, which
    /// batches can carry out of order.
    pub fn latest_events(&self, device: Option<&str>, limit: u32) -> Result<Vec<Event>> {
        let sql = match device {
            Some(_) => {
                "SELECT id, bucket, ts_start, duration_s, data, device, synced
                 FROM events WHERE device = ?1 ORDER BY id DESC LIMIT ?2"
            }
            None => {
                "SELECT id, bucket, ts_start, duration_s, data, device, synced
                 FROM events ORDER BY id DESC LIMIT ?1"
            }
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = Vec::new();
        match device {
            Some(device) => {
                for row in stmt.query_map(params![device, limit], map_row)? {
                    rows.push(Event::try_from(&row?)?);
                }
            }
            None => {
                for row in stmt.query_map(params![limit], map_row)? {
                    rows.push(Event::try_from(&row?)?);
                }
            }
        }
        Ok(rows)
    }

    /// Count events contributed by `device` whose timestamp instant is at or
    /// after `from_ts` (inclusive; RFC3339 with offset). Timestamps are
    /// compared as parsed instants - a device legitimately mixes offsets
    /// (window events carry the local offset, media index rows are UTC), so
    /// this cannot be a lexical SQL comparison.
    pub fn count_device_events_since(&self, device: &str, from_ts: &str) -> Result<i64> {
        let from = parse_rfc3339(from_ts)?;
        let mut stmt = self
            .conn
            .prepare("SELECT ts_start FROM events WHERE device = ?1")?;
        let mut count = 0i64;
        for ts in stmt.query_map(params![device], |r| r.get::<_, String>(0))? {
            if parse_rfc3339(&ts?)? >= from {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Per-device summaries for hub status views: total events and the last
    /// arrival per device (see [`DeviceOverview::last_seen`]).
    pub fn device_overview(&self) -> Result<Vec<DeviceOverview>> {
        let mut stmt = self.conn.prepare(
            "SELECT device, COUNT(*), MAX(id) FROM events GROUP BY device ORDER BY device",
        )?;
        let mut rows: Vec<(String, i64, i64)> = Vec::new();
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })? {
            rows.push(row?);
        }
        let mut out = Vec::with_capacity(rows.len());
        for (device, events_total, max_id) in rows {
            let last_seen = self
                .conn
                .query_row(
                    "SELECT ts_start FROM events WHERE id = ?1",
                    params![max_id],
                    |r| r.get::<_, String>(0),
                )
                .ok();
            out.push(DeviceOverview {
                device,
                events_total,
                last_seen,
            });
        }
        Ok(out)
    }

    /// Mark the given row ids as synced (after successful forwarding to the
    /// central hub). Empty input is a no-op.
    pub fn mark_synced(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!("UPDATE events SET synced = 1 WHERE id IN ({placeholders})");
        self.conn.execute(&sql, params_from_iter(ids.iter()))?;
        Ok(())
    }

    /// Fetch up to `limit` unsynced events in insertion order (forwarder
    /// batch input).
    pub fn fetch_unsynced_events(&self, limit: u32) -> Result<Vec<EventRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, bucket, ts_start, duration_s, data, device, synced
             FROM events WHERE synced = 0 ORDER BY id LIMIT ?1",
        )?;
        let mut rows = Vec::new();
        for row in stmt.query_map(params![limit], map_row)? {
            rows.push(row?);
        }
        Ok(rows)
    }

    /// Total number of events.
    pub fn event_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?)
    }

    /// Number of events not yet synced to the central hub.
    pub fn unsynced_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM events WHERE synced = 0", [], |r| {
                r.get(0)
            })?)
    }

    /// Event counts grouped by bucket (status reporting stub).
    pub fn counts_by_bucket(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT bucket, COUNT(*) FROM events GROUP BY bucket ORDER BY bucket")?;
        let mut out = Vec::new();
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            out.push(row?);
        }
        Ok(out)
    }

    /// Event counts grouped by `(device, bucket)`: the per-device profile
    /// view for the hub's web UI (cheap aggregate over the
    /// `(device, ts_start)` index).
    pub fn counts_by_device_bucket(&self) -> Result<Vec<(String, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT device, bucket, COUNT(*) FROM events GROUP BY device, bucket ORDER BY device, bucket",
        )?;
        let mut out = Vec::new();
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })? {
            out.push(row?);
        }
        Ok(out)
    }
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        id: row.get(0)?,
        bucket: row.get(1)?,
        ts_start: row.get(2)?,
        duration_s: row.get(3)?,
        data: row.get(4)?,
        device: row.get(5)?,
        synced: row.get::<_, i64>(6)? != 0,
    })
}

fn validate_bucket(bucket: &str) -> Result<()> {
    if bucket.is_empty() {
        return Err(CoreError::Validation(
            "bucket must be non-empty".to_string(),
        ));
    }
    Ok(())
}

/// Parse an RFC3339 timestamp that must carry a timezone offset
/// (`+hh:mm` or `Z`). Naive datetimes are rejected.
fn parse_rfc3339(ts: &str) -> Result<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(ts).map_err(|e| {
        CoreError::Validation(format!(
            "timestamp '{ts}' is not RFC3339 with a timezone offset: {e}"
        ))
    })
}

fn validate_timestamp(ts: &str) -> Result<()> {
    parse_rfc3339(ts).map(|_| ())
}

/// SQLite stores NaN as NULL, which would poison later reads, so non-finite
/// durations are rejected up front.
fn validate_duration(duration_s: f64) -> Result<()> {
    if duration_s.is_finite() {
        Ok(())
    } else {
        Err(CoreError::Validation(
            "duration_s must be a finite number".to_string(),
        ))
    }
}

fn validate_data_json(data_json: &str) -> Result<()> {
    serde_json::from_str::<serde_json::Value>(data_json)
        .map(|_| ())
        .map_err(|e| CoreError::Validation(format!("data must be valid JSON: {e}")))
}

/// Current UTC time as RFC3339 with offset (used for media index rows).
fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("activity-daemon-test-{tag}-{nanos}.db"))
    }

    fn cleanup(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn sample_event() -> EventEnvelope {
        EventEnvelope {
            bucket: BUCKET_WINDOW.to_string(),
            timestamp: "2026-11-04T09:14:32+08:00".to_string(),
            duration_s: 123.4,
            data: serde_json::json!({"app": "Code.exe", "title": "main.rs - activity-daemon"}),
        }
    }

    fn insert_at(db: &Db, bucket: &str, ts: &str, duration_s: f64) -> i64 {
        let env = EventEnvelope {
            bucket: bucket.to_string(),
            timestamp: ts.to_string(),
            duration_s,
            data: serde_json::json!({"marker": ts}),
        };
        db.insert_event(&env, "dev-laptop").expect("insert")
    }

    use std::path::PathBuf;

    #[test]
    fn schema_insert_fetch_roundtrip_on_temp_file_db() {
        let path = temp_db_path("roundtrip");
        let env = sample_event();
        let id = {
            let db = Db::open_db(&path).expect("open temp db");
            let id = db.insert_event(&env, "dev-laptop").expect("insert");
            assert_eq!(db.event_count().expect("count"), 1);
            assert_eq!(db.unsynced_count().expect("unsynced"), 1);
            id
        }; // drop the connection, then reopen to prove persistence

        let db = Db::open_db(&path).expect("reopen temp db");
        let rows = db.fetch_unsynced_events(10).expect("fetch");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.id, id);
        assert_eq!(row.bucket, BUCKET_WINDOW);
        assert_eq!(row.ts_start, "2026-11-04T09:14:32+08:00");
        assert!((row.duration_s - 123.4).abs() < 1e-9);
        assert_eq!(row.device, "dev-laptop");
        assert!(!row.synced);
        let parsed: serde_json::Value = serde_json::from_str(&row.data).expect("data is json");
        assert_eq!(parsed["app"], "Code.exe");
        assert_eq!(parsed["title"], "main.rs - activity-daemon");

        db.mark_synced(&[row.id]).expect("mark synced");
        assert_eq!(db.unsynced_count().expect("unsynced after mark"), 0);
        assert!(db.fetch_unsynced_events(10).expect("fetch").is_empty());
        drop(db);
        cleanup(&path);
    }

    #[test]
    fn counts_by_bucket_and_batches() {
        let db = Db::open_in_memory().expect("in-memory db");
        for i in 0..5 {
            let mut env = sample_event();
            if i % 2 == 0 {
                env.bucket = BUCKET_AFK.to_string();
            }
            db.insert_event(&env, "dev-laptop").expect("insert");
        }
        let counts = db.counts_by_bucket().expect("counts");
        assert_eq!(
            counts,
            vec![
                ("afk-watcher".to_string(), 3),
                ("window-watcher".to_string(), 2),
            ]
        );
        // limit respected
        let batch = db.fetch_unsynced_events(2).expect("batch");
        assert_eq!(batch.len(), 2);
        // mark synced is idempotent and empty-input safe
        db.mark_synced(&[]).expect("empty mark");
        let ids: Vec<i64> = batch.iter().map(|r| r.id).collect();
        db.mark_synced(&ids).expect("mark batch");
        assert_eq!(db.unsynced_count().expect("unsynced"), 3);
    }

    #[test]
    fn insert_rejects_empty_bucket() {
        let db = Db::open_in_memory().expect("in-memory db");
        let mut env = sample_event();
        env.bucket = String::new();
        let err = db
            .insert_event(&env, "dev-laptop")
            .expect_err("empty bucket must be rejected");
        assert!(matches!(err, CoreError::Validation(_)));
        assert_eq!(db.event_count().expect("count"), 0);
    }

    #[test]
    fn insert_rejects_non_rfc3339_timestamp() {
        let db = Db::open_in_memory().expect("in-memory db");
        for bad in [
            "not-a-timestamp",
            "2026-11-04 09:14:32",
            "2026-11-04T09:14:32", // valid RFC3339 shape but missing the offset
            "2026-11-04T09:14:32+0800",
        ] {
            let mut env = sample_event();
            env.timestamp = bad.to_string();
            let err = db
                .insert_event(&env, "dev-laptop")
                .expect_err("bad timestamp must be rejected");
            assert!(matches!(err, CoreError::Validation(_)), "case {bad}");
        }
        assert_eq!(db.event_count().expect("count"), 0);
    }

    #[test]
    fn insert_accepts_offset_and_z_timestamps_rejects_non_finite_duration() {
        let db = Db::open_in_memory().expect("in-memory db");
        for good in ["2026-11-04T09:14:32+08:00", "2026-11-04T01:14:32Z"] {
            let mut env = sample_event();
            env.timestamp = good.to_string();
            db.insert_event(&env, "dev-laptop")
                .expect("RFC3339 with offset must be accepted");
        }
        let mut env = sample_event();
        env.duration_s = f64::NAN;
        let err = db
            .insert_event(&env, "dev-laptop")
            .expect_err("NaN duration must be rejected");
        assert!(matches!(err, CoreError::Validation(_)));
        assert_eq!(db.event_count().expect("count"), 2);
    }

    #[test]
    fn validation_rejects_invalid_json_data() {
        // insert_event serializes `data` itself (a serde_json::Value is
        // always valid JSON), so the JSON branch of validation is exercised
        // directly here; it also guards any future string-typed entry point.
        assert!(matches!(
            validate_data_json("{not json"),
            Err(CoreError::Validation(_))
        ));
        assert!(validate_data_json("{\"app\": \"Code.exe\"}").is_ok());
        assert!(validate_data_json("null").is_ok());
    }

    #[test]
    fn event_envelope_row_conversions_and_serde() {
        let db = Db::open_in_memory().expect("in-memory db");
        let env = sample_event();
        let id = db.insert_event(&env, "dev-laptop").expect("insert");
        let row = &db.fetch_unsynced_events(10).expect("fetch")[0];

        let event = Event::try_from(row).expect("row to event");
        assert_eq!(event.bucket, BUCKET_WINDOW);
        assert_eq!(event.timestamp, "2026-11-04T09:14:32+08:00");
        assert_eq!(event.device, "dev-laptop");
        assert!(!row.synced);
        assert_eq!(event.data["app"], "Code.exe");
        assert!(event.validate().is_ok());

        // Event -> row rebuild is lossless for the stored row
        assert_eq!(event.to_row(id, false), *row);
        // envelope roundtrip drops only the device
        assert_eq!(event.envelope(), env);

        // Event is the wire format: JSON roundtrip
        let wire = serde_json::to_string(&event).expect("serialize event");
        let back: Event = serde_json::from_str(&wire).expect("deserialize event");
        assert_eq!(back, event);
    }

    #[test]
    fn row_with_corrupt_data_fails_conversion() {
        let row = EventRow {
            id: 1,
            bucket: BUCKET_MEDIA.to_string(),
            ts_start: "2026-11-04T09:14:32+08:00".to_string(),
            duration_s: 0.0,
            data: "{nope".to_string(),
            device: "dev-laptop".to_string(),
            synced: false,
        };
        assert!(matches!(Event::try_from(&row), Err(CoreError::Json(_))));
    }

    #[test]
    fn media_index_roundtrip() {
        let db = Db::open_in_memory().expect("in-memory db");
        let id = db
            .insert_media_index(
                "screen-frame",
                "media/screen/2026/11/03/frame-000123.jpg",
                48_213,
                "a1b2c3d4e5f60718",
                "watcher-screen",
                "dev-laptop",
            )
            .expect("insert media index");
        assert_eq!(db.event_count().expect("count"), 1);
        // a media index row is an event like any other: starts unsynced
        assert_eq!(db.unsynced_count().expect("unsynced"), 1);

        let events = db
            .query_events(BUCKET_MEDIA, None, None)
            .expect("query media bucket");
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.bucket, "media");
        assert_eq!(e.device, "dev-laptop");
        assert_eq!(e.duration_s, 0.0);
        assert_eq!(e.data["kind"], "screen-frame");
        assert_eq!(e.data["path"], "media/screen/2026/11/03/frame-000123.jpg");
        assert_eq!(e.data["bytes"], 48_213);
        assert_eq!(e.data["sha16"], "a1b2c3d4e5f60718");
        assert_eq!(e.data["producer"], "watcher-screen");
        // generated timestamp is RFC3339 with a timezone offset
        assert!(DateTime::parse_from_rfc3339(&e.timestamp).is_ok());

        // row <-> Event conversions line up
        let row = &db.fetch_unsynced_events(10).expect("fetch")[0];
        assert_eq!(row.id, id);
        assert_eq!(Event::try_from(row).expect("row to event"), *e);
        assert_eq!(e.to_row(id, false), *row);

        db.mark_synced(&[id]).expect("mark synced");
        assert!(
            db.query_events(BUCKET_MEDIA, None, None)
                .expect("query synced media")
                .len()
                == 1
        );
    }

    #[test]
    fn query_events_filters_by_bucket_and_time_instant() {
        let db = Db::open_in_memory().expect("in-memory db");
        insert_at(&db, BUCKET_WINDOW, "2026-11-04T09:00:00+08:00", 1.0); // 01:00Z
        insert_at(&db, BUCKET_WINDOW, "2026-11-04T10:00:00+08:00", 2.0); // 02:00Z
        insert_at(&db, BUCKET_WINDOW, "2026-11-04T03:00:00Z", 3.0); // 03:00Z
        insert_at(&db, BUCKET_AFK, "2026-11-04T09:30:00+08:00", 4.0);

        // full window bucket, ordered by instant (not by string)
        let all = db.query_events(BUCKET_WINDOW, None, None).expect("all");
        let markers: Vec<&str> = all
            .iter()
            .map(|e| e.data["marker"].as_str().expect("marker"))
            .collect();
        assert_eq!(
            markers,
            vec![
                "2026-11-04T09:00:00+08:00",
                "2026-11-04T10:00:00+08:00",
                "2026-11-04T03:00:00Z",
            ]
        );
        assert_eq!(all.len(), 3);

        // half-open bounds (None = unbounded)
        let from_only = db
            .query_events(BUCKET_WINDOW, Some("2026-11-04T02:30:00Z"), None)
            .expect("from only");
        assert_eq!(from_only.len(), 1);
        assert_eq!(from_only[0].duration_s, 3.0);
        let to_only = db
            .query_events(BUCKET_WINDOW, None, Some("2026-11-04T01:30:00Z"))
            .expect("to only");
        assert_eq!(to_only.len(), 1);
        assert_eq!(to_only[0].duration_s, 1.0);

        // inclusive window over mixed offsets: excludes the 09:00 and 03:00 rows
        let hits = db
            .query_events(
                BUCKET_WINDOW,
                Some("2026-11-04T09:30:00+08:00"),
                Some("2026-11-04T02:30:00Z"),
            )
            .expect("window");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].duration_s, 2.0);

        // bounds are inclusive, and an upper bound written in Z matches a row
        // stored as +08:00 at the same instant (lexicographic compare would not)
        let both = db
            .query_events(
                BUCKET_WINDOW,
                Some("2026-11-04T10:00:00+08:00"),
                Some("2026-11-04T02:00:00Z"),
            )
            .expect("equal-instant bounds");
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].duration_s, 2.0);

        // other buckets are not leaked into this bucket's results
        assert_eq!(
            db.query_events(BUCKET_AFK, None, None)
                .expect("afk query")
                .len(),
            1
        );
        assert!(db
            .query_events("no-such-bucket", None, None)
            .expect("empty bucket")
            .is_empty());

        // inverted window is empty, not an error
        assert!(db
            .query_events(
                BUCKET_WINDOW,
                Some("2026-11-04T03:00:00Z"),
                Some("2026-11-04T01:00:00Z"),
            )
            .expect("inverted")
            .is_empty());
    }

    #[test]
    fn query_rejects_bad_bucket_and_bad_bounds() {
        let db = Db::open_in_memory().expect("in-memory db");
        let err = db
            .query_events("", None, None)
            .expect_err("empty bucket rejected");
        assert!(matches!(err, CoreError::Validation(_)));
        let err = db
            .query_events(BUCKET_WINDOW, Some("nope"), None)
            .expect_err("bad from bound rejected");
        assert!(matches!(err, CoreError::Validation(_)));
        let err = db
            .query_events(BUCKET_WINDOW, None, Some("2026-11-04T09:00:00"))
            .expect_err("bound without offset rejected");
        assert!(matches!(err, CoreError::Validation(_)));
    }

    fn insert_for(db: &Db, bucket: &str, device: &str, ts: &str, duration_s: f64) -> i64 {
        let env = EventEnvelope {
            bucket: bucket.to_string(),
            timestamp: ts.to_string(),
            duration_s,
            data: serde_json::json!({"marker": ts}),
        };
        db.insert_event(&env, device).expect("insert")
    }

    #[test]
    fn query_events_in_filters_by_device_and_bucket() {
        let db = Db::open_in_memory().expect("in-memory db");
        insert_for(
            &db,
            BUCKET_WINDOW,
            "dev-a",
            "2026-11-04T09:00:00+08:00",
            1.0,
        );
        insert_for(&db, BUCKET_AFK, "dev-a", "2026-11-04T10:00:00+08:00", 2.0);
        insert_for(
            &db,
            BUCKET_WINDOW,
            "dev-b",
            "2026-11-04T11:00:00+08:00",
            3.0,
        );

        // device only, all buckets, instant-ordered
        let a = db
            .query_events_in(None, Some("dev-a"), None, None)
            .expect("device query");
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].bucket, BUCKET_WINDOW);
        assert_eq!(a[1].bucket, BUCKET_AFK);

        // device + bucket + from bound (instant compare across offsets):
        // 00:30Z excludes nothing here except demonstrating the bound - the
        // dev-a window event is 09:00+08 = 01:00Z, after the bound
        let hit = db
            .query_events_in(
                Some(BUCKET_WINDOW),
                Some("dev-a"),
                Some("2026-11-04T00:30:00Z"),
                None,
            )
            .expect("device+bucket+bound");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].data["marker"], "2026-11-04T09:00:00+08:00");

        // the same bound AFTER the event (02:30Z > 01:00Z) filters it out
        let none = db
            .query_events_in(
                Some(BUCKET_WINDOW),
                Some("dev-a"),
                Some("2026-11-04T02:30:00Z"),
                None,
            )
            .expect("bound after event");
        assert_eq!(none.len(), 0);

        // no filters = everything, ordered by instant
        let all = db
            .query_events_in(None, None, None, None)
            .expect("unfiltered");
        assert_eq!(all.len(), 3);

        // empty bucket string stays rejected
        assert!(matches!(
            db.query_events_in(Some(""), None, None, None),
            Err(CoreError::Validation(_))
        ));
    }

    #[test]
    fn latest_events_arrival_order_newest_first() {
        let db = Db::open_in_memory().expect("in-memory db");
        // arrival order deliberately disagrees with event-timestamp order
        insert_for(
            &db,
            BUCKET_WINDOW,
            "dev-a",
            "2026-11-01T09:00:00+08:00",
            1.0,
        ); // id 1
        insert_for(
            &db,
            BUCKET_WINDOW,
            "dev-b",
            "2026-11-02T09:00:00+08:00",
            2.0,
        ); // id 2
        insert_for(&db, BUCKET_AFK, "dev-a", "2026-10-30T09:00:00+08:00", 3.0); // id 3

        let feed = db.latest_events(Some("dev-a"), 10).expect("device feed");
        let markers: Vec<&str> = feed
            .iter()
            .map(|e| e.data["marker"].as_str().expect("marker"))
            .collect();
        assert_eq!(
            markers,
            vec!["2026-10-30T09:00:00+08:00", "2026-11-01T09:00:00+08:00"]
        );

        let all = db.latest_events(None, 2).expect("global feed");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].device, "dev-a"); // id 3 arrived last
        assert_eq!(all[1].device, "dev-b");

        let one = db.latest_events(None, 1).expect("limit feed");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].device, "dev-a");
    }

    #[test]
    fn count_device_events_since_compares_instants_across_offsets() {
        let db = Db::open_in_memory().expect("in-memory db");
        // local-midnight boundary written in Z: 2026-11-03T16:00:00Z == 2026-11-04T00:00:00+08:00
        let midnight = "2026-11-03T16:00:00Z";
        insert_for(&db, BUCKET_WINDOW, "dev-a", "2026-11-03T15:00:00Z", 1.0); // before: 23:00+08
        insert_for(
            &db,
            BUCKET_WINDOW,
            "dev-a",
            "2026-11-04T01:00:00+08:00",
            2.0,
        ); // after
        insert_for(&db, BUCKET_MEDIA, "dev-a", "2026-11-04T02:00:00Z", 0.0); // after (UTC row)
        insert_for(
            &db,
            BUCKET_WINDOW,
            "dev-b",
            "2026-11-05T09:00:00+08:00",
            4.0,
        ); // other device

        let count = db
            .count_device_events_since("dev-a", midnight)
            .expect("count");
        assert_eq!(count, 2); // the +08:00 event and the UTC media row, not dev-b

        let all_time = db
            .count_device_events_since("dev-a", "2020-01-01T00:00:00Z")
            .expect("count all");
        assert_eq!(all_time, 3);
    }

    #[test]
    fn device_overview_counts_and_last_arrival() {
        let db = Db::open_in_memory().expect("in-memory db");
        assert!(db.device_overview().expect("empty").is_empty());

        insert_for(
            &db,
            BUCKET_WINDOW,
            "dev-b",
            "2026-11-05T09:00:00+08:00",
            4.0,
        ); // arrived first
        insert_for(
            &db,
            BUCKET_WINDOW,
            "dev-a",
            "2026-11-01T09:00:00+08:00",
            1.0,
        );
        insert_for(&db, BUCKET_AFK, "dev-a", "2026-10-30T09:00:00+08:00", 2.0); // dev-a's last arrival

        let overview = db.device_overview().expect("overview");
        assert_eq!(overview.len(), 2);
        assert_eq!(overview[0].device, "dev-a"); // ordered by device name
        assert_eq!(overview[0].events_total, 2);
        assert_eq!(
            overview[0].last_seen.as_deref(),
            Some("2026-10-30T09:00:00+08:00") // MAX(id), not newest instant
        );
        assert_eq!(overview[1].device, "dev-b");
        assert_eq!(overview[1].events_total, 1);
        assert_eq!(
            overview[1].last_seen.as_deref(),
            Some("2026-11-05T09:00:00+08:00")
        );
    }

    #[test]
    fn unsynced_flow_insert_fetch_mark_empty() {
        let db = Db::open_in_memory().expect("in-memory db");
        for i in 0..3 {
            let mut env = sample_event();
            env.data["i"] = serde_json::json!(i);
            db.insert_event(&env, "dev-laptop").expect("insert");
        }
        assert_eq!(db.unsynced_count().expect("unsynced"), 3);

        let batch = db.fetch_unsynced_events(2).expect("fetch batch");
        assert_eq!(batch.len(), 2);
        let ids: Vec<i64> = batch.iter().map(|r| r.id).collect();
        db.mark_synced(&ids).expect("mark batch");
        assert_eq!(db.unsynced_count().expect("unsynced"), 1);

        let rest = db.fetch_unsynced_events(10).expect("fetch rest");
        assert_eq!(rest.len(), 1);
        db.mark_synced(&[rest[0].id]).expect("mark rest");
        assert!(db.fetch_unsynced_events(10).expect("fetch").is_empty());
        assert_eq!(db.unsynced_count().expect("unsynced"), 0);
        assert_eq!(db.event_count().expect("total"), 3);
    }

    #[test]
    fn query_event_rows_in_returns_ids_and_updates_rewrite_data() {
        let db = Db::open_in_memory().expect("in-memory db");
        let id_a = insert_for(
            &db,
            BUCKET_WINDOW,
            "dev-a",
            "2026-11-04T09:00:00+08:00",
            1.0,
        );
        let id_b = insert_for(
            &db,
            BUCKET_WINDOW,
            "dev-a",
            "2026-11-04T10:00:00+08:00",
            2.0,
        );
        insert_for(&db, BUCKET_AFK, "dev-a", "2026-11-04T11:00:00+08:00", 3.0);

        // row-level view carries ids and the same instant ordering
        let rows = db
            .query_event_rows_in(Some(BUCKET_WINDOW), None, None, None)
            .expect("rows");
        assert_eq!(
            rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![id_a, id_b]
        );
        assert_eq!(rows[0].duration_s, 1.0);
        assert_eq!(rows[1].duration_s, 2.0);

        // update_event_data rewrites the payload, other columns untouched
        let new_json = r#"{"superseded_by":"batch-2"}"#;
        assert!(db
            .update_event_data(id_a, new_json)
            .expect("update must succeed"));
        let reread = db
            .query_event_rows_in(Some(BUCKET_WINDOW), None, None, None)
            .expect("rows after update");
        assert_eq!(reread[0].id, id_a);
        assert_eq!(reread[0].data, new_json);
        assert_eq!(reread[0].duration_s, 1.0);
        assert_eq!(reread[0].device, "dev-a");
        assert_eq!(reread[1].data, "{\"marker\":\"2026-11-04T10:00:00+08:00\"}");

        // unknown id: false, not an error; invalid JSON: rejected before write
        assert!(!db.update_event_data(999, new_json).expect("unknown id"));
        assert!(matches!(
            db.update_event_data(id_b, "{not json"),
            Err(CoreError::Validation(_))
        ));
    }

    #[test]
    fn wal_mode_and_indexes_on_file_db() {
        let path = temp_db_path("wal-indexes");
        {
            let db = Db::open_db(&path).expect("open");
            let mode: String = db
                .conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .expect("journal_mode pragma");
            assert_eq!(mode.to_ascii_lowercase(), "wal");

            let mut stmt = db
                .conn
                .prepare(
                    "SELECT name FROM sqlite_master
                     WHERE type = 'index' AND tbl_name = 'events'
                     ORDER BY name",
                )
                .expect("prepare sqlite_master query");
            let names: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .expect("map names")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("index names");
            assert_eq!(
                names,
                vec![
                    "idx_events_bucket_ts".to_string(),
                    "idx_events_device_ts".to_string(),
                    "idx_events_synced".to_string(),
                ]
            );
        } // drop the connection, then reopen: schema creation must be idempotent
        let db = Db::open_db(&path).expect("reopen");
        assert!(db
            .query_events(BUCKET_WINDOW, None, None)
            .expect("query after reopen")
            .is_empty());
        drop(db);
        cleanup(&path);
    }
}
