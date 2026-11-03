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

use std::path::Path;

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
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreError::Sqlite(e) => write!(f, "sqlite error: {e}"),
            CoreError::Json(e) => write!(f, "json error: {e}"),
            CoreError::Io(e) => write!(f, "io error: {e}"),
            CoreError::Config(msg) => write!(f, "config error: {msg}"),
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

/// Handle to the local (or central) SQLite event store.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (creating if needed) the database file in WAL mode and ensure
    /// the schema exists.
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
            );",
        )?;
        Ok(Db { conn })
    }

    /// Append-only insert of one event envelope for the given device.
    /// Returns the new row id.
    pub fn insert_event(&self, event: &EventEnvelope, device: &str) -> Result<i64> {
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
}
