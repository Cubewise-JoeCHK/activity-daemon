//! Month-end xlsx export in the Cubewise offline-template upload format
//! (issue #46; template contract = ADR addendum decision 4 on #16).
//!
//! Input is the month's approved activities (bucket
//! `timesheet-approved`, written by the hub review UI): one upload row per
//! approved activity, columns `Project | Date | Line Item | Category |
//! Task | Comment | Billable | NonBillable`. Rows the contract cannot
//! express (project string or category not in the vocabulary, hour code
//! pattern with no billable rule, non-positive duration) are refused and
//! reported as warnings - never silently dropped; they land on a
//! `Warnings` sheet of the produced workbook and surface as the
//! `X-Export-Warnings` count at the API layer.
//!
//! Cell conventions mirror the template's own upload sheet: `Date` and the
//! zero-padded `Line Item` are text ("2026-10-01", "0001"), the hour
//! columns are numbers rounded to 0.25 h. Output is a fresh in-memory
//! workbook: the `upload` sheet, a `Sheet1` mirroring the template's
//! vocabulary reference columns (categories A2.., projects C2..), and a
//! `Warnings` sheet when there are warnings. No input file is ever
//! written.
//!
//! Defence-in-depth dedup (issue #60): approved events that overlap in
//! time on the same project + category would double-count hours, so the
//! exporter keeps only the earliest-`approved_at` row of each overlapping
//! group (missing stamps lose to known ones; equal stamps break to the
//! earlier start, then input order) and warns on every dropped duplicate.

use std::path::Path;

use activity_core::{CoreError, Event};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate};
use rust_xlsxwriter::Workbook;
use serde::Deserialize;

use crate::vocab::CubewiseVocab;

/// Bucket holding owner-approved activities (ADR addendum on #16: drafts
/// in `timesheet-draft`, approved rows here).
pub const BUCKET_TIMESHEET_APPROVED: &str = "timesheet-approved";

/// Upload sheet header row, byte-exact per the template.
pub const UPLOAD_HEADERS: [&str; 8] = [
    "Project",
    "Date",
    "Line Item",
    "Category",
    "Task",
    "Comment",
    "Billable",
    "NonBillable",
];

/// One cell of an upload row: text where the template uses text (dates,
/// zero-padded line items, labels), numbers for the hour columns.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportCell {
    Text(String),
    Number(f64),
}

/// The export result: upload rows in final order plus every refusal
/// warning, in input order.
#[derive(Debug, Default, PartialEq)]
pub struct ExportOutcome {
    /// Fully assembled upload rows (8 cells each), ordered by day then
    /// activity start; `Line Item` numbering already applied.
    pub rows: Vec<Vec<ExportCell>>,
    /// Refused rows and fallback notices; one human-readable line each.
    pub warnings: Vec<String>,
}

/// Defensive view of an approved activity's `data` JSON: the draft shape
/// (issue #45) plus `approved_at` (issue #47, unused here). Every field is
/// optional - an approved row missing something the export needs is
/// refused with a warning, never a parse panic.
#[derive(Debug, Default, Deserialize)]
struct ApprovedActivity {
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    end: Option<String>,
    /// Draft field name (issue #45). `project` is accepted as an alias so
    /// either vocabulary survives a producer change.
    #[serde(default, alias = "project")]
    project_guess: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    task: Option<String>,
    /// Owner comment as approved. The draft's `comment` is the evidence
    /// summary (issue #45); when the approved comment is empty the
    /// exporter falls back to a separately carried evidence summary.
    #[serde(default)]
    comment: Option<String>,
    #[serde(default)]
    evidence_summary: Option<String>,
    /// Approval stamp (issue #47): the overlap guard's keep-earliest key
    /// (issue #60).
    #[serde(default)]
    approved_at: Option<String>,
}

/// A row that passed every check, ready for line-item numbering.
struct PickedActivity {
    /// Local date of the activity start (at the start's own offset) - the
    /// grouping and numbering key and the `Date` cell.
    date: NaiveDate,
    /// Parsed start, the secondary sort key within a day.
    start: DateTime<FixedOffset>,
    /// True span end (unrounded): the overlap guard compares true spans so
    /// quarter-hour rounding cannot invent overlaps between adjacent
    /// activities.
    end: DateTime<FixedOffset>,
    /// Parsed approval stamp; `None` (missing or unparseable) sorts last
    /// in the keep-earliest rule - known provenance beats unknown.
    approved_at: Option<DateTime<FixedOffset>>,
    project: String,
    category: String,
    task: String,
    comment: String,
    /// Hours rounded to 0.25 h.
    hours: f64,
    /// `true` = client code, hours go in Billable; `false` = internal
    /// (C000P*), hours go in NonBillable.
    billable: bool,
}

/// A row the overlap guard kept (issue #60): the `(project, category)`
/// pair with its true span, plus the approval stamp named in the warning
/// when a later duplicate is dropped.
struct KeptSpan {
    project: String,
    category: String,
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
    stamp: String,
}

/// `YYYY-MM` -> `(year, month)`. Strict: exactly 4 and 2 digits, month
/// 1-12 - the endpoint must not guess what `2026-1` or `2026-13` meant.
pub fn parse_month(month: &str) -> Option<(i32, u32)> {
    let mut parts = month.split('-');
    let (year_text, month_text) = (parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    if year_text.len() != 4 || month_text.len() != 2 {
        return None;
    }
    if !year_text.chars().all(|c| c.is_ascii_digit())
        || !month_text.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let year: i32 = year_text.parse().ok()?;
    let month_num: u32 = month_text.parse().ok()?;
    if !(1..=12).contains(&month_num) {
        return None;
    }
    Some((year, month_num))
}

/// The project code: the first ` - ` segment of the vocabulary string
/// (`A143P010 - Frasers - ...` -> `A143P010`).
fn project_code(project: &str) -> &str {
    project.split(" - ").next().unwrap_or(project)
}

/// The billable rule per the template contract: internal codes (`C000P*`)
/// put hours in NonBillable; client codes (`A*`/`C*` otherwise) put hours
/// in Billable. `None` = the contract defines no column for this code
/// pattern (today's vocabulary is entirely A*/C*, so this only fires on a
/// hand-edited vocabulary) - the row is refused rather than guessed.
fn billable_column(code: &str) -> Option<bool> {
    if code.starts_with("C000P") {
        Some(false)
    } else if code.starts_with('A') || code.starts_with('C') {
        Some(true)
    } else {
        None
    }
}

/// Round hours to the nearest quarter hour (the template's granularity).
fn round_to_quarter(hours: f64) -> f64 {
    (hours / 0.25).round() * 0.25
}

/// One locator line for warnings: the activity start, `YYYY-MM-DDTHH:MM:SS+off`.
fn locator(start: DateTime<FixedOffset>) -> String {
    start.format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// A parsed approval stamp for warning text; `None` prints as "unknown".
fn stamp_display(stamp: Option<DateTime<FixedOffset>>) -> String {
    stamp
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Build the month's upload rows from the bucket's approved activities.
///
/// Reads every approved event (the bucket holds owner-approved rows only,
/// a few dozen per month - no time-window query needed), keeps those whose
/// local start date falls in `year-month`, sorts by day then start and
/// numbers line items sequentially within each day starting at 0001.
pub fn build_month_grid(
    year: i32,
    month: u32,
    events: &[Event],
    vocab: &CubewiseVocab,
) -> ExportOutcome {
    let mut warnings = Vec::new();
    let mut picked = Vec::new();

    for event in events {
        let activity: ApprovedActivity = match serde_json::from_value(event.data.clone()) {
            Ok(activity) => activity,
            Err(e) => {
                warnings.push(format!(
                    "refused row at {}: data is not an activity object ({e})",
                    event.timestamp
                ));
                continue;
            }
        };
        let project = activity.project_guess.as_deref().unwrap_or("");
        let category = activity.category.as_deref().unwrap_or("");

        // One locator per refusal: the activity start when it parses, the
        // stored envelope timestamp otherwise.
        let start = activity
            .start
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .or_else(|| DateTime::parse_from_rfc3339(&event.timestamp).ok());
        let locator = start
            .map(locator)
            .unwrap_or_else(|| event.timestamp.clone());

        let Some(start) = start else {
            warnings.push(format!(
                "refused row for '{project}': start is missing or not RFC3339"
            ));
            continue;
        };
        let date = start.date_naive();
        if date.year() != year || date.month() != month {
            continue; // other months live in the same bucket; not a refusal
        }
        if project.is_empty() || !vocab.has_project(project) {
            warnings.push(format!(
                "refused row at {locator}: project '{project}' is not in the Cubewise vocabulary"
            ));
            continue;
        }
        if category.is_empty() || !vocab.has_category(category) {
            warnings.push(format!(
                "refused row at {locator}: category '{category}' is not one of the 7 template categories"
            ));
            continue;
        }
        let Some(billable) = billable_column(project_code(project)) else {
            warnings.push(format!(
                "refused row at {locator}: code '{}' has no Billable/NonBillable rule in the template contract",
                project_code(project)
            ));
            continue;
        };

        // Hours: end - start when the end parses, the stored envelope
        // duration otherwise (a present-but-unparseable end is reported).
        let end = activity
            .end
            .as_deref()
            .and_then(|e| DateTime::parse_from_rfc3339(e).ok());
        let true_end = end.unwrap_or_else(|| {
            start + chrono::Duration::milliseconds((event.duration_s * 1000.0).round() as i64)
        });
        let hours = match end {
            Some(end) => (end - start).num_seconds() as f64 / 3600.0,
            None => {
                if activity.end.is_some() {
                    warnings.push(format!(
                        "notice for row at {locator}: end is not RFC3339; used the stored duration"
                    ));
                }
                event.duration_s
            }
        };
        let hours = round_to_quarter(hours);
        if !hours.is_finite() || hours <= 0.0 {
            warnings.push(format!(
                "refused row at {locator}: duration rounds to {hours:.2} h, nothing to upload"
            ));
            continue;
        }

        picked.push(PickedActivity {
            date,
            start,
            end: true_end,
            approved_at: activity
                .approved_at
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok()),
            project: project.to_string(),
            category: category.to_string(),
            task: activity.task.unwrap_or_default(),
            comment: activity
                .comment
                .filter(|comment| !comment.trim().is_empty())
                .or(activity.evidence_summary)
                .unwrap_or_default(),
            hours,
            billable,
        });
    }

    // Overlap guard (issue #60, defence in depth): approved events that
    // overlap in time on the same project + category double-count hours.
    // Keep the earliest approved_at (missing stamps lose to known ones;
    // equal stamps break to the earlier start, then input order - a bulk
    // day-approve stamps every row alike) and warn on every dropped
    // duplicate. Spans compare unrounded so quarter-hour rounding cannot
    // invent overlaps between adjacent activities.
    picked.sort_by_key(|a| (a.approved_at.is_none(), a.approved_at, a.start));
    let mut kept_spans: Vec<KeptSpan> = Vec::new();
    let mut survivors = Vec::with_capacity(picked.len());
    for activity in picked {
        let duplicate = kept_spans.iter().find(|kept| {
            kept.project == activity.project
                && kept.category == activity.category
                && activity.start < kept.end
                && kept.start < activity.end
        });
        match duplicate {
            Some(kept) => warnings.push(format!(
                "duplicate row at {}: overlaps approved time for '{}' / '{}' already covered by the earlier approval (approved_at {}); dropped (this row's approved_at {})",
                locator(activity.start),
                activity.project,
                activity.category,
                kept.stamp,
                stamp_display(activity.approved_at),
            )),
            None => {
                kept_spans.push(KeptSpan {
                    project: activity.project.clone(),
                    category: activity.category.clone(),
                    start: activity.start,
                    end: activity.end,
                    stamp: stamp_display(activity.approved_at),
                });
                survivors.push(activity);
            }
        }
    }
    let mut picked = survivors;

    // Stable sort: days ascending, starts within a day ascending, and
    // equal starts keep their bucket insertion order.
    picked.sort_by_key(|activity| (activity.date, activity.start));

    let mut rows = Vec::new();
    let mut current_date = None;
    let mut line_item = 0u32;
    for activity in picked {
        if current_date != Some(activity.date) {
            current_date = Some(activity.date);
            line_item = 0;
        }
        line_item += 1;
        rows.push(vec![
            ExportCell::Text(activity.project),
            ExportCell::Text(activity.date.to_string()),
            ExportCell::Text(format!("{line_item:04}")),
            ExportCell::Text(activity.category),
            ExportCell::Text(activity.task),
            ExportCell::Text(activity.comment),
            ExportCell::Number(if activity.billable {
                activity.hours
            } else {
                0.0
            }),
            ExportCell::Number(if activity.billable {
                0.0
            } else {
                activity.hours
            }),
        ]);
    }

    ExportOutcome { rows, warnings }
}

/// Render the outcome into the workbook bytes: the `upload` sheet with its
/// headers and data rows, a `Sheet1` mirroring the template's vocabulary
/// reference columns (categories in A2.., projects in C2..) and a
/// `Warnings` sheet when there are warnings. Everything stays in memory -
/// no input or output file is touched.
pub fn write_workbook(
    outcome: &ExportOutcome,
    vocab: &CubewiseVocab,
) -> Result<Vec<u8>, CoreError> {
    fn fail(e: rust_xlsxwriter::XlsxError) -> CoreError {
        CoreError::Config(format!("xlsx export failed: {e}"))
    }
    let mut workbook = Workbook::new();

    let upload = workbook.add_worksheet();
    upload.set_name("upload").map_err(fail)?;
    for (col, header) in UPLOAD_HEADERS.iter().enumerate() {
        upload.write_string(0, col as u16, *header).map_err(fail)?;
    }
    for (row, cells) in outcome.rows.iter().enumerate() {
        // Data rows start one below the header row.
        let row = row as u32 + 1;
        for (col, cell) in cells.iter().enumerate() {
            match cell {
                ExportCell::Text(text) => {
                    upload.write_string(row, col as u16, text).map_err(fail)?;
                }
                ExportCell::Number(value) => {
                    upload.write_number(row, col as u16, *value).map_err(fail)?;
                }
            }
        }
    }

    // Vocabulary reference sheet, mirroring the template's Sheet1 layout.
    let sheet1 = workbook.add_worksheet();
    sheet1.set_name("Sheet1").map_err(fail)?;
    for (n, category) in vocab.categories.iter().enumerate() {
        sheet1
            .write_string(1 + n as u32, 0, category)
            .map_err(fail)?;
    }
    for (n, project) in vocab.projects.iter().enumerate() {
        sheet1
            .write_string(1 + n as u32, 2, project)
            .map_err(fail)?;
    }

    if !outcome.warnings.is_empty() {
        let sheet = workbook.add_worksheet();
        sheet.set_name("Warnings").map_err(fail)?;
        sheet.write_string(0, 0, "Warnings").map_err(fail)?;
        for (n, warning) in outcome.warnings.iter().enumerate() {
            sheet.write_string(1 + n as u32, 0, warning).map_err(fail)?;
        }
    }

    workbook.save_to_buffer().map_err(fail)
}

/// Convenience: load the vocabulary from an optional path override, build
/// the grid and render the workbook in one call (the endpoint's
/// `spawn_blocking` body).
pub fn export_month_bytes(
    events: &[Event],
    year: i32,
    month: u32,
    vocab_path: Option<&Path>,
) -> Result<(Vec<u8>, usize), CoreError> {
    let vocab = CubewiseVocab::load(vocab_path)?;
    let outcome = build_month_grid(year, month, events, &vocab);
    let warning_count = outcome.warnings.len();
    let bytes = write_workbook(&outcome, &vocab)?;
    Ok((bytes, warning_count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::Reader;

    #[test]
    fn parse_month_strict() {
        assert_eq!(parse_month("2026-10"), Some((2026, 10)));
        assert_eq!(parse_month("2026-01"), Some((2026, 1)));
        assert_eq!(parse_month("2026-12"), Some((2026, 12)));
        for bad in [
            "",
            "2026",
            "2026-1",
            "2026-13",
            "2026-00",
            "26-10",
            "2026-10-01",
            "202x-10",
            "2026/10",
        ] {
            assert_eq!(parse_month(bad), None, "{bad} must not parse");
        }
    }

    #[test]
    fn billable_column_rules() {
        assert_eq!(billable_column("A143P010"), Some(true));
        assert_eq!(billable_column("C920P020"), Some(true));
        assert_eq!(billable_column("C000P000"), Some(false));
        assert_eq!(billable_column("C000P109"), Some(false));
        assert_eq!(billable_column("D123P001"), None);
    }

    #[test]
    fn project_code_first_segment() {
        assert_eq!(
            project_code("A143P010 - Frasers - H2 FH - BU Core"),
            "A143P010"
        );
        assert_eq!(project_code("C000P000 - Cubewise - Internal"), "C000P000");
    }

    #[test]
    fn quarter_rounding() {
        assert_eq!(round_to_quarter(2.25), 2.25);
        assert_eq!(round_to_quarter(1.0 + 10.0 / 60.0), 1.25); // 1h10m
        assert_eq!(round_to_quarter(0.5 + 5.0 / 60.0), 0.5); // 35m -> nearest quarter
        assert_eq!(round_to_quarter(0.1), 0.0);
        assert_eq!(round_to_quarter(4.0), 4.0);
    }

    #[test]
    fn empty_month_yields_headers_only_and_no_warnings() {
        let vocab = CubewiseVocab::embedded().clone();
        let outcome = build_month_grid(2026, 9, &[], &vocab);
        assert!(outcome.rows.is_empty());
        assert!(outcome.warnings.is_empty());
        let bytes = write_workbook(&outcome, &vocab).expect("workbook");
        assert!(bytes.len() > 1000);

        let mut book: calamine::Xlsx<_> =
            calamine::open_workbook_from_rs(std::io::Cursor::new(bytes)).expect("open workbook");
        let range = book.worksheet_range("upload").expect("upload sheet");
        assert_eq!(range.height(), 1); // headers only
        let headers: Vec<String> = (0..8)
            .map(|col| range.get_value((0, col)).unwrap().to_string())
            .collect();
        assert_eq!(headers, UPLOAD_HEADERS.to_vec());
    }

    // ------------------ overlap guard fixtures (issue #60)

    use activity_core::Event;

    const GUARD_PROJECT: &str = "A143P010 - Frasers - H2 FH - BU Core";
    const OTHER_PROJECT: &str = "C000P003 - Cubewise - Timesheet Development";

    /// An approved `Event` for the guard fixtures: `project`/`category`
    /// work from `start` to `end`, stamped `approved_at` (None = absent).
    fn approved(
        start: &str,
        end: &str,
        approved_at: Option<&str>,
        project: &str,
        category: &str,
        task: &str,
    ) -> Event {
        Event {
            bucket: BUCKET_TIMESHEET_APPROVED.to_string(),
            timestamp: start.to_string(),
            duration_s: 0.0,
            data: serde_json::json!({
                "start": start,
                "end": end,
                "project_guess": project,
                "category": category,
                "task": task,
                "comment": "",
                "approved_at": approved_at,
            }),
            device: "laptop".to_string(),
        }
    }

    #[test]
    fn duplicate_overlapping_approvals_keep_earliest_stamp_and_warn() {
        let vocab = CubewiseVocab::embedded().clone();
        let events = vec![
            approved(
                "2026-10-06T09:00:00+08:00",
                "2026-10-06T10:00:00+08:00",
                Some("2026-10-06T11:00:00.000Z"),
                GUARD_PROJECT,
                "Consulting",
                "later copy",
            ),
            approved(
                "2026-10-06T09:00:00+08:00",
                "2026-10-06T10:00:00+08:00",
                Some("2026-10-06T10:00:00.000Z"),
                GUARD_PROJECT,
                "Consulting",
                "earlier copy",
            ),
        ];
        let outcome = build_month_grid(2026, 10, &events, &vocab);
        assert_eq!(
            outcome.rows.len(),
            1,
            "the duplicated span must upload exactly once"
        );
        assert_eq!(
            outcome.rows[0][4],
            ExportCell::Text("earlier copy".to_string())
        );
        assert_eq!(outcome.rows[0][6], ExportCell::Number(1.0));
        assert_eq!(outcome.warnings.len(), 1);
        let warning = &outcome.warnings[0];
        assert!(
            warning.contains("duplicate row at 2026-10-06T09:00:00+08:00"),
            "{warning}"
        );
        assert!(warning.contains(GUARD_PROJECT), "{warning}");
        assert!(
            warning.contains("2026-10-06T10:00:00+00:00"),
            "kept stamp named in the warning: {warning}"
        );
    }

    #[test]
    fn same_span_on_different_projects_is_not_a_duplicate() {
        let vocab = CubewiseVocab::embedded().clone();
        let events = vec![
            approved(
                "2026-10-06T09:00:00+08:00",
                "2026-10-06T10:00:00+08:00",
                Some("2026-10-06T10:00:00.000Z"),
                GUARD_PROJECT,
                "Consulting",
                "a",
            ),
            approved(
                "2026-10-06T09:00:00+08:00",
                "2026-10-06T10:00:00+08:00",
                Some("2026-10-06T10:00:00.000Z"),
                OTHER_PROJECT,
                "Consulting",
                "b",
            ),
        ];
        let outcome = build_month_grid(2026, 10, &events, &vocab);
        assert_eq!(outcome.rows.len(), 2);
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn touching_spans_do_not_overlap() {
        let vocab = CubewiseVocab::embedded().clone();
        let events = vec![
            approved(
                "2026-10-06T09:00:00+08:00",
                "2026-10-06T10:00:00+08:00",
                Some("2026-10-06T10:00:00.000Z"),
                GUARD_PROJECT,
                "Consulting",
                "morning",
            ),
            approved(
                "2026-10-06T10:00:00+08:00",
                "2026-10-06T11:00:00+08:00",
                Some("2026-10-06T10:00:00.000Z"),
                GUARD_PROJECT,
                "Consulting",
                "follow-on",
            ),
        ];
        let outcome = build_month_grid(2026, 10, &events, &vocab);
        assert_eq!(outcome.rows.len(), 2);
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn missing_approved_at_loses_to_known_stamp() {
        let vocab = CubewiseVocab::embedded().clone();
        let events = vec![
            approved(
                "2026-10-06T09:00:00+08:00",
                "2026-10-06T10:00:00+08:00",
                None,
                GUARD_PROJECT,
                "Consulting",
                "unstamped",
            ),
            approved(
                "2026-10-06T09:00:00+08:00",
                "2026-10-06T10:00:00+08:00",
                Some("2026-10-06T10:00:00.000Z"),
                GUARD_PROJECT,
                "Consulting",
                "stamped",
            ),
        ];
        let outcome = build_month_grid(2026, 10, &events, &vocab);
        assert_eq!(outcome.rows.len(), 1);
        assert_eq!(outcome.rows[0][4], ExportCell::Text("stamped".to_string()));
        assert_eq!(outcome.warnings.len(), 1);
        assert!(
            outcome.warnings[0].contains("unknown"),
            "dropped unstamped row reported as unknown: {}",
            outcome.warnings[0]
        );
    }

    #[test]
    fn overlap_uses_unrounded_true_spans() {
        // True spans 09:00-09:40 and 09:42-10:30 do not overlap, but the
        // quarter-hour-rounded hours (0.75 h from 09:00 -> 09:45) would
        // swallow the 09:42 start: the guard must not drop real work.
        let vocab = CubewiseVocab::embedded().clone();
        let events = vec![
            approved(
                "2026-10-06T09:00:00+08:00",
                "2026-10-06T09:40:00+08:00",
                Some("2026-10-06T10:00:00.000Z"),
                GUARD_PROJECT,
                "Consulting",
                "first",
            ),
            approved(
                "2026-10-06T09:42:00+08:00",
                "2026-10-06T10:30:00+08:00",
                Some("2026-10-06T10:00:00.000Z"),
                GUARD_PROJECT,
                "Consulting",
                "second",
            ),
        ];
        let outcome = build_month_grid(2026, 10, &events, &vocab);
        assert_eq!(outcome.rows.len(), 2);
        assert!(outcome.warnings.is_empty());
    }
}
