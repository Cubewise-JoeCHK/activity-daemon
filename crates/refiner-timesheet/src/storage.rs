//! SQLite storage for the Timesheet AI question queue (issue #44).
//!
//! Schema (`ts_questions` table, ADR addendum on issue #16):
//! `id INTEGER PRIMARY KEY, day TEXT NOT NULL, question TEXT NOT NULL,
//! evidence_json TEXT NOT NULL, status TEXT NOT NULL
//! ('open'|'answered'|'superseded'),
//! answer TEXT, created_at TEXT NOT NULL, answered_at TEXT`.
//!
//! `evidence_json` holds a JSON list of `{frame_path, frame_sha16, ts}`
//! objects - references to captured screen frames backing the question.
//! `frame_path` uses the media convention (`media/<kind>/<YYYY>/<MM>/<DD>/
//! <filename>`, relative to the hub data root); serving the bytes is the
//! hub API's job (issue #44), this module only stores the references.
//!
//! The store opens the same SQLite file as the core event store and runs
//! WAL, so the hub server can hold both connections without read-write
//! contention; a busy timeout covers the short write overlaps. Answered
//! questions keep their evidence (nothing is deleted on answer - the row
//! only flips `status` and fills `answer`/`answered_at`). Superseded
//! questions (issue #66) keep their evidence too: regeneration or the
//! maintenance endpoint closes them when their span is zero-length
//! debris or fully covered by the recompiled drafts - `status` flips to
//! `superseded`, `answered_at` doubles as the closure stamp and
//! `answer` stays NULL (nobody answered).

use std::path::Path;
use std::time::Duration;

use activity_core::CoreError;
use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// A captured frame cited as evidence for a question. `frame_path` is the
/// media-relative path convention (`media/<kind>/...`), `frame_sha16` the
/// producer's content-hash prefix (the watcher-screen dedup key), `ts` the
/// RFC3339 capture instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceFrame {
    pub frame_path: String,
    pub frame_sha16: String,
    pub ts: String,
}

/// Lifecycle of a question: created `open` by the analysis worker or
/// the compiler, flipped to `answered` (with the answer text) by the
/// owner, or closed `superseded` (issue #66: zero-length or fully
/// covered span; `answered_at` is the closure stamp, `answer` stays
/// NULL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TsQuestionStatus {
    Open,
    Answered,
    Superseded,
}

impl TsQuestionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TsQuestionStatus::Open => "open",
            TsQuestionStatus::Answered => "answered",
            TsQuestionStatus::Superseded => "superseded",
        }
    }

    fn from_str(status: &str) -> Result<TsQuestionStatus, CoreError> {
        match status {
            "open" => Ok(TsQuestionStatus::Open),
            "answered" => Ok(TsQuestionStatus::Answered),
            "superseded" => Ok(TsQuestionStatus::Superseded),
            other => Err(CoreError::Validation(format!(
                "unknown question status '{other}' in store"
            ))),
        }
    }
}

/// Status filter for [`TsQuestionStore::list_questions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Open,
    Answered,
    All,
}

/// A question row as read back from the database, evidence parsed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TsQuestion {
    pub id: i64,
    /// The owner's local day the question belongs to, `YYYY-MM-DD`.
    pub day: String,
    pub question: String,
    pub evidence: Vec<EvidenceFrame>,
    pub status: TsQuestionStatus,
    pub answer: Option<String>,
    pub created_at: String,
    pub answered_at: Option<String>,
}

/// Handle to the question-queue store: a dedicated connection to the hub
/// SQLite file (`ts_questions` table), following the core `Db` pattern.
pub struct TsQuestionStore {
    conn: Connection,
}

impl TsQuestionStore {
    /// Open (creating if needed) the store on the given database file in
    /// WAL mode and ensure the schema exists.
    pub fn open(path: &Path) -> Result<TsQuestionStore, CoreError> {
        let conn = Connection::open(path)?;
        TsQuestionStore::init(conn)
    }

    /// In-memory database, useful for tests and throwaway queries.
    pub fn open_in_memory() -> Result<TsQuestionStore, CoreError> {
        let conn = Connection::open_in_memory()?;
        TsQuestionStore::init(conn)
    }

    fn init(conn: Connection) -> Result<TsQuestionStore, CoreError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // The hub server keeps its own connection to the same file (core
        // event store); writes are short INSERT/UPDATEs, so a busy timeout
        // turns the rare overlap into a wait instead of an error.
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ts_questions (
                id INTEGER PRIMARY KEY,
                day TEXT NOT NULL,
                question TEXT NOT NULL,
                evidence_json TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'open',
                answer TEXT,
                created_at TEXT NOT NULL,
                answered_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_ts_questions_status ON ts_questions (status);
            CREATE INDEX IF NOT EXISTS idx_ts_questions_day ON ts_questions (day);",
        )?;
        Ok(TsQuestionStore { conn })
    }

    /// Create one question (the analysis worker's entry point, used by the
    /// frame-analysis worker, issue #43). Returns the new row id.
    ///
    /// `day` must be `YYYY-MM-DD` and `question` non-empty; both are
    /// validated and rejected with [`CoreError::Validation`] before
    /// anything is written. Evidence frame paths are stored as given and
    /// resolved against the media root only when served (the hub API
    /// rejects any stored path that does not follow the media convention).
    pub fn create_question(
        &self,
        day: &str,
        question: &str,
        evidence: &[EvidenceFrame],
    ) -> Result<i64, CoreError> {
        validate_day(day)?;
        if question.trim().is_empty() {
            return Err(CoreError::Validation(
                "question must be non-empty".to_string(),
            ));
        }
        let evidence_json = serde_json::to_string(&evidence)?;
        self.conn.execute(
            "INSERT INTO ts_questions (day, question, evidence_json, status, created_at)
             VALUES (?1, ?2, ?3, 'open', ?4)",
            params![day, question, evidence_json, now_rfc3339()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Fetch one question by id (evidence parsed), `None` if unknown.
    pub fn get_question(&self, id: i64) -> Result<Option<TsQuestion>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, day, question, evidence_json, status, answer, created_at, answered_at
             FROM ts_questions WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], map_question)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Questions newest first (id descending - the "what did the hub learn
    /// lately" order, matching the core `latest_events` convention),
    /// optionally narrowed to open or answered rows.
    pub fn list_questions(&self, filter: StatusFilter) -> Result<Vec<TsQuestion>, CoreError> {
        let sql = match filter {
            StatusFilter::All => {
                "SELECT id, day, question, evidence_json, status, answer, created_at, answered_at
                 FROM ts_questions ORDER BY id DESC"
            }
            StatusFilter::Open | StatusFilter::Answered => {
                "SELECT id, day, question, evidence_json, status, answer, created_at, answered_at
                 FROM ts_questions WHERE status = ?1 ORDER BY id DESC"
            }
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = Vec::new();
        match filter {
            StatusFilter::All => {
                for row in stmt.query_map([], map_question)? {
                    rows.push(row?);
                }
            }
            StatusFilter::Open | StatusFilter::Answered => {
                let status = match filter {
                    StatusFilter::Open => TsQuestionStatus::Open,
                    _ => TsQuestionStatus::Answered,
                };
                for row in stmt.query_map(params![status.as_str()], map_question)? {
                    rows.push(row?);
                }
            }
        }
        Ok(rows)
    }

    /// Answer a question: fills `answer`/`answered_at` and flips `status`
    /// to `answered`; evidence is never touched (answered questions keep
    /// their evidence link). Re-answering an already-answered question
    /// updates the answer (last write wins; single-owner hub). Returns the
    /// updated row, or `None` if the id is unknown. The answer is trimmed;
    /// an empty answer is rejected with [`CoreError::Validation`].
    pub fn answer_question(&self, id: i64, answer: &str) -> Result<Option<TsQuestion>, CoreError> {
        let answer = answer.trim();
        if answer.is_empty() {
            return Err(CoreError::Validation(
                "answer must be non-empty".to_string(),
            ));
        }
        let updated = self.conn.execute(
            "UPDATE ts_questions
             SET answer = ?2, status = 'answered', answered_at = ?3
             WHERE id = ?1",
            params![id, answer, now_rfc3339()],
        )?;
        if updated == 0 {
            return Ok(None);
        }
        self.get_question(id)
    }

    /// Close open questions as superseded (issue #66): the only write
    /// regeneration's supersession guard and the maintenance endpoint
    /// make. Only currently-`open` rows flip - an owner answer wins over
    /// supersession (never overwritten) - so re-running with the same
    /// ids is a no-op. `answered_at` carries the closure stamp; `answer`
    /// stays NULL. Returns the ids actually closed (input order).
    pub fn supersede_questions(&self, ids: &[i64]) -> Result<Vec<i64>, CoreError> {
        let mut closed = Vec::with_capacity(ids.len());
        for id in ids {
            let updated = self.conn.execute(
                "UPDATE ts_questions
                 SET status = 'superseded', answered_at = ?2
                 WHERE id = ?1 AND status = 'open'",
                params![id, now_rfc3339()],
            )?;
            if updated == 1 {
                closed.push(*id);
            }
        }
        Ok(closed)
    }
}

fn map_question(row: &rusqlite::Row<'_>) -> rusqlite::Result<TsQuestion> {
    let evidence_json: String = row.get(3)?;
    let evidence: Vec<EvidenceFrame> = serde_json::from_str(&evidence_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(TsQuestion {
        id: row.get(0)?,
        day: row.get(1)?,
        question: row.get(2)?,
        evidence,
        status: TsQuestionStatus::from_str(&row.get::<_, String>(4)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?,
        answer: row.get(5)?,
        created_at: row.get(6)?,
        answered_at: row.get(7)?,
    })
}

/// `YYYY-MM-DD` calendar day, nothing else accepted. The parsed date is
/// re-rendered and compared, so non-canonical spellings ("2026-11-6") are
/// rejected too - the daily itinerary compiler groups by this key, and
/// the stored form must stay byte-identical across questions.
fn validate_day(day: &str) -> Result<(), CoreError> {
    match chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") {
        Ok(parsed) if parsed.format("%Y-%m-%d").to_string() == day => Ok(()),
        _ => Err(CoreError::Validation(format!(
            "day '{day}' must be YYYY-MM-DD"
        ))),
    }
}

/// Current UTC time as RFC3339 with offset (same convention as the core's
/// media index rows).
fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("refiner-timesheet-test-{tag}-{nanos}.db"))
    }

    fn cleanup(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn frame(path: &str) -> EvidenceFrame {
        EvidenceFrame {
            frame_path: path.to_string(),
            frame_sha16: "a1b2c3d4e5f60718".to_string(),
            ts: "2026-11-06T09:14:32+08:00".to_string(),
        }
    }

    fn create_at(store: &TsQuestionStore, day: &str, question: &str) -> i64 {
        store
            .create_question(day, question, &[frame("media/screen/2026/11/06/f1.jpg")])
            .expect("create")
    }

    #[test]
    fn create_and_get_roundtrip_on_file_db() {
        let path = temp_db_path("roundtrip");
        let id = {
            let store = TsQuestionStore::open(&path).expect("open");
            store
                .create_question(
                    "2026-11-06",
                    "What were you working on at 09:14?",
                    &[
                        frame("media/screen/2026/11/06/f1.jpg"),
                        frame("media/screen/2026/11/06/f2.jpg"),
                    ],
                )
                .expect("create")
        }; // drop the connection, then reopen to prove persistence

        let store = TsQuestionStore::open(&path).expect("reopen");
        let q = store
            .get_question(id)
            .expect("get")
            .expect("question exists");
        assert_eq!(q.id, id);
        assert_eq!(q.day, "2026-11-06");
        assert_eq!(q.question, "What were you working on at 09:14?");
        assert_eq!(q.status, TsQuestionStatus::Open);
        assert_eq!(q.answer, None);
        assert_eq!(q.answered_at, None);
        // generated created_at is RFC3339 with a timezone offset
        assert!(chrono::DateTime::parse_from_rfc3339(&q.created_at).is_ok());
        // evidence survives the JSON round trip in order
        assert_eq!(q.evidence.len(), 2);
        assert_eq!(q.evidence[0].frame_path, "media/screen/2026/11/06/f1.jpg");
        assert_eq!(q.evidence[1].frame_path, "media/screen/2026/11/06/f2.jpg");
        assert_eq!(q.evidence[0].frame_sha16, "a1b2c3d4e5f60718");
        drop(store);
        cleanup(&path);
    }

    #[test]
    fn create_validates_day_and_question() {
        let store = TsQuestionStore::open_in_memory().expect("open");
        for bad_day in ["", "2026-11-6", "2026/11/06", "06-11-2026", "not-a-day"] {
            let err = store
                .create_question(bad_day, "q", &[])
                .expect_err("bad day must be rejected");
            assert!(matches!(err, CoreError::Validation(_)), "case {bad_day}");
        }
        let err = store
            .create_question("2026-11-06", "   ", &[])
            .expect_err("empty question must be rejected");
        assert!(matches!(err, CoreError::Validation(_)));
        // nothing was written by the rejected calls
        assert!(store
            .list_questions(StatusFilter::All)
            .expect("list")
            .is_empty());
    }

    #[test]
    fn list_filters_by_status_newest_first() {
        let store = TsQuestionStore::open_in_memory().expect("open");
        let first = create_at(&store, "2026-11-04", "q1");
        let second = create_at(&store, "2026-11-05", "q2");
        let third = create_at(&store, "2026-11-06", "q3");
        store
            .answer_question(second, "it was the build")
            .expect("answer");

        let ids = |rows: Vec<TsQuestion>| rows.into_iter().map(|q| q.id).collect::<Vec<_>>();

        // newest first
        assert_eq!(
            ids(store.list_questions(StatusFilter::All).expect("all")),
            vec![third, second, first]
        );
        // open only: the answered one is gone
        assert_eq!(
            ids(store.list_questions(StatusFilter::Open).expect("open")),
            vec![third, first]
        );
        // answered only
        assert_eq!(
            ids(store
                .list_questions(StatusFilter::Answered)
                .expect("answered")),
            vec![second]
        );
        // every listed question carries parsed evidence
        let listed = store.list_questions(StatusFilter::Open).expect("open");
        assert_eq!(listed[0].evidence.len(), 1);
    }

    #[test]
    fn answer_marks_answered_and_keeps_evidence() {
        let store = TsQuestionStore::open_in_memory().expect("open");
        let id = create_at(&store, "2026-11-06", "What was this terminal session?");

        let answered = store
            .answer_question(id, "  refactoring the hub server  ")
            .expect("answer")
            .expect("question exists");
        assert_eq!(answered.status, TsQuestionStatus::Answered);
        assert_eq!(
            answered.answer,
            Some("refactoring the hub server".to_string())
        );
        assert!(chrono::DateTime::parse_from_rfc3339(
            answered.answered_at.as_deref().expect("answered_at set")
        )
        .is_ok());
        // acceptance criterion: answered questions keep their evidence link
        assert_eq!(answered.evidence.len(), 1);

        // persisted, and visible under the answered filter
        let reread = store.get_question(id).expect("get").expect("exists");
        assert_eq!(reread, answered);
        assert_eq!(
            store
                .list_questions(StatusFilter::Answered)
                .expect("answered")
                .len(),
            1
        );
        assert!(store
            .list_questions(StatusFilter::Open)
            .expect("open")
            .is_empty());

        // re-answering updates the answer (last write wins), evidence intact
        let updated = store
            .answer_question(id, "actually it was code review")
            .expect("re-answer")
            .expect("still exists");
        assert_eq!(
            updated.answer,
            Some("actually it was code review".to_string())
        );
        assert_eq!(updated.evidence.len(), 1);

        // unknown id: None, not an error
        assert!(store.answer_question(999, "x").expect("unknown").is_none());
        // empty answer is a validation error, before any write
        let err = store
            .answer_question(id, "   ")
            .expect_err("empty answer must be rejected");
        assert!(matches!(err, CoreError::Validation(_)));
    }

    #[test]
    fn schema_is_idempotent_on_reopen() {
        let path = temp_db_path("reopen");
        {
            let store = TsQuestionStore::open(&path).expect("open");
            create_at(&store, "2026-11-06", "q1");
        }
        {
            let store = TsQuestionStore::open(&path).expect("reopen");
            create_at(&store, "2026-11-06", "q2");
            assert_eq!(
                store.list_questions(StatusFilter::All).expect("all").len(),
                2
            );
        }
        cleanup(&path);
    }

    #[test]
    fn supersede_questions_closes_open_rows_idempotently() {
        let store = TsQuestionStore::open_in_memory().expect("open");
        let debris = create_at(&store, "2026-11-06", "debris");
        let grey = create_at(&store, "2026-11-06", "grey");
        let answered = create_at(&store, "2026-11-06", "answered");
        store
            .answer_question(answered, "owner answered")
            .expect("answer")
            .expect("answered");

        // Unknown ids are skipped, answered rows are never touched.
        let closed = store
            .supersede_questions(&[debris, 999, answered])
            .expect("supersede");
        assert_eq!(closed, vec![debris]);

        let status = |id: i64| store.get_question(id).expect("get").expect("exists").status;
        assert_eq!(status(debris), TsQuestionStatus::Superseded);
        assert_eq!(status(grey), TsQuestionStatus::Open);
        assert_eq!(status(answered), TsQuestionStatus::Answered);
        let closed_row = store.get_question(debris).expect("get").expect("exists");
        assert_eq!(closed_row.answer, None, "no one answered the debris");
        assert!(chrono::DateTime::parse_from_rfc3339(
            closed_row
                .answered_at
                .as_deref()
                .expect("closure stamp set")
        )
        .is_ok());

        // Idempotent: an already-closed row no longer matches.
        assert!(store
            .supersede_questions(&[debris])
            .expect("re-run")
            .is_empty());
        assert_eq!(
            store
                .list_questions(StatusFilter::Open)
                .expect("open")
                .len(),
            1,
            "only the grey question stays in the open queue"
        );
    }
}
