//! Daily itinerary compiler (issue #45, ADR addendum on issue #16): merges
//! one local day's raw material into draft activities for owner review.
//!
//! Contract (issue #45):
//! - Window sessions are the duration backbone: every draft activity's
//!   start/end comes from a session (`ts_start` .. `ts_start + duration_s`).
//!   Gaps and AFK time never become activities.
//! - Frame analyses classify a session by nearest in time (issue #63): the
//!   analysis whose instant sits closest to the session's span provides
//!   `project_guess` / `category` / confidence - distance 0 inside the
//!   span, the earliest analysis wins distance ties - as long as it is
//!   within `classify_window_s` (default 600 s) of the span. One keyframe
//!   therefore classifies the surrounding minutes instead of only the
//!   single short session containing its instant, so flip-flop chains of
//!   adjacent classifications coalesce into reviewable blocks; sessions
//!   with no analysis inside the window stay unclassified (grey question
//!   hand-off). All in-window analyses' frame hashes join the evidence
//!   sources.
//! - Adjacent sessions with the same project + category merge up to
//!   `max_block_min` (default 2 h). "Adjacent" means the gap between the
//!   spans is at most [`ADJACENT_GAP_MAX_S`] and no AFK span covers the
//!   gap; a single session longer than the cap is never split.
//! - Low-confidence spans that cannot be resolved (no analysis, or best
//!   confidence below [`RESOLVE_MIN_CONFIDENCE`]) become question items
//!   through the issue #44 store instead of activities. An already-open
//!   question whose evidence frame falls inside the span (containment,
//!   issue #65) or whose text matches is never duplicated; an answered
//!   one resolves the span with the owner's answer (confidence 1.0).
//! - [`store_day`] writes drafts via the normal core insert: bucket
//!   `timesheet-draft`, `ts_start` = activity start, `duration_s` = length,
//!   `device` = the day's majority session device, `data` = the activity
//!   JSON (plus a `batch` stamp). Regenerating a day marks the old current
//!   drafts superseded via the data field `superseded_by` (batch id).
//! - Approve-aware regeneration (issue #60): approved time is never
//!   re-drafted. Sessions overlapping a `timesheet-approved` span of the
//!   day are skipped entirely (SKIP semantics - the alternative
//!   link-and-update-in-place design was rejected: it couples the approve
//!   flow to regeneration and rewrites approved history), and approved
//!   draft rows are excluded from supersession so they stay the day's
//!   visible record of what is already approved. Re-approving a
//!   regenerated day therefore cannot produce duplicate approved events.
//!
//! Rows are attributed to the day by their own UTC offset (the device's
//! local date of `ts_start`), so a hub in a different timezone than the
//! device still groups events into the right local day.
//!
//! [`plan_day`] holds every merge decision and is deterministic and pure
//! apart from the injected [`BlockTitler`]; [`store_day`] is the only
//! writer. Unit tests drive both with fixture event rows.

use std::collections::BTreeMap;

use activity_core::{CoreError, Db, EventEnvelope, EventRow};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{StyleConfig, TimesheetConfig};
use crate::storage::{EvidenceFrame, TsQuestion, TsQuestionStore};

/// Bucket for draft activities awaiting owner review (ADR addendum on
/// issue #16; #47 approves them into `timesheet-approved`).
pub const BUCKET_TIMESHEET_DRAFT: &str = "timesheet-draft";

/// Bucket holding the frame-analysis worker's events (issue #43). The
/// compiler only reads this bucket; the documented event shape is
/// `{frame_sha16, frame_path, activity, project_guess, category,
/// confidence, evidence, model, sampled}`.
pub const BUCKET_FRAME_ANALYSIS: &str = "frame-analysis";

/// Spans whose best analysis confidence is below this never become
/// activities on their own - they become question items instead. Compiler
/// policy; issue #43 owns the per-frame confidence scale itself.
pub const RESOLVE_MIN_CONFIDENCE: f64 = 0.6;

/// Two sessions are merge-adjacent when the gap between them is at most
/// this many seconds (~2 missed poll intervals) and no AFK span covers the
/// gap. Larger gaps stay separate activities - the unexplained time would
/// silently vanish inside a merged block.
pub const ADJACENT_GAP_MAX_S: f64 = 120.0;

/// `project_guess` for spans with no analysis-provided project.
pub const PROJECT_UNKNOWN: &str = "Unknown";
/// `category` for spans with no analysis-provided category.
pub const CATEGORY_UNCLASSIFIED: &str = "Uncategorized";

/// Device stamped on drafts when the day has no window sessions at all
/// (nothing is ever inserted in that case, but the field needs a value).
const FALLBACK_DEVICE: &str = "hub";

/// Evidence sources cited by a draft activity (issue #45 output schema:
/// "sources: [frame sha16s, session ids, question ids]").
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ActivitySources {
    pub frame_sha16s: Vec<String>,
    pub session_ids: Vec<i64>,
    pub question_ids: Vec<i64>,
}

/// One draft activity as stored in the `timesheet-draft` bucket's `data`
/// field (issue #45 output schema). Supersession adds a `superseded_by`
/// key on regeneration; storage adds a `batch` stamp - both are ignored by
/// this struct's serde (unknown fields skipped on read).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DraftActivity {
    pub start: String,
    pub end: String,
    pub project_guess: String,
    pub category: String,
    /// Short AI-style title.
    pub task: String,
    /// Evidence summary.
    pub comment: String,
    pub confidence: f64,
    pub sources: ActivitySources,
}

/// A question the compiler would create for an unresolvable span (created
/// through `TsQuestionStore::create_question` by [`store_day`], reusing
/// the issue #44 queue).
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedQuestion {
    pub question: String,
    pub evidence: Vec<EvidenceFrame>,
}

/// The day's raw material, gathered by the caller from the core store
/// (bucket-filtered row queries over the day's instant window). The
/// compiler re-filters rows to the day by their own UTC offset, so a
/// slightly wider query window is always safe.
#[derive(Debug, Clone, Default)]
pub struct DayInput {
    pub day: NaiveDate,
    /// `window-watcher` rows (the duration backbone).
    pub sessions: Vec<EventRow>,
    /// `frame-analysis` rows (issue #43).
    pub analyses: Vec<EventRow>,
    /// `afk-watcher` rows (break merge adjacency; never activities).
    pub afk: Vec<EventRow>,
    /// `timesheet-draft` rows currently stored for the day (superseded on
    /// regeneration; approved ones are kept - issue #60).
    pub old_drafts: Vec<EventRow>,
    /// `timesheet-approved` rows overlapping the day (issue #60): any
    /// session overlapping one of these spans is never re-drafted, so a
    /// regenerate-then-reapprove loop cannot duplicate approved time.
    pub approved: Vec<EventRow>,
}

/// One merge group as the titler sees it: only what a task/comment can be
/// written from, never raw rows.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedBlock {
    pub start: String,
    pub end: String,
    pub project_guess: String,
    pub category: String,
    /// Winning analysis activity text (what the frames showed).
    pub activity: String,
    /// Distinct foreground apps across the group's sessions.
    pub apps: Vec<String>,
    pub session_count: usize,
    pub frame_count: usize,
    pub confidence: f64,
}

/// Task + comment produced for one [`MergedBlock`]. The contract is one
/// title per input block, in order, never a failure: implementors fall
/// back internally.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockTitle {
    pub task: String,
    pub comment: String,
}

/// Source of task/comment text for merge groups. Implementations:
/// [`HeuristicTitler`] (always works) and the Lemonade titler (`crate::llm`,
/// fail-soft to the heuristic).
pub trait BlockTitler {
    fn title_blocks(&self, blocks: &[MergedBlock], style: &StyleConfig) -> Vec<BlockTitle>;
}

/// Deterministic heuristic titles: task = the winning analysis' activity
/// text (sentence-cased, word-boundary truncated); comment = the evidence
/// summary. Ignores the style config (the owner style prompt shapes AI
/// text, not the mechanical fallback).
pub struct HeuristicTitler;

const TASK_MAX_CHARS: usize = 80;
/// Apps listed in a heuristic comment before "..." takes over.
const COMMENT_APP_LIMIT: usize = 3;

impl BlockTitler for HeuristicTitler {
    fn title_blocks(&self, blocks: &[MergedBlock], _style: &StyleConfig) -> Vec<BlockTitle> {
        blocks.iter().map(heuristic_title).collect()
    }
}

/// Heuristic title for one block (also the Lemonade titler's fallback).
pub fn heuristic_title(block: &MergedBlock) -> BlockTitle {
    let apps_joined = {
        let listed: Vec<&str> = block
            .apps
            .iter()
            .take(COMMENT_APP_LIMIT)
            .map(String::as_str)
            .collect();
        let mut joined = listed.join(", ");
        if block.apps.len() > COMMENT_APP_LIMIT {
            joined.push_str(", ...");
        }
        joined
    };
    let task = if block.activity.trim().is_empty() {
        if apps_joined.is_empty() {
            "Work session".to_string()
        } else {
            format!("Work in {apps_joined}")
        }
    } else {
        truncate_words(&sentence_case(block.activity.trim()), TASK_MAX_CHARS)
    };
    let comment = format!(
        "{} window session(s) ({}); {} frame(s) analysed; confidence {:.2}",
        block.session_count, apps_joined, block.frame_count, block.confidence
    );
    BlockTitle { task, comment }
}

/// `YYYY-MM-DD` -> `NaiveDate`, canonical spellings only (same rule as the
/// question store's day validation: non-canonical input like "2026-11-6"
/// is rejected so the stored day key stays byte-identical everywhere).
pub fn parse_day(day: &str) -> Result<NaiveDate, CoreError> {
    match NaiveDate::parse_from_str(day, "%Y-%m-%d") {
        Ok(parsed) if parsed.format("%Y-%m-%d").to_string() == day => Ok(parsed),
        _ => Err(CoreError::Validation(format!(
            "date '{day}' must be YYYY-MM-DD"
        ))),
    }
}

/// The compiler's full decisions for one day. Pure output of [`plan_day`].
#[derive(Debug, Clone)]
pub struct DayPlan {
    pub day: NaiveDate,
    pub activities: Vec<DraftActivity>,
    pub questions: Vec<PlannedQuestion>,
    /// Device stamped on inserted drafts: the day's majority session
    /// device (single-owner hub; ties break to the smallest name).
    pub device: String,
    /// Input rows skipped as malformed (unparseable timestamp/data).
    pub skipped_rows: usize,
    /// Current (non-superseded, non-approved) draft rows the storage step
    /// must mark superseded - kept as full rows so `store_day` can rewrite
    /// their data payload in place without re-reading. Approved drafts are
    /// never superseded (issue #60): they stay the day's carried-over
    /// record of approved time.
    pub supersede_rows: Vec<EventRow>,
}

/// Everything [`store_day`] wrote, for the API response and logging.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredDay {
    /// Batch stamp shared by this regeneration's drafts and the
    /// `superseded_by` pointers written onto the old drafts.
    pub batch: String,
    pub draft_ids: Vec<i64>,
    pub question_ids: Vec<i64>,
    pub superseded_ids: Vec<i64>,
}

// ---------------------------------------------------------------- parsing

struct SessionRec {
    id: i64,
    device: String,
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    app: String,
}

struct AnalysisRec {
    ts: DateTime<FixedOffset>,
    frame_sha16: String,
    frame_path: String,
    activity: String,
    project_guess: String,
    category: String,
    confidence: f64,
}

struct AfkRec {
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
}

/// Parse one row's timestamp; `None` marks the row malformed.
fn row_ts(row: &EventRow) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(&row.ts_start).ok()
}

/// Day attribution by the row's own UTC offset (the device's local date).
fn own_offset_date(ts: DateTime<FixedOffset>) -> chrono::NaiveDate {
    ts.date_naive()
}

fn parse_sessions(rows: &[EventRow], day: NaiveDate) -> (Vec<SessionRec>, usize) {
    let mut skipped = 0usize;
    let mut out = Vec::new();
    for row in rows {
        let Some(ts) = row_ts(row) else {
            skipped += 1;
            continue;
        };
        if own_offset_date(ts) != day {
            continue;
        }
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&row.data) else {
            skipped += 1;
            continue;
        };
        if !row.duration_s.is_finite() || row.duration_s < 0.0 {
            skipped += 1;
            continue;
        }
        let end = ts + Duration::milliseconds((row.duration_s * 1000.0).round() as i64);
        out.push(SessionRec {
            id: row.id,
            device: row.device.clone(),
            start: ts,
            end,
            app: data["app"].as_str().unwrap_or_default().to_string(),
        });
    }
    // Stable ordering by start instant, ties by row id.
    out.sort_by_key(|s| (s.start, s.id));
    (out, skipped)
}

fn parse_analyses(rows: &[EventRow], day: NaiveDate) -> (Vec<AnalysisRec>, usize) {
    let mut skipped = 0usize;
    let mut out = Vec::new();
    for row in rows {
        let Some(ts) = row_ts(row) else {
            skipped += 1;
            continue;
        };
        if own_offset_date(ts) != day {
            continue;
        }
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&row.data) else {
            skipped += 1;
            continue;
        };
        // A frame hash is the minimum viable evidence: without it the
        // analysis can neither classify a session nor back a question.
        let Some(frame_sha16) = data["frame_sha16"].as_str().filter(|s| !s.is_empty()) else {
            skipped += 1;
            continue;
        };
        let Some(confidence) = data["confidence"].as_f64() else {
            skipped += 1;
            continue;
        };
        let str_field = |key: &str| data[key].as_str().unwrap_or_default().to_string();
        out.push(AnalysisRec {
            ts,
            frame_sha16: frame_sha16.to_string(),
            frame_path: str_field("frame_path"),
            activity: str_field("activity"),
            project_guess: str_field("project_guess"),
            category: str_field("category"),
            confidence,
        });
    }
    out.sort_by_key(|a| (a.ts, a.frame_sha16.clone()));
    (out, skipped)
}

fn parse_afk(rows: &[EventRow], day: NaiveDate) -> (Vec<AfkRec>, usize) {
    let mut skipped = 0usize;
    let mut out = Vec::new();
    for row in rows {
        let Some(ts) = row_ts(row) else {
            skipped += 1;
            continue;
        };
        if own_offset_date(ts) != day {
            continue;
        }
        if !row.duration_s.is_finite() || row.duration_s < 0.0 {
            skipped += 1;
            continue;
        }
        out.push(AfkRec {
            start: ts,
            end: ts + Duration::milliseconds((row.duration_s * 1000.0).round() as i64),
        });
    }
    out.sort_by_key(|a| a.start);
    (out, skipped)
}

// ---------------------------------------------------------------- planning

/// The compiler's decision for one session.
struct Candidate {
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    project_guess: String,
    category: String,
    confidence: f64,
    task: String,
    comment: String,
    /// Winning analysis' activity text and its confidence/timestamp (the
    /// group title source; comparisons stay deterministic: higher
    /// confidence wins, earliest timestamp breaks ties).
    activity: String,
    activity_confidence: f64,
    activity_ts: DateTime<FixedOffset>,
    shas: Vec<String>,
    session_ids: Vec<i64>,
    question_ids: Vec<i64>,
    app: String,
    /// True when the candidate may merge with same-project neighbours
    /// (analysis-classified spans). Owner-answered spans never merge:
    /// their task text is the owner's own words.
    mergeable: bool,
}

/// Absolute gap in whole milliseconds between an instant and a session's
/// span: 0 when the instant falls inside `[start, end)`, otherwise the
/// distance to the nearer edge. This is the metric the classification
/// window (`classify_window_s`, issue #63) is applied to.
fn span_distance_ms(session: &SessionRec, ts: DateTime<FixedOffset>) -> i64 {
    if ts < session.start {
        (session.start - ts).num_milliseconds()
    } else if ts >= session.end {
        (ts - session.end).num_milliseconds()
    } else {
        0
    }
}

/// Does a question's evidence frame fall inside the session's span
/// (`[start, end)`, the containment [`span_distance_ms`] uses)? The
/// existing-question dedup's frame-hash arm matches only frames inside
/// the candidate span (issue #65): since #63 a session cites every
/// analysis within `classify_window_s`, so a bare hash overlap would let
/// a far-away open question suppress a real session. A frame whose
/// timestamp does not parse never matches; the question-text arm still
/// covers the question.
fn frame_in_span(session: &SessionRec, frame: &EvidenceFrame) -> bool {
    match DateTime::parse_from_rfc3339(&frame.ts) {
        Ok(ts) => ts >= session.start && ts < session.end,
        Err(_) => false,
    }
}

/// The classification window in whole milliseconds (`classify_window_s`,
/// issue #63). An analysis at span distance 0 (inside the session) is
/// always within it.
fn classification_window_ms(cfg: &TimesheetConfig) -> i64 {
    i64::try_from(cfg.classify_window_s.saturating_mul(1000)).unwrap_or(i64::MAX)
}

/// The `(project_guess, category)` pair a session's best analysis implies,
/// with the documented fallbacks when fields are empty.
fn classify_labels(best: Option<&AnalysisRec>) -> (String, String) {
    match best {
        Some(analysis) => {
            let project = if analysis.project_guess.is_empty() {
                PROJECT_UNKNOWN.to_string()
            } else {
                analysis.project_guess.clone()
            };
            let category = if analysis.category.is_empty() {
                CATEGORY_UNCLASSIFIED.to_string()
            } else {
                analysis.category.clone()
            };
            (project, category)
        }
        None => (
            PROJECT_UNKNOWN.to_string(),
            CATEGORY_UNCLASSIFIED.to_string(),
        ),
    }
}

/// A merge group under construction.
struct Group {
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    project_guess: String,
    category: String,
    confidence: f64,
    activity: String,
    activity_confidence: f64,
    activity_ts: DateTime<FixedOffset>,
    shas: Vec<String>,
    session_ids: Vec<i64>,
    apps: Vec<String>,
    session_count: usize,
}

impl Group {
    fn from_candidate(candidate: &Candidate) -> Group {
        Group {
            start: candidate.start,
            end: candidate.end,
            project_guess: candidate.project_guess.clone(),
            category: candidate.category.clone(),
            confidence: candidate.confidence,
            activity: candidate.activity.clone(),
            activity_confidence: candidate.activity_confidence,
            activity_ts: candidate.activity_ts,
            shas: candidate.shas.clone(),
            session_ids: candidate.session_ids.clone(),
            apps: if candidate.app.is_empty() {
                Vec::new()
            } else {
                vec![candidate.app.clone()]
            },
            session_count: 1,
        }
    }

    fn absorb(&mut self, candidate: &Candidate) {
        if candidate.end > self.end {
            self.end = candidate.end;
        }
        if candidate.confidence < self.confidence {
            self.confidence = candidate.confidence;
        }
        if candidate.activity_confidence > self.activity_confidence
            || (candidate.activity_confidence == self.activity_confidence
                && candidate.activity_ts < self.activity_ts)
        {
            self.activity = candidate.activity.clone();
            self.activity_confidence = candidate.activity_confidence;
            self.activity_ts = candidate.activity_ts;
        }
        extend_distinct(&mut self.shas, &candidate.shas);
        self.session_ids.extend_from_slice(&candidate.session_ids);
        if !candidate.app.is_empty() && !self.apps.contains(&candidate.app) {
            self.apps.push(candidate.app.clone());
        }
        self.session_count += 1;
    }
}

fn extend_distinct(target: &mut Vec<String>, source: &[String]) {
    for sha in source {
        if !target.contains(sha) {
            target.push(sha.clone());
        }
    }
}

/// Does any AFK span overlap the open interval (start, end)?
fn afk_overlaps(start: DateTime<FixedOffset>, end: DateTime<FixedOffset>, afk: &[AfkRec]) -> bool {
    afk.iter().any(|span| span.start < end && span.end > start)
}

/// Does any session other than the candidate's own overlap the open
/// interval (start, end)? An owner-answered session between two mergeable
/// ones breaks their adjacency exactly like AFK does - merging through it
/// would produce two overlapping drafts.
fn session_overlaps(
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    sessions: &[SessionRec],
    exclude_ids: &[i64],
) -> bool {
    sessions.iter().any(|session| {
        !exclude_ids.contains(&session.id) && session.start < end && session.end > start
    })
}

/// Heuristic question text for an unresolvable session (deterministic: the
/// same session produces the same text, which doubles as the regeneration
/// dedup key for frame-less questions).
fn heuristic_question(session: &SessionRec) -> String {
    let app = if session.app.is_empty() {
        "your screen"
    } else {
        &session.app
    };
    format!(
        "What were you working on {}-{} ({app})?",
        session.start.format("%H:%M"),
        session.end.format("%H:%M")
    )
}

fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Truncate at a word boundary, appending "..." when cut.
fn truncate_words(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    match cut.rfind(' ') {
        Some(pos) if pos > max_chars / 2 => format!("{}...", &cut[..pos]),
        _ => format!("{cut}..."),
    }
}

/// A draft row's payload as an object (the only shape the compiler wrote;
/// anything else is not a draft of ours).
fn draft_object(row: &EventRow) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str::<serde_json::Value>(&row.data).ok()? {
        serde_json::Value::Object(map) => Some(map),
        _ => None,
    }
}

/// A draft payload's approved flag (ADR addendum on #16: drafts carry
/// `approved_at` + the approved flag once approved). The review state wins:
/// an approved draft is never superseded by a regeneration (issue #60).
fn approved_flag(data: &serde_json::Map<String, serde_json::Value>) -> bool {
    data.get("approved")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// The time span an approved event covers (issue #60): the activity's own
/// `start`/`end` when both parse (the approve flow stores the draft JSON
/// verbatim), the envelope's timestamp + duration otherwise. `None` marks
/// a row whose envelope timestamp does not even parse - only that case
/// shields nothing (insert validation makes it impossible for rows the
/// hub wrote; the export's overlap guard remains the defence in depth).
fn approved_span(row: &EventRow) -> Option<(DateTime<FixedOffset>, DateTime<FixedOffset>)> {
    let data = serde_json::from_str::<serde_json::Value>(&row.data).ok()?;
    let parse = |value: Option<&str>| value.and_then(|s| DateTime::parse_from_rfc3339(s).ok());
    if let (Some(start), Some(end)) = (parse(data["start"].as_str()), parse(data["end"].as_str())) {
        return Some((start, end));
    }
    let ts = row_ts(row)?;
    let end = ts + Duration::milliseconds((row.duration_s * 1000.0).round() as i64);
    Some((ts, end))
}

/// Compile one day's raw material into a [`DayPlan`] (activities, question
/// hand-offs, supersession set). Deterministic; the only external influence
/// is the injected [`BlockTitler`].
pub fn plan_day(
    input: &DayInput,
    existing_questions: &[TsQuestion],
    cfg: &TimesheetConfig,
    titler: &dyn BlockTitler,
) -> DayPlan {
    let day = input.day;
    let (sessions, mut skipped) = parse_sessions(&input.sessions, day);
    let (analyses, analysis_skipped) = parse_analyses(&input.analyses, day);
    skipped += analysis_skipped;
    let (afk, afk_skipped) = parse_afk(&input.afk, day);
    skipped += afk_skipped;
    let day_str = day.format("%Y-%m-%d").to_string();

    // Supersession set: current (non-superseded, non-approved) draft rows
    // of this day. Approved drafts are excluded (issue #60): regeneration
    // never tombstones approved time, the approved rows stay current so the
    // review page keeps showing what is already approved.
    let supersede_rows: Vec<EventRow> = input
        .old_drafts
        .iter()
        .filter(|row| match row_ts(row) {
            Some(ts) if own_offset_date(ts) == day => draft_object(row)
                .map(|data| !data.contains_key("superseded_by") && !approved_flag(&data))
                .unwrap_or(false),
            _ => false,
        })
        .cloned()
        .collect();

    // Approved spans of the day (issue #60), extracted from the
    // `timesheet-approved` bucket rows the caller gathered. Reading the
    // bucket (not just draft flags) also shields the crash window where an
    // approved event exists but its draft flag write never landed.
    let approved_spans: Vec<(DateTime<FixedOffset>, DateTime<FixedOffset>)> = input
        .approved
        .iter()
        .filter(|row| matches!(row_ts(row), Some(ts) if own_offset_date(ts) == day))
        .filter_map(approved_span)
        .collect();

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut planned_questions: Vec<PlannedQuestion> = Vec::new();
    let window_ms = classification_window_ms(cfg);

    for session in &sessions {
        // Approve-aware regeneration (issue #60): a session overlapping an
        // approved span is already carried by the `timesheet-approved`
        // bucket - re-drafting it would let a second approval duplicate
        // approved time (the export would double-count). Skip it entirely;
        // the residual unapproved time of a straddling session is left to
        // the export's overlap guard rather than re-drafted.
        if approved_spans
            .iter()
            .any(|(start, end)| session.start < *end && session.end > *start)
        {
            continue;
        }
        // Nearest-in-time classification (issue #63): the analysis closest
        // to the session's span classifies it while it stays within the
        // configured window, so one keyframe covers the surrounding
        // minutes. `attached` gathers every in-window analysis (evidence
        // sources and question hand-off); `best` is the single nearest one.
        // Analyses are (ts, sha)-sorted, so scanning keeps the earliest on
        // distance ties ("ties -> earlier analysis"); a same-instant frame
        // with higher confidence replaces the winner deterministically.
        let mut attached: Vec<&AnalysisRec> = Vec::new();
        let mut best: Option<&AnalysisRec> = None;
        let mut best_distance_ms = i64::MAX;
        for analysis in &analyses {
            let distance_ms = span_distance_ms(session, analysis.ts);
            if distance_ms > window_ms {
                continue;
            }
            attached.push(analysis);
            let wins = match best {
                None => true,
                Some(winner) => {
                    distance_ms < best_distance_ms
                        || (distance_ms == best_distance_ms
                            && analysis.ts == winner.ts
                            && analysis.confidence > winner.confidence)
                }
            };
            if wins {
                best = Some(analysis);
                best_distance_ms = distance_ms;
            }
        }
        let mut shas: Vec<String> = Vec::new();
        for analysis in &attached {
            if !shas.contains(&analysis.frame_sha16) {
                shas.push(analysis.frame_sha16.clone());
            }
        }
        let question_text = heuristic_question(session);

        // Existing questions covering this span: an evidence frame inside
        // the span (containment, issue #65 - after #63 a session cites
        // every analysis within `classify_window_s`, so a bare hash
        // overlap would let a question ten minutes away suppress the
        // session), or the deterministic question text (covers frame-less
        // questions).
        let matches: Vec<&TsQuestion> = existing_questions
            .iter()
            .filter(|q| {
                q.day == day_str
                    && (q
                        .evidence
                        .iter()
                        .any(|f| shas.contains(&f.frame_sha16) && frame_in_span(session, f))
                        || q.question == question_text)
            })
            .collect();
        let answered = matches
            .iter()
            .filter(|q| q.answer.is_some())
            .max_by_key(|q| q.id)
            .copied();
        let has_open = matches.iter().any(|q| q.answer.is_none());

        if let Some(question) = answered {
            // Owner-confirmed: the answer becomes the task, confidence is
            // maximal, the span never merges (owner's own words stay).
            let (project, category) = classify_labels(best);
            candidates.push(Candidate {
                start: session.start,
                end: session.end,
                project_guess: project,
                category,
                confidence: 1.0,
                task: question
                    .answer
                    .clone()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                comment: format!(
                    "Owner answer to question #{} (\"{}\")",
                    question.id, question.question
                ),
                activity: String::new(),
                activity_confidence: f64::NEG_INFINITY,
                activity_ts: session.start,
                shas,
                session_ids: vec![session.id],
                question_ids: matches
                    .iter()
                    .filter(|q| q.answer.is_some())
                    .map(|q| q.id)
                    .collect(),
                app: session.app.clone(),
                mergeable: false,
            });
        } else if has_open {
            // Unresolved and already asked: no activity, no duplicate
            // question.
        } else if let Some(analysis) = best.filter(|b| b.confidence >= RESOLVE_MIN_CONFIDENCE) {
            let (project, category) = classify_labels(best);
            candidates.push(Candidate {
                start: session.start,
                end: session.end,
                project_guess: project,
                category,
                confidence: analysis.confidence,
                task: String::new(),
                comment: String::new(),
                activity: analysis.activity.clone(),
                activity_confidence: analysis.confidence,
                activity_ts: analysis.ts,
                shas,
                session_ids: vec![session.id],
                question_ids: Vec::new(),
                app: session.app.clone(),
                mergeable: true,
            });
        } else {
            // Unresolvable: hand off to the question queue.
            planned_questions.push(PlannedQuestion {
                question: question_text,
                evidence: attached
                    .iter()
                    .map(|a| EvidenceFrame {
                        frame_path: a.frame_path.clone(),
                        frame_sha16: a.frame_sha16.clone(),
                        ts: a.ts.to_rfc3339(),
                    })
                    .collect(),
            });
        }
    }

    // Merge adjacent mergeable candidates (start-ordered: sessions are) of
    // the same project + category, up to the max block.
    let max_block =
        Duration::milliseconds((cfg.max_block_min.saturating_mul(60) as i64).saturating_mul(1000));
    let mut groups: Vec<Group> = Vec::new();
    for candidate in &candidates {
        if !candidate.mergeable {
            continue;
        }
        let merged_into_last = match groups.last_mut() {
            Some(last)
                if last.project_guess == candidate.project_guess
                    && last.category == candidate.category
                    && (candidate.start - last.end).num_milliseconds() as f64 / 1000.0
                        <= ADJACENT_GAP_MAX_S
                    && !afk_overlaps(last.end, candidate.start, &afk)
                    && !session_overlaps(
                        last.end,
                        candidate.start,
                        &sessions,
                        &candidate.session_ids,
                    )
                    && (candidate.end - last.start) <= max_block =>
            {
                last.absorb(candidate);
                true
            }
            _ => false,
        };
        if !merged_into_last {
            groups.push(Group::from_candidate(candidate));
        }
    }

    // One titler call for the whole day ("consult Lemonade once per day").
    let blocks: Vec<MergedBlock> = groups
        .iter()
        .map(|group| MergedBlock {
            start: group.start.to_rfc3339(),
            end: group.end.to_rfc3339(),
            project_guess: group.project_guess.clone(),
            category: group.category.clone(),
            activity: group.activity.clone(),
            apps: group.apps.clone(),
            session_count: group.session_count,
            frame_count: group.shas.len(),
            confidence: group.confidence,
        })
        .collect();
    let titles = titler.title_blocks(&blocks, &cfg.style);

    let mut activities: Vec<DraftActivity> = groups
        .iter()
        .enumerate()
        .map(|(index, group)| {
            // Fail-soft: a titler that returns fewer titles than blocks
            // leaves the remaining groups on the heuristic.
            let title = titles
                .get(index)
                .cloned()
                .unwrap_or_else(|| heuristic_title(&blocks[index]));
            DraftActivity {
                start: group.start.to_rfc3339(),
                end: group.end.to_rfc3339(),
                project_guess: group.project_guess.clone(),
                category: group.category.clone(),
                task: title.task,
                comment: title.comment,
                confidence: group.confidence,
                sources: ActivitySources {
                    frame_sha16s: group.shas.clone(),
                    session_ids: group.session_ids.clone(),
                    question_ids: Vec::new(),
                },
            }
        })
        .collect();
    // Owner-answered candidates are already complete drafts.
    for candidate in &candidates {
        if !candidate.mergeable {
            activities.push(DraftActivity {
                start: candidate.start.to_rfc3339(),
                end: candidate.end.to_rfc3339(),
                project_guess: candidate.project_guess.clone(),
                category: candidate.category.clone(),
                task: candidate.task.clone(),
                comment: candidate.comment.clone(),
                confidence: candidate.confidence,
                sources: ActivitySources {
                    frame_sha16s: candidate.shas.clone(),
                    session_ids: candidate.session_ids.clone(),
                    question_ids: candidate.question_ids.clone(),
                },
            });
        }
    }
    activities.sort_by_key(|a| DateTime::parse_from_rfc3339(&a.start).unwrap_or_default());

    // Majority session device (ties break to the smallest name via the
    // reverse-ordered key).
    let mut devices: BTreeMap<&str, usize> = BTreeMap::new();
    for session in &sessions {
        *devices.entry(session.device.as_str()).or_default() += 1;
    }
    let device = devices
        .into_iter()
        .max_by_key(|(name, count)| (*count, std::cmp::Reverse(*name)))
        .map(|(name, _)| name.to_string())
        .unwrap_or_else(|| FALLBACK_DEVICE.to_string());

    DayPlan {
        day,
        activities,
        questions: planned_questions,
        device,
        skipped_rows: skipped,
        supersede_rows,
    }
}

// ---------------------------------------------------------------- storage

/// Current UTC time as RFC3339 (same convention as the rest of the crate).
fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Write a [`DayPlan`] to the hub stores: create planned questions through
/// the issue #44 store, mark the old current drafts superseded
/// (`superseded_by` = this batch; approved drafts are never marked -
/// issue #60), insert the new drafts into the `timesheet-draft` bucket
/// (and mark them synced - hub rows are terminal).
pub fn store_day(
    db: &Db,
    questions: &TsQuestionStore,
    plan: &DayPlan,
) -> Result<StoredDay, CoreError> {
    let batch = now_rfc3339();
    let day_str = plan.day.format("%Y-%m-%d").to_string();

    let mut question_ids = Vec::with_capacity(plan.questions.len());
    for planned in &plan.questions {
        question_ids.push(questions.create_question(
            &day_str,
            &planned.question,
            &planned.evidence,
        )?);
    }

    let mut superseded_ids = Vec::with_capacity(plan.supersede_rows.len());
    for row in &plan.supersede_rows {
        let mut data = draft_object(row).ok_or_else(|| {
            CoreError::Validation(format!("draft row {} has no object payload", row.id))
        })?;
        data.insert("superseded_by".to_string(), serde_json::json!(batch));
        if db.update_event_data(row.id, &serde_json::Value::Object(data).to_string())? {
            superseded_ids.push(row.id);
        }
    }

    let mut draft_ids = Vec::with_capacity(plan.activities.len());
    for activity in &plan.activities {
        let start = DateTime::parse_from_rfc3339(&activity.start)
            .map_err(|e| CoreError::Validation(format!("draft start '{}': {e}", activity.start)))?;
        let end = DateTime::parse_from_rfc3339(&activity.end)
            .map_err(|e| CoreError::Validation(format!("draft end '{}': {e}", activity.end)))?;
        let mut data = serde_json::to_value(activity)?;
        data["batch"] = serde_json::json!(batch);
        let envelope = EventEnvelope {
            bucket: BUCKET_TIMESHEET_DRAFT.to_string(),
            timestamp: activity.start.clone(),
            duration_s: (end - start).num_milliseconds() as f64 / 1000.0,
            data,
        };
        draft_ids.push(db.insert_event(&envelope, &plan.device)?);
    }
    db.mark_synced(&draft_ids)?;

    Ok(StoredDay {
        batch,
        draft_ids,
        question_ids,
        superseded_ids,
    })
}

/// Read path: parse a `timesheet-draft` row into its activity, or `None`
/// when the row is superseded (or its payload is malformed). The server's
/// GET endpoint lists exactly the `Some` rows.
pub fn current_draft_from_row(row: &EventRow) -> Option<DraftActivity> {
    let data: serde_json::Value = serde_json::from_str(&row.data).ok()?;
    if data.get("superseded_by").is_some() {
        return None;
    }
    serde_json::from_value(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StatusFilter;

    const DAY: &str = "2026-11-06";
    const OFF: &str = "+08:00";

    fn day() -> NaiveDate {
        parse_day(DAY).expect("fixture day")
    }

    fn row(id: i64, bucket: &str, ts: &str, duration_s: f64, data: serde_json::Value) -> EventRow {
        EventRow {
            id,
            bucket: bucket.to_string(),
            ts_start: ts.to_string(),
            duration_s,
            data: serde_json::to_string(&data).expect("fixture json"),
            device: "laptop".to_string(),
            synced: true,
        }
    }

    fn session(id: i64, hour: u32, min: u32, dur_s: f64, app: &str) -> EventRow {
        row(
            id,
            "window-watcher",
            &format!("2026-11-06T{hour:02}:{min:02}:00{OFF}"),
            dur_s,
            serde_json::json!({"app": app, "title": "window title"}),
        )
    }

    fn analysis(
        id: i64,
        hour: u32,
        min: u32,
        project: &str,
        category: &str,
        confidence: f64,
    ) -> EventRow {
        row(
            id,
            BUCKET_FRAME_ANALYSIS,
            &format!("2026-11-06T{hour:02}:{min:02}:00{OFF}"),
            0.0,
            serde_json::json!({
                "frame_sha16": format!("sha-{id:04}"),
                "frame_path": format!("media/screen/2026/11/06/f{id}.jpg"),
                "activity": format!("working on {project}"),
                "project_guess": project,
                "category": category,
                "confidence": confidence,
                "evidence": [],
                "model": "qwen",
                "sampled": true,
            }),
        )
    }

    fn afk(id: i64, hour: u32, min: u32, dur_s: f64) -> EventRow {
        row(
            id,
            "afk-watcher",
            &format!("2026-11-06T{hour:02}:{min:02}:00{OFF}"),
            dur_s,
            serde_json::json!({"reason": "idle"}),
        )
    }

    /// A stored draft row (issue #60 fixtures): optionally carrying the
    /// approve flow's flags (`approved` + `approved_at`).
    fn draft_row(id: i64, hour: u32, dur_s: f64, approved: bool) -> EventRow {
        let start = format!("2026-11-06T{hour:02}:00:00{OFF}");
        let end = format!(
            "2026-11-06T{:02}:{:02}:00{OFF}",
            hour + (dur_s as u32) / 3600,
            (dur_s as u32 % 3600) / 60
        );
        let mut data = serde_json::json!({
            "start": start,
            "end": end,
            "project_guess": "C1234",
            "category": "Consulting",
            "task": "Working on C1234",
            "comment": "evidence",
            "confidence": 0.9,
            "sources": {
                "frame_sha16s": [], "session_ids": [], "question_ids": []
            },
            "batch": "2026-11-06T08:00:00.000Z",
        });
        if approved {
            data["approved"] = serde_json::json!(true);
            data["approved_at"] = serde_json::json!("2026-11-06T18:00:00.000Z");
            data["approved_event_id"] = serde_json::json!(900 + id);
        }
        row(id, BUCKET_TIMESHEET_DRAFT, &start, dur_s, data)
    }

    /// A stored approved event (issue #60 fixtures): the approve flow's
    /// `timesheet-approved` insert (draft JSON verbatim + `approved_at`).
    fn approved_event(id: i64, hour: u32, dur_s: f64) -> EventRow {
        let start = format!("2026-11-06T{hour:02}:00:00{OFF}");
        let end = format!(
            "2026-11-06T{:02}:{:02}:00{OFF}",
            hour + (dur_s as u32) / 3600,
            (dur_s as u32 % 3600) / 60
        );
        row(
            id,
            crate::export::BUCKET_TIMESHEET_APPROVED,
            &start,
            dur_s,
            serde_json::json!({
                "start": start,
                "end": end,
                "project_guess": "C1234",
                "category": "Consulting",
                "task": "Working on C1234",
                "comment": "evidence",
                "confidence": 0.9,
                "sources": {
                    "frame_sha16s": [], "session_ids": [], "question_ids": []
                },
                "approved_at": "2026-11-06T18:00:00.000Z",
            }),
        )
    }

    fn cfg() -> TimesheetConfig {
        TimesheetConfig::default()
    }

    fn plan(input: &DayInput) -> DayPlan {
        plan_day(input, &[], &cfg(), &HeuristicTitler)
    }

    fn input_for(rows: Vec<EventRow>) -> DayInput {
        let mut input = DayInput {
            day: day(),
            ..DayInput::default()
        };
        for r in rows {
            match r.bucket.as_str() {
                "window-watcher" => input.sessions.push(r),
                BUCKET_FRAME_ANALYSIS => input.analyses.push(r),
                "afk-watcher" => input.afk.push(r),
                BUCKET_TIMESHEET_DRAFT => input.old_drafts.push(r),
                crate::export::BUCKET_TIMESHEET_APPROVED => input.approved.push(r),
                other => panic!("fixture bucket {other}"),
            }
        }
        input
    }

    fn draft_rows(db: &Db) -> Vec<EventRow> {
        db.query_event_rows_in(Some(BUCKET_TIMESHEET_DRAFT), None, None, None)
            .expect("query draft rows")
    }

    // ----------------------------------------- fixture: sessions only

    #[test]
    fn sessions_only_become_question_items_not_activities() {
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            session(2, 11, 0, 1800.0, "Excel.exe"),
        ]);
        let planned = plan(&input);
        // Unresolvable spans never become activities: each session becomes
        // a question item instead.
        assert!(planned.activities.is_empty());
        assert_eq!(planned.questions.len(), 2);
        assert_eq!(planned.device, "laptop");
        assert!(planned.questions[0].question.contains("09:00-10:00"));
        assert!(planned.questions[0].question.contains("Code.exe"));
        assert!(planned.questions[1].question.contains("Excel.exe"));
        // Frame-less questions carry no evidence.
        assert!(planned.questions[0].evidence.is_empty());

        // Storage creates the question rows and nothing else.
        let db = Db::open_in_memory().expect("db");
        let questions = TsQuestionStore::open_in_memory().expect("store");
        let stored = store_day(&db, &questions, &planned).expect("store");
        assert!(stored.draft_ids.is_empty());
        assert_eq!(stored.question_ids.len(), 2);
        assert!(draft_rows(&db).is_empty());
    }

    // --------------------------------- fixture: sessions + analyses

    #[test]
    fn analyses_classify_sessions_and_adjacent_same_project_merge() {
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),  // 09:00-10:00
            session(2, 10, 0, 3600.0, "Code.exe"), // 10:00-11:00
            analysis(10, 9, 30, "C1234", "Consulting", 0.9),
            analysis(11, 10, 30, "C1234", "Consulting", 0.8),
        ]);
        let planned = plan(&input);
        assert_eq!(planned.questions.len(), 0);
        assert_eq!(planned.activities.len(), 1, "adjacent same project merges");
        let activity = &planned.activities[0];
        assert_eq!(activity.start, "2026-11-06T09:00:00+08:00");
        assert_eq!(activity.end, "2026-11-06T11:00:00+08:00");
        assert_eq!(activity.project_guess, "C1234");
        assert_eq!(activity.category, "Consulting");
        // min confidence of the merged members
        assert!((activity.confidence - 0.8).abs() < 1e-9);
        // sources carry both sessions and both frames
        assert_eq!(activity.sources.session_ids, vec![1, 2]);
        assert_eq!(activity.sources.frame_sha16s, vec!["sha-0010", "sha-0011"]);
        // heuristic task comes from the winning (highest-confidence)
        // analysis' activity text, sentence-cased
        assert!(activity.task.starts_with("Working on C1234"));
        assert!(activity.comment.contains("2 window session(s)"));
        assert!(activity.comment.contains("Code.exe"));
    }

    #[test]
    fn boundary_frame_classifies_both_adjacent_sessions() {
        // A frame exactly at a shared boundary sits at span distance 0 for
        // both neighbours (issue #63 nearest-in-time semantics), so both
        // are classified by it and - same labels - merge into one block.
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            session(2, 10, 0, 3600.0, "Excel.exe"),
            analysis(10, 10, 0, "C9999", "Support", 0.95),
        ]);
        let planned = plan(&input);
        assert_eq!(planned.activities.len(), 1);
        let activity = &planned.activities[0];
        assert_eq!(activity.sources.session_ids, vec![1, 2]);
        assert_eq!(activity.project_guess, "C9999");
        assert_eq!(planned.questions.len(), 0);
    }

    // ------------------ fixture: nearest-in-time classification (#63)

    /// RFC3339 timestamp `offset_s` seconds after 15:00:00 (+08:00).
    fn offset_ts(offset_s: i64) -> String {
        format!(
            "2026-11-06T{:02}:{:02}:{:02}+08:00",
            15 + offset_s / 3600,
            (offset_s % 3600) / 60,
            offset_s % 60
        )
    }

    /// A 10 s window session starting `offset_s` seconds after 15:00:00
    /// (+08:00) - the pilot-day poll cadence (issue #63 fixtures).
    fn poll_session(id: i64, offset_s: i64) -> EventRow {
        row(
            id,
            "window-watcher",
            &offset_ts(offset_s),
            10.0,
            serde_json::json!({"app": "Code.exe", "title": "window title"}),
        )
    }

    /// A frame analysis at `offset_s` seconds after 15:00:00 (+08:00).
    fn offset_analysis(
        id: i64,
        offset_s: i64,
        project: &str,
        category: &str,
        confidence: f64,
    ) -> EventRow {
        row(
            id,
            BUCKET_FRAME_ANALYSIS,
            &offset_ts(offset_s),
            0.0,
            serde_json::json!({
                "frame_sha16": format!("sha-{id:04}"),
                "frame_path": format!("media/screen/2026/11/06/f{id}.jpg"),
                "activity": format!("working on {project}"),
                "project_guess": project,
                "category": category,
                "confidence": confidence,
                "evidence": [],
                "model": "qwen",
                "sampled": true,
            }),
        )
    }

    fn span_seconds(activity: &DraftActivity) -> i64 {
        let start = DateTime::parse_from_rfc3339(&activity.start).expect("start");
        let end = DateTime::parse_from_rfc3339(&activity.end).expect("end");
        (end - start).num_seconds()
    }

    #[test]
    fn flip_flop_keyframes_coalesce_into_order_of_ten_blocks() {
        // Pilot-day cadence (issue #63): 10 s poll sessions tiling one
        // hour, 8 keyframes/hour whose labels flip-flop between two
        // project/category pairs. Containment classified only the ~10 s
        // session containing each frame (8 sub-minute drafts + ~352
        // questions); nearest-in-time gives one ~7.5 min block per
        // keyframe zone - order of ten, no sub-minute draft.
        let mut rows = Vec::new();
        for k in 0..360 {
            rows.push(poll_session(k + 1, k * 10));
        }
        let keyframes = [0_i64, 450, 900, 1350, 1800, 2250, 2700, 3150];
        for (k, offset) in keyframes.iter().enumerate() {
            let (project, category) = if k % 2 == 0 {
                ("C1234", "Consulting")
            } else {
                ("C9999", "QA")
            };
            rows.push(offset_analysis(
                1000 + k as i64,
                *offset,
                project,
                category,
                0.9,
            ));
        }
        let planned = plan(&input_for(rows));
        assert_eq!(planned.activities.len(), 8, "one block per keyframe zone");
        assert!(planned.questions.is_empty());
        for activity in &planned.activities {
            assert!(
                span_seconds(activity) >= 60,
                "sub-minute draft in a flip-flop hour"
            );
        }
        // zones alternate, so neighbours never merge
        assert_eq!(planned.activities[0].project_guess, "C1234");
        assert_eq!(planned.activities[1].project_guess, "C9999");
        assert_eq!(planned.activities[2].project_guess, "C1234");
    }

    #[test]
    fn pilot_day_burst_stays_bounded_with_sub_minute_exception() {
        // The 15:07 pattern from issue #63: a burst of five analyses
        // seconds apart answering five different project/category pairs,
        // embedded in a calm hour of stable keyframes. The day compiles
        // to a handful of reviewable blocks; only the burst's flip-flop
        // slivers stay sub-minute.
        let mut rows = Vec::new();
        for k in 0..360 {
            rows.push(poll_session(k + 1, k * 10));
        }
        let mut id = 1000_i64;
        for offset in [0_i64, 450, 900] {
            rows.push(offset_analysis(id, offset, "C1234", "Consulting", 0.9));
            id += 1;
        }
        let burst = [
            ("C000P021", "QA"),
            ("C000P001", "Pre-Sales"),
            ("C000P027", "Consulting"),
            ("C000P001", "Consulting"),
            ("C000P027", "QA"),
        ];
        for (frame, (project, category)) in burst.iter().enumerate() {
            rows.push(offset_analysis(
                id,
                1350 + frame as i64 * 10,
                project,
                category,
                0.9,
            ));
            id += 1;
        }
        for offset in [1800_i64, 2250, 2700, 3150] {
            rows.push(offset_analysis(id, offset, "C1234", "Consulting", 0.9));
            id += 1;
        }
        let planned = plan(&input_for(rows));
        assert!(planned.questions.is_empty());
        assert_eq!(
            planned.activities.len(),
            7,
            "calm C1234 flank - 5 burst zones - calm C1234 flank"
        );
        let projects: Vec<&str> = planned
            .activities
            .iter()
            .map(|a| a.project_guess.as_str())
            .collect();
        assert_eq!(
            projects,
            vec!["C1234", "C000P021", "C000P001", "C000P027", "C000P001", "C000P027", "C1234"]
        );
        let sub_minute = planned
            .activities
            .iter()
            .filter(|a| span_seconds(a) < 60)
            .count();
        assert_eq!(sub_minute, 3, "only the burst's middle flip-flop slivers");
    }

    #[test]
    fn sessions_beyond_classify_window_stay_grey() {
        // One keyframe at 15:06:00. The default 600 s window classifies
        // the surrounding +-10 min; sessions farther out stay unclassified
        // and go to the question queue (no force-classification).
        let build_rows = || {
            let mut rows = Vec::new();
            for k in 0..120 {
                rows.push(poll_session(k + 1, k * 10)); // 15:00:00-15:20:00
            }
            rows.push(offset_analysis(1000, 360, "C1234", "Consulting", 0.9));
            rows
        };
        let planned = plan(&input_for(build_rows()));
        assert_eq!(planned.activities.len(), 1);
        let activity = &planned.activities[0];
        // The window reaches 600 s in both directions: the whole slice up
        // to 15:16:00 (exactly 600 s after the frame) is classified; the
        // next session (610 s) is not.
        assert_eq!(activity.start, "2026-11-06T15:00:00+08:00");
        assert_eq!(activity.end, "2026-11-06T15:16:10+08:00");
        assert_eq!(planned.questions.len(), 23, "15:16:10-15:20:00 stays grey");

        // A tight window only classifies the frame's immediate surround.
        let mut tight = cfg();
        tight.classify_window_s = 60;
        let planned = plan_day(&input_for(build_rows()), &[], &tight, &HeuristicTitler);
        assert_eq!(planned.activities.len(), 1);
        assert_eq!(planned.activities[0].start, "2026-11-06T15:04:50+08:00");
        assert_eq!(planned.activities[0].end, "2026-11-06T15:07:10+08:00");
        assert_eq!(planned.questions.len(), 106);
    }

    #[test]
    fn equidistant_frames_pick_the_earlier_analysis() {
        // One frame 120 s before the span, one 120 s after it: the
        // distance tie goes to the earlier analysis (issue #63 rule); the
        // later frame's higher confidence does not override it.
        let input = input_for(vec![
            session(1, 10, 0, 600.0, "Code.exe"), // 10:00:00-10:10:00
            analysis(10, 9, 58, "C1111", "Consulting", 0.8), // 120 s before
            analysis(11, 10, 12, "C2222", "QA", 0.9), // 120 s after
        ]);
        let planned = plan(&input);
        assert_eq!(planned.activities.len(), 1);
        assert_eq!(planned.activities[0].project_guess, "C1111");
        assert!((planned.activities[0].confidence - 0.8).abs() < 1e-9);
    }

    #[test]
    fn frames_inside_a_session_tie_on_distance_earliest_wins() {
        // All frames inside a session sit at distance 0: the earliest
        // wins even when a later frame is more confident - distance, not
        // confidence, is the selector (issue #63).
        let input = input_for(vec![
            session(1, 10, 0, 1800.0, "Code.exe"), // 10:00-10:30
            analysis(10, 10, 10, "C1111", "Consulting", 0.7),
            analysis(11, 10, 20, "C2222", "QA", 0.95),
        ]);
        let planned = plan(&input);
        assert_eq!(planned.activities.len(), 1);
        assert_eq!(planned.activities[0].project_guess, "C1111");
        assert!((planned.activities[0].confidence - 0.7).abs() < 1e-9);
    }

    #[test]
    fn nearest_frame_gates_resolution_even_when_a_farther_one_is_confident() {
        // The nearest frame classifies the session and its own confidence
        // walks the usual resolution gate: a farther high-confidence frame
        // inside the window does not rescue the span.
        let input = input_for(vec![
            session(1, 10, 0, 1200.0, "Code.exe"),     // 10:00:00-10:20:00
            analysis(10, 10, 19, "?", "?", 0.3),       // nearest, inside
            analysis(11, 10, 22, "C2222", "QA", 0.95), // 120 s after end
        ]);
        let planned = plan(&input);
        assert!(planned.activities.is_empty());
        assert_eq!(planned.questions.len(), 1);
    }

    // -------------------------------------- fixture: AFK gaps

    #[test]
    fn afk_prevents_merging_and_never_becomes_activity() {
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),   // 09:00-10:00
            session(2, 10, 30, 3600.0, "Code.exe"), // 10:30-11:30, 30 min gap
            afk(10, 10, 0, 1800.0),                 // AFK 10:00-10:30 covers the gap
            analysis(11, 9, 30, "C1234", "Consulting", 0.9),
            analysis(12, 11, 0, "C1234", "Consulting", 0.9),
        ]);
        let planned = plan(&input);
        assert_eq!(planned.activities.len(), 2, "AFK gap breaks adjacency");
        assert_eq!(planned.activities[0].end, "2026-11-06T10:00:00+08:00");
        assert_eq!(planned.activities[1].start, "2026-11-06T10:30:00+08:00");
    }

    #[test]
    fn afk_covers_small_gap_between_adjacent_sessions() {
        // 30 s gap: inside the adjacency limit, so the AFK span covering it
        // is what breaks the merge (not the gap limit).
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"), // 09:00:00-10:00:00
            afk(10, 10, 0, 30.0),                 // AFK 10:00:00-10:00:30
            EventRow {
                id: 2,
                ts_start: "2026-11-06T10:00:30+08:00".to_string(),
                ..session(2, 10, 0, 3570.0, "Code.exe") // 10:00:30-11:00:00
            },
            analysis(11, 9, 30, "C1234", "Consulting", 0.9),
            analysis(12, 10, 5, "C1234", "Consulting", 0.9),
        ]);
        let planned = plan(&input);
        assert_eq!(
            planned.activities.len(),
            2,
            "AFK on a small gap breaks adjacency"
        );
        assert_eq!(planned.activities[0].end, "2026-11-06T10:00:00+08:00");
        assert_eq!(planned.activities[1].start, "2026-11-06T10:00:30+08:00");
    }

    #[test]
    fn answered_session_between_mergeables_breaks_the_chain() {
        // Sessions 1 and 3 are the same project and only 30 s apart, but
        // the owner-answered 30 s session sits exactly in the gap: merging
        // 1+3 would produce a block overlapping the answered draft. (The
        // question text is the deterministic heuristic text for the small
        // session, which is the frame-less dedup key.)
        let questions = TsQuestionStore::open_in_memory().expect("store");
        let question_id = questions
            .create_question(
                DAY,
                "What were you working on 10:05-10:05 (Excel.exe)?",
                &[],
            )
            .expect("create");
        questions
            .answer_question(question_id, "Quarterly numbers check")
            .expect("answer");
        let input = input_for(vec![
            session(1, 10, 0, 300.0, "Code.exe"), // 10:00:00-10:05:00
            EventRow {
                id: 2,
                ts_start: "2026-11-06T10:05:00+08:00".to_string(),
                ..session(2, 10, 0, 30.0, "Excel.exe") // 10:05:00-10:05:30 (answered)
            },
            EventRow {
                id: 3,
                ts_start: "2026-11-06T10:05:30+08:00".to_string(),
                ..session(3, 10, 0, 570.0, "Code.exe") // 10:05:30-10:15:00
            },
            analysis(11, 10, 2, "C777", "Support", 0.9),
            analysis(12, 10, 6, "C777", "Support", 0.9),
            analysis(13, 10, 9, "C777", "Support", 0.9),
        ]);
        let existing = questions.list_questions(StatusFilter::All).expect("list");
        let planned = plan_day(&input, &existing, &cfg(), &HeuristicTitler);
        // session 1 + 3 stay separate around the answered middle span.
        assert_eq!(planned.activities.len(), 3);
        assert_eq!(planned.questions.len(), 0);
        let answered = planned
            .activities
            .iter()
            .find(|a| a.sources.question_ids.contains(&question_id))
            .expect("answered draft present");
        assert_eq!(answered.task, "Quarterly numbers check");
        assert_eq!(answered.sources.session_ids, vec![2]);
    }

    #[test]
    fn gap_beyond_adjacency_limit_stays_unmerged() {
        // Same project either side, but a 30 min unexplained gap with no
        // AFK row: still not adjacent (the gap would silently vanish).
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            session(2, 10, 30, 3600.0, "Code.exe"),
            analysis(11, 9, 30, "C1234", "Consulting", 0.9),
            analysis(12, 11, 0, "C1234", "Consulting", 0.9),
        ]);
        assert_eq!(plan(&input).activities.len(), 2);
    }

    // ------------------------------- fixture: merge up to max block

    #[test]
    fn merge_stops_at_max_block() {
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),  // 09:00-10:00
            session(2, 10, 0, 3600.0, "Code.exe"), // 10:00-11:00
            session(3, 11, 0, 3600.0, "Code.exe"), // 11:00-12:00
            analysis(10, 9, 30, "C1234", "Consulting", 0.9),
            analysis(11, 10, 30, "C1234", "Consulting", 0.9),
            analysis(12, 11, 30, "C1234", "Consulting", 0.9),
        ]);
        let mut config = cfg();
        config.max_block_min = 120; // 2 h
        let planned = plan_day(&input, &[], &config, &HeuristicTitler);
        assert_eq!(planned.activities.len(), 2, "1h+1h merges, third stays");
        assert_eq!(planned.activities[0].start, "2026-11-06T09:00:00+08:00");
        assert_eq!(planned.activities[0].end, "2026-11-06T11:00:00+08:00");
        assert_eq!(planned.activities[1].start, "2026-11-06T11:00:00+08:00");
        assert_eq!(planned.activities[1].end, "2026-11-06T12:00:00+08:00");
        assert_eq!(planned.activities[0].sources.session_ids, vec![1, 2]);
        assert_eq!(planned.activities[1].sources.session_ids, vec![3]);

        // A wider cap merges all three.
        let mut wider = cfg();
        wider.max_block_min = 180;
        let planned = plan_day(&input, &[], &wider, &HeuristicTitler);
        assert_eq!(planned.activities.len(), 1);
        assert_eq!(planned.activities[0].sources.session_ids, vec![1, 2, 3]);
    }

    // ------------------------- fixture: low-confidence hand-off

    #[test]
    fn low_confidence_becomes_question_with_frame_evidence() {
        let db = Db::open_in_memory().expect("db");
        let questions = TsQuestionStore::open_in_memory().expect("store");
        let input = input_for(vec![
            session(1, 14, 0, 1800.0, "Photoshop.exe"),
            analysis(10, 14, 10, "?", "?", 0.3),
        ]);
        let planned = plan(&input);
        assert!(planned.activities.is_empty());
        assert_eq!(planned.questions.len(), 1);
        assert_eq!(planned.questions[0].evidence.len(), 1);
        assert_eq!(planned.questions[0].evidence[0].frame_sha16, "sha-0010");

        let stored = store_day(&db, &questions, &planned).expect("store");
        assert!(stored.draft_ids.is_empty());
        assert_eq!(stored.question_ids.len(), 1);
        let question = questions
            .get_question(stored.question_ids[0])
            .expect("get")
            .expect("created");
        assert_eq!(question.day, DAY);
        assert!(question.question.contains("14:00-14:30"));
        assert_eq!(
            question.evidence[0].frame_path,
            "media/screen/2026/11/06/f10.jpg"
        );
        // nothing landed in the draft bucket
        assert!(draft_rows(&db).is_empty());
    }

    // ------------------------------- fixture: answered questions

    #[test]
    fn answered_question_resolves_the_span() {
        let questions = TsQuestionStore::open_in_memory().expect("store");
        let question_id = questions
            .create_question(
                DAY,
                "What were you working on 09:00-10:00 (Code.exe)?",
                &[EvidenceFrame {
                    frame_path: "media/screen/2026/11/06/f10.jpg".to_string(),
                    frame_sha16: "sha-0010".to_string(),
                    ts: "2026-11-06T09:30:00+08:00".to_string(),
                }],
            )
            .expect("create");
        questions
            .answer_question(question_id, "Refining the TM1 cube rules")
            .expect("answer");

        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            session(2, 10, 0, 3600.0, "Code.exe"), // adjacent, same project...
            analysis(10, 9, 30, "C1234", "Consulting", 0.4), // ...but low confidence
            analysis(11, 10, 30, "C1234", "Consulting", 0.9),
        ]);
        let existing = questions.list_questions(StatusFilter::All).expect("list");
        let planned = plan_day(&input, &existing, &cfg(), &HeuristicTitler);
        assert_eq!(planned.activities.len(), 2, "answered span stays alone");
        let answered = &planned.activities[0];
        assert_eq!(answered.task, "Refining the TM1 cube rules");
        assert!((answered.confidence - 1.0).abs() < 1e-9);
        assert_eq!(answered.sources.question_ids, vec![question_id]);
        assert_eq!(answered.sources.session_ids, vec![1]);
        assert!(answered.comment.contains(&format!("#{question_id}")));
        // the owner-answered span did NOT merge with the adjacent
        // same-project span (owner's words stay intact)
        assert_eq!(planned.activities[1].sources.session_ids, vec![2]);
    }

    #[test]
    fn open_question_is_not_duplicated_and_span_stays_unset() {
        let questions = TsQuestionStore::open_in_memory().expect("store");
        questions
            .create_question(
                DAY,
                "What were you working on 09:00-10:00 (Code.exe)?",
                &[EvidenceFrame {
                    frame_path: "media/screen/2026/11/06/f10.jpg".to_string(),
                    frame_sha16: "sha-0010".to_string(),
                    ts: "2026-11-06T09:30:00+08:00".to_string(),
                }],
            )
            .expect("create");
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            analysis(10, 9, 30, "?", "?", 0.3),
        ]);
        let existing = questions.list_questions(StatusFilter::All).expect("list");
        let planned = plan_day(&input, &existing, &cfg(), &HeuristicTitler);
        // no new question (already open), no activity (unresolved)
        assert!(planned.questions.is_empty());
        assert!(planned.activities.is_empty());
    }

    #[test]
    fn far_open_question_does_not_suppress_real_sessions() {
        // Issue #65 regression, the #49 pilot's vanished sessions: one
        // open question (confidence 0.30, evidence frame 6a17e7484e59b1b9
        // at 21:34:03) sits within the 600 s classify window of four
        // real, high-confidence sessions (6497 ~39 min, 6516, 6598,
        // 6668), so the frame-overlap dedup arm silently skipped all
        // four. With the arm restricted to evidence frames inside the
        // session span, all four draft and nothing new is asked.
        let questions = TsQuestionStore::open_in_memory().expect("store");
        questions
            .create_question(
                DAY,
                "What were you working on at 2026-11-06T21:34:03+08:00? \
                 (auto-detected: Creating a new project in the IDE; confidence 0.30)",
                &[EvidenceFrame {
                    frame_path: "media/frames/2026/11/06/213403_DISPLAY7_6a17e7484e59b1b9.jpg"
                        .to_string(),
                    frame_sha16: "6a17e7484e59b1b9".to_string(),
                    ts: "2026-11-06T21:34:03+08:00".to_string(),
                }],
            )
            .expect("create");
        // The question's own low-confidence analysis (post-#63 every
        // session within 600 s cites it as evidence - the widening that
        // armed the bug).
        let question_frame = row(
            2000,
            BUCKET_FRAME_ANALYSIS,
            "2026-11-06T21:34:03+08:00",
            0.0,
            serde_json::json!({
                "frame_sha16": "6a17e7484e59b1b9",
                "frame_path": "media/frames/2026/11/06/213403_DISPLAY7_6a17e7484e59b1b9.jpg",
                "activity": "Creating a new project in the IDE",
                "project_guess": "C000P001",
                "category": "Pre-Sales",
                "confidence": 0.3,
                "evidence": [],
                "model": "qwen",
                "sampled": true,
            }),
        );
        // The four vanished sessions (real ids from the #49 record), all
        // within 600 s of the 21:34:03 frame and none containing it.
        let vanished = |id: i64, ts: &str, dur_s: f64| {
            row(
                id,
                "window-watcher",
                ts,
                dur_s,
                serde_json::json!({"app": "Code.exe", "title": "window title"}),
            )
        };
        let input = input_for(vec![
            vanished(6497, "2026-11-06T20:50:00+08:00", 2340.0), // 20:50-21:29
            vanished(6668, "2026-11-06T21:36:00+08:00", 60.0),
            vanished(6516, "2026-11-06T21:38:30+08:00", 60.0),
            vanished(6598, "2026-11-06T21:42:00+08:00", 60.0),
            // High-confidence frames (offsets after 15:00:00) so every
            // session resolves; spaced and labelled so none merge.
            offset_analysis(2002, 21300, "C1234", "Consulting", 0.9), // 20:55:00
            offset_analysis(2003, 23790, "C1234", "Consulting", 0.9), // 21:36:30
            offset_analysis(2004, 23940, "C9999", "QA", 0.9),         // 21:39:00
            offset_analysis(2005, 24150, "C1234", "Consulting", 0.9), // 21:42:30
            question_frame,
        ]);
        let existing = questions.list_questions(StatusFilter::All).expect("list");
        let planned = plan_day(&input, &existing, &cfg(), &HeuristicTitler);
        assert!(
            planned.questions.is_empty(),
            "no new questions for resolved sessions"
        );
        assert_eq!(planned.activities.len(), 4, "all four sessions draft");
        let mut drafted_ids: Vec<i64> = planned
            .activities
            .iter()
            .flat_map(|a| a.sources.session_ids.iter().copied())
            .collect();
        drafted_ids.sort();
        assert_eq!(drafted_ids, vec![6497, 6516, 6598, 6668]);
    }

    // --------------------------- fixture: regeneration superseding

    #[test]
    fn regeneration_supersedes_drafts_and_dedups_questions() {
        let db = Db::open_in_memory().expect("db");
        let questions = TsQuestionStore::open_in_memory().expect("store");
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            analysis(10, 9, 30, "C1234", "Consulting", 0.9),
        ]);

        // First compile: one draft, no question (confidence high).
        let first = store_day(&db, &questions, &plan(&input)).expect("store 1");
        assert_eq!(first.draft_ids.len(), 1);
        assert!(first.superseded_ids.is_empty());

        // Second compile (the caller re-gathers the day's current draft
        // rows, exactly like the server endpoint): old draft superseded,
        // new draft inserted, no duplicate question.
        let mut second_input = input.clone();
        second_input.old_drafts = draft_rows(&db);
        let second = store_day(&db, &questions, &plan(&second_input)).expect("store 2");
        assert_eq!(second.draft_ids.len(), 1);
        assert_eq!(second.superseded_ids, vec![first.draft_ids[0]]);
        assert!(second.question_ids.is_empty());

        // The old row now carries superseded_by = the new batch...
        let rows = draft_rows(&db);
        assert_eq!(rows.len(), 2);
        let superseded = rows
            .iter()
            .find(|r| r.id == first.draft_ids[0])
            .expect("old row");
        let data: serde_json::Value = serde_json::from_str(&superseded.data).expect("json");
        assert_eq!(data["superseded_by"], second.batch.as_str());
        // ...and the read path hides it while keeping the new one.
        let current: Vec<&EventRow> = rows
            .iter()
            .filter(|r| current_draft_from_row(r).is_some())
            .collect();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, second.draft_ids[0]);

        // A regeneration over a day whose data vanished (no sessions left)
        // still supersedes the stale drafts.
        let mut third_input = DayInput {
            day: day(),
            ..DayInput::default()
        };
        third_input.old_drafts = draft_rows(&db);
        let third = store_day(&db, &questions, &plan(&third_input)).expect("store 3");
        assert_eq!(third.superseded_ids, vec![second.draft_ids[0]]);
    }

    // ------------------ fixture: approve-aware regeneration (issue #60)

    #[test]
    fn regeneration_skips_approved_spans_and_keeps_approved_drafts() {
        let db = Db::open_in_memory().expect("db");
        let questions = TsQuestionStore::open_in_memory().expect("store");
        // The day as first compiled: two non-adjacent sessions, two
        // drafts (the 1 h gap exceeds the adjacency limit, so the default
        // 2 h max block cannot merge them).
        let raw = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),  // 09:00-10:00
            session(2, 11, 0, 3600.0, "Code.exe"), // 11:00-12:00
            analysis(10, 9, 30, "C1234", "Consulting", 0.9),
            analysis(11, 11, 30, "C1234", "Consulting", 0.9),
        ]);
        let first = store_day(&db, &questions, &plan(&raw)).expect("store 1");
        assert_eq!(first.draft_ids.len(), 2);

        // The owner approves both drafts (approve flow: draft flags + one
        // `timesheet-approved` event per draft).
        for draft_id in &first.draft_ids {
            let rows = draft_rows(&db);
            let row = rows.iter().find(|r| r.id == *draft_id).expect("draft");
            let mut data: serde_json::Value = serde_json::from_str(&row.data).expect("json");
            data["approved"] = serde_json::json!(true);
            data["approved_at"] = serde_json::json!("2026-11-06T18:00:00.000Z");
            db.update_event_data(row.id, &data.to_string())
                .expect("flag");
            db.insert_event(
                &activity_core::EventEnvelope {
                    bucket: crate::export::BUCKET_TIMESHEET_APPROVED.to_string(),
                    timestamp: row.ts_start.clone(),
                    duration_s: row.duration_s,
                    data: data.clone(),
                },
                "laptop",
            )
            .expect("approved event");
        }

        // Regeneration after approval: the caller re-gathers every bucket,
        // approved included, exactly like the server endpoint.
        let mut regen_input = raw.clone();
        regen_input.old_drafts = draft_rows(&db);
        regen_input.approved = db
            .query_event_rows_in(
                Some(crate::export::BUCKET_TIMESHEET_APPROVED),
                None,
                None,
                None,
            )
            .expect("query approved");
        let plan = plan(&regen_input);
        assert!(
            plan.activities.is_empty(),
            "every session is already approved: nothing re-drafted"
        );
        assert!(
            plan.supersede_rows.is_empty(),
            "approved drafts are never superseded"
        );
        assert!(plan.questions.is_empty());

        let second = store_day(&db, &questions, &plan).expect("store 2");
        assert!(second.draft_ids.is_empty());
        assert!(second.superseded_ids.is_empty());

        // Both approved drafts remain current (the review page keeps
        // showing them) and the approved bucket still holds exactly one
        // event per span - re-approving cannot double-count.
        let rows = draft_rows(&db);
        let current_count = rows
            .iter()
            .filter(|r| current_draft_from_row(r).is_some())
            .count();
        assert_eq!(current_count, 2);
        let approved_count = db
            .query_event_rows_in(
                Some(crate::export::BUCKET_TIMESHEET_APPROVED),
                None,
                None,
                None,
            )
            .expect("query approved")
            .len();
        assert_eq!(approved_count, 2);
    }

    #[test]
    fn regeneration_redrafts_only_unapproved_time() {
        // Span 1 (09:00-10:00) is approved; span 2 (10:00-11:00) is not.
        // Regeneration must keep the approved draft, supersede the stale
        // unapproved one, and re-draft only the unapproved session.
        let approved_draft = draft_row(50, 9, 3600.0, true);
        let stale_draft = draft_row(51, 10, 3600.0, false);
        let expected_stale = stale_draft.clone();
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            session(2, 10, 0, 3600.0, "Code.exe"),
            analysis(10, 9, 30, "C1234", "Consulting", 0.9),
            analysis(11, 10, 30, "C1234", "Consulting", 0.9),
            approved_draft,
            stale_draft,
            approved_event(60, 9, 3600.0),
        ]);
        let planned = plan(&input);
        assert_eq!(planned.activities.len(), 1, "only 10:00-11:00 re-drafted");
        assert_eq!(planned.activities[0].start, "2026-11-06T10:00:00+08:00");
        assert_eq!(planned.supersede_rows, vec![expected_stale]);
    }

    #[test]
    fn session_straddling_an_approved_span_is_skipped_entirely() {
        // The approved span 10:00-11:00 (as stored) and a session that
        // starts inside it: partial overlap still skips - re-drafting any
        // part of it would duplicate approved time once re-approved.
        let input = input_for(vec![
            EventRow {
                ts_start: "2026-11-06T10:30:00+08:00".to_string(),
                ..session(1, 10, 0, 3600.0, "Code.exe") // 10:30-11:30
            },
            analysis(10, 10, 40, "C1234", "Consulting", 0.9),
            approved_event(60, 10, 3600.0), // 10:00-11:00
        ]);
        let planned = plan(&input);
        assert!(planned.activities.is_empty());
    }

    #[test]
    fn approved_rows_with_unparseable_timestamp_shield_nothing() {
        // The only row approved_span rejects is one whose envelope
        // timestamp does not parse: it yields no span, so the session is
        // re-drafted (the export overlap guard is the defence in depth for
        // anything that slips through and gets approved twice).
        let broken = row(
            60,
            crate::export::BUCKET_TIMESHEET_APPROVED,
            "not-a-timestamp",
            3600.0,
            serde_json::json!({ "start": "2026-11-06T09:00:00+08:00" }),
        );
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            analysis(10, 9, 30, "C1234", "Consulting", 0.9),
            broken,
        ]);
        let planned = plan(&input);
        assert_eq!(planned.activities.len(), 1, "session still re-drafted");
    }

    #[test]
    fn approved_span_falls_back_to_envelope_timestamp_and_duration() {
        // An approved event whose data lost its start/end fields still
        // shields its span via the envelope (ts_start + duration_s).
        let approved = row(
            60,
            crate::export::BUCKET_TIMESHEET_APPROVED,
            "2026-11-06T09:00:00+08:00",
            3600.0,
            serde_json::json!({ "task": "reconstructed" }),
        );
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            analysis(10, 9, 30, "C1234", "Consulting", 0.9),
            approved,
        ]);
        let planned = plan(&input);
        assert!(planned.activities.is_empty(), "span shielded by envelope");
    }

    // ------------------------------------- defensive parsing

    #[test]
    fn malformed_rows_are_skipped_and_counted() {
        let bad_json = EventRow {
            id: 99,
            bucket: "window-watcher".to_string(),
            ts_start: "2026-11-06T09:00:00+08:00".to_string(),
            duration_s: 60.0,
            data: "{nope".to_string(),
            device: "laptop".to_string(),
            synced: true,
        };
        let bad_confidence = row(
            98,
            BUCKET_FRAME_ANALYSIS,
            "2026-11-06T09:10:00+08:00",
            0.0,
            serde_json::json!({"frame_sha16": "sha-0098", "confidence": "high"}),
        );
        let bad_ts = EventRow {
            id: 97,
            bucket: "window-watcher".to_string(),
            ts_start: "not-a-timestamp".to_string(),
            duration_s: 60.0,
            data: "{}".to_string(),
            device: "laptop".to_string(),
            synced: true,
        };
        let input = input_for(vec![
            bad_json,
            bad_confidence,
            bad_ts,
            session(1, 9, 0, 3600.0, "Code.exe"),
            analysis(10, 9, 30, "C1234", "Consulting", 0.9),
        ]);
        let planned = plan(&input);
        assert_eq!(planned.skipped_rows, 3);
        assert_eq!(planned.activities.len(), 1);
    }

    #[test]
    fn rows_of_other_days_are_ignored_by_own_offset_date() {
        // 2026-11-06T00:30+08:00 is 2026-11-05 in UTC: still day 06 by its
        // own offset. 2026-11-07T00:30+08:00 is the next local day: out.
        let early = EventRow {
            ts_start: "2026-11-06T00:30:00+08:00".to_string(),
            ..session(1, 0, 30, 900.0, "Code.exe")
        };
        let late = EventRow {
            id: 2,
            ts_start: "2026-11-07T00:30:00+08:00".to_string(),
            ..session(2, 0, 30, 900.0, "Excel.exe")
        };
        let planned = plan(&input_for(vec![early, late]));
        assert_eq!(planned.questions.len(), 1);
        assert!(planned.questions[0].question.contains("00:30-00:45"));
    }

    #[test]
    fn device_is_majority_of_session_rows() {
        let mut other = session(2, 10, 0, 3600.0, "Excel.exe");
        other.device = "desktop".to_string();
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            other,
            session(3, 11, 0, 3600.0, "Code.exe"),
        ]);
        assert_eq!(plan(&input).device, "laptop");
    }

    #[test]
    fn style_config_flows_to_titler_contract() {
        // The titler receives the style config verbatim (issue #53
        // injection contract); the heuristic titler ignores it, but the
        // call must compile through and return one title per block.
        let mut config = cfg();
        config.style.style_prompt = "German, telegraphic.".to_string();
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            analysis(10, 9, 30, "C1234", "Consulting", 0.9),
        ]);
        let planned = plan_day(&input, &[], &config, &HeuristicTitler);
        assert_eq!(planned.activities.len(), 1);
        assert!(!planned.activities[0].task.is_empty());
    }

    #[test]
    fn failing_titler_falls_back_to_heuristics() {
        // A titler honouring the contract badly (empty list) never breaks
        // the compile: every group gets the heuristic title.
        struct Broken;
        impl BlockTitler for Broken {
            fn title_blocks(
                &self,
                _blocks: &[MergedBlock],
                _style: &StyleConfig,
            ) -> Vec<BlockTitle> {
                Vec::new()
            }
        }
        let input = input_for(vec![
            session(1, 9, 0, 3600.0, "Code.exe"),
            analysis(10, 9, 30, "C1234", "Consulting", 0.9),
        ]);
        let planned = plan_day(&input, &[], &cfg(), &Broken);
        assert_eq!(planned.activities.len(), 1);
        assert!(planned.activities[0].task.starts_with("Working on C1234"));
    }
}
