//! Timesheet AI APIs (issues #44/#45, ADR addendum on #16).
//!
//! Question queue (issue #44):
//! - `GET /api/timesheet/questions?status=open|answered|all` - list with
//!   evidence refs (each frame carries its `/evidence/{n}` URL). Default
//!   filter `open`; newest first.
//! - `GET /api/timesheet/questions/{id}/evidence/{n}` - the n-th evidence
//!   frame as JPEG bytes, resolved against the media root. The stored
//!   `frame_path` (`media/<kind>/...` convention, same as the media index
//!   rows) is validated segment-by-segment with the same component rules
//!   `/ingest/media` enforces, so a stored path cannot escape the media
//!   root.
//! - `POST /api/timesheet/questions/{id}/answer` - body `{"answer": "..."}`;
//!   marks the question answered (last write wins on re-answer).
//!
//! Daily itinerary drafts (issue #45):
//! - `GET /api/timesheet/draft?date=YYYY-MM-DD` - the day's current draft
//!   activities (superseded ones hidden) plus status.
//! - `POST /api/timesheet/draft?date=YYYY-MM-DD` - regenerate the day:
//!   the compiler merges window sessions (backbone), frame analyses, AFK
//!   spans and answered questions into fresh drafts (old drafts marked
//!   `superseded_by`); optionally one Lemonade call titles/comments the
//!   blocks (`use_llm_titles`, fail-soft to heuristics).
//!
//! Token policy (issues #44/#45): the answer POST and the draft POST
//! follow the same open precedent as the other `/api` reads (crate docs,
//! `/health` precedent) - the hub is single-owner and never proxied
//! outside the tailnet, so owner-facing `/api` stays open while
//! device-facing `/ingest` writes keep their token guard.
//!
//! All storage and file work runs on `spawn_blocking`, exactly like the
//! `/api/machines` and `/ingest` handlers. `GET /health` is untouched.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use activity_core::{CoreError, BUCKET_AFK, BUCKET_WINDOW};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use chrono::{Local, NaiveDate, TimeZone};
use refiner_timesheet::{
    current_draft_from_row, parse_day, plan_day, store_day, BlockTitler, DayInput, HeuristicTitler,
    LemonadeTitler, StatusFilter, TimesheetConfig, TsQuestion, TsQuestionStore,
    BUCKET_FRAME_ANALYSIS, BUCKET_TIMESHEET_DRAFT,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{bad_request, internal_error, json_response, valid_component, AppState};

/// Routes for the Timesheet AI APIs (merged into `build_router`, unguarded -
/// see the token policy note in the module docs).
pub fn timesheet_router() -> Router<AppState> {
    Router::new()
        .route("/api/timesheet/questions", get(api_list_questions))
        .route(
            "/api/timesheet/questions/{id}/evidence/{n}",
            get(api_question_evidence),
        )
        .route(
            "/api/timesheet/questions/{id}/answer",
            post(api_answer_question),
        )
        .route(
            "/api/timesheet/draft",
            get(api_get_draft).post(api_post_draft),
        )
}

// -------------------------------------------------- GET/POST /api/timesheet/draft

#[derive(Debug, Deserialize)]
pub struct DraftQuery {
    /// The owner's local day, `YYYY-MM-DD`.
    date: String,
}

/// Titler selection for a compile run: the config flag picks the real
/// Lemonade client or the deterministic heuristic (tests pin the heuristic
/// so runs stay hermetic).
enum DayTitler {
    Heuristic(HeuristicTitler),
    Lemonade(LemonadeTitler),
}

impl BlockTitler for DayTitler {
    fn title_blocks(
        &self,
        blocks: &[refiner_timesheet::MergedBlock],
        style: &refiner_timesheet::StyleConfig,
    ) -> Vec<refiner_timesheet::BlockTitle> {
        match self {
            DayTitler::Heuristic(titler) => titler.title_blocks(blocks, style),
            DayTitler::Lemonade(titler) => titler.title_blocks(blocks, style),
        }
    }
}

fn select_titler(cfg: &TimesheetConfig) -> DayTitler {
    if cfg.use_llm_titles {
        DayTitler::Lemonade(LemonadeTitler::from_config(cfg))
    } else {
        DayTitler::Heuristic(HeuristicTitler)
    }
}

/// Hub-local instants bounding the owner's local day (inclusive both ends;
/// the compiler re-filters rows by their own UTC offset, so an event
/// exactly at the next local midnight cannot leak through as the wrong
/// day).
fn local_day_bounds(day: NaiveDate) -> Result<(String, String), CoreError> {
    let midnight = |d: NaiveDate| {
        Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight is valid"))
            .earliest()
    };
    let next = day
        .succ_opt()
        .ok_or_else(|| CoreError::Validation(format!("date {day} has no successor")))?;
    let from = midnight(day)
        .ok_or_else(|| CoreError::Validation(format!("date {day} has no local midnight")))?;
    let to = midnight(next)
        .ok_or_else(|| CoreError::Validation(format!("date {next} has no local midnight")))?;
    Ok((from.to_rfc3339(), to.to_rfc3339()))
}

/// A draft row belongs to `day` when its own UTC offset says so (the
/// device's local date of `ts_start`).
fn row_in_day(row: &activity_core::EventRow, day: NaiveDate) -> bool {
    chrono::DateTime::parse_from_rfc3339(&row.ts_start)
        .map(|ts| ts.date_naive() == day)
        .unwrap_or(false)
}

async fn api_get_draft(State(state): State<AppState>, Query(query): Query<DraftQuery>) -> Response {
    let day = match parse_day(&query.date) {
        Ok(day) => day,
        Err(e) => return bad_request(e),
    };
    let outcome = tokio::task::spawn_blocking(move || draft_payload(&state, day)).await;
    match outcome {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("draft task failed: {e}")),
    }
}

async fn api_post_draft(
    State(state): State<AppState>,
    Query(query): Query<DraftQuery>,
) -> Response {
    let day = match parse_day(&query.date) {
        Ok(day) => day,
        Err(e) => return bad_request(e),
    };
    let titler = select_titler(&state.timesheet);
    let outcome =
        tokio::task::spawn_blocking(move || regenerate_payload(&state, day, &titler)).await;
    match outcome {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("regenerate task failed: {e}")),
    }
}

/// The GET payload: the day's current drafts (superseded ones hidden) plus
/// status. `generated_at` is the newest batch stamp among them.
fn draft_payload(state: &AppState, day: NaiveDate) -> Result<Value, CoreError> {
    let rows = {
        let db = state
            .db
            .lock()
            .map_err(|_| CoreError::Config("database lock poisoned".to_string()))?;
        let (from, to) = local_day_bounds(day)?;
        db.query_event_rows_in(Some(BUCKET_TIMESHEET_DRAFT), None, Some(&from), Some(&to))?
    };
    let mut drafts = Vec::new();
    let mut latest_batch: Option<String> = None;
    for row in &rows {
        if !row_in_day(row, day) {
            continue;
        }
        let Some(activity) = current_draft_from_row(row) else {
            continue;
        };
        let batch: Option<String> =
            serde_json::from_str::<Value>(&row.data)
                .ok()
                .and_then(|data| {
                    data.get("batch")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
        if let Some(batch) = &batch {
            if latest_batch.as_ref().is_none_or(|latest| batch > latest) {
                latest_batch = Some(batch.clone());
            }
        }
        let mut payload = serde_json::to_value(&activity)?;
        payload["id"] = json!(row.id);
        payload["batch"] = batch.map(Value::String).unwrap_or(Value::Null);
        drafts.push(payload);
    }
    Ok(json!({
        "day": day.format("%Y-%m-%d").to_string(),
        "status": {
            "generated_at": latest_batch,
            "count": drafts.len(),
        },
        "drafts": drafts,
    }))
}

/// The POST payload: compile the day fresh, store it, then report exactly
/// what GET would (plus the regeneration counters).
fn regenerate_payload(
    state: &AppState,
    day: NaiveDate,
    titler: &dyn BlockTitler,
) -> Result<Value, CoreError> {
    let (from, to) = local_day_bounds(day)?;
    let day_str = day.format("%Y-%m-%d").to_string();
    let stored = {
        let db = state
            .db
            .lock()
            .map_err(|_| CoreError::Config("database lock poisoned".to_string()))?;
        let questions = state
            .ts_questions
            .lock()
            .map_err(|_| CoreError::Config("question store lock poisoned".to_string()))?;
        let sessions = db.query_event_rows_in(Some(BUCKET_WINDOW), None, Some(&from), Some(&to))?;
        let analyses =
            db.query_event_rows_in(Some(BUCKET_FRAME_ANALYSIS), None, Some(&from), Some(&to))?;
        let afk = db.query_event_rows_in(Some(BUCKET_AFK), None, Some(&from), Some(&to))?;
        let old_drafts =
            db.query_event_rows_in(Some(BUCKET_TIMESHEET_DRAFT), None, Some(&from), Some(&to))?;
        let existing_questions: Vec<TsQuestion> = questions
            .list_questions(StatusFilter::All)?
            .into_iter()
            .filter(|q| q.day == day_str)
            .collect();
        let input = DayInput {
            day,
            sessions,
            analyses,
            afk,
            old_drafts,
        };
        let plan = plan_day(&input, &existing_questions, &state.timesheet, titler);
        store_day(&db, &questions, &plan)?
    };
    let mut payload = draft_payload(state, day)?;
    payload["questions_created"] = json!(stored.question_ids.len());
    payload["superseded"] = json!(stored.superseded_ids.len());
    Ok(payload)
}

// ------------------------------------------------ GET /api/timesheet/questions

#[derive(Debug, Deserialize)]
pub struct QuestionListQuery {
    /// `open` (default) | `answered` | `all`.
    status: Option<String>,
}

async fn api_list_questions(
    State(state): State<AppState>,
    Query(query): Query<QuestionListQuery>,
) -> Response {
    let filter = match query.status.as_deref() {
        None | Some("open") => StatusFilter::Open,
        Some("answered") => StatusFilter::Answered,
        Some("all") => StatusFilter::All,
        Some(other) => return bad_request(format!("unknown status '{other}' (open|answered|all)")),
    };
    let store = state.ts_questions.clone();
    let outcome = tokio::task::spawn_blocking(move || list_payload(&store, filter)).await;
    match outcome {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("questions task failed: {e}")),
    }
}

fn list_payload(
    store: &Arc<Mutex<TsQuestionStore>>,
    filter: StatusFilter,
) -> Result<Value, CoreError> {
    let store = store
        .lock()
        .map_err(|_| CoreError::Config("question store lock poisoned".to_string()))?;
    let questions: Vec<Value> = store
        .list_questions(filter)?
        .iter()
        .map(question_payload)
        .collect();
    Ok(json!({ "questions": questions }))
}

/// One question as the API returns it: evidence refs enriched with the
/// `n` index and the `/evidence/{n}` URL, so clients never build one.
fn question_payload(question: &TsQuestion) -> Value {
    let evidence: Vec<Value> = question
        .evidence
        .iter()
        .enumerate()
        .map(|(n, frame)| {
            json!({
                "n": n,
                "frame_path": frame.frame_path,
                "frame_sha16": frame.frame_sha16,
                "ts": frame.ts,
                "url": format!("/api/timesheet/questions/{}/evidence/{}", question.id, n),
            })
        })
        .collect();
    json!({
        "id": question.id,
        "day": question.day,
        "question": question.question,
        "status": question.status.as_str(),
        "answer": question.answer,
        "created_at": question.created_at,
        "answered_at": question.answered_at,
        "evidence": evidence,
    })
}

// --------------------------------- GET /api/timesheet/questions/{id}/evidence/{n}

async fn api_question_evidence(
    State(state): State<AppState>,
    AxumPath((id, n)): AxumPath<(i64, usize)>,
) -> Response {
    let store = state.ts_questions.clone();
    let media_dir = state.media_dir.clone();
    let outcome =
        tokio::task::spawn_blocking(move || evidence_frame_bytes(&store, &media_dir, id, n)).await;
    match outcome {
        // join error | storage error | the 404 cases (already a Response)
        Ok(Ok(Ok(bytes))) => ([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response(),
        Ok(Ok(Err(not_found))) => not_found,
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("evidence task failed: {e}")),
    }
}

/// Load the question, pick evidence frame `n`, resolve its stored path
/// against the media root and read the bytes. The outer `Err` carries
/// storage failures; the inner `Err` is an already-built 404 for
/// everything that simply does not exist (unknown question, out-of-range
/// frame, path outside the media convention, missing file).
fn evidence_frame_bytes(
    store: &Arc<Mutex<TsQuestionStore>>,
    media_dir: &Path,
    id: i64,
    n: usize,
) -> Result<Result<Vec<u8>, Response>, CoreError> {
    let not_found = |msg: String| -> Response { (StatusCode::NOT_FOUND, msg).into_response() };
    let store = store
        .lock()
        .map_err(|_| CoreError::Config("question store lock poisoned".to_string()))?;
    let question = match store.get_question(id)? {
        Some(question) => question,
        None => return Ok(Err(not_found(format!("no question {id}")))),
    };
    let frame = match question.evidence.get(n) {
        Some(frame) => frame,
        None => {
            return Ok(Err(not_found(format!(
                "question {id} has no evidence frame {n}"
            ))))
        }
    };
    let path = match resolve_frame_path(media_dir, &frame.frame_path) {
        Some(path) => path,
        None => {
            return Ok(Err(not_found(format!(
                "evidence path '{}' is not a media-relative path",
                frame.frame_path
            ))))
        }
    };
    Ok(match std::fs::read(&path) {
        Ok(bytes) => Ok(bytes),
        Err(_) => Err(not_found(format!(
            "evidence frame '{}' not found on disk",
            frame.frame_path
        ))),
    })
}

/// Resolve an evidence `frame_path` (media convention
/// `media/<kind>/<YYYY>/<MM>/<DD>/<filename>`, relative to the hub data
/// root) onto the media root. Every segment must pass the same component
/// rules `/ingest/media` enforces (`valid_component` rejects `.`/`..`,
/// separators and whitespace), so a stored path cannot escape the media
/// root; anything else fails to resolve and the endpoint 404s.
fn resolve_frame_path(media_dir: &Path, frame_path: &str) -> Option<PathBuf> {
    let rel = frame_path.strip_prefix("media/")?;
    let mut resolved = media_dir.to_path_buf();
    for segment in rel.split('/') {
        if !valid_component(segment) {
            return None;
        }
        resolved.push(segment);
    }
    Some(resolved)
}

// -------------------------------- POST /api/timesheet/questions/{id}/answer

#[derive(Debug, Deserialize)]
pub struct AnswerBody {
    answer: String,
}

async fn api_answer_question(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<i64>,
    axum::Json(body): axum::Json<AnswerBody>,
) -> Response {
    if body.answer.trim().is_empty() {
        return bad_request("answer must not be empty");
    }
    let store = state.ts_questions.clone();
    let answer = body.answer;
    let outcome = tokio::task::spawn_blocking(move || answer_payload(&store, id, &answer)).await;
    match outcome {
        Ok(Ok(Some(question))) => json_response(StatusCode::OK, question),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, format!("no question {id}")).into_response(),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("answer task failed: {e}")),
    }
}

fn answer_payload(
    store: &Arc<Mutex<TsQuestionStore>>,
    id: i64,
    answer: &str,
) -> Result<Option<Value>, CoreError> {
    let store = store
        .lock()
        .map_err(|_| CoreError::Config("question store lock poisoned".to_string()))?;
    Ok(store
        .answer_question(id, answer)?
        .as_ref()
        .map(question_payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use refiner_timesheet::{EvidenceFrame, TsQuestionStatus};

    fn resolve(media_dir: &str, frame_path: &str) -> Option<PathBuf> {
        resolve_frame_path(Path::new(media_dir), frame_path)
    }

    #[test]
    fn resolve_frame_path_maps_media_convention_under_media_root() {
        let resolved = resolve("/data/media", "media/screen/2026/11/06/frame-000123.jpg")
            .expect("conventional path resolves");
        assert_eq!(
            resolved,
            Path::new("/data/media")
                .join("screen")
                .join("2026")
                .join("11")
                .join("06")
                .join("frame-000123.jpg")
        );
        // single-file frame at the kind root still resolves
        assert!(resolve("/data/media", "media/screen/x.jpg").is_some());
    }

    #[test]
    fn resolve_frame_path_rejects_escape_attempts_and_wrong_convention() {
        // no media/ prefix: not the convention, never resolved
        assert!(resolve("/data/media", "screen/2026/11/06/f.jpg").is_none());
        assert!(resolve("/data/media", "/etc/passwd").is_none());
        // prefix alone, or empty segments
        assert!(resolve("/data/media", "media/").is_none());
        assert!(resolve("/data/media", "media//f.jpg").is_none());
        // traversal in any form
        assert!(resolve("/data/media", "media/../secret.txt").is_none());
        assert!(resolve("/data/media", "media/screen/../../secret.txt").is_none());
        assert!(resolve("/data/media", "media/screen/./f.jpg").is_none());
        // backslashes are not separators: the segment fails the component
        // rules instead of resolving to a Windows-style escape
        assert!(resolve("/data/media", "media/screen\\..\\..\\secret.txt").is_none());
        // whitespace and other characters never pass
        assert!(resolve("/data/media", "media/screen/sp ace.jpg").is_none());
        assert!(resolve("/data/media", "media/screen/ünïcode.jpg").is_none());
    }

    #[test]
    fn question_payload_shape_and_evidence_urls() {
        let question = TsQuestion {
            id: 7,
            day: "2026-11-06".to_string(),
            question: "What was this?".to_string(),
            evidence: vec![EvidenceFrame {
                frame_path: "media/screen/2026/11/06/f.jpg".to_string(),
                frame_sha16: "a1b2c3d4e5f60718".to_string(),
                ts: "2026-11-06T09:14:32+08:00".to_string(),
            }],
            status: TsQuestionStatus::Open,
            answer: None,
            created_at: "2026-11-06T08:00:00.000Z".to_string(),
            answered_at: None,
        };
        let v = question_payload(&question);
        assert_eq!(v["id"], 7);
        assert_eq!(v["status"], "open");
        assert_eq!(v["answer"], Value::Null);
        assert_eq!(v["answered_at"], Value::Null);
        assert_eq!(v["evidence"][0]["n"], 0);
        assert_eq!(
            v["evidence"][0]["url"],
            "/api/timesheet/questions/7/evidence/0"
        );
        assert_eq!(v["evidence"][0]["frame_sha16"], "a1b2c3d4e5f60718");
    }

    #[test]
    fn titler_selection_follows_config_flag() {
        // use_llm_titles = false -> deterministic heuristic (what tests pin)
        let off = TimesheetConfig {
            use_llm_titles: false,
            ..TimesheetConfig::default()
        };
        let on = TimesheetConfig::default();
        assert!(matches!(select_titler(&off), DayTitler::Heuristic(_)));
        assert!(matches!(select_titler(&on), DayTitler::Lemonade(_)));
    }

    #[test]
    fn local_day_bounds_are_instant_strings_in_order() {
        let day = parse_day("2026-11-06").expect("valid day");
        let (from, to) = local_day_bounds(day).expect("bounds");
        let from = chrono::DateTime::parse_from_rfc3339(&from).expect("from parses");
        let to = chrono::DateTime::parse_from_rfc3339(&to).expect("to parses");
        assert_eq!(from.date_naive(), day);
        assert_eq!(to.date_naive().pred_opt(), Some(day));
        assert!(from < to);
    }
}
