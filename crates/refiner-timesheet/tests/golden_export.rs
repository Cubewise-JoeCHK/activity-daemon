//! Golden-file test for the month-end Cubewise xlsx export (issue #46):
//! fixture approved activities -> exact expected cell grid, read back from
//! the produced workbook bytes with calamine. Covers the upload grid
//! (order, zero-padded line items, billable split, quarter rounding,
//! comment fallback), the Sheet1 vocabulary mirror at the template's
//! reference positions, the Warnings sheet for refused rows, and the
//! headers-only empty month.

use std::io::Cursor;

use activity_core::Event;
use calamine::{Data, Range, Reader};
use refiner_timesheet::{
    build_month_grid, export_month_bytes, parse_month, write_workbook, CubewiseVocab, ExportCell,
    BUCKET_TIMESHEET_APPROVED, UPLOAD_HEADERS,
};
use serde_json::json;

const FRASERS: &str = "A143P010 - Frasers - H2 FH - BU Core";
const AMWAY_BTP: &str = "A469P001 - Amway - BTP Phase 1";
const AMWAY_CR: &str = "A469P002 - Amway - CR- 2026";
const KPMG: &str = "A289P001 - KPMG - KPMG Global Services Private Limited";
const TIMEDEV: &str = "C000P003 - Cubewise - Timesheet Development";

/// An approved activity event; `ts_start` = activity start per the draft
/// compiler's insert convention (issue #45).
fn approved(timestamp: &str, duration_s: f64, data: serde_json::Value) -> Event {
    Event {
        bucket: BUCKET_TIMESHEET_APPROVED.to_string(),
        timestamp: timestamp.to_string(),
        duration_s,
        data,
        device: "owner-laptop".to_string(),
    }
}

/// Text cell as Option: xlsx empty-string cells may come back as
/// `Data::Empty`; anything else non-text is a failure.
fn cell_str(range: &Range<Data>, row: u32, col: u32) -> Option<String> {
    match range.get_value((row, col)) {
        None | Some(Data::Empty) => None,
        Some(Data::String(s)) => Some(s.clone()),
        Some(other) => panic!("expected text at ({row},{col}), got {other:?}"),
    }
}

fn cell_f64(range: &Range<Data>, row: u32, col: u32) -> f64 {
    match range.get_value((row, col)) {
        Some(Data::Float(f)) => *f,
        Some(other) => panic!("expected number at ({row},{col}), got {other:?}"),
        None => panic!("missing number at ({row},{col})"),
    }
}

fn fixture_events() -> Vec<Event> {
    vec![
        // Other month: silently skipped, no warning.
        approved(
            "2026-11-01T09:00:00+08:00",
            3600.0,
            json!({
                "start": "2026-11-01T09:00:00+08:00",
                "end": "2026-11-01T10:00:00+08:00",
                "project_guess": FRASERS,
                "category": "Consulting",
                "task": "next month",
                "comment": "",
            }),
        ),
        // Day 2, inserted first to prove the day-then-start ordering.
        approved(
            "2026-10-02T08:30:00+08:00",
            14400.0,
            json!({
                "start": "2026-10-02T08:30:00+08:00",
                "end": "2026-10-02T12:30:00+08:00",
                "project_guess": KPMG,
                "category": "Support",
                "task": "KPMG GS day",
                "comment": "",
                "confidence": 0.95,
                "sources": ["a1b2c3d4e5f60718"],
            }),
        ),
        // Day 1 line 0002: internal C000P* code -> NonBillable.
        approved(
            "2026-10-01T13:00:00+08:00",
            3600.0,
            json!({
                "start": "2026-10-01T13:00:00+08:00",
                "end": "2026-10-01T14:00:00+08:00",
                "project_guess": TIMEDEV,
                "category": "Management",
                "task": "Timesheet pipeline",
                "comment": "Owner note here",
            }),
        ),
        // Day 1 line 0001: empty comment falls back to the evidence summary.
        approved(
            "2026-10-01T09:00:00+08:00",
            8100.0,
            json!({
                "start": "2026-10-01T09:00:00+08:00",
                "end": "2026-10-01T11:15:00+08:00",
                "project_guess": FRASERS,
                "category": "Consulting",
                "task": "Frasers H2 BU Core build",
                "comment": "",
                "evidence_summary": "Reviewed frame evidence for the morning build",
            }),
        ),
        // Day 1 line 0003: 1h10m rounds to 1.25 h; `project` alias accepted;
        // missing task/comment -> empty cells.
        approved(
            "2026-10-01T15:00:00+08:00",
            4200.0,
            json!({
                "start": "2026-10-01T15:00:00+08:00",
                "end": "2026-10-01T16:10:00+08:00",
                "project": AMWAY_BTP,
                "category": "Support",
                "confidence": 0.6,
            }),
        ),
        // Refusals, in input order: unknown project, unknown category,
        // unparseable start, zero duration.
        approved(
            "2026-10-01T10:00:00+08:00",
            3600.0,
            json!({
                "start": "2026-10-01T10:00:00+08:00",
                "end": "2026-10-01T11:00:00+08:00",
                "project_guess": "Z999P999 - Mystery - Not In Vocabulary",
                "category": "Consulting",
            }),
        ),
        approved(
            "2026-10-01T14:30:00+08:00",
            1800.0,
            json!({
                "start": "2026-10-01T14:30:00+08:00",
                "end": "2026-10-01T15:00:00+08:00",
                "project_guess": AMWAY_CR,
                "category": "Cooking",
            }),
        ),
        approved(
            "not-a-date",
            3600.0,
            json!({
                "start": "not-a-date",
                "end": "2026-10-01T12:00:00+08:00",
                "project_guess": FRASERS,
                "category": "Consulting",
            }),
        ),
        approved(
            "2026-10-01T18:00:00+08:00",
            0.0,
            json!({
                "start": "2026-10-01T18:00:00+08:00",
                "end": "2026-10-01T18:00:00+08:00",
                "project_guess": TIMEDEV,
                "category": "QA",
            }),
        ),
    ]
}

fn golden_rows() -> Vec<Vec<ExportCell>> {
    vec![
        vec![
            ExportCell::Text(FRASERS.to_string()),
            ExportCell::Text("2026-10-01".to_string()),
            ExportCell::Text("0001".to_string()),
            ExportCell::Text("Consulting".to_string()),
            ExportCell::Text("Frasers H2 BU Core build".to_string()),
            ExportCell::Text("Reviewed frame evidence for the morning build".to_string()),
            ExportCell::Number(2.25),
            ExportCell::Number(0.0),
        ],
        vec![
            ExportCell::Text(TIMEDEV.to_string()),
            ExportCell::Text("2026-10-01".to_string()),
            ExportCell::Text("0002".to_string()),
            ExportCell::Text("Management".to_string()),
            ExportCell::Text("Timesheet pipeline".to_string()),
            ExportCell::Text("Owner note here".to_string()),
            ExportCell::Number(0.0),
            ExportCell::Number(1.0),
        ],
        vec![
            ExportCell::Text(AMWAY_BTP.to_string()),
            ExportCell::Text("2026-10-01".to_string()),
            ExportCell::Text("0003".to_string()),
            ExportCell::Text("Support".to_string()),
            ExportCell::Text(String::new()),
            ExportCell::Text(String::new()),
            ExportCell::Number(1.25),
            ExportCell::Number(0.0),
        ],
        vec![
            ExportCell::Text(KPMG.to_string()),
            ExportCell::Text("2026-10-02".to_string()),
            ExportCell::Text("0001".to_string()),
            ExportCell::Text("Support".to_string()),
            ExportCell::Text("KPMG GS day".to_string()),
            ExportCell::Text(String::new()),
            ExportCell::Number(4.0),
            ExportCell::Number(0.0),
        ],
    ]
}

fn open_book(bytes: Vec<u8>) -> calamine::Xlsx<Cursor<Vec<u8>>> {
    calamine::open_workbook_from_rs(Cursor::new(bytes)).expect("workbook opens")
}

#[test]
fn golden_month_export_exact_grid() {
    let (year, month) = parse_month("2026-10").expect("fixture month");
    let events = fixture_events();
    let vocab = CubewiseVocab::load(None).expect("embedded vocab");

    let outcome = build_month_grid(year, month, &events, &vocab);

    // 4 upload rows from 9 fixture events; refusals reported, never silent.
    assert_eq!(outcome.rows, golden_rows(), "upload grid mismatch");
    assert_eq!(
        outcome.warnings.len(),
        4,
        "warnings: {:#?}",
        outcome.warnings
    );
    assert!(outcome.warnings[0].contains("Z999P999 - Mystery - Not In Vocabulary"));
    assert!(outcome.warnings[0].contains("not in the Cubewise vocabulary"));
    assert!(outcome.warnings[1].contains("'Cooking'"));
    assert!(outcome.warnings[2].contains("not RFC3339"));
    assert!(outcome.warnings[3].contains("rounds to 0.00 h"));
    assert!(outcome.warnings.iter().all(|w| w.starts_with("refused")));

    let bytes = write_workbook(&outcome, &vocab).expect("workbook");
    let mut book = open_book(bytes);

    // Upload sheet: header row byte-exact, then the golden grid.
    let upload = book.worksheet_range("upload").expect("upload sheet");
    let headers: Vec<String> = (0..UPLOAD_HEADERS.len() as u32)
        .map(|col| cell_str(&upload, 0, col).expect("header cell"))
        .collect();
    assert_eq!(headers, UPLOAD_HEADERS.to_vec());
    assert_eq!(upload.height(), 5, "4 data rows + header");
    for (row, expected) in golden_rows().iter().enumerate() {
        let row = row as u32 + 1;
        for (col, cell) in expected.iter().enumerate() {
            match cell {
                ExportCell::Text(text) => {
                    assert_eq!(
                        cell_str(&upload, row, col as u32).as_deref(),
                        if text.is_empty() {
                            None
                        } else {
                            Some(text.as_str())
                        },
                        "cell ({row},{col})"
                    );
                }
                ExportCell::Number(value) => {
                    assert_eq!(
                        cell_f64(&upload, row, col as u32),
                        *value,
                        "cell ({row},{col})"
                    );
                }
            }
        }
    }

    // Sheet1 mirrors the template's vocabulary reference positions:
    // categories A2..A8, projects C2..C190.
    let sheet1 = book.worksheet_range("Sheet1").expect("Sheet1 sheet");
    assert_eq!(cell_str(&sheet1, 1, 0).as_deref(), Some("Consulting"));
    assert_eq!(
        cell_str(&sheet1, 7, 0).as_deref(),
        Some("Customer Returned")
    );
    let first_project = vocab.projects.first().expect("vocab not empty");
    let last_project = vocab.projects.last().expect("vocab not empty");
    assert_eq!(
        cell_str(&sheet1, 1, 2).as_deref(),
        Some(first_project.as_str())
    );
    assert_eq!(
        cell_str(&sheet1, 189, 2).as_deref(),
        Some(last_project.as_str())
    );
    assert_eq!(cell_str(&sheet1, 190, 2), None, "exactly 189 project rows");
    assert_eq!(
        cell_str(&sheet1, 1, 1),
        None,
        "column B stays empty like the template"
    );

    // Warnings sheet: header + one row per refusal, input order.
    let warnings = book.worksheet_range("Warnings").expect("warnings sheet");
    assert_eq!(warnings.height(), 5);
    assert_eq!(cell_str(&warnings, 0, 0).as_deref(), Some("Warnings"));
    for (n, warning) in outcome.warnings.iter().enumerate() {
        assert_eq!(
            cell_str(&warnings, 1 + n as u32, 0).as_deref(),
            Some(warning.as_str())
        );
    }
}

#[test]
fn empty_month_exports_headers_only() {
    let (bytes, warnings) = export_month_bytes(&[], 2026, 9, None).expect("empty export");
    assert_eq!(warnings, 0);
    let mut book = open_book(bytes);
    let upload = book.worksheet_range("upload").expect("upload sheet");
    assert_eq!(upload.height(), 1, "headers only");
    assert_eq!(cell_str(&upload, 0, 0).as_deref(), Some("Project"));
    assert_eq!(cell_str(&upload, 0, 7).as_deref(), Some("NonBillable"));
    assert!(
        book.worksheet_range("Warnings").is_err(),
        "no Warnings sheet when nothing was refused"
    );
}

#[test]
fn unknown_code_pattern_is_refused_not_guessed() {
    // A hand-edited vocabulary with a code the template contract has no
    // billable rule for: the row must be refused, not silently zeroed.
    let vocab = CubewiseVocab::parse_toml(
        "categories = [\"Consulting\"]\nprojects = [\"D001P001 - Mystery - Off Contract\"]\n",
    )
    .expect("custom vocab");
    let events = vec![approved(
        "2026-10-01T09:00:00+08:00",
        3600.0,
        json!({
            "start": "2026-10-01T09:00:00+08:00",
            "end": "2026-10-01T10:00:00+08:00",
            "project_guess": "D001P001 - Mystery - Off Contract",
            "category": "Consulting",
        }),
    )];
    let outcome = build_month_grid(2026, 10, &events, &vocab);
    assert!(outcome.rows.is_empty());
    assert_eq!(outcome.warnings.len(), 1);
    assert!(outcome.warnings[0].contains("no Billable/NonBillable rule"));
}
