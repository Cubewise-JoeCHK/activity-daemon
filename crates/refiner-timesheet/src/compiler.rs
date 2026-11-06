//! Daily itinerary compiler (issue #45, ADR addendum on issue #16): merges
//! one local day's raw material into draft activities for owner review.
//!
//! Contract (issues #45 + #67, segment-then-classify):
//! - Window sessions are the duration backbone: every draft activity's
//!   start/end comes from sessions (`ts_start` .. `ts_start + duration_s`).
//!   Gaps and AFK time never become activities.
//! - SEGMENT FIRST (issue #67 step 1): adjacent window sessions whose gap
//!   is at most `segment_gap_s` (default 300 s) and covered by no AFK span
//!   form one contiguous work segment. Sessions overlapping an approved
//!   span are skipped entirely (issue #60); a session whose question match
//!   is answered resolves as its own draft and splits the segment around
//!   it (the owner's own words never merge). Segments split at
//!   `max_block_min` (default 2 h) on session boundaries; a single session
//!   longer than the cap is never split.
//! - THEN CLASSIFY (issue #67 step 2): every analysis within
//!   `classify_window_s` (default 600 s, issue #63) of a member session's
//!   span attaches to the segment (distance 0 inside the span, the
//!   earliest analysis wins distance ties). The segment's classification
//!   is the plurality vote over the attached analyses' normalized
//!   `(project, category)` pairs; vote ties go to the pair whose analysis
//!   sits nearest the segment's midpoint (then the earlier analysis). The
//!   selected analysis' activity text titles the block and its confidence
//!   walks the resolution gate: a segment with no attached analyses, or
//!   whose selected analysis sits below [`RESOLVE_MIN_CONFIDENCE`], stays
//!   unclassified (grey) - never force-classified.
//! - One draft per classified segment (issue #67 step 3; the 2 h splitter
//!   keeps month-end rows sane). Zero sub-minute blocks expected except
//!   pathological cases.
//! - Grey segments hand off to the question queue with #64 semantics
//!   (issue #67 step 4): consecutive grey segments of one run form ONE
//!   stretch and raise ONE question covering the span, with the attached
//!   frames from inside the stretch as evidence; at most
//!   `daily_question_cap` (default 5) such questions per day, beyond the
//!   cap the remaining stretches fold into one aggregate summary question.
//!   An already-open question whose evidence frame falls inside the
//!   stretch (containment, issue #65) or whose text matches is never
//!   duplicated; an answered one resolves the stretch with the owner's
//!   answer (confidence 1.0).
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
//! [`plan_day`] holds every segmentation and classification decision and
//! is deterministic and pure apart from the injected [`BlockTitler`];
//! [`store_day`] is the only writer. Unit tests drive both with fixture
//! event rows.

use std::collections::BTreeMap;

use activity_core::{CoreError, Db, EventEnvelope, EventRow};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{StyleConfig, TimesheetConfig};
use crate::storage::{EvidenceFrame, TsQuestion, TsQuestionStatus, TsQuestionStore};

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

/// Per-session analysis attachment (issue #63): every analysis within
/// `classify_window_s` of the session's span, plus the single nearest one
/// (distance ties go to the earlier analysis; a same-instant higher-
/// confidence frame replaces the winner deterministically).
struct Attachment<'a> {
    attached: Vec<&'a AnalysisRec>,
    best: Option<&'a AnalysisRec>,
}

/// One contiguous work segment (issue #67 step 1): a maximal run of
/// adjacent segmentable sessions - every internal gap is at most
/// `segment_gap_s` and covered by no AFK span and no owner-answered
/// session - split at `max_block_min` on session boundaries.
struct Segment<'a> {
    sessions: Vec<&'a SessionRec>,
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    apps: Vec<String>,
}

/// The majority-vote classification of one segment (issue #67 step 2).
/// The pool holds every analysis attached to the segment's sessions; the
/// plurality `(project, category)` pair wins, vote ties go to the pair
/// whose nearest-to-midpoint analysis is nearest (then the earlier one).
/// The selected analysis' activity text titles the block and its
/// confidence walks the resolution gate - a segment whose selected
/// analysis sits below [`RESOLVE_MIN_CONFIDENCE`] stays unclassified
/// (grey), never force-classified.
struct Classification<'a> {
    project_guess: String,
    category: String,
    /// The vote's tiebreak analysis (the winning pair's analysis nearest
    /// the segment midpoint); `None` only for an empty pool.
    selected: Option<&'a AnalysisRec>,
    pool: Vec<&'a AnalysisRec>,
}

/// A complete owner-answered draft (the answer is the task, confidence
/// maximal, never merged - the owner's own words). Built per matched
/// session (session-span question) or per matched grey stretch
/// (stretch-span question).
struct AnsweredDraft {
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    project_guess: String,
    category: String,
    task: String,
    comment: String,
    shas: Vec<String>,
    session_ids: Vec<i64>,
    question_ids: Vec<i64>,
}

/// One classified draft block (issue #67 step 3): a segment with its
/// winning labels, title source and evidence.
struct Group {
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    project_guess: String,
    category: String,
    /// Block confidence: the minimum over all attached analyses (the
    /// block is only as strong as its weakest evidence).
    confidence: f64,
    /// The selected analysis' activity text (what the frames showed).
    activity: String,
    apps: Vec<String>,
    session_count: usize,
    shas: Vec<String>,
    session_ids: Vec<i64>,
}

/// One contiguous grey stretch (issues #64 + #67 step 4): consecutive
/// grey segments of one run merged into a single question covering the
/// whole span, with the attached frames from inside it as evidence.
struct Stretch<'a> {
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    sessions: Vec<&'a SessionRec>,
    /// Every analysis attached to the stretch's sessions (the evidence
    /// source and the dedup-arm sha set).
    pool: Vec<&'a AnalysisRec>,
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

/// Does a question's evidence frame fall inside the span
/// (`[start, end)`, the containment [`span_distance_ms`] uses)? The
/// existing-question dedup's frame-hash arm matches only frames inside
/// the candidate span - session spans for answered questions, stretch
/// spans for grey questions (issue #65): since #63 a session cites every
/// analysis within `classify_window_s`, so a bare hash overlap would let
/// a far-away open question suppress a real session. A frame whose
/// timestamp does not parse never matches; the question-text arm still
/// covers the question.
fn frame_in_span(
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    frame: &EvidenceFrame,
) -> bool {
    match DateTime::parse_from_rfc3339(&frame.ts) {
        Ok(ts) => ts >= start && ts < end,
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

/// Per-session analysis attachment (issue #63): scan the (ts, sha)-sorted
/// analyses, keeping every in-window one and the single nearest (distance
/// ties go to the earlier analysis; a same-instant higher-confidence
/// frame replaces the winner deterministically).
fn attach_analyses<'a>(
    session: &SessionRec,
    analyses: &'a [AnalysisRec],
    window_ms: i64,
) -> Attachment<'a> {
    let mut attached: Vec<&AnalysisRec> = Vec::new();
    let mut best: Option<&AnalysisRec> = None;
    let mut best_distance_ms = i64::MAX;
    for analysis in analyses {
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
    Attachment { attached, best }
}

/// Maximal runs of adjacent segmentable sessions (issue #67 step 1): a
/// gap beyond `segment_gap_ms`, an AFK span covering the gap, or an
/// owner-answered session sitting in the gap starts a new run - the
/// unexplained time would silently vanish inside a merged block, and a
/// merged draft would overlap the answered one. Indices arrive in start
/// order (sessions are sorted).
fn runs_of<'a>(
    indices: &[usize],
    sessions: &'a [SessionRec],
    segment_gap_ms: i64,
    afk: &[AfkRec],
    answered: &[(DateTime<FixedOffset>, DateTime<FixedOffset>)],
) -> Vec<Vec<&'a SessionRec>> {
    let mut runs: Vec<Vec<&SessionRec>> = Vec::new();
    for &index in indices {
        let session = &sessions[index];
        let extends_last = match runs.last_mut() {
            Some(last) => {
                let previous = *last.last().expect("runs are non-empty");
                let gap_ms = (session.start - previous.end).num_milliseconds();
                gap_ms <= segment_gap_ms
                    && !afk_overlaps(previous.end, session.start, afk)
                    && !answered_overlaps(previous.end, session.start, answered)
            }
            None => false,
        };
        if extends_last {
            runs.last_mut().expect("checked above").push(session);
        } else {
            runs.push(vec![session]);
        }
    }
    runs
}

/// Split a run at `max_block` on session boundaries (greedy: absorb
/// sessions while the chunk stays within the cap; a single session longer
/// than the cap is never split). The run is non-empty.
fn split_at_max_block<'a>(run: &[&'a SessionRec], max_block: Duration) -> Vec<Vec<&'a SessionRec>> {
    let mut chunks: Vec<Vec<&SessionRec>> = Vec::new();
    let mut chunk: Vec<&SessionRec> = Vec::new();
    let mut chunk_start = run[0].start;
    for session in run {
        if !chunk.is_empty() && session.end - chunk_start > max_block {
            chunks.push(std::mem::take(&mut chunk));
            chunk_start = session.start;
        }
        chunk.push(session);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

/// Build one [`Segment`] from a session chunk (session-boundary split of
/// a run): span from first start to last end, apps distinct in session
/// order.
fn build_segment(sessions: Vec<&SessionRec>) -> Segment<'_> {
    let start = sessions[0].start;
    let end = sessions
        .iter()
        .map(|session| session.end)
        .max()
        .unwrap_or(start);
    let mut apps: Vec<String> = Vec::new();
    for session in &sessions {
        if !session.app.is_empty() && !apps.contains(&session.app) {
            apps.push(session.app.clone());
        }
    }
    Segment {
        sessions,
        start,
        end,
        apps,
    }
}

/// Classify one segment by majority vote (issue #67 step 2). The pool is
/// the union of the sessions' attachments, deduplicated by (instant,
/// frame hash) and kept in (ts, sha) order; the plurality normalized pair
/// wins, vote ties go to the pair whose nearest-to-midpoint analysis is
/// nearest (then the earlier analysis; full ties keep the lexicographically
/// first pair - `votes` iterates a sorted map).
fn classify_segment<'a>(
    segment: &Segment,
    analyses: &'a [AnalysisRec],
    window_ms: i64,
) -> Classification<'a> {
    let mut pool: Vec<&AnalysisRec> = Vec::new();
    for session in &segment.sessions {
        for analysis in &attach_analyses(session, analyses, window_ms).attached {
            if !pool
                .iter()
                .any(|seen| seen.ts == analysis.ts && seen.frame_sha16 == analysis.frame_sha16)
            {
                pool.push(analysis);
            }
        }
    }
    pool.sort_by_key(|analysis| (analysis.ts, analysis.frame_sha16.clone()));

    if pool.is_empty() {
        return Classification {
            project_guess: PROJECT_UNKNOWN.to_string(),
            category: CATEGORY_UNCLASSIFIED.to_string(),
            selected: None,
            pool,
        };
    }

    let midpoint = segment.start + (segment.end - segment.start) / 2;
    // Nearest-to-midpoint analysis within a voter list (ties -> earlier).
    let nearest_to_midpoint = |voters: &[&'a AnalysisRec]| -> (&'a AnalysisRec, i64) {
        let mut best = voters[0];
        let mut best_distance = (best.ts - midpoint).num_milliseconds().abs();
        for analysis in &voters[1..] {
            let distance = (analysis.ts - midpoint).num_milliseconds().abs();
            if distance < best_distance || (distance == best_distance && analysis.ts < best.ts) {
                best = analysis;
                best_distance = distance;
            }
        }
        (best, best_distance)
    };

    let mut votes: BTreeMap<(String, String), Vec<&AnalysisRec>> = BTreeMap::new();
    for analysis in &pool {
        let (project, category) = classify_labels(Some(analysis));
        votes.entry((project, category)).or_default().push(analysis);
    }
    let mut winner: Option<((String, String), &'a AnalysisRec)> = None;
    let mut winner_key: Option<(usize, i64, DateTime<FixedOffset>)> = None;
    for (pair, voters) in &votes {
        let (selected, distance) = nearest_to_midpoint(voters);
        // More votes wins; then nearer midpoint; then earlier analysis.
        let key = (voters.len(), distance, selected.ts);
        let wins = match winner_key {
            None => true,
            Some(best) => {
                key.0 > best.0
                    || (key.0 == best.0 && (key.1 < best.1 || (key.1 == best.1 && key.2 < best.2)))
            }
        };
        if wins {
            winner_key = Some(key);
            winner = Some((pair.clone(), selected));
        }
    }
    let (pair, selected) = winner.expect("the vote pool is non-empty");
    Classification {
        project_guess: pair.0,
        category: pair.1,
        selected: Some(selected),
        pool,
    }
}

/// The `(project, category)` fallback labels for a grey stretch's
/// answered draft: the pool's analysis nearest the stretch midpoint (ties
/// -> earlier), Unknown/Uncategorized for an empty pool.
fn pool_labels(
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    pool: &[&AnalysisRec],
) -> (String, String) {
    let midpoint = start + (end - start) / 2;
    let nearest = pool.iter().copied().min_by_key(|analysis| {
        (
            (analysis.ts - midpoint).num_milliseconds().abs(),
            analysis.ts,
        )
    });
    classify_labels(nearest)
}

/// Distinct frame hashes of an analysis list, kept in list (ts) order.
fn pool_shas(pool: &[&AnalysisRec]) -> Vec<String> {
    let mut shas: Vec<String> = Vec::new();
    for analysis in pool {
        if !shas.contains(&analysis.frame_sha16) {
            shas.push(analysis.frame_sha16.clone());
        }
    }
    shas
}

/// The evidence frames of a grey stretch: attached analyses whose instant
/// falls inside the stretch (issues #64 + #67: referenced evidence comes
/// from inside the span), chronological.
fn stretch_evidence(
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    pool: &[&AnalysisRec],
) -> Vec<EvidenceFrame> {
    pool.iter()
        .filter(|analysis| analysis.ts >= start && analysis.ts < end)
        .map(|analysis| EvidenceFrame {
            frame_path: analysis.frame_path.clone(),
            frame_sha16: analysis.frame_sha16.clone(),
            ts: analysis.ts.to_rfc3339(),
        })
        .collect()
}

/// Fold consecutive grey segments (with their merged attachment pool)
/// into one [`Stretch`] (issues #64 + #67 step 4): ONE question per
/// contiguous grey stretch. The segments are non-empty.
fn fold_stretch<'a>(segments: Vec<Segment<'a>>, pool: Vec<&'a AnalysisRec>) -> Stretch<'a> {
    let start = segments[0].start;
    let end = segments
        .last()
        .map(|segment| segment.end)
        .expect("a stretch has segments");
    let sessions: Vec<&SessionRec> = segments
        .into_iter()
        .flat_map(|segment| segment.sessions.into_iter())
        .collect();
    Stretch {
        start,
        end,
        sessions,
        pool,
    }
}

/// Assemble one classified [`Group`] from a segment and its vote result.
fn group_from(segment: &Segment, classification: &Classification) -> Group {
    Group {
        start: segment.start,
        end: segment.end,
        project_guess: classification.project_guess.clone(),
        category: classification.category.clone(),
        confidence: classification
            .pool
            .iter()
            .map(|analysis| analysis.confidence)
            .fold(f64::INFINITY, f64::min),
        activity: classification
            .selected
            .map(|selected| selected.activity.clone())
            .unwrap_or_default(),
        apps: segment.apps.clone(),
        session_count: segment.sessions.len(),
        shas: pool_shas(&classification.pool),
        session_ids: segment.sessions.iter().map(|s| s.id).collect(),
    }
}

/// Does any AFK span overlap the open interval (start, end)?
fn afk_overlaps(start: DateTime<FixedOffset>, end: DateTime<FixedOffset>, afk: &[AfkRec]) -> bool {
    afk.iter().any(|span| span.start < end && span.end > start)
}

/// Does any owner-answered span overlap the open interval (start, end)?
/// An answered session between two segmentable ones breaks their
/// adjacency exactly like AFK does - segmenting through it would produce
/// a draft overlapping the answered draft.
fn answered_overlaps(
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    answered: &[(DateTime<FixedOffset>, DateTime<FixedOffset>)],
) -> bool {
    answered
        .iter()
        .any(|(span_start, span_end)| *span_start < end && *span_end > start)
}

/// Heuristic question text for an unresolvable session (deterministic: the
/// same session produces the same text, which doubles as the regeneration
/// dedup key for frame-less questions).
fn heuristic_question(session: &SessionRec) -> String {
    span_question(session.start, session.end, &session.app)
}

/// Heuristic question text for a span (deterministic dedup key for
/// frame-less questions; shared by session spans and grey-stretch spans).
fn span_question(start: DateTime<FixedOffset>, end: DateTime<FixedOffset>, app: &str) -> String {
    let app = if app.is_empty() { "your screen" } else { app };
    format!(
        "What were you working on {}-{} ({app})?",
        start.format("%H:%M"),
        end.format("%H:%M")
    )
}

/// The stretch's question text: its span plus the majority session app
/// (ties break to the smallest name, like the device stamp).
fn stretch_question(stretch: &Stretch) -> String {
    let mut apps: BTreeMap<&str, usize> = BTreeMap::new();
    for session in &stretch.sessions {
        if !session.app.is_empty() {
            *apps.entry(session.app.as_str()).or_default() += 1;
        }
    }
    let app = apps
        .into_iter()
        .max_by_key(|(name, count)| (*count, std::cmp::Reverse(*name)))
        .map(|(name, _)| name.to_string())
        .unwrap_or_default();
    span_question(stretch.start, stretch.end, &app)
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

// ------------------------------------------- question supersession (issue #66)

/// The span an open question covers, as far as the row itself tells it.
/// Two producer shapes exist:
/// - compiler questions: "What were you working on HH:MM-HH:MM (app)?" -
///   the span lives in the text, local to the question's `day`;
/// - analysis-worker questions: "What were you working on at <RFC3339>?
///   (auto-detected: ...)" - a single instant, repeated by every evidence
///   frame's `ts`.
///
/// A question of neither shape (or with nothing parseable) has an
/// unknown span; the supersession rule never closes it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum QuestionSpan {
    /// Local-day wall-clock span from question text (no offset attached).
    Text(NaiveTime, NaiveTime),
    /// Concrete instants (text instant or evidence-frame bounds).
    Instants(DateTime<FixedOffset>, DateTime<FixedOffset>),
}

const QUESTION_TEXT_PREFIX: &str = "What were you working on ";

/// Parse a question's text span (see [`QuestionSpan`]).
fn question_text_span(question: &TsQuestion) -> Option<QuestionSpan> {
    let rest = question.question.strip_prefix(QUESTION_TEXT_PREFIX)?;
    if let Some(instant) = rest.strip_prefix("at ") {
        // Analysis-worker format: the RFC3339 instant runs up to the '?'.
        let raw = instant.split('?').next()?.trim();
        let ts = DateTime::parse_from_rfc3339(raw).ok()?;
        return Some(QuestionSpan::Instants(ts, ts));
    }
    // Compiler format: "HH:MM-HH:MM (app)?".
    let times = rest.split(" (").next()?;
    let (start, end) = times.split_once('-')?;
    Some(QuestionSpan::Text(
        NaiveTime::parse_from_str(start.trim(), "%H:%M").ok()?,
        NaiveTime::parse_from_str(end.trim(), "%H:%M").ok()?,
    ))
}

/// A question's evidence-frame bounds, when every frame ts parses.
fn question_evidence_span(question: &TsQuestion) -> Option<QuestionSpan> {
    let mut times = question
        .evidence
        .iter()
        .filter_map(|frame| DateTime::parse_from_rfc3339(&frame.ts).ok());
    let first = times.next()?;
    let (mut min, mut max) = (first, first);
    for ts in times {
        if ts < min {
            min = ts;
        }
        if ts > max {
            max = ts;
        }
    }
    Some(QuestionSpan::Instants(min, max))
}

/// The question's own span: text first, evidence frames as fallback.
fn question_span(question: &TsQuestion) -> Option<QuestionSpan> {
    question_text_span(question).or_else(|| question_evidence_span(question))
}

/// Materialize a text span onto the drafts' own UTC offset (the device's
/// local wall clock the compiler wrote the times in). `None` when there
/// are no drafts to borrow the offset from or the day is malformed.
fn materialize_question_span(
    span: QuestionSpan,
    question: &TsQuestion,
    offset: Option<FixedOffset>,
) -> Option<(DateTime<FixedOffset>, DateTime<FixedOffset>)> {
    match span {
        QuestionSpan::Instants(start, end) => Some((start, end)),
        QuestionSpan::Text(start_time, end_time) => {
            let offset = offset?;
            let day = parse_day(&question.day).ok()?;
            let start = day
                .and_time(start_time)
                .and_local_timezone(offset)
                .earliest()?;
            let end = day
                .and_time(end_time)
                .and_local_timezone(offset)
                .earliest()?;
            Some((start, end))
        }
    }
}

/// Is `[start, end)` fully covered by the union of the sorted draft
/// spans? A zero-length span (an instant) counts as covered when the
/// instant falls inside some draft.
fn covered_by_drafts(
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    draft_spans: &[(DateTime<FixedOffset>, DateTime<FixedOffset>)],
) -> bool {
    if end <= start {
        return draft_spans
            .iter()
            .any(|(draft_start, draft_end)| *draft_start <= start && start < *draft_end);
    }
    let mut cursor = start;
    for (draft_start, draft_end) in draft_spans {
        if *draft_end <= cursor {
            continue; // an earlier draft, already behind the cursor
        }
        if *draft_start > cursor {
            return false; // a gap the drafts never cover
        }
        cursor = *draft_end;
        if cursor >= end {
            return true;
        }
    }
    false
}

/// The open questions the recompiled drafts supersede (issue #66). A
/// question closes when
/// - its text span is zero-length (the superseded pre-#63 compile's
///   sub-minute debris - dead by construction, drafts or not), or
/// - its span is fully covered by the union of the current drafts: the
///   owner reviews the drafts, not a queue of questions about drafted
///   time. An evidence-backed instant question closes only through
///   coverage - a still-grey instant stays askable even though a single
///   instant is zero-length.
///
/// Non-open questions and unknown spans are never returned. Pure; the
/// caller writes the closures through
/// `TsQuestionStore::supersede_questions` (the regeneration guard and
/// the maintenance endpoint share this exact rule).
pub fn superseded_question_ids(questions: &[TsQuestion], drafts: &[DraftActivity]) -> Vec<i64> {
    let mut draft_spans: Vec<(DateTime<FixedOffset>, DateTime<FixedOffset>)> = drafts
        .iter()
        .filter_map(|activity| {
            let start = DateTime::parse_from_rfc3339(&activity.start).ok()?;
            let end = DateTime::parse_from_rfc3339(&activity.end).ok()?;
            (end > start).then_some((start, end))
        })
        .collect();
    draft_spans.sort();
    let offset = draft_spans.first().map(|(start, _)| *start.offset());

    questions
        .iter()
        .filter(|question| question.status == TsQuestionStatus::Open)
        .filter(|question| match question_span(question) {
            // Zero-length compile debris closes regardless of drafts.
            Some(QuestionSpan::Text(start, end)) if start == end => true,
            Some(span) => match materialize_question_span(span, question, offset) {
                Some((start, end)) => covered_by_drafts(start, end, &draft_spans),
                None => false,
            },
            None => false,
        })
        .map(|question| question.id)
        .collect()
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

    let window_ms = classification_window_ms(cfg);
    let segment_gap_ms = i64::try_from(cfg.segment_gap_s.saturating_mul(1000)).unwrap_or(i64::MAX);
    let max_block =
        Duration::milliseconds((cfg.max_block_min.saturating_mul(60) as i64).saturating_mul(1000));

    // Dispositions: approved-overlapping sessions are skipped entirely
    // (issue #60); sessions whose question match is answered resolve as
    // their own drafts and split the segmentation around them (the
    // owner's own words never merge); everything else segments. An open
    // question match does not split - the session's time belongs to its
    // block, the question stays open until answered or superseded (issue
    // #66 closes covered questions on regeneration).
    let mut answered_drafts: Vec<AnsweredDraft> = Vec::new();
    let mut answered_spans: Vec<(DateTime<FixedOffset>, DateTime<FixedOffset>)> = Vec::new();
    let mut segmentable: Vec<usize> = Vec::new();
    for (index, session) in sessions.iter().enumerate() {
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
        // Existing questions covering this session: an evidence frame
        // inside the span (containment, issue #65 - a session cites every
        // analysis within `classify_window_s`, so a bare hash overlap
        // would let a question ten minutes away suppress it), or the
        // deterministic question text (covers frame-less questions).
        let attachment = attach_analyses(session, &analyses, window_ms);
        let shas = pool_shas(&attachment.attached);
        let question_text = heuristic_question(session);
        let matches: Vec<&TsQuestion> = existing_questions
            .iter()
            .filter(|q| {
                q.day == day_str
                    && (q.evidence.iter().any(|f| {
                        shas.contains(&f.frame_sha16)
                            && frame_in_span(session.start, session.end, f)
                    }) || q.question == question_text)
            })
            .collect();
        if let Some(question) = matches
            .iter()
            .filter(|q| q.answer.is_some())
            .max_by_key(|q| q.id)
            .copied()
        {
            // Owner-confirmed: the answer becomes the task, confidence is
            // maximal, the span never merges (owner's own words stay).
            let (project, category) = classify_labels(attachment.best);
            answered_drafts.push(AnsweredDraft {
                start: session.start,
                end: session.end,
                project_guess: project,
                category,
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
                shas,
                session_ids: vec![session.id],
                question_ids: matches
                    .iter()
                    .filter(|q| q.answer.is_some())
                    .map(|q| q.id)
                    .collect(),
            });
            answered_spans.push((session.start, session.end));
            continue;
        }
        segmentable.push(index);
    }

    // Segment FIRST (issue #67 step 1), then classify each segment by
    // majority vote (step 2). Consecutive grey segments of one run
    // accumulate into a single stretch (issue #64); a classified segment
    // flushes the stretch under construction.
    let mut groups: Vec<Group> = Vec::new();
    let mut stretches: Vec<Stretch> = Vec::new();
    for run in runs_of(
        &segmentable,
        &sessions,
        segment_gap_ms,
        &afk,
        &answered_spans,
    ) {
        let mut grey: Vec<Segment> = Vec::new();
        let mut grey_pool: Vec<&AnalysisRec> = Vec::new();
        for chunk in split_at_max_block(&run, max_block) {
            let segment = build_segment(chunk);
            let classification = classify_segment(&segment, &analyses, window_ms);
            let resolvable = classification
                .selected
                .is_some_and(|s| s.confidence >= RESOLVE_MIN_CONFIDENCE);
            if resolvable {
                if !grey.is_empty() {
                    stretches.push(fold_stretch(
                        std::mem::take(&mut grey),
                        std::mem::take(&mut grey_pool),
                    ));
                }
                groups.push(group_from(&segment, &classification));
            } else {
                // Unresolvable (no attached analyses, or the vote's
                // selected analysis sits below the gate): grey.
                grey.push(segment);
                for analysis in classification.pool {
                    if !grey_pool.iter().any(|seen| {
                        seen.ts == analysis.ts && seen.frame_sha16 == analysis.frame_sha16
                    }) {
                        grey_pool.push(analysis);
                    }
                }
            }
        }
        if !grey.is_empty() {
            stretches.push(fold_stretch(grey, grey_pool));
        }
    }

    // Grey stretches -> #64 question semantics (issue #67 step 4): ONE
    // question per stretch with the frames from inside it; an answered
    // match resolves the whole stretch, an open match suppresses the
    // duplicate; beyond the daily cap the remaining stretches fold into
    // one summary question.
    let mut planned_questions: Vec<PlannedQuestion> = Vec::new();
    let mut pending: Vec<&Stretch> = Vec::new();
    for stretch in &stretches {
        let question_text = stretch_question(stretch);
        let shas = pool_shas(&stretch.pool);
        let matches: Vec<&TsQuestion> = existing_questions
            .iter()
            .filter(|q| {
                q.day == day_str
                    && (q.evidence.iter().any(|f| {
                        shas.contains(&f.frame_sha16)
                            && frame_in_span(stretch.start, stretch.end, f)
                    }) || q.question == question_text)
            })
            .collect();
        if let Some(question) = matches
            .iter()
            .filter(|q| q.answer.is_some())
            .max_by_key(|q| q.id)
            .copied()
        {
            // The owner's answer resolves the whole stretch.
            let (project, category) = pool_labels(stretch.start, stretch.end, &stretch.pool);
            answered_drafts.push(AnsweredDraft {
                start: stretch.start,
                end: stretch.end,
                project_guess: project,
                category,
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
                shas,
                session_ids: stretch.sessions.iter().map(|s| s.id).collect(),
                question_ids: matches
                    .iter()
                    .filter(|q| q.answer.is_some())
                    .map(|q| q.id)
                    .collect(),
            });
        } else if matches.iter().any(|q| q.answer.is_none()) {
            // Unresolved and already asked: no duplicate question.
        } else {
            pending.push(stretch);
        }
    }
    let cap = usize::try_from(cfg.daily_question_cap).unwrap_or(usize::MAX);
    for stretch in pending.iter().take(cap) {
        planned_questions.push(PlannedQuestion {
            question: stretch_question(stretch),
            evidence: stretch_evidence(stretch.start, stretch.end, &stretch.pool),
        });
    }
    if pending.len() > cap {
        // Beyond the cap: one aggregate summary question (issue #64). Its
        // text parses as one span (first start .. last end) so the issue
        // #66 supersession rule can close it once drafts cover the span.
        let folded = &pending[cap..];
        let first = folded.first().expect("a non-empty tail");
        let last = folded.last().expect("a non-empty tail");
        let spans: Vec<String> = folded
            .iter()
            .map(|stretch| {
                format!(
                    "{}-{}",
                    stretch.start.format("%H:%M"),
                    stretch.end.format("%H:%M")
                )
            })
            .collect();
        let question = format!(
            "What were you working on {}-{} ({} unclassified stretches: {})?",
            first.start.format("%H:%M"),
            last.end.format("%H:%M"),
            folded.len(),
            spans.join(", ")
        );
        let evidence: Vec<EvidenceFrame> = folded
            .iter()
            .flat_map(|stretch| stretch_evidence(stretch.start, stretch.end, &stretch.pool))
            .collect();
        planned_questions.push(PlannedQuestion { question, evidence });
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
    // Owner-answered drafts are already complete (session-span and
    // stretch-span answers alike).
    for draft in &answered_drafts {
        activities.push(DraftActivity {
            start: draft.start.to_rfc3339(),
            end: draft.end.to_rfc3339(),
            project_guess: draft.project_guess.clone(),
            category: draft.category.clone(),
            task: draft.task.clone(),
            comment: draft.comment.clone(),
            confidence: 1.0,
            sources: ActivitySources {
                frame_sha16s: draft.shas.clone(),
                session_ids: draft.session_ids.clone(),
                question_ids: draft.question_ids.clone(),
            },
        });
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
    fn flip_flop_labels_majority_vote_into_one_block() {
        // Pilot-day cadence (issues #63/#67): 10 s poll sessions tiling
        // one hour, 8 keyframes whose labels flip-flop between two
        // project/category pairs. Containment classified only the ~10 s
        // session containing each frame (45 of 52 drafts sub-minute on
        // the real day); nearest-in-time still left one block per
        // keyframe zone. Segment-then-classify merges the whole hour and
        // majority-votes the labels: 4 x (C1234, Consulting) vs
        // 4 x (C9999, QA) ties, the midpoint tiebreak picks the analysis
        // nearest the block's midpoint (15:30 - the k=4 frame at exactly
        // 15:30:00) -> C1234 wins. One draft, zero questions, no
        // sub-minute slivers.
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
        assert_eq!(planned.activities.len(), 1, "one majority-vote block");
        assert!(planned.questions.is_empty());
        let activity = &planned.activities[0];
        assert!(span_seconds(activity) >= 60 * 30, "the whole hour drafts");
        assert_eq!(activity.project_guess, "C1234", "midpoint tiebreak");
        assert_eq!(activity.category, "Consulting");
        assert_eq!(activity.sources.session_ids.len(), 360);
        assert_eq!(activity.sources.frame_sha16s.len(), 8);
    }

    #[test]
    fn plurality_beats_a_minority_zone_inside_a_segment() {
        // A 2-1 vote: two C1234 frames against one C9999 frame all inside
        // one contiguous run - the plurality pair wins the block (issue
        // #67 majority vote), no per-zone split.
        let input = input_for(vec![
            poll_session(1, 0),
            poll_session(2, 10),
            poll_session(3, 20),
            poll_session(4, 30),
            poll_session(5, 40),
            poll_session(6, 50),
            offset_analysis(10, 0, "C1234", "Consulting", 0.9),
            offset_analysis(11, 25, "C9999", "QA", 0.95),
            offset_analysis(12, 50, "C1234", "Consulting", 0.8),
        ]);
        let planned = plan(&input);
        assert_eq!(planned.activities.len(), 1);
        assert_eq!(planned.activities[0].project_guess, "C1234");
        assert_eq!(planned.activities[0].category, "Consulting");
        // block confidence is the minimum over all attached analyses
        assert!((planned.activities[0].confidence - 0.8).abs() < 1e-9);
        assert!(planned.questions.is_empty());
    }

    #[test]
    fn pilot_day_burst_loses_the_majority_vote() {
        // The 15:07 pattern from issue #63: a burst of five analyses
        // seconds apart answering five different project/category pairs,
        // embedded in a calm hour of stable keyframes. Nearest-in-time
        // still produced one sub-minute block per burst zone; segment-
        // then-classify majority-votes the whole contiguous hour: the 7
        // calm C1234 votes outvote the five 1-vote burst pairs, the day
        // compiles to one reviewable block, zero sub-minute drafts.
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
        assert_eq!(planned.activities.len(), 1, "one block for the hour");
        assert_eq!(planned.activities[0].project_guess, "C1234");
        assert_eq!(planned.activities[0].category, "Consulting");
        assert!(span_seconds(&planned.activities[0]) >= 60 * 30);
        let sub_minute = planned
            .activities
            .iter()
            .filter(|a| span_seconds(a) < 60)
            .count();
        assert_eq!(sub_minute, 0, "no sub-minute slivers survive the vote");
    }

    #[test]
    fn a_segment_with_a_frame_classifies_wholly_grey_stays_one_question() {
        // Segment-then-classify (issue #67): one keyframe inside a
        // contiguous run classifies the whole segment - the classify
        // window gates attachment to individual sessions, not the block
        // extent (nearest-in-time still split 15:00-15:20 into a
        // classified head and grey tail). A run with no analysis within
        // the window stays grey as ONE stretch raising ONE question
        // (issue #64), not one question per ~10 s session.
        let build_rows = || {
            let mut rows = Vec::new();
            for k in 0..120 {
                rows.push(poll_session(k + 1, k * 10)); // 15:00:00-15:20:00
            }
            for k in 0..30 {
                // A 5-minute grey stretch later in the day (a 55-minute
                // gap from any frame of the day).
                rows.push(poll_session(1000 + k, 3600 + k * 10)); // 16:00:00-16:05:00
            }
            rows.push(offset_analysis(2000, 360, "C1234", "Consulting", 0.9)); // 15:06:00
            rows
        };
        let planned = plan(&input_for(build_rows()));
        assert_eq!(planned.activities.len(), 1, "the 15:00 run classifies");
        let activity = &planned.activities[0];
        assert_eq!(activity.start, "2026-11-06T15:00:00+08:00");
        assert_eq!(activity.end, "2026-11-06T15:20:00+08:00");
        assert_eq!(planned.questions.len(), 1, "the grey run asks once");
        let question = &planned.questions[0];
        assert!(question.question.contains("16:00-16:05"));
        assert!(
            question.evidence.is_empty(),
            "no frames fall inside the stretch"
        );

        // A tight window changes nothing about the classified block: the
        // frame still attaches to its surrounding sessions and feeds the
        // segment's vote.
        let mut tight = cfg();
        tight.classify_window_s = 60;
        let planned = plan_day(&input_for(build_rows()), &[], &tight, &HeuristicTitler);
        assert_eq!(planned.activities.len(), 1);
        assert_eq!(planned.activities[0].end, "2026-11-06T15:20:00+08:00");
        assert_eq!(planned.questions.len(), 1);
    }

    #[test]
    fn low_confidence_frames_inside_a_grey_stretch_become_its_evidence() {
        // Issue #64/#67: a grey stretch (the vote's selected analysis sits
        // below the gate) cites the frames that fall INSIDE the stretch -
        // not far-away ones.
        let mut rows = Vec::new();
        for k in 0..30 {
            rows.push(poll_session(k + 1, k * 10)); // 15:00:00-15:05:00
        }
        rows.push(offset_analysis(10, 120, "?", "?", 0.3)); // 15:02:00, in-stretch
        rows.push(offset_analysis(11, 3600, "C1234", "Consulting", 0.3)); // 16:00, outside
        let planned = plan(&input_for(rows));
        assert!(planned.activities.is_empty());
        assert_eq!(planned.questions.len(), 1);
        assert_eq!(planned.questions[0].evidence.len(), 1, "in-stretch only");
        assert_eq!(planned.questions[0].evidence[0].frame_sha16, "sha-0010");
        assert_eq!(
            planned.questions[0].evidence[0].ts,
            "2026-11-06T15:02:00+08:00"
        );
    }

    #[test]
    fn grey_stretches_beyond_the_daily_cap_fold_into_one_summary() {
        // Issue #64 acceptance: a 30-minute grey stretch produces exactly
        // 1 question; with more stretches than the daily cap the
        // remainder folds into one aggregate summary question whose span
        // (first start .. last end) the #66 supersession rule can parse.
        let build_rows = |count: usize| {
            let mut rows = Vec::new();
            // `count` five-minute grey stretches, each 1 h apart, no
            // analyses anywhere (every stretch is its own run).
            for n in 0..count {
                for k in 0..30 {
                    rows.push(poll_session(
                        (1000 * (n + 1) + k) as i64,
                        (n as i64 * 3600) + k as i64 * 10,
                    ));
                }
            }
            rows
        };
        // Under the cap: one question per stretch.
        let planned = plan(&input_for(build_rows(3)));
        assert_eq!(planned.questions.len(), 3);
        assert!(planned.questions[0].question.contains("15:00-15:05"));

        // Beyond the cap (7 stretches, cap 5): 5 questions + 1 summary.
        let planned = plan(&input_for(build_rows(7)));
        assert_eq!(planned.questions.len(), 6);
        let summary = &planned.questions[5];
        assert!(
            summary.question.contains("2 unclassified stretches"),
            "summary names the folded stretch count: {}",
            summary.question
        );
        assert!(
            summary.question.contains("20:00-20:05"),
            "summary lists the folded spans: {}",
            summary.question
        );
        // The summary's span parses (first start .. last end) for #66.
        let span = question_text_span(&TsQuestion {
            id: 0,
            day: DAY.to_string(),
            question: summary.question.clone(),
            evidence: Vec::new(),
            status: TsQuestionStatus::Open,
            answer: None,
            created_at: String::new(),
            answered_at: None,
        });
        assert_eq!(
            span,
            Some(QuestionSpan::Text(
                NaiveTime::from_hms_opt(20, 0, 0).expect("20:00"),
                NaiveTime::from_hms_opt(21, 5, 0).expect("21:05")
            ))
        );

        // A cap of 0 folds everything into the summary alone.
        let mut zero = cfg();
        zero.daily_question_cap = 0;
        let planned = plan_day(&input_for(build_rows(2)), &[], &zero, &HeuristicTitler);
        assert_eq!(planned.questions.len(), 1);
        assert!(planned.questions[0]
            .question
            .contains("2 unclassified stretches"));
    }

    #[test]
    fn answered_stretch_question_resolves_the_whole_stretch() {
        // A previously asked stretch question (issue #64 text shape) that
        // the owner answered resolves the whole stretch as one draft -
        // the same rule session-span answers have always had.
        let questions = TsQuestionStore::open_in_memory().expect("store");
        let question_id = questions
            .create_question(DAY, "What were you working on 15:00-15:05 (Code.exe)?", &[])
            .expect("create");
        questions
            .answer_question(question_id, "Invoice run for C1234")
            .expect("answer");
        let mut rows = Vec::new();
        for k in 0..30 {
            rows.push(poll_session(k + 1, k * 10)); // 15:00:00-15:05:00
        }
        let input = input_for(rows);
        let existing = questions.list_questions(StatusFilter::All).expect("list");
        let planned = plan_day(&input, &existing, &cfg(), &HeuristicTitler);
        assert!(planned.questions.is_empty(), "answered, not re-asked");
        assert_eq!(planned.activities.len(), 1, "one draft for the stretch");
        let activity = &planned.activities[0];
        assert_eq!(activity.start, "2026-11-06T15:00:00+08:00");
        assert_eq!(activity.end, "2026-11-06T15:05:00+08:00");
        assert_eq!(activity.task, "Invoice run for C1234");
        assert!((activity.confidence - 1.0).abs() < 1e-9);
        assert_eq!(activity.sources.question_ids, vec![question_id]);
        assert_eq!(activity.sources.session_ids.len(), 30);
    }

    #[test]
    fn open_stretch_question_suppresses_the_duplicate() {
        // An open stretch question (evidence frame inside the stretch)
        // suppresses the new question; the stretch stays un-drafted.
        let questions = TsQuestionStore::open_in_memory().expect("store");
        questions
            .create_question(
                DAY,
                "What were you working on 15:00-15:05 (Code.exe)?",
                &[EvidenceFrame {
                    frame_path: "media/screen/2026/11/06/f10.jpg".to_string(),
                    frame_sha16: "sha-0010".to_string(),
                    ts: "2026-11-06T15:02:00+08:00".to_string(),
                }],
            )
            .expect("create");
        let mut rows = Vec::new();
        for k in 0..30 {
            rows.push(poll_session(k + 1, k * 10)); // 15:00:00-15:05:00
        }
        // The stretch's own low-confidence analysis (post-#63 every
        // session within 600 s cites it as evidence).
        rows.push(offset_analysis(10, 120, "?", "?", 0.3));
        let input = input_for(rows);
        let existing = questions.list_questions(StatusFilter::All).expect("list");
        let planned = plan_day(&input, &existing, &cfg(), &HeuristicTitler);
        assert!(planned.questions.is_empty(), "no duplicate question");
        assert!(planned.activities.is_empty(), "grey stretch stays grey");
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
        // Segment-then-classify (issue #67): the three short sessions
        // (60-150 s apart) segment into ONE majority-vote block, so two
        // drafts carry the day. The regression stays pinned: ALL FOUR
        // session ids still draft - no session vanishes to an open
        // question.
        assert_eq!(planned.activities.len(), 2);
        let mut drafted_ids: Vec<i64> = planned
            .activities
            .iter()
            .flat_map(|a| a.sources.session_ids.iter().copied())
            .collect();
        drafted_ids.sort();
        assert_eq!(drafted_ids, vec![6497, 6516, 6598, 6668]);
    }

    // --------------------- fixture: question supersession (issue #66)

    /// An open question row without a store round trip (the store path
    /// is covered in storage and server integration tests).
    fn open_question(id: i64, question: &str, evidence: &[EvidenceFrame]) -> TsQuestion {
        TsQuestion {
            id,
            day: DAY.to_string(),
            question: question.to_string(),
            evidence: evidence.to_vec(),
            status: TsQuestionStatus::Open,
            answer: None,
            created_at: "2026-11-06T20:00:00Z".to_string(),
            answered_at: None,
        }
    }

    fn activity(start: &str, end: &str) -> DraftActivity {
        DraftActivity {
            start: start.to_string(),
            end: end.to_string(),
            project_guess: "C1234".to_string(),
            category: "Consulting".to_string(),
            task: String::new(),
            comment: String::new(),
            confidence: 0.9,
            sources: ActivitySources::default(),
        }
    }

    fn instant_frame(ts: &str) -> EvidenceFrame {
        EvidenceFrame {
            frame_path: "media/frames/2026/11/06/f.jpg".to_string(),
            frame_sha16: "aaaabbbbccccdddd".to_string(),
            ts: ts.to_string(),
        }
    }

    #[test]
    fn zero_length_text_debris_closes_without_drafts() {
        // The superseded pre-#63 compile's sub-minute questions
        // ("12:04-12:04 (explorer.exe)" style): zero-length by
        // construction, dead with or without drafts; a wide grey span
        // stays askable.
        let debris = open_question(
            1,
            "What were you working on 12:04-12:04 (explorer.exe)?",
            &[],
        );
        let wide = open_question(2, "What were you working on 09:00-10:00 (Code.exe)?", &[]);
        assert_eq!(superseded_question_ids(&[debris, wide], &[]), vec![1]);
    }

    #[test]
    fn wide_text_span_closes_only_when_fully_covered() {
        let wide = |id| open_question(id, "What were you working on 09:00-10:00 (Code.exe)?", &[]);
        // Union coverage across two adjacent drafts closes it...
        let drafts = [
            activity("2026-11-06T09:00:00+08:00", "2026-11-06T09:30:00+08:00"),
            activity("2026-11-06T09:30:00+08:00", "2026-11-06T10:00:00+08:00"),
        ];
        assert_eq!(superseded_question_ids(&[wide(1)], &drafts), vec![1]);
        // ...partial coverage does not...
        let drafts = [activity(
            "2026-11-06T09:00:00+08:00",
            "2026-11-06T09:30:00+08:00",
        )];
        assert!(superseded_question_ids(&[wide(1)], &drafts).is_empty());
        // ...and neither does a hole between two drafts.
        let drafts = [
            activity("2026-11-06T09:00:00+08:00", "2026-11-06T09:30:00+08:00"),
            activity("2026-11-06T09:45:00+08:00", "2026-11-06T10:00:00+08:00"),
        ];
        assert!(superseded_question_ids(&[wide(1)], &drafts).is_empty());
        // Fully disjoint drafts leave it open.
        let drafts = [
            activity("2026-11-06T08:00:00+08:00", "2026-11-06T08:30:00+08:00"),
            activity("2026-11-06T11:00:00+08:00", "2026-11-06T12:00:00+08:00"),
        ];
        assert!(superseded_question_ids(&[wide(1)], &drafts).is_empty());
    }

    #[test]
    fn auto_detected_instant_closes_only_when_covered() {
        // Analysis-worker hand-off: a real grey instant with real frame
        // evidence. Zero-length alone must NOT close it - it closes only
        // once a draft covers the instant (text-parsed or frame-derived,
        // both shapes agree here).
        let worker_text = |ts: &str| {
            format!(
                "What were you working on at {ts}? \
                 (auto-detected: Creating a new project in the IDE; confidence 0.30)"
            )
        };
        let covered = open_question(
            1,
            &worker_text("2026-11-06T09:15:00+08:00"),
            &[instant_frame("2026-11-06T09:15:00+08:00")],
        );
        let grey = open_question(
            2,
            &worker_text("2026-11-06T11:45:00+08:00"),
            &[instant_frame("2026-11-06T11:45:00+08:00")],
        );
        let drafts = [
            activity("2026-11-06T09:00:00+08:00", "2026-11-06T09:30:00+08:00"),
            activity("2026-11-06T10:00:00+08:00", "2026-11-06T11:00:00+08:00"),
        ];
        assert_eq!(superseded_question_ids(&[covered, grey], &drafts), vec![1]);
        // Text-only instant (no evidence rows) follows the same rule.
        let text_only = open_question(3, &worker_text("2026-11-06T09:15:00+08:00"), &[]);
        assert_eq!(superseded_question_ids(&[text_only], &drafts), vec![3]);
    }

    #[test]
    fn unknown_span_and_non_open_questions_never_close() {
        let drafts = [activity(
            "2026-11-06T09:00:00+08:00",
            "2026-11-06T17:00:00+08:00",
        )];
        // Neither producer shape, no evidence: unknown span stays open.
        let mystery = open_question(1, "Why is the sky blue?", &[]);
        // An owner answer wins over supersession (already answered).
        let mut answered =
            open_question(2, "What were you working on 09:00-10:00 (Code.exe)?", &[]);
        answered.status = TsQuestionStatus::Answered;
        answered.answer = Some("cube work".to_string());
        // Already-superseded debris is not reported a second time.
        let mut closed = open_question(
            3,
            "What were you working on 12:04-12:04 (explorer.exe)?",
            &[],
        );
        closed.status = TsQuestionStatus::Superseded;
        assert!(superseded_question_ids(&[mystery, answered, closed], &drafts).is_empty());
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
