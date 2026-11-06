//! Timesheet AI APIs (issues #44/#45/#46, ADR addendum on #16).
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
//! - `POST /api/timesheet/questions/maintenance?date=YYYY-MM-DD` - the
//!   question debris cleanup (issue #66): closes the day's open questions
//!   whose span is zero-length or fully covered by the day's current
//!   drafts (status `superseded`; `answer` stays NULL, evidence kept).
//!   Idempotent - re-running closes nothing. Regeneration applies the
//!   same rule automatically and reports `questions_closed`.
//!
//! Month-end export (issue #46):
//! - `GET /api/timesheet/export?month=YYYY-MM` - the month's approved
//!   activities (bucket `timesheet-approved`) as a Cubewise offline
//!   template workbook: `Content-Disposition: attachment;
//!   filename=timesheet_<month>.xlsx`. Rows the template contract cannot
//!   express (unknown project/category, hour code with no billable rule,
//!   zero duration) are refused onto a `Warnings` sheet inside the
//!   workbook, never silently dropped; the refusal count also surfaces as
//!   `X-Export-Warnings`. The vocabulary comes from the Cubewise template
//!   asset embedded in `refiner-timesheet`, or the file pointed at by
//!   `[timesheet] vocab_path` in the server config.
//!
//! Daily itinerary drafts (issue #45, approve-aware regeneration #60):
//! - `GET /api/timesheet/draft?date=YYYY-MM-DD` - the day's draft
//!   activities plus status. Every parseable draft row of the day is
//!   listed with a `state` field: `approved` (carries the approve flow's
//!   flag; stays listed across regenerations), `superseded` (replaced by
//!   a regeneration - history, invisible to the review flow), or `new`
//!   (awaiting review). `status.count` covers the current rows
//!   (approved + new) only.
//! - `POST /api/timesheet/draft?date=YYYY-MM-DD` - regenerate the day:
//!   the compiler merges window sessions (backbone), frame analyses, AFK
//!   spans and answered questions into fresh drafts (old unapproved
//!   drafts marked `superseded_by`); optionally one Lemonade call
//!   titles/comments the blocks (`use_llm_titles`, fail-soft to
//!   heuristics). Regeneration is approve-aware (issue #60): sessions
//!   overlapping a `timesheet-approved` span are never re-drafted and
//!   approved drafts are never superseded, so a
//!   regenerate-then-reapprove loop cannot duplicate approved time.
//!
//! Owner writing style (issue #53):
//! - `GET /api/timesheet/style` - the owner's `[timesheet.style]` section
//!   as `{style_prompt, style_examples}`. The prompt is returned as
//!   configured; empty means the built-in default
//!   (`refiner_timesheet::DEFAULT_STYLE_PROMPT`, resolved by
//!   `StyleConfig::effective_prompt`, tested in refiner-timesheet).
//! - `PUT /api/timesheet/style` - body `{style_prompt, style_examples}`;
//!   validates the shape (total content cap 8 KiB, at most 10 example
//!   pairs), writes `[timesheet.style]` into the server config file
//!   (`toml_edit` in-place edit: other sections, key order and comments
//!   are preserved; a one-time `config.toml.bak-style` backup is written
//!   before the first style write), reloads the in-memory config from the
//!   written file and returns it. The frame-analysis worker (#43) reads
//!   its config copy at startup, so a style PUT reaches it on the next
//!   hub restart.
//!
//! Owner review (issue #47):
//! - `GET /api/timesheet/vocab` - the Cubewise template vocabulary
//!   (categories + projects), loaded through the same `CubewiseVocab`
//!   loader as the worker and the export - one source of truth for what
//!   the review UI's dropdowns offer.
//! - `GET /api/timesheet/days?from=YYYY-MM-DD&to=YYYY-MM-DD` - per-day
//!   counts of drafts by review state (draft / reviewed / approved /
//!   superseded) for the day list. Approved drafts keep their draft row
//!   (flag, not tombstone) and survive regeneration untouched (issue
//!   #60), so one bucket query covers every state.
//! - `PUT /api/timesheet/draft/{id}` - persist an owner edit (project,
//!   category, task, comment) onto the stored draft and mark it
//!   `reviewed`. Saved edits live on the draft, not in the approve call:
//!   the review UI saves and approves as two separate actions, and the
//!   approved event stays an exact copy of the stored draft plus
//!   `approved_at`.
//! - `POST /api/timesheet/approve` - body
//!   `{"date": "YYYY-MM-DD", "draft_id": 42?}`; approves one draft or all
//!   remaining drafts of the day: inserts a `timesheet-approved` event
//!   (the draft JSON verbatim plus `approved_at`, ADR addendum on #16)
//!   through the normal core insert, then marks the source draft approved
//!   via data fields (`approved`, `approved_at`, `approved_event_id`).
//!   Already-approved drafts are skipped idempotently. The approved event
//!   is written before the flag - a crash between the two costs a visible
//!   duplicate in the worst case, never a silently missing activity.
//! - `GET /api/timesheet/frame/{sha16}?date=YYYY-MM-DD` - the analysed
//!   frame with that content-hash prefix as JPEG bytes (the review rows'
//!   evidence thumbnails; same media-root resolution rules as the
//!   question evidence endpoint).
//!
//! Token policy (issues #44/#45/#47): the owner review writes (answer,
//! draft regenerate/edit, approve) follow the same open precedent as the
//! other `/api` reads (crate docs, `/health` precedent) - the hub is
//! single-owner and never proxied outside the tailnet, so owner-facing
//! `/api` stays open while device-facing `/ingest` writes keep their
//! token guard.
//!
//! All storage and file work runs on `spawn_blocking`, exactly like the
//! `/api/machines` and `/ingest` handlers. `GET /health` is untouched.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use activity_core::{CoreError, Db, EventRow, BUCKET_AFK, BUCKET_WINDOW};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Router;
use chrono::{Local, NaiveDate, SecondsFormat, TimeZone, Utc};
use refiner_timesheet::{
    current_draft_from_row, export_month_bytes, parse_day, parse_month, plan_day, store_day,
    superseded_question_ids, BlockTitler, CubewiseVocab, DayInput, DraftActivity, HeuristicTitler,
    LemonadeTitler, StatusFilter, StyleConfig, StyleExample, TimesheetConfig, TsQuestion,
    TsQuestionStore, BUCKET_FRAME_ANALYSIS, BUCKET_TIMESHEET_APPROVED, BUCKET_TIMESHEET_DRAFT,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{bad_request, internal_error, json_response, valid_component, AppState};

/// Routes for the question queue, the month-end export, the daily
/// itinerary drafts and the owner review loop (merged into `build_router`,
/// unguarded - see the token policy note in the module docs).
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
            "/api/timesheet/questions/maintenance",
            post(api_questions_maintenance),
        )
        .route("/api/timesheet/export", get(api_export_month))
        .route(
            "/api/timesheet/draft",
            get(api_get_draft).post(api_post_draft),
        )
        .route(
            "/api/timesheet/style",
            get(api_get_style).put(api_put_style),
        )
        .route("/api/timesheet/draft/{id}", put(api_edit_draft))
        .route("/api/timesheet/approve", post(api_approve))
        .route("/api/timesheet/days", get(api_days))
        .route("/api/timesheet/vocab", get(api_vocab))
        .route("/api/timesheet/frame/{sha}", get(api_frame))
}

// ------------------------------------- GET/POST /api/timesheet/draft

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
    // One consistent config copy for titler selection and the compile run
    // (a concurrent style PUT must not change settings mid-request).
    let timesheet = match state.timesheet_snapshot() {
        Ok(timesheet) => timesheet,
        Err(e) => return internal_error(e),
    };
    let titler = select_titler(&timesheet);
    let outcome =
        tokio::task::spawn_blocking(move || regenerate_payload(&state, day, &titler, &timesheet))
            .await;
    match outcome {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("regenerate task failed: {e}")),
    }
}

/// The GET payload: the day's draft rows plus status (issue #60 payload).
/// Every parseable row is listed with a `state` field - `approved`
/// (approved time carried over across regenerations), `superseded`
/// (replaced history), `new` (awaiting review) - so the review page can
/// render the distinction; `status.count` covers the current rows
/// (approved + new) and `generated_at` is the newest batch stamp among
/// them.
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
    let mut current_count = 0usize;
    let mut latest_batch: Option<String> = None;
    for row in &rows {
        if !row_in_day(row, day) {
            continue;
        }
        let Ok(mut payload) = serde_json::from_str::<Value>(&row.data) else {
            continue;
        };
        // The stored payload IS the response shape (activity fields + the
        // `batch` stamp + the review flags the UI renders); the row id and
        // the lifecycle `state` are added. The activity parse guards the
        // shape: a JSON object that is not one of our drafts stays hidden.
        if serde_json::from_value::<DraftActivity>(payload.clone()).is_err() {
            continue;
        }
        let superseded = payload.get("superseded_by").is_some();
        let state = if row_approved(&payload) {
            "approved"
        } else if superseded {
            "superseded"
        } else {
            "new"
        };
        // The review state wins (the same precedence as `row_approved`):
        // a legacy row carrying both flags (an artefact of the original
        // bug) still lists as approved and counts as current.
        if state != "superseded" {
            current_count += 1;
            if let Some(batch) = payload.get("batch").and_then(Value::as_str) {
                if latest_batch
                    .as_ref()
                    .is_none_or(|latest| batch > latest.as_str())
                {
                    latest_batch = Some(batch.to_string());
                }
            }
        }
        payload["id"] = json!(row.id);
        payload["state"] = json!(state);
        drafts.push(payload);
    }
    Ok(json!({
        "day": day.format("%Y-%m-%d").to_string(),
        "status": {
            "generated_at": latest_batch,
            "count": current_count,
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
    timesheet: &TimesheetConfig,
) -> Result<Value, CoreError> {
    let (from, to) = local_day_bounds(day)?;
    let day_str = day.format("%Y-%m-%d").to_string();
    let (stored, plan, existing_questions) = {
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
        // Approve-aware regeneration (issue #60): the approved bucket is
        // the authoritative record of what must not be re-drafted.
        let approved = db.query_event_rows_in(
            Some(BUCKET_TIMESHEET_APPROVED),
            None,
            Some(&from),
            Some(&to),
        )?;
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
            approved,
        };
        let plan = plan_day(&input, &existing_questions, timesheet, titler);
        let stored = store_day(&db, &questions, &plan)?;
        (stored, plan, existing_questions)
    };
    // Supersession guard (issue #66): the recompile closes the day's open
    // questions whose span is zero-length or fully covered by the new
    // drafts, so regeneration never leaves debris in the owner's queue
    // (the maintenance endpoint applies the same rule to existing rows).
    let closed_ids = {
        let questions = state
            .ts_questions
            .lock()
            .map_err(|_| CoreError::Config("question store lock poisoned".to_string()))?;
        let superseded = superseded_question_ids(&existing_questions, &plan.activities);
        questions.supersede_questions(&superseded)?
    };
    let mut payload = draft_payload(state, day)?;
    payload["questions_created"] = json!(stored.question_ids.len());
    payload["questions_closed"] = json!(closed_ids.len());
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

// -------------------------------- POST /api/timesheet/questions/maintenance

#[derive(Debug, Deserialize)]
pub struct MaintenanceQuery {
    /// The owner's local day, `YYYY-MM-DD`.
    date: String,
}

async fn api_questions_maintenance(
    State(state): State<AppState>,
    Query(query): Query<MaintenanceQuery>,
) -> Response {
    let day = match parse_day(&query.date) {
        Ok(day) => day,
        Err(e) => return bad_request(e),
    };
    let outcome = tokio::task::spawn_blocking(move || maintenance_payload(&state, day)).await;
    match outcome {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("maintenance task failed: {e}")),
    }
}

/// The one-off debris cleanup (issue #66): apply the regeneration's
/// supersession rule to existing rows - close the day's open questions
/// whose span is zero-length or fully covered by the day's CURRENT drafts
/// (approved or new; superseded draft history never covers anything).
/// Idempotent: closed questions are no longer open, so re-running closes
/// nothing.
fn maintenance_payload(state: &AppState, day: NaiveDate) -> Result<Value, CoreError> {
    let drafts: Vec<DraftActivity> = {
        let db = state
            .db
            .lock()
            .map_err(|_| CoreError::Config("database lock poisoned".to_string()))?;
        let (from, to) = local_day_bounds(day)?;
        let rows =
            db.query_event_rows_in(Some(BUCKET_TIMESHEET_DRAFT), None, Some(&from), Some(&to))?;
        rows.iter()
            .filter(|row| row_in_day(row, day))
            .filter_map(current_draft_from_row)
            .collect()
    };
    let day_str = day.format("%Y-%m-%d").to_string();
    let store = state
        .ts_questions
        .lock()
        .map_err(|_| CoreError::Config("question store lock poisoned".to_string()))?;
    let open: Vec<TsQuestion> = store
        .list_questions(StatusFilter::Open)?
        .into_iter()
        .filter(|q| q.day == day_str)
        .collect();
    let superseded = superseded_question_ids(&open, &drafts);
    let closed = store.supersede_questions(&superseded)?;
    Ok(json!({
        "date": day_str,
        "closed_ids": closed,
        "open_remaining": open.len() - closed.len(),
    }))
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

// -------------------------------------------------- GET /api/timesheet/export

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    /// The export month, `YYYY-MM`.
    month: String,
}

/// Content type of an .xlsx download (Office Open XML spreadsheet).
const XLSX_CONTENT_TYPE: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

async fn api_export_month(
    State(state): State<AppState>,
    Query(query): Query<ExportQuery>,
) -> Response {
    let Some((year, month)) = parse_month(&query.month) else {
        return bad_request(format!(
            "month must be YYYY-MM with month 01-12, got '{}'",
            query.month
        ));
    };
    let db = state.db.clone();
    let vocab_path = match state.timesheet_snapshot() {
        Ok(timesheet) => timesheet.vocab_path.clone(),
        Err(e) => return internal_error(e),
    };
    let month_label = query.month;
    // Blocking SQLite + xlsx assembly stays off the async runtime.
    let outcome =
        tokio::task::spawn_blocking(move || export_payload(&db, &vocab_path, year, month)).await;
    let (bytes, warning_count) = match outcome {
        Ok(Ok(payload)) => payload,
        Ok(Err(e)) => return internal_error(e),
        Err(e) => return internal_error(format!("export task failed: {e}")),
    };
    let filename = format!("timesheet_{month_label}.xlsx");
    let headers = [
        (header::CONTENT_TYPE, XLSX_CONTENT_TYPE.to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        ),
        (
            header::HeaderName::from_static("x-export-warnings"),
            warning_count.to_string(),
        ),
    ];
    (headers, bytes).into_response()
}

/// Load the vocabulary, query the month's approved activities and render
/// the workbook. The `Err` side carries storage/config failures; refused
/// rows are part of the payload (the workbook's Warnings sheet + count).
fn export_payload(
    db: &Arc<Mutex<activity_core::Db>>,
    vocab_path: &Option<PathBuf>,
    year: i32,
    month: u32,
) -> Result<(Vec<u8>, usize), CoreError> {
    // The bucket holds owner-approved rows only (a few dozen per month), so
    // no time-window query: filter by local start month in the grid build.
    // The lock covers the query only - vocabulary load and xlsx assembly
    // run unlocked so ingest never waits behind them.
    let events = {
        let db = db
            .lock()
            .map_err(|_| CoreError::Config("database lock poisoned".to_string()))?;
        db.query_events(BUCKET_TIMESHEET_APPROVED, None, None)?
    };
    export_month_bytes(&events, year, month, vocab_path.as_deref())
}

// -------------------------------------------------- GET/PUT /api/timesheet/style

/// Total content cap from issue #53: prompt bytes plus example text bytes.
const STYLE_TOTAL_CAP_BYTES: usize = 8 * 1024;
/// Few-shot pair cap from issue #53.
const STYLE_MAX_EXAMPLES: usize = 10;

async fn api_get_style(State(state): State<AppState>) -> Response {
    match state.timesheet_snapshot() {
        Ok(timesheet) => json_response(StatusCode::OK, style_payload(&timesheet.style)),
        Err(e) => internal_error(e),
    }
}

async fn api_put_style(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<StyleBody>,
) -> Response {
    let style = match validate_style(&body.style_prompt, &body.style_examples) {
        Ok(style) => style,
        Err(reason) => return bad_request(reason),
    };
    let Some(config_path) = state.config_path.clone() else {
        return internal_error(
            "no server config file known to this state; the style cannot be persisted",
        );
    };
    let outcome =
        tokio::task::spawn_blocking(move || persist_style_section(&config_path, &style)).await;
    match outcome {
        Ok(Ok(timesheet)) => {
            // Reload: the written file is the source of truth now; swap the
            // shared config so every clone of AppState sees the new style.
            {
                let mut guard = match state.timesheet.lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        return internal_error("timesheet config lock poisoned");
                    }
                };
                *guard = timesheet.clone();
            }
            json_response(StatusCode::OK, style_payload(&timesheet.style))
        }
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("style persist task failed: {e}")),
    }
}

#[derive(Debug, Deserialize)]
pub struct StyleBody {
    style_prompt: String,
    style_examples: Vec<StyleExample>,
}

/// The `{style_prompt, style_examples}` payload every style endpoint
/// returns. The prompt is the configured text - empty means the built-in
/// default, which the prompt builders resolve via
/// `StyleConfig::effective_prompt`.
fn style_payload(style: &StyleConfig) -> Value {
    json!({
        "style_prompt": style.style_prompt,
        "style_examples": style.style_examples,
    })
}

/// Validate and normalize a style PUT body (issue #53): trim the prompt and
/// every example field, enforce the 10-pair cap and the 8 KiB total content
/// cap (prompt + example text, bytes). Returns the normalized style that is
/// persisted and echoed.
fn validate_style(
    style_prompt: &str,
    style_examples: &[StyleExample],
) -> Result<StyleConfig, String> {
    if style_examples.len() > STYLE_MAX_EXAMPLES {
        return Err(format!(
            "at most {STYLE_MAX_EXAMPLES} style_examples are allowed, got {}",
            style_examples.len()
        ));
    }
    let style_prompt = style_prompt.trim();
    let examples: Vec<StyleExample> = style_examples
        .iter()
        .map(|example| StyleExample {
            task: example.task.trim().to_string(),
            comment: example.comment.trim().to_string(),
        })
        .collect();
    let total_bytes = style_prompt.len()
        + examples
            .iter()
            .map(|example| example.task.len() + example.comment.len())
            .sum::<usize>();
    if total_bytes > STYLE_TOTAL_CAP_BYTES {
        return Err(format!(
            "style content is {total_bytes} bytes; the cap is {STYLE_TOTAL_CAP_BYTES} \
             (style_prompt + example text)"
        ));
    }
    Ok(StyleConfig {
        style_prompt: style_prompt.to_string(),
        style_examples: examples,
    })
}

/// Suffix of the one-time backup written before the first style write
/// (`config.toml.bak-style` next to the config file).
const STYLE_BACKUP_SUFFIX: &str = "bak-style";

/// Write `[timesheet.style]` into the server config file (issue #53):
/// `toml_edit` replaces only that section, so every other section, key
/// order and comment of the owner's file is preserved. A one-time backup
/// (`<config>.bak-style`) is written before the first style write and is
/// never overwritten afterwards. The file is replaced atomically
/// (tmp + rename) so a crash cannot leave a torn config behind, and the
/// returned config is re-parsed from the written file: what the caller
/// stores is exactly what the disk now says.
fn persist_style_section(
    config_path: &Path,
    style: &StyleConfig,
) -> Result<TimesheetConfig, CoreError> {
    let original = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(CoreError::Config(format!(
                "cannot read {}: {e}",
                config_path.display()
            )))
        }
    };
    let mut doc = original.parse::<toml_edit::DocumentMut>().map_err(|e| {
        CoreError::Config(format!(
            "cannot parse {}: {e} (the style was NOT written; fix the file by hand \
             or restore it from the .bak-style backup)",
            config_path.display()
        ))
    })?;

    let mut style_tbl = toml_edit::Table::new();
    style_tbl.insert(
        "style_prompt",
        toml_edit::value(style.style_prompt.as_str()),
    );
    if style.style_examples.is_empty() {
        style_tbl.insert("style_examples", toml_edit::value(toml_edit::Array::new()));
    } else {
        let mut examples = toml_edit::ArrayOfTables::new();
        for example in &style.style_examples {
            let mut table = toml_edit::Table::new();
            table.insert("task", toml_edit::value(example.task.as_str()));
            table.insert("comment", toml_edit::value(example.comment.as_str()));
            examples.push(table);
        }
        style_tbl.insert("style_examples", toml_edit::Item::ArrayOfTables(examples));
    }

    if !doc.contains_key("timesheet") {
        // Implicit: only `[timesheet.style]` should appear in a fresh file,
        // matching the spelling the refiner config parser documents.
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        doc.insert("timesheet", toml_edit::Item::Table(table));
    }
    let Some(timesheet) = doc
        .get_mut("timesheet")
        .and_then(|item| item.as_table_mut())
    else {
        return Err(CoreError::Config(format!(
            "{}: [timesheet] is not a table; the style was NOT written",
            config_path.display()
        )));
    };
    timesheet["style"] = toml_edit::Item::Table(style_tbl);

    // One-time backup before the first write; never overwritten afterwards.
    let backup_path = config_path.with_file_name(format!(
        "{}.{STYLE_BACKUP_SUFFIX}",
        config_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ));
    if !backup_path.exists() {
        std::fs::write(&backup_path, &original).map_err(|e| {
            CoreError::Config(format!(
                "cannot write backup {}: {e}",
                backup_path.display()
            ))
        })?;
    }

    let tmp_path = config_path.with_file_name(format!(
        "{}.tmp",
        config_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ));
    std::fs::write(&tmp_path, doc.to_string())
        .map_err(|e| CoreError::Config(format!("cannot write {}: {e}", tmp_path.display())))?;
    std::fs::rename(&tmp_path, config_path).map_err(|e| {
        CoreError::Config(format!(
            "cannot replace {} with the written config: {e}",
            config_path.display()
        ))
    })?;

    // Reload from disk: the return value is exactly what the file says now.
    TimesheetConfig::load_file(config_path)
        .map_err(|e| CoreError::Config(format!("style write-back reload failed: {e}")))
}

// --------------------------------------------------------- shared review helpers

/// `true` when the row's payload is a JSON object without the supersession
/// marker - the rows the review UI lists, counts and approves (superseded
/// and malformed rows are invisible, same rule as `current_draft_from_row`).
fn is_current_draft(row: &EventRow) -> bool {
    serde_json::from_str::<Value>(&row.data)
        .ok()
        .and_then(|data| {
            data.as_object()
                .map(|obj| !obj.contains_key("superseded_by"))
        })
        .unwrap_or(false)
}

/// Whether a draft row carries the approved flag (the review state wins
/// over everything else).
fn row_approved(data: &Value) -> bool {
    data.get("approved")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Current UTC time, RFC3339 with milliseconds - the same stamp convention
/// as the question store and the compiler's batch ids.
fn now_stamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Failure modes of the review write endpoints (draft edit, approve),
/// mapped to status codes at the handler boundary.
enum ReviewError {
    /// 404: unknown draft id (or no such current draft on the day).
    NotFound(String),
    /// 409: the draft moved on (superseded by a regeneration, or already
    /// approved) - the stored state wins over the stale request.
    Conflict(String),
    /// 500: storage failure.
    Storage(CoreError),
}

impl From<CoreError> for ReviewError {
    fn from(e: CoreError) -> ReviewError {
        ReviewError::Storage(e)
    }
}

fn review_error_response(e: ReviewError) -> Response {
    match e {
        ReviewError::NotFound(msg) => (StatusCode::NOT_FOUND, msg).into_response(),
        ReviewError::Conflict(msg) => (StatusCode::CONFLICT, msg).into_response(),
        ReviewError::Storage(e) => internal_error(e),
    }
}

/// The day's current draft rows (superseded and malformed ones excluded),
/// ascending by start instant.
fn current_day_draft_rows(db: &Db, day: NaiveDate) -> Result<Vec<EventRow>, CoreError> {
    let (from, to) = local_day_bounds(day)?;
    let rows =
        db.query_event_rows_in(Some(BUCKET_TIMESHEET_DRAFT), None, Some(&from), Some(&to))?;
    Ok(rows
        .into_iter()
        .filter(|row| row_in_day(row, day) && is_current_draft(row))
        .collect())
}

// ---------------------------------------------------- PUT /api/timesheet/draft/{id}

/// An owner edit on a stored draft: the four fields the review UI lets the
/// owner change (the row's start/end, confidence and evidence sources are
/// compiler facts and stay untouched).
#[derive(Debug, Deserialize)]
pub struct DraftEditBody {
    project_guess: String,
    category: String,
    task: String,
    comment: String,
}

async fn api_edit_draft(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<i64>,
    axum::Json(body): axum::Json<DraftEditBody>,
) -> Response {
    let project = body.project_guess.trim().to_string();
    let category = body.category.trim().to_string();
    let task = body.task.trim().to_string();
    if project.is_empty() || category.is_empty() || task.is_empty() {
        return bad_request("project_guess, category and task must be non-empty");
    }
    let edits = (project, category, task, body.comment.trim().to_string());
    let outcome = tokio::task::spawn_blocking(move || edit_draft_payload(&state, id, edits)).await;
    match outcome {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload),
        Ok(Err(e)) => review_error_response(e),
        Err(e) => internal_error(format!("draft edit task failed: {e}")),
    }
}

/// Persist the edit onto the draft row in place and mark it `reviewed`.
/// The approved flow reads the draft as stored - that is the documented
/// saved-edits decision (issue #47): edits live on the draft, so Save and
/// Approve are two independent actions and the approved event is always an
/// exact copy of what the owner saw when approving.
fn edit_draft_payload(
    state: &AppState,
    id: i64,
    edits: (String, String, String, String),
) -> Result<Value, ReviewError> {
    let (project, category, task, comment) = edits;
    let db = state.db.lock().map_err(|_| {
        ReviewError::Storage(CoreError::Config("database lock poisoned".to_string()))
    })?;
    let rows = db
        .query_event_rows_in(Some(BUCKET_TIMESHEET_DRAFT), None, None, None)
        .map_err(ReviewError::Storage)?;
    let row = rows
        .into_iter()
        .find(|row| row.id == id)
        .ok_or_else(|| ReviewError::NotFound(format!("no draft {id}")))?;
    let mut data: Value = serde_json::from_str(&row.data)
        .map_err(CoreError::from)
        .map_err(ReviewError::Storage)?;
    if data.get("superseded_by").is_some() {
        return Err(ReviewError::Conflict(format!(
            "draft {id} is superseded by a regeneration"
        )));
    }
    if row_approved(&data) {
        return Err(ReviewError::Conflict(format!(
            "draft {id} is already approved"
        )));
    }
    data["project_guess"] = json!(project);
    data["category"] = json!(category);
    data["task"] = json!(task);
    data["comment"] = json!(comment);
    data["reviewed"] = json!(true);
    data["reviewed_at"] = json!(now_stamp());
    db.update_event_data(id, &data.to_string())
        .map_err(ReviewError::Storage)?;
    data["id"] = json!(id);
    Ok(data)
}

// -------------------------------------------------- POST /api/timesheet/approve

#[derive(Debug, Deserialize)]
pub struct ApproveBody {
    /// The owner's local day, `YYYY-MM-DD`; approves the day's remaining
    /// unapproved drafts (or only `draft_id` when given).
    date: String,
    /// Approve exactly this draft instead of the whole day.
    draft_id: Option<i64>,
}

async fn api_approve(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<ApproveBody>,
) -> Response {
    let day = match parse_day(&body.date) {
        Ok(day) => day,
        Err(e) => return bad_request(e),
    };
    let draft_id = body.draft_id;
    let outcome = tokio::task::spawn_blocking(move || approve_payload(&state, day, draft_id)).await;
    match outcome {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload),
        Ok(Err(e)) => review_error_response(e),
        Err(e) => internal_error(format!("approve task failed: {e}")),
    }
}

/// Approve one draft (or every remaining draft of the day): insert the
/// `timesheet-approved` event - the draft JSON verbatim plus `approved_at`,
/// same timestamp/duration/device as the draft row (ADR addendum on #16:
/// "same JSON shape as drafts plus approved_at") - then flag the draft.
/// Approving an already-approved draft is an idempotent no-op, reported in
/// `already_approved_ids`.
fn approve_payload(
    state: &AppState,
    day: NaiveDate,
    draft_id: Option<i64>,
) -> Result<Value, ReviewError> {
    let db = state.db.lock().map_err(|_| {
        ReviewError::Storage(CoreError::Config("database lock poisoned".to_string()))
    })?;
    let rows = current_day_draft_rows(&db, day)?;
    let mut targets: Vec<EventRow> = Vec::new();
    let mut already_approved_ids: Vec<i64> = Vec::new();
    match draft_id {
        Some(id) => match rows.into_iter().find(|row| row.id == id) {
            Some(row) => {
                if row_approved(&draft_data(&row)?) {
                    already_approved_ids.push(id);
                } else {
                    targets.push(row);
                }
            }
            None => {
                return Err(ReviewError::NotFound(format!(
                    "no current draft {id} on {day}"
                )));
            }
        },
        None => {
            for row in rows {
                if row_approved(&draft_data(&row)?) {
                    already_approved_ids.push(row.id);
                } else {
                    targets.push(row);
                }
            }
        }
    }
    let stamp = now_stamp();
    let mut approved_ids = Vec::with_capacity(targets.len());
    for row in &targets {
        let mut data = draft_data(row)?;
        data["approved_at"] = json!(stamp);
        // Approved event first, flag second: a crash between the two writes
        // costs a visible duplicate on re-approve, never a silently missing
        // activity in the export.
        let envelope = activity_core::EventEnvelope {
            bucket: BUCKET_TIMESHEET_APPROVED.to_string(),
            timestamp: row.ts_start.clone(),
            duration_s: row.duration_s,
            data: data.clone(),
        };
        let approved_id = db.insert_event(&envelope, &row.device)?;
        db.mark_synced(&[approved_id])?;
        data["approved"] = json!(true);
        data["approved_event_id"] = json!(approved_id);
        db.update_event_data(row.id, &data.to_string())?;
        approved_ids.push(row.id);
    }
    Ok(json!({
        "day": day.format("%Y-%m-%d").to_string(),
        "approved": approved_ids.len(),
        "approved_ids": approved_ids,
        "already_approved_ids": already_approved_ids,
    }))
}

/// A draft row's payload as a JSON value (storage failures carried as
/// [`ReviewError::Storage`]).
fn draft_data(row: &EventRow) -> Result<Value, ReviewError> {
    serde_json::from_str(&row.data)
        .map_err(CoreError::from)
        .map_err(ReviewError::Storage)
}

// ----------------------------------------------------- GET /api/timesheet/days

/// Maximum day-list range (the UI navigates one month at a time; the cap
/// mirrors the calendar's bounded-range rule).
const DAY_RANGE_MAX_DAYS: i64 = 62;

#[derive(Debug, Deserialize)]
pub struct DaysQuery {
    /// Range start, `YYYY-MM-DD` (inclusive).
    from: String,
    /// Range end, `YYYY-MM-DD` (inclusive).
    to: String,
}

/// Per-day review-state counts for the day list (issue #47, superseded
/// count per issue #60): current drafts counted by state - `draft`
/// (untouched), `reviewed` (owner edited it), `approved` (never removed
/// by a regeneration) - plus `superseded` (rows replaced by
/// regenerations). Days without any draft rows are absent.
async fn api_days(State(state): State<AppState>, Query(query): Query<DaysQuery>) -> Response {
    let from = match parse_day(&query.from) {
        Ok(day) => day,
        Err(e) => return bad_request(e),
    };
    let to = match parse_day(&query.to) {
        Ok(day) => day,
        Err(e) => return bad_request(e),
    };
    if to < from {
        return bad_request("from must not be after to");
    }
    if (to - from).num_days() + 1 > DAY_RANGE_MAX_DAYS {
        return bad_request(format!("range too long (max {DAY_RANGE_MAX_DAYS} days)"));
    }
    let outcome = tokio::task::spawn_blocking(move || days_payload(&state, from, to)).await;
    match outcome {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("days task failed: {e}")),
    }
}

fn days_payload(state: &AppState, from: NaiveDate, to: NaiveDate) -> Result<Value, CoreError> {
    let range_start = local_day_bounds(from)?.0;
    let range_end = local_day_bounds(to)?.1;
    let rows = {
        let db = state
            .db
            .lock()
            .map_err(|_| CoreError::Config("database lock poisoned".to_string()))?;
        db.query_event_rows_in(
            Some(BUCKET_TIMESHEET_DRAFT),
            None,
            Some(&range_start),
            Some(&range_end),
        )?
    };
    // (draft, reviewed, approved, superseded) per day, keyed by the row's
    // own local date so a differently-offset device still lands on its own
    // day.
    let mut counts: std::collections::BTreeMap<NaiveDate, (u32, u32, u32, u32)> =
        std::collections::BTreeMap::new();
    for row in &rows {
        let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&row.ts_start) else {
            continue;
        };
        let day = ts.date_naive();
        if day < from || day > to {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(&row.data) else {
            continue;
        };
        let entry = counts.entry(day).or_default();
        // Same precedence as the draft payload's `state` (review state
        // wins): approved, then superseded, then reviewed, then draft.
        if row_approved(&data) {
            entry.2 += 1;
        } else if data.get("superseded_by").is_some() {
            entry.3 += 1;
        } else if data
            .get("reviewed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            entry.1 += 1;
        } else {
            entry.0 += 1;
        }
    }
    let days: Vec<Value> = counts
        .into_iter()
        .map(|(day, (draft, reviewed, approved, superseded))| {
            json!({
                "day": day.format("%Y-%m-%d").to_string(),
                "draft": draft,
                "reviewed": reviewed,
                "approved": approved,
                "superseded": superseded,
            })
        })
        .collect();
    Ok(json!({ "days": days }))
}

// ----------------------------------------------------- GET /api/timesheet/vocab

/// Dropdown source for the review rows: the exact template strings the
/// export accepts, from the same vocabulary loader as the worker and the
/// export (embedded seed or `[timesheet] vocab_path` override) - one
/// source of truth, so an approved row can always round-trip the export.
async fn api_vocab(State(state): State<AppState>) -> Response {
    // Snapshot pattern (issue #53): the shared config lives behind an
    // Arc<Mutex<>> - read it through the accessor so a style PUT's swap
    // is visible here, never a stale startup copy.
    let vocab_path = match state.timesheet_snapshot() {
        Ok(timesheet) => timesheet.vocab_path.clone(),
        Err(e) => return internal_error(e),
    };
    let outcome = tokio::task::spawn_blocking(move || vocab_payload(vocab_path.as_deref())).await;
    match outcome {
        Ok(Ok(payload)) => json_response(StatusCode::OK, payload),
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("vocab task failed: {e}")),
    }
}

fn vocab_payload(vocab_path: Option<&Path>) -> Result<Value, CoreError> {
    let vocab = CubewiseVocab::load(vocab_path)?;
    Ok(json!({
        "categories": vocab.categories,
        "projects": vocab.projects,
    }))
}

// ------------------------------------------------ GET /api/timesheet/frame/{sha}

#[derive(Debug, Deserialize)]
pub struct FrameQuery {
    /// The owner's local day whose analyses to search, `YYYY-MM-DD`.
    date: String,
}

/// A frame evidence key: exactly 16 hex characters (the watcher-screen
/// dedup prefix). Doubles as the route segment's path-safety check.
fn valid_sha16(sha: &str) -> bool {
    sha.len() == 16 && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// The analysed frame with content-hash prefix `sha` as JPEG bytes: the
/// evidence thumbnails of the review rows. The day narrows the search to
/// that day's `frame-analysis` rows (the draft's sources reference frames
/// of its own day); the stored path resolves against the media root under
/// the same component rules as the question evidence endpoint.
async fn api_frame(
    State(state): State<AppState>,
    AxumPath(sha): AxumPath<String>,
    Query(query): Query<FrameQuery>,
) -> Response {
    if !valid_sha16(&sha) {
        return bad_request("sha must be 16 hex characters");
    }
    let day = match parse_day(&query.date) {
        Ok(day) => day,
        Err(e) => return bad_request(e),
    };
    let sha = sha.to_ascii_lowercase();
    let db = state.db.clone();
    let media_dir = state.media_dir.clone();
    let outcome =
        tokio::task::spawn_blocking(move || frame_bytes(&db, &media_dir, day, &sha)).await;
    match outcome {
        // join error | storage error | the 404 cases (already a Response)
        Ok(Ok(Ok(bytes))) => ([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response(),
        Ok(Ok(Err(not_found))) => not_found,
        Ok(Err(e)) => internal_error(e),
        Err(e) => internal_error(format!("frame task failed: {e}")),
    }
}

/// Same split as [`evidence_frame_bytes`]: outer `Err` = storage failure,
/// inner `Err` = an already-built 404 for everything that simply does not
/// exist (no such analysis that day, path outside the media convention,
/// missing file).
fn frame_bytes(
    db: &Arc<Mutex<Db>>,
    media_dir: &Path,
    day: NaiveDate,
    sha: &str,
) -> Result<Result<Vec<u8>, Response>, CoreError> {
    let not_found = |msg: String| -> Response { (StatusCode::NOT_FOUND, msg).into_response() };
    let (from, to) = local_day_bounds(day)?;
    let rows = {
        let db = db
            .lock()
            .map_err(|_| CoreError::Config("database lock poisoned".to_string()))?;
        db.query_event_rows_in(Some(BUCKET_FRAME_ANALYSIS), None, Some(&from), Some(&to))?
    };
    let mut frame_path: Option<String> = None;
    for row in &rows {
        if !row_in_day(row, day) {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(&row.data) else {
            continue;
        };
        if data.get("frame_sha16").and_then(Value::as_str) == Some(sha) {
            frame_path = data
                .get("frame_path")
                .and_then(Value::as_str)
                .map(str::to_string);
            break;
        }
    }
    let Some(frame_path) = frame_path else {
        return Ok(Err(not_found(format!("no frame analysis {sha} on {day}"))));
    };
    let path = match resolve_frame_path(media_dir, &frame_path) {
        Some(path) => path,
        None => {
            return Ok(Err(not_found(format!(
                "frame path '{}' is not a media-relative path",
                frame_path
            ))))
        }
    };
    Ok(match std::fs::read(&path) {
        Ok(bytes) => Ok(bytes),
        Err(_) => Err(not_found(format!(
            "frame '{}' not found on disk",
            frame_path
        ))),
    })
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

    // -------------------------------------------------- export endpoint

    use std::time::{SystemTime, UNIX_EPOCH};

    use activity_core::EventEnvelope;
    use axum::body::Body;
    use calamine::{Data, DataType, Reader};
    use tower::ServiceExt;

    /// Self-cleaning temp directory (same pattern as the retention tests).
    struct ExportTempDir(PathBuf);

    impl ExportTempDir {
        fn new(tag: &str) -> ExportTempDir {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("activity-server-export-{tag}-{nanos}"));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            ExportTempDir(dir)
        }
    }

    impl Drop for ExportTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An approved-activity envelope for the export fixtures: 1 h of
    /// `project`/`category` work starting at `ts`.
    fn approved_envelope(ts: &str, project: &str, category: &str) -> EventEnvelope {
        let end = format!("{}T10:00:00+08:00", &ts[..10]);
        EventEnvelope {
            bucket: BUCKET_TIMESHEET_APPROVED.to_string(),
            timestamp: ts.to_string(),
            duration_s: 3600.0,
            data: json!({
                "start": ts,
                "end": end,
                "project_guess": project,
                "category": category,
                "task": "fixture task",
                "comment": "",
            }),
        }
    }

    fn insert(state: &AppState, envelope: EventEnvelope) {
        state
            .db
            .lock()
            .expect("db lock")
            .insert_event(&envelope, "owner-laptop")
            .expect("insert");
    }

    async fn get_export(app: Router, uri: &str) -> axum::http::Response<Body> {
        app.oneshot(
            axum::http::Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response")
    }

    async fn response_bytes(response: axum::http::Response<Body>) -> Vec<u8> {
        use http_body_util::BodyExt;
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        bytes.to_vec()
    }

    #[tokio::test]
    async fn export_month_downloads_workbook_with_warnings() {
        let dir = ExportTempDir::new("happy");
        let state = AppState::open(&dir.0.join("activity.db"), dir.0.join("media")).expect("state");
        insert(
            &state,
            approved_envelope(
                "2026-10-06T09:00:00+08:00",
                "A143P010 - Frasers - H2 FH - BU Core",
                "Consulting",
            ),
        );
        insert(
            &state,
            approved_envelope(
                "2026-10-07T09:00:00+08:00",
                "C000P003 - Cubewise - Timesheet Development",
                "Management",
            ),
        );
        // Refused: not in the vocabulary -> reported, never silent.
        insert(
            &state,
            approved_envelope(
                "2026-10-08T09:00:00+08:00",
                "Z999P999 - Mystery - Not In Vocabulary",
                "Consulting",
            ),
        );
        // Other buckets and months are ignored.
        insert(
            &state,
            EventEnvelope {
                bucket: "timesheet-draft".to_string(),
                timestamp: "2026-10-06T11:00:00+08:00".to_string(),
                duration_s: 600.0,
                data: json!({ "task": "drafts never export" }),
            },
        );
        insert(
            &state,
            approved_envelope(
                "2026-11-06T09:00:00+08:00",
                "A143P010 - Frasers - H2 FH - BU Core",
                "Consulting",
            ),
        );

        let app = crate::build_router(state);
        let response = get_export(app, "/api/timesheet/export?month=2026-10").await;

        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(
            headers.get("Content-Type").expect("content type"),
            XLSX_CONTENT_TYPE
        );
        assert_eq!(
            headers.get("Content-Disposition").expect("disposition"),
            "attachment; filename=\"timesheet_2026-10.xlsx\""
        );
        assert_eq!(headers.get("X-Export-Warnings").expect("warnings"), "1");

        let bytes = response_bytes(response).await;
        assert_eq!(&bytes[..4], b"PK\x03\x04", "xlsx is a zip container");
        let mut book: calamine::Xlsx<_> =
            calamine::open_workbook_from_rs(std::io::Cursor::new(bytes)).expect("workbook opens");
        let upload = book.worksheet_range("upload").expect("upload sheet");
        assert_eq!(upload.height(), 3, "header + the 2 in-vocabulary rows");
        assert_eq!(
            upload.get_value((1, 0)),
            Some(&Data::String(
                "A143P010 - Frasers - H2 FH - BU Core".to_string()
            ))
        );
        assert_eq!(upload.get_value((1, 6)), Some(&Data::Float(1.0)));
        assert_eq!(
            upload.get_value((2, 0)),
            Some(&Data::String(
                "C000P003 - Cubewise - Timesheet Development".to_string()
            ))
        );
        assert_eq!(upload.get_value((2, 7)), Some(&Data::Float(1.0)));
        let warnings = book.worksheet_range("Warnings").expect("warnings sheet");
        assert_eq!(warnings.height(), 2);
        assert!(warnings
            .get_value((1, 0))
            .and_then(|v| v.as_string())
            .expect("warning text")
            .contains("not in the Cubewise vocabulary"));
    }

    #[tokio::test]
    async fn export_month_empty_is_headers_only() {
        let dir = ExportTempDir::new("empty");
        let state = AppState::open(&dir.0.join("activity.db"), dir.0.join("media")).expect("state");
        let app = crate::build_router(state);
        let response = get_export(app, "/api/timesheet/export?month=2026-01").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("X-Export-Warnings")
                .expect("warnings"),
            "0"
        );
        let bytes = response_bytes(response).await;
        let mut book: calamine::Xlsx<_> =
            calamine::open_workbook_from_rs(std::io::Cursor::new(bytes)).expect("workbook opens");
        let upload = book.worksheet_range("upload").expect("upload sheet");
        assert_eq!(upload.height(), 1, "headers only");
        assert!(book.worksheet_range("Warnings").is_err());
    }

    #[tokio::test]
    async fn export_month_rejects_bad_month() {
        let dir = ExportTempDir::new("badmonth");
        let state = AppState::open(&dir.0.join("activity.db"), dir.0.join("media")).expect("state");
        let app = crate::build_router(state);
        for uri in [
            "/api/timesheet/export",
            "/api/timesheet/export?month=2026-1",
            "/api/timesheet/export?month=2026-13",
            "/api/timesheet/export?month=October",
        ] {
            let response = get_export(app.clone(), uri).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        }
    }

    #[tokio::test]
    async fn export_uses_configured_vocab_path() {
        let dir = ExportTempDir::new("vocab");
        let vocab_path = dir.0.join("custom_vocab.toml");
        std::fs::write(
            &vocab_path,
            "categories = [\"Consulting\"]\nprojects = [\"A100P001 - Test - Project\"]\n",
        )
        .expect("write vocab");
        let state = AppState::open_with_retention(
            &dir.0.join("activity.db"),
            dir.0.join("media"),
            crate::AuthConfig::disabled(),
            crate::retention::RetentionConfig::default(),
            TimesheetConfig {
                vocab_path: Some(vocab_path),
                ..TimesheetConfig::default()
            },
        )
        .expect("state");
        insert(
            &state,
            approved_envelope(
                "2026-10-06T09:00:00+08:00",
                "A100P001 - Test - Project",
                "Consulting",
            ),
        );
        // Known in the template vocabulary but not in the override -> refused.
        insert(
            &state,
            approved_envelope(
                "2026-10-07T09:00:00+08:00",
                "A143P010 - Frasers - H2 FH - BU Core",
                "Consulting",
            ),
        );
        let app = crate::build_router(state);
        let response = get_export(app, "/api/timesheet/export?month=2026-10").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("X-Export-Warnings")
                .expect("warnings"),
            "1"
        );
        let bytes = response_bytes(response).await;
        let mut book: calamine::Xlsx<_> =
            calamine::open_workbook_from_rs(std::io::Cursor::new(bytes)).expect("workbook opens");
        let upload = book.worksheet_range("upload").expect("upload sheet");
        assert_eq!(upload.height(), 2);
        assert_eq!(
            upload.get_value((1, 0)),
            Some(&Data::String("A100P001 - Test - Project".to_string()))
        );
    }

    #[tokio::test]
    async fn export_fails_loudly_on_broken_vocab_path() {
        let dir = ExportTempDir::new("brokenvocab");
        let vocab_path = dir.0.join("broken_vocab.toml");
        std::fs::write(
            &vocab_path,
            "categories = [\"Consulting\", \"Consulting\"]\n",
        )
        .expect("write broken vocab");
        let state = AppState::open_with_retention(
            &dir.0.join("activity.db"),
            dir.0.join("media"),
            crate::AuthConfig::disabled(),
            crate::retention::RetentionConfig::default(),
            TimesheetConfig {
                vocab_path: Some(vocab_path),
                ..TimesheetConfig::default()
            },
        )
        .expect("state");
        let app = crate::build_router(state);
        let response = get_export(app, "/api/timesheet/export?month=2026-10").await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a configured vocabulary that fails to parse must not export with another"
        );
    }

    // ------------------ approve-aware regeneration (issue #60)

    const ISSUE60_DAY: &str = "2026-11-06";
    const ISSUE60_PROJECT: &str = "C000P003 - Cubewise - Timesheet Development";

    /// RFC3339 stamp at `hour:min` of the fixture day in the LOCAL offset:
    /// the day queries (`local_day_bounds`) and the compiler's own-offset
    /// attribution then agree regardless of the test box's timezone.
    fn issue60_ts(hour: u32, min: u32) -> String {
        let date = parse_day(ISSUE60_DAY).expect("fixture day");
        Local
            .from_local_datetime(&date.and_hms_opt(hour, min, 0).expect("valid time"))
            .earliest()
            .expect("local time resolves")
            .to_rfc3339()
    }

    fn issue60_session(hour: u32, min: u32) -> EventEnvelope {
        EventEnvelope {
            bucket: BUCKET_WINDOW.to_string(),
            timestamp: issue60_ts(hour, min),
            duration_s: 3600.0,
            data: json!({"app": "Code.exe", "title": "window title"}),
        }
    }

    fn issue60_analysis(id: i64, hour: u32, min: u32) -> EventEnvelope {
        EventEnvelope {
            bucket: BUCKET_FRAME_ANALYSIS.to_string(),
            timestamp: issue60_ts(hour, min),
            duration_s: 0.0,
            data: json!({
                "frame_sha16": format!("sha-{id:04}"),
                "frame_path": "media/screen/2026/11/06/frame.jpg",
                "activity": "working on the timesheet pipeline",
                "project_guess": ISSUE60_PROJECT,
                "category": "Management",
                "confidence": 0.9,
                "evidence": [],
                "model": "qwen",
                "sampled": true,
            }),
        }
    }

    /// Heuristic-titled app state (hermetic: no Lemonade call) plus the
    /// temp dir that owns its database file - kept in scope by the caller
    /// so the file outlives the connection.
    fn issue60_setup(tag: &str) -> (ExportTempDir, AppState) {
        let dir = ExportTempDir::new(tag);
        let state = AppState::open_with_retention(
            &dir.0.join("activity.db"),
            dir.0.join("media"),
            crate::AuthConfig::disabled(),
            crate::retention::RetentionConfig::default(),
            TimesheetConfig {
                use_llm_titles: false,
                ..TimesheetConfig::default()
            },
        )
        .expect("state");
        (dir, state)
    }

    fn issue60_seed(state: &AppState) {
        // Two non-adjacent same-project sessions (the 1 h gap exceeds the
        // adjacency limit, so no merging): two separate drafts per compile.
        let db = state.db.lock().expect("db lock");
        for envelope in [
            issue60_session(10, 0),
            issue60_session(12, 0),
            issue60_analysis(1, 10, 30),
            issue60_analysis(2, 12, 30),
        ] {
            db.insert_event(&envelope, "owner-laptop").expect("insert");
        }
    }

    async fn post_json(app: Router, uri: &str, body: &Value) -> axum::http::Response<Body> {
        app.oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response")
    }

    /// The regression core (issue #60 acceptance): approve a day,
    /// regenerate, approve again - the export must hold exactly one row
    /// per real span, with no warnings.
    #[tokio::test]
    async fn approve_regenerate_reapprove_never_double_counts() {
        let (_dir, state) = issue60_setup("issue60-regen");
        issue60_seed(&state);
        let app = crate::build_router(state.clone());

        // Compile day one: two fresh drafts.
        let response = post_json(
            app.clone(),
            "/api/timesheet/draft?date=2026-11-06",
            &json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = response_json(response).await;
        assert_eq!(payload["drafts"].as_array().expect("drafts").len(), 2);
        for draft in payload["drafts"].as_array().expect("drafts") {
            assert_eq!(draft["state"], "new");
        }

        // Approve everything.
        let response = post_json(
            app.clone(),
            "/api/timesheet/approve",
            &json!({"date": ISSUE60_DAY}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = response_json(response).await;
        assert_eq!(payload["approved"], 2);

        // Regenerate after approval.
        let response = post_json(
            app.clone(),
            "/api/timesheet/draft?date=2026-11-06",
            &json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = response_json(response).await;

        // The day still shows the two approved activities (state
        // `approved`), and nothing was re-drafted on top of them.
        let drafts = payload["drafts"].as_array().expect("drafts");
        assert_eq!(drafts.len(), 2, "approved rows stay, nothing re-drafted");
        for draft in drafts {
            assert_eq!(draft["state"], "approved");
            assert_eq!(draft["approved"], true);
        }
        assert_eq!(payload["status"]["count"], 2);
        assert_eq!(payload["superseded"], 0, "approved drafts never superseded");

        // Approve again: idempotent, no new approved events.
        let response = post_json(
            app.clone(),
            "/api/timesheet/approve",
            &json!({"date": ISSUE60_DAY}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = response_json(response).await;
        assert_eq!(payload["approved"], 0);
        assert_eq!(
            payload["already_approved_ids"]
                .as_array()
                .expect("ids")
                .len(),
            2
        );

        // The month export holds exactly one row per span - no duplicated
        // hours - and the guard had nothing to warn about.
        let response = get_export(app, "/api/timesheet/export?month=2026-11").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("X-Export-Warnings").expect("hdr"),
            "0"
        );
        let bytes = response_bytes(response).await;
        let mut book: calamine::Xlsx<_> =
            calamine::open_workbook_from_rs(std::io::Cursor::new(bytes)).expect("workbook opens");
        let upload = book.worksheet_range("upload").expect("upload sheet");
        assert_eq!(upload.height(), 3, "header + exactly 2 rows");
        // C000P* is internal: 1 h lands in NonBillable (col 7) per row.
        assert_eq!(upload.get_value((1, 7)), Some(&Data::Float(1.0)));
        assert_eq!(upload.get_value((2, 7)), Some(&Data::Float(1.0)));
    }

    #[tokio::test]
    async fn approve_one_regenerate_approve_rest_keeps_one_row_per_span() {
        let (_dir, state) = issue60_setup("issue60-mixed");
        issue60_seed(&state);
        let app = crate::build_router(state.clone());

        // Compile, approve only the first draft.
        let response = post_json(
            app.clone(),
            "/api/timesheet/draft?date=2026-11-06",
            &json!({}),
        )
        .await;
        let payload = response_json(response).await;
        let first_id = payload["drafts"][0]["id"].as_i64().expect("draft id");
        let response = post_json(
            app.clone(),
            "/api/timesheet/approve",
            &json!({"date": ISSUE60_DAY, "draft_id": first_id}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // Regenerate: the approved draft stays listed as `approved`, the
        // unapproved span comes back as a fresh `new` draft, the replaced
        // stale copy is exposed as `superseded`.
        let response = post_json(
            app.clone(),
            "/api/timesheet/draft?date=2026-11-06",
            &json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = response_json(response).await;
        let states: Vec<String> = payload["drafts"]
            .as_array()
            .expect("drafts")
            .iter()
            .map(|d| d["state"].as_str().expect("state").to_string())
            .collect();
        let mut sorted_states = states.clone();
        sorted_states.sort();
        assert_eq!(sorted_states, vec!["approved", "new", "superseded"]);
        assert_eq!(payload["status"]["count"], 2, "current rows only");
        assert_eq!(payload["superseded"], 1);

        // The day list exposes the same distinction.
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/timesheet/days?from=2026-11-06&to=2026-11-06")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = response_json(response).await;
        let day = &payload["days"][0];
        assert_eq!(day["day"], ISSUE60_DAY);
        assert_eq!(day["draft"], 1, "the fresh unapproved draft");
        assert_eq!(day["reviewed"], 0);
        assert_eq!(day["approved"], 1, "the approved draft stays counted");
        assert_eq!(day["superseded"], 1, "the replaced stale copy");

        // Approve the rest, export: one row per span, nothing doubled.
        let response = post_json(
            app.clone(),
            "/api/timesheet/approve",
            &json!({"date": ISSUE60_DAY}),
        )
        .await;
        let payload = response_json(response).await;
        assert_eq!(payload["approved"], 1, "only the fresh draft approves");

        let response = get_export(app, "/api/timesheet/export?month=2026-11").await;
        assert_eq!(
            response.headers().get("X-Export-Warnings").expect("hdr"),
            "0"
        );
        let bytes = response_bytes(response).await;
        let mut book: calamine::Xlsx<_> =
            calamine::open_workbook_from_rs(std::io::Cursor::new(bytes)).expect("workbook opens");
        let upload = book.worksheet_range("upload").expect("upload sheet");
        assert_eq!(upload.height(), 3, "header + exactly 2 rows");
    }

    #[tokio::test]
    async fn export_dedups_overlapping_approved_rows_with_warning() {
        // Defence-in-depth guard (issue #60, through the API): two
        // identical approved events on the same project + category with
        // different stamps keep the earliest and warn; a same-instant row
        // on another project is untouched.
        let dir = ExportTempDir::new("issue60-guard");
        let state = AppState::open(&dir.0.join("activity.db"), dir.0.join("media")).expect("state");
        let overlap = |project: &str, approved_at: &str| EventEnvelope {
            bucket: BUCKET_TIMESHEET_APPROVED.to_string(),
            timestamp: "2026-10-06T09:00:00+08:00".to_string(),
            duration_s: 3600.0,
            data: json!({
                "start": "2026-10-06T09:00:00+08:00",
                "end": "2026-10-06T10:00:00+08:00",
                "project_guess": project,
                "category": "Consulting",
                "task": "duplicated span",
                "comment": "",
                "approved_at": approved_at,
            }),
        };
        {
            let db = state.db.lock().expect("db lock");
            for envelope in [
                overlap(ISSUE60_PROJECT, "2026-10-06T11:00:00.000Z"),
                overlap(ISSUE60_PROJECT, "2026-10-06T10:00:00.000Z"),
                overlap(
                    "A143P010 - Frasers - H2 FH - BU Core",
                    "2026-10-06T10:00:00.000Z",
                ),
            ] {
                db.insert_event(&envelope, "owner-laptop").expect("insert");
            }
        }
        let app = crate::build_router(state);
        let response = get_export(app, "/api/timesheet/export?month=2026-10").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("X-Export-Warnings").expect("hdr"),
            "1",
            "the duplicated pair warns once"
        );
        let bytes = response_bytes(response).await;
        let mut book: calamine::Xlsx<_> =
            calamine::open_workbook_from_rs(std::io::Cursor::new(bytes)).expect("workbook opens");
        let upload = book.worksheet_range("upload").expect("upload sheet");
        assert_eq!(upload.height(), 3, "header + 2 rows (one per project)");
        let warnings = book.worksheet_range("Warnings").expect("warnings sheet");
        assert!(warnings
            .get_value((1, 0))
            .and_then(|v| v.as_string())
            .expect("warning text")
            .contains("duplicate row"));
    }

    // -------------------------------------------------- draft endpoints

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

    // -------------------------------------------------- style endpoints

    use refiner_timesheet::StyleExample;

    /// App state with a config file seeded at `dir/config.toml` (or none
    /// seeded when `config_text` is None) and the style PUT wired to it.
    fn style_app(dir: &Path, config_text: Option<&str>) -> (AppState, PathBuf) {
        let config_path = dir.join("config.toml");
        if let Some(text) = config_text {
            std::fs::write(&config_path, text).expect("seed config file");
        }
        let state = AppState::open_with_retention(
            &dir.join("activity.db"),
            dir.join("media"),
            crate::AuthConfig::disabled(),
            crate::retention::RetentionConfig::default(),
            TimesheetConfig::default(),
        )
        .expect("state")
        .with_config_path(config_path.clone());
        (state, config_path)
    }

    async fn put_style(app: Router, body: &Value) -> axum::http::Response<Body> {
        app.oneshot(
            axum::http::Request::builder()
                .method("PUT")
                .uri("/api/timesheet/style")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response")
    }

    async fn get_style(app: Router) -> axum::http::Response<Body> {
        app.oneshot(
            axum::http::Request::builder()
                .uri("/api/timesheet/style")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response")
    }

    async fn response_json(response: axum::http::Response<Body>) -> Value {
        let bytes = response_bytes(response).await;
        serde_json::from_slice(&bytes).expect("response is JSON")
    }

    fn style_body(prompt: &str, examples: &[(&str, &str)]) -> Value {
        json!({
            "style_prompt": prompt,
            "style_examples": examples
                .iter()
                .map(|(task, comment)| json!({ "task": task, "comment": comment }))
                .collect::<Vec<_>>(),
        })
    }

    #[test]
    fn validate_style_caps_are_inclusive_and_normalizes() {
        // Exactly at the cap passes, one byte over fails.
        let at_cap = "x".repeat(STYLE_TOTAL_CAP_BYTES);
        assert!(validate_style(&at_cap, &[]).is_ok());
        let over_cap = "x".repeat(STYLE_TOTAL_CAP_BYTES + 1);
        assert!(validate_style(&over_cap, &[]).is_err());
        // Prompt and example text count toward the same cap.
        let half_prompt = "x".repeat(STYLE_TOTAL_CAP_BYTES / 2 + 1);
        let half_task = "y".repeat(STYLE_TOTAL_CAP_BYTES / 2 + 1);
        let half_examples = [StyleExample {
            task: half_task,
            comment: String::new(),
        }];
        assert!(validate_style(&half_prompt, &half_examples).is_err());
        // Ten pairs pass, eleven fail.
        let ten: Vec<StyleExample> = (0..10)
            .map(|n| StyleExample {
                task: format!("task {n}"),
                comment: String::new(),
            })
            .collect();
        assert!(validate_style("ok", &ten).is_ok());
        let eleven: Vec<StyleExample> = ten
            .into_iter()
            .chain(std::iter::once(StyleExample {
                task: "one more".to_string(),
                comment: String::new(),
            }))
            .collect();
        assert!(validate_style("ok", &eleven).is_err());
        // The persisted value is the trimmed/normalized one.
        let untrimmed = [StyleExample {
            task: "  task  ".to_string(),
            comment: "  comment  ".to_string(),
        }];
        let style = validate_style("  spaced prompt  ", &untrimmed).expect("valid style");
        assert_eq!(style.style_prompt, "spaced prompt");
        assert_eq!(style.style_examples[0].task, "task");
        assert_eq!(style.style_examples[0].comment, "comment");
    }

    #[tokio::test]
    async fn style_put_get_round_trip_preserves_other_sections() {
        let dir = ExportTempDir::new("style-rt");
        let seed =
            "# hub config\n\n[retention]\nenabled = false # keep raw media\nhot_days = 365\n\n\
                    [timesheet]\nuse_llm_titles = false\nmax_block_min = 90\n\n\
                    [timesheet.style]\nstyle_prompt = \"\"\n";
        let (state, config_path) = style_app(&dir.0, Some(seed));
        let state_for_assert = state.clone();
        let app = crate::build_router(state);

        let response = put_style(
            app.clone(),
            &style_body(
                "German, telegraphic.",
                &[
                    ("Debugged TM1 rule", "Fixed 3 rules"),
                    ("Prepared demo", ""),
                ],
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let stored = response_json(response).await;
        assert_eq!(stored["style_prompt"], "German, telegraphic.");
        assert_eq!(stored["style_examples"][0]["task"], "Debugged TM1 rule");
        assert_eq!(stored["style_examples"][0]["comment"], "Fixed 3 rules");
        assert_eq!(stored["style_examples"][1]["comment"], "");

        // GET returns the persisted value.
        let fetched = response_json(get_style(app).await).await;
        assert_eq!(fetched, stored);

        // The config file keeps every other section and comment.
        let file = std::fs::read_to_string(&config_path).expect("read config");
        assert!(file.contains("# hub config"), "file:\n{file}");
        assert!(file.contains("# keep raw media"), "file:\n{file}");
        assert!(file.contains("hot_days = 365"), "file:\n{file}");
        assert!(file.contains("use_llm_titles = false"), "file:\n{file}");
        assert!(file.contains("max_block_min = 90"), "file:\n{file}");
        // ...and the style round-trips through the refiner parse side.
        let cfg = TimesheetConfig::from_toml_str(&file).expect("written config parses");
        assert_eq!(cfg.style.style_prompt, "German, telegraphic.");
        assert_eq!(cfg.style.style_examples.len(), 2);
        assert_eq!(cfg.style.style_examples[0].comment, "Fixed 3 rules");

        // The in-memory config was reloaded with the written value.
        let snapshot = state_for_assert.timesheet_snapshot().expect("snapshot");
        assert_eq!(snapshot.style, cfg.style);
    }

    #[tokio::test]
    async fn style_put_rejects_oversize_and_too_many_examples() {
        let dir = ExportTempDir::new("style-validate");
        let (state, config_path) = style_app(&dir.0, Some("[timesheet]\n"));
        let before = std::fs::read_to_string(&config_path).expect("read config");
        let app = crate::build_router(state);

        // 11 pairs exceed the 10-pair cap.
        let many: Vec<Value> = (0..11)
            .map(|n| json!({ "task": format!("task {n}"), "comment": "" }))
            .collect();
        let response = put_style(
            app.clone(),
            &json!({ "style_prompt": "ok", "style_examples": many }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Total content above 8 KiB is rejected.
        let big_prompt = "x".repeat(STYLE_TOTAL_CAP_BYTES + 1);
        let response = put_style(app.clone(), &style_body(&big_prompt, &[])).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Rejections leave the config file and the served style untouched.
        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read config"),
            before
        );
        let fetched = response_json(get_style(app).await).await;
        assert_eq!(fetched["style_prompt"], "");
        assert_eq!(fetched["style_examples"].as_array().expect("list").len(), 0);
        // No backup file appeared either (nothing was ever written).
        assert!(!config_path.with_file_name("config.toml.bak-style").exists());
    }

    #[tokio::test]
    async fn style_write_survives_a_restart() {
        let dir = ExportTempDir::new("style-restart");
        let (state, config_path) = style_app(&dir.0, Some("# cfg\n[timesheet]\n"));
        let app = crate::build_router(state);
        let response = put_style(
            app,
            &style_body("Bullet style, no prose.", &[("Demo task", "Demo comment")]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // A fresh process re-reads the config from disk (the main.rs
        // startup path) and serves the persisted style.
        let restarted = TimesheetConfig::load_file(&config_path).expect("config on disk");
        let state = AppState::open_with_retention(
            &dir.0.join("activity.db"),
            dir.0.join("media"),
            crate::AuthConfig::disabled(),
            crate::retention::RetentionConfig::default(),
            restarted,
        )
        .expect("state");
        let app = crate::build_router(state);
        let fetched = response_json(get_style(app).await).await;
        assert_eq!(fetched["style_prompt"], "Bullet style, no prose.");
        assert_eq!(fetched["style_examples"][0]["task"], "Demo task");
        assert_eq!(fetched["style_examples"][0]["comment"], "Demo comment");
    }

    #[tokio::test]
    async fn style_backup_is_written_once_and_never_overwritten() {
        let dir = ExportTempDir::new("style-backup");
        let seed = "# original\n[retention]\nenabled = false\n";
        let (state, config_path) = style_app(&dir.0, Some(seed));
        let backup = config_path.with_file_name("config.toml.bak-style");
        assert!(!backup.exists());
        let app = crate::build_router(state);

        let response = put_style(app.clone(), &style_body("first", &[])).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(std::fs::read_to_string(&backup).expect("backup"), seed);

        let response = put_style(app, &style_body("second", &[])).await;
        assert_eq!(response.status(), StatusCode::OK);
        // Still the original content after the second write.
        assert_eq!(std::fs::read_to_string(&backup).expect("backup"), seed);
    }

    #[tokio::test]
    async fn style_get_defaults_without_a_configured_section() {
        let dir = ExportTempDir::new("style-default");
        let (state, _config_path) = style_app(&dir.0, Some("[retention]\nenabled = false\n"));
        let app = crate::build_router(state);
        let fetched = response_json(get_style(app).await).await;
        assert_eq!(fetched["style_prompt"], "");
        assert_eq!(fetched["style_examples"].as_array().expect("list").len(), 0);
    }

    #[tokio::test]
    async fn style_put_creates_a_missing_config_file() {
        let dir = ExportTempDir::new("style-create");
        let (state, config_path) = style_app(&dir.0, None);
        assert!(!config_path.exists());
        let app = crate::build_router(state);

        let response = put_style(app.clone(), &style_body("fresh file", &[])).await;
        assert_eq!(response.status(), StatusCode::OK);
        let file = std::fs::read_to_string(&config_path).expect("config created");
        let cfg = TimesheetConfig::from_toml_str(&file).expect("created config parses");
        assert_eq!(cfg.style.style_prompt, "fresh file");
        // The backup records that the file did not exist before this write.
        let backup = config_path.with_file_name("config.toml.bak-style");
        assert_eq!(std::fs::read_to_string(&backup).expect("backup"), "");
        // And it round-trips.
        let fetched = response_json(get_style(app).await).await;
        assert_eq!(fetched["style_prompt"], "fresh file");
    }
}
