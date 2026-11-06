//! Integration tests for the owner review loop (issue #47):
//! `GET /api/timesheet/vocab`, `GET /api/timesheet/days`,
//! `PUT /api/timesheet/draft/{id}`, `POST /api/timesheet/approve` and
//! `GET /api/timesheet/frame/{sha}?date=`, following the
//! `timesheet_draft.rs` oneshot pattern. Drafts are produced the real way:
//! seed window sessions + frame analyses through `/ingest/events`, then
//! compile with `POST /api/timesheet/draft`.
//!
//! Timesheet config pins `use_llm_titles = false` so compiles stay
//! hermetic. The full loop ends in the month-end export: an edited,
//! approved draft must appear in the workbook with the owner's values.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use activity_core::EventEnvelope;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use calamine::Reader;
use chrono::{DateTime, Utc};
use refiner_timesheet::TimesheetConfig;
use serde_json::{json, Value};
use server::{build_router, AppState, AuthConfig};
use tower::ServiceExt;

/// Self-cleaning temp directory (the db and media root live in here).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("activity-server-ts-review-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_state(tag: &str) -> (TempDir, AppState) {
    test_state_with_vocab(tag, None)
}

/// `vocab_toml` overrides `[timesheet] vocab_path` (vocabulary override
/// tests).
fn test_state_with_vocab(tag: &str, vocab_toml: Option<&str>) -> (TempDir, AppState) {
    let dir = TempDir::new(tag);
    let vocab_path = vocab_toml.map(|text| {
        let path = dir.0.join("custom_vocab.toml");
        std::fs::write(&path, text).expect("write vocab");
        path
    });
    // Heuristic titling: no Lemonade in CI, no network in tests.
    let timesheet = TimesheetConfig {
        use_llm_titles: false,
        vocab_path,
        ..TimesheetConfig::default()
    };
    let state = AppState::open_with_retention(
        &dir.0.join("activity.db"),
        dir.0.join("media"),
        AuthConfig::disabled(),
        server::retention::RetentionConfig::default(),
        timesheet,
    )
    .expect("test app state");
    (dir, state)
}

async fn body_json(res: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json body")
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

fn post_json(uri: &str, value: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .expect("request")
}

fn put_json(uri: &str, value: &Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .expect("request")
}

async fn run(app: &axum::Router, request: Request<Body>) -> axum::response::Response {
    app.clone().oneshot(request).await.expect("oneshot")
}

/// Seed device events through the real ingest path.
async fn seed_events(app: &axum::Router, device: &str, events: Vec<EventEnvelope>) {
    let res = run(
        app,
        post_json(
            "/ingest/events",
            &json!({ "device": device, "events": events }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "ingest must accept");
}

fn session(hour: u32, dur_s: f64, app_name: &str) -> EventEnvelope {
    EventEnvelope {
        bucket: "window-watcher".to_string(),
        timestamp: format!("2026-11-06T{hour:02}:00:00+08:00"),
        duration_s: dur_s,
        data: json!({ "app": app_name, "title": "window title" }),
    }
}

fn analysis(
    hour: u32,
    minute: u32,
    project: &str,
    category: &str,
    confidence: f64,
) -> EventEnvelope {
    analysis_with_sha(
        hour,
        minute,
        project,
        category,
        confidence,
        &format!(
            "sha-{hour:02}{minute:02}{:04}",
            (confidence * 10000.0) as u32
        ),
    )
}

fn analysis_with_sha(
    hour: u32,
    minute: u32,
    project: &str,
    category: &str,
    confidence: f64,
    sha16: &str,
) -> EventEnvelope {
    EventEnvelope {
        bucket: "frame-analysis".to_string(),
        timestamp: format!("2026-11-06T{hour:02}:{minute:02}:00+08:00"),
        duration_s: 0.0,
        data: json!({
            "frame_sha16": sha16,
            "frame_path": format!("media/screen/2026/11/06/f{hour:02}{minute:02}.jpg"),
            "activity": format!("working on {project}"),
            "project_guess": project,
            "category": category,
            "confidence": confidence,
            "evidence": [],
            "model": "qwen",
            "sampled": true,
        }),
    }
}

/// Compile the fixture day (three 1 h blocks with distinct projects, so
/// nothing merges) and return the current drafts (each carrying `id`).
async fn compile_three_drafts(app: &axum::Router) -> Vec<Value> {
    seed_events(
        app,
        "windows-client",
        vec![
            session(9, 3600.0, "Code.exe"),
            session(11, 3600.0, "Excel.exe"),
            session(13, 3600.0, "Teams.exe"),
            analysis(
                9,
                30,
                "C000P003 - Cubewise - Timesheet Development",
                "Management",
                0.9,
            ),
            analysis(
                11,
                30,
                "C000P004 - Cubewise - Internal Tools",
                "Management",
                0.85,
            ),
            analysis(
                13,
                30,
                "C000P005 - Cubewise - Training Material",
                "Training",
                0.8,
            ),
        ],
    )
    .await;
    let res = run(
        app,
        post_json("/api/timesheet/draft?date=2026-11-06", &json!({})),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["status"]["count"], 3, "three separate drafts");
    v["drafts"].as_array().expect("drafts").to_vec()
}

fn edit_body(project: &str, category: &str, task: &str, comment: &str) -> Value {
    json!({
        "project_guess": project,
        "category": category,
        "task": task,
        "comment": comment,
    })
}

async fn days_payload(app: &axum::Router, uri: &str) -> Value {
    let res = run(app, get(uri)).await;
    assert_eq!(res.status(), StatusCode::OK, "{uri}");
    body_json(res).await
}

// ------------------------------------------------------------- vocab

#[tokio::test]
async fn vocab_serves_the_embedded_template_vocabulary() {
    let (_dir, state) = test_state("vocab");
    let app = build_router(state);
    let res = run(&app, get("/api/timesheet/vocab")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    let categories = v["categories"].as_array().expect("categories");
    let projects = v["projects"].as_array().expect("projects");
    assert_eq!(categories.len(), 7, "the 7 template categories");
    assert_eq!(projects.len(), 189, "the 189 template projects");
    assert_eq!(categories[0], "Consulting");
    assert_eq!(categories[6], "Customer Returned");
    // exact template string, ready for the export round-trip
    assert!(projects
        .iter()
        .any(|p| p == "A143P010 - Frasers - H2 FH - BU Core"));
}

#[tokio::test]
async fn vocab_uses_the_configured_vocab_path() {
    let (_dir, state) = test_state_with_vocab(
        "vocab-override",
        Some("categories = [\"Consulting\"]\nprojects = [\"A100P001 - Test - Project\"]\n"),
    );
    let app = build_router(state);
    let res = run(&app, get("/api/timesheet/vocab")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(
        v["categories"].as_array().expect("categories").len(),
        1,
        "override vocabulary replaces the embedded seed"
    );
    assert_eq!(v["projects"][0], "A100P001 - Test - Project");
}

// ------------------------------------------------------------- draft edit

#[tokio::test]
async fn edit_persists_to_the_draft_and_marks_it_reviewed() {
    let (_dir, state) = test_state("edit");
    let app = build_router(state);
    let drafts = compile_three_drafts(&app).await;
    let id = drafts[0]["id"].as_i64().expect("draft id");

    let res = run(
        &app,
        put_json(
            &format!("/api/timesheet/draft/{id}"),
            &edit_body(
                "A143P010 - Frasers - H2 FH - BU Core",
                "Consulting",
                "Client cube refactor",
                "owner note",
            ),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["project_guess"], "A143P010 - Frasers - H2 FH - BU Core");
    assert_eq!(v["category"], "Consulting");
    assert_eq!(v["task"], "Client cube refactor");
    assert_eq!(v["comment"], "owner note");
    assert_eq!(v["reviewed"], true);
    assert!(v["reviewed_at"].as_str().is_some());
    // compiler facts untouched
    assert_eq!(v["start"], drafts[0]["start"]);
    assert_eq!(v["confidence"], drafts[0]["confidence"]);

    // GET reflects the edit, the day list counts the row as reviewed.
    let res = run(&app, get("/api/timesheet/draft?date=2026-11-06")).await;
    let v = body_json(res).await;
    let drafts = v["drafts"].as_array().expect("drafts");
    let edited = drafts.iter().find(|d| d["id"] == id).expect("edited draft");
    assert_eq!(edited["task"], "Client cube refactor");
    assert_eq!(edited["reviewed"], true);

    let v = days_payload(&app, "/api/timesheet/days?from=2026-11-06&to=2026-11-06").await;
    assert_eq!(v["days"][0]["day"], "2026-11-06");
    assert_eq!(v["days"][0]["draft"], 2, "untouched rows stay draft");
    assert_eq!(v["days"][0]["reviewed"], 1);
    assert_eq!(v["days"][0]["approved"], 0);
}

#[tokio::test]
async fn edit_rejects_empty_fields_unknown_ids_superseded_and_approved() {
    let (_dir, state) = test_state("edit-guard");
    let app = build_router(state);
    let drafts = compile_three_drafts(&app).await;
    let id = drafts[0]["id"].as_i64().expect("draft id");

    // empty required fields
    for body in [
        edit_body("", "Consulting", "task", ""),
        edit_body("P", "", "task", ""),
        edit_body("P", "Consulting", "  ", ""),
    ] {
        let res = run(&app, put_json(&format!("/api/timesheet/draft/{id}"), &body)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    // unknown id
    let res = run(
        &app,
        put_json(
            "/api/timesheet/draft/99999",
            &edit_body("P", "Consulting", "t", ""),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // superseded: regenerating marks the old draft superseded
    let res = run(
        &app,
        post_json("/api/timesheet/draft?date=2026-11-06", &json!({})),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = run(
        &app,
        put_json(
            &format!("/api/timesheet/draft/{id}"),
            &edit_body("P", "Consulting", "t", ""),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);

    // approved: the current draft cannot be edited after approval
    let current = body_json(run(&app, get("/api/timesheet/draft?date=2026-11-06")).await).await;
    let current_id = current["drafts"][0]["id"].as_i64().expect("current id");
    let res = run(
        &app,
        post_json(
            "/api/timesheet/approve",
            &json!({ "date": "2026-11-06", "draft_id": current_id }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = run(
        &app,
        put_json(
            &format!("/api/timesheet/draft/{current_id}"),
            &edit_body("P", "Consulting", "t", ""),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);
}

// ------------------------------------------------------------- approve

#[tokio::test]
async fn approve_single_writes_the_approved_event_and_marks_the_draft() {
    let (_dir, state) = test_state("approve-single");
    let app = build_router(state.clone());
    let drafts = compile_three_drafts(&app).await;
    let first = &drafts[0];
    let id = first["id"].as_i64().expect("draft id");

    let res = run(
        &app,
        post_json(
            "/api/timesheet/approve",
            &json!({ "date": "2026-11-06", "draft_id": id }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["approved_ids"], json!([id]));
    assert_eq!(v["already_approved_ids"], json!([]));

    // the approved bucket carries the draft JSON verbatim + approved_at,
    // same timestamp/duration/device as the draft row (ADR addendum #16)
    let approved = state
        .db_handle()
        .lock()
        .expect("db lock")
        .query_events("timesheet-approved", None, None)
        .expect("query approved");
    assert_eq!(approved.len(), 1, "exactly one approved event");
    let ev = &approved[0];
    assert_eq!(ev.timestamp, first["start"].as_str().expect("start"));
    assert_eq!(ev.device, "windows-client");
    let duration = ev.duration_s;
    let expected_s = 3600.0;
    assert!(
        (duration - expected_s).abs() < 1e-6,
        "duration mirrors the draft row: {duration}"
    );
    let approved_at = ev.data["approved_at"].as_str().expect("approved_at");
    let parsed: DateTime<Utc> = DateTime::parse_from_rfc3339(approved_at)
        .expect("RFC3339 approved_at")
        .with_timezone(&Utc);
    assert!(parsed <= Utc::now(), "approved_at is a past stamp");
    assert_eq!(ev.data["start"], first["start"]);
    assert_eq!(ev.data["project_guess"], first["project_guess"]);
    assert_eq!(ev.data["task"], first["task"]);
    assert_eq!(ev.data["sources"], first["sources"]);
    assert!(
        ev.data.get("approved").is_none(),
        "no flag inside the event"
    );

    // the source draft is flagged approved (GET shows it)
    let current = body_json(run(&app, get("/api/timesheet/draft?date=2026-11-06")).await).await;
    let row = current["drafts"]
        .as_array()
        .expect("drafts")
        .iter()
        .find(|d| d["id"] == id)
        .expect("approved draft still listed");
    assert_eq!(row["approved"], true);
    assert!(row["approved_at"].as_str().is_some());
    assert!(row["approved_event_id"].as_i64().is_some());
    // the other two drafts are untouched
    assert_eq!(current["status"]["count"], 3);

    // the day list counts one approved
    let v = days_payload(&app, "/api/timesheet/days?from=2026-11-06&to=2026-11-06").await;
    assert_eq!(v["days"][0]["approved"], 1);
    assert_eq!(v["days"][0]["draft"], 2);
}

#[tokio::test]
async fn approve_all_approves_remaining_and_is_idempotent() {
    let (_dir, state) = test_state("approve-all");
    let app = build_router(state.clone());
    compile_three_drafts(&app).await;

    let res = run(
        &app,
        post_json("/api/timesheet/approve", &json!({ "date": "2026-11-06" })),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["approved"], 3);
    assert_eq!(
        v["approved_ids"].as_array().expect("ids").len(),
        3,
        "whole-day approve covers every unapproved draft"
    );

    // second approve-all: everything is already approved, nothing changes
    let res = run(
        &app,
        post_json("/api/timesheet/approve", &json!({ "date": "2026-11-06" })),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["approved"], 0);
    assert_eq!(
        v["already_approved_ids"].as_array().expect("ids").len(),
        3,
        "idempotent re-approve"
    );

    // re-approving a single approved draft by id is also an idempotent no-op
    let current = body_json(run(&app, get("/api/timesheet/draft?date=2026-11-06")).await).await;
    let id = current["drafts"][0]["id"].as_i64().expect("id");
    let res = run(
        &app,
        post_json(
            "/api/timesheet/approve",
            &json!({ "date": "2026-11-06", "draft_id": id }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["approved"], 0);
    assert_eq!(v["already_approved_ids"], json!([id]));

    // exactly three approved events exist despite the retries
    let approved = state
        .db_handle()
        .lock()
        .expect("db lock")
        .query_events("timesheet-approved", None, None)
        .expect("query approved");
    assert_eq!(approved.len(), 3);
}

#[tokio::test]
async fn approve_rejects_bad_dates_and_stale_or_unknown_ids() {
    let (_dir, state) = test_state("approve-guard");
    let app = build_router(state.clone());
    let drafts = compile_three_drafts(&app).await;
    let id = drafts[0]["id"].as_i64().expect("draft id");

    // non-canonical date
    for date in ["2026-11-6", "not-a-day", ""] {
        let res = run(
            &app,
            post_json("/api/timesheet/approve", &json!({ "date": date })),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{date}");
    }

    // unknown draft id on the day
    let res = run(
        &app,
        post_json(
            "/api/timesheet/approve",
            &json!({ "date": "2026-11-06", "draft_id": 424242 }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // superseded draft: regenerate, then approve the stale id - approve
    // searches current drafts only, so the stale id is simply not found
    // (nothing stale is ever approved)
    let res = run(
        &app,
        post_json("/api/timesheet/draft?date=2026-11-06", &json!({})),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = run(
        &app,
        post_json(
            "/api/timesheet/approve",
            &json!({ "date": "2026-11-06", "draft_id": id }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    // nothing was approved by the rejected calls
    let approved = state
        .db_handle()
        .lock()
        .expect("db lock")
        .query_events("timesheet-approved", None, None)
        .expect("query approved");
    assert!(approved.is_empty());
}

// ------------------------------------------------------------- day list

#[tokio::test]
async fn days_counts_states_across_a_month_range() {
    let (_dir, state) = test_state("days-range");
    let app = build_router(state);
    let drafts = compile_three_drafts(&app).await;

    // edit draft 0, approve draft 1
    let id0 = drafts[0]["id"].as_i64().expect("id0");
    let id1 = drafts[1]["id"].as_i64().expect("id1");
    let res = run(
        &app,
        put_json(
            &format!("/api/timesheet/draft/{id0}"),
            &edit_body("P", "Consulting", "t", ""),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = run(
        &app,
        post_json(
            "/api/timesheet/approve",
            &json!({ "date": "2026-11-06", "draft_id": id1 }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);

    // a month range around the fixture day; days without drafts are absent
    let v = days_payload(&app, "/api/timesheet/days?from=2026-11-01&to=2026-11-30").await;
    let days = v["days"].as_array().expect("days");
    assert_eq!(days.len(), 1, "only days with current drafts appear");
    assert_eq!(days[0]["day"], "2026-11-06");
    assert_eq!(days[0]["draft"], 1);
    assert_eq!(days[0]["reviewed"], 1);
    assert_eq!(days[0]["approved"], 1);

    // validation: reversed range, non-canonical dates, oversized range
    for uri in [
        "/api/timesheet/days?from=2026-11-06&to=2026-11-05",
        "/api/timesheet/days?from=2026-11-6&to=2026-11-30",
        "/api/timesheet/days?from=2026-01-01&to=2026-04-01",
        "/api/timesheet/days?from=2026-11-01",
        "/api/timesheet/days",
    ] {
        let res = run(&app, get(uri)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{uri}");
    }
}

// ------------------------------------------------------------- frame endpoint

#[tokio::test]
async fn frame_endpoint_serves_the_analysed_frame_scoped_to_its_day() {
    let (dir, state) = test_state("frame");
    let app = build_router(state);
    let sha = "0123456789abcdef";
    let ghost_sha = "fedcba9876543210";
    seed_events(
        &app,
        "windows-client",
        vec![
            session(9, 3600.0, "Code.exe"),
            analysis_with_sha(
                9,
                30,
                "C000P003 - Cubewise - Timesheet Development",
                "Management",
                0.9,
                sha,
            ),
            analysis_with_sha(
                10,
                30,
                "C000P003 - Cubewise - Timesheet Development",
                "Management",
                0.9,
                ghost_sha,
            ),
        ],
    )
    .await;

    // the analysed frame exists on disk for `sha`, not for `ghost_sha`
    let frame_path = dir.0.join("media/screen/2026/11/06/f0930.jpg");
    std::fs::create_dir_all(frame_path.parent().expect("parent")).expect("mkdir");
    let jpeg = vec![0xFFu8, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
    std::fs::write(&frame_path, &jpeg).expect("write frame");

    let res = run(
        &app,
        get(&format!("/api/timesheet/frame/{sha}?date=2026-11-06")),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .expect("content type"),
        "image/jpeg"
    );
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    assert_eq!(bytes, jpeg, "served bytes are the stored frame");

    // unknown sha on the day / known sha but missing file / bad sha shape /
    // wrong date
    let res = run(
        &app,
        get("/api/timesheet/frame/0000000000000000?date=2026-11-06"),
    )
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = run(
        &app,
        get(&format!("/api/timesheet/frame/{ghost_sha}?date=2026-11-06")),
    )
    .await;
    assert_eq!(
        res.status(),
        StatusCode::NOT_FOUND,
        "analysis exists, file missing"
    );
    for bad in ["short", "gggggggggggggggg", "0123456789abcdef0"] {
        let res = run(
            &app,
            get(&format!("/api/timesheet/frame/{bad}?date=2026-11-06")),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    let res = run(
        &app,
        get(&format!("/api/timesheet/frame/{sha}?date=2026-11-07")),
    )
    .await;
    assert_eq!(
        res.status(),
        StatusCode::NOT_FOUND,
        "wrong day has no such analysis"
    );
}

// ------------------------------------------------------------- full loop

#[tokio::test]
async fn full_loop_edit_approve_export_roundtrip() {
    let (_dir, state) = test_state("full-loop");
    let app = build_router(state);

    // one merged 2 h draft (two adjacent same-project sessions)
    seed_events(
        &app,
        "windows-client",
        vec![
            session(9, 3600.0, "Code.exe"),
            session(10, 3600.0, "Code.exe"),
            analysis(
                9,
                30,
                "C000P003 - Cubewise - Timesheet Development",
                "Management",
                0.9,
            ),
            analysis(
                10,
                30,
                "C000P003 - Cubewise - Timesheet Development",
                "Management",
                0.85,
            ),
        ],
    )
    .await;
    let res = run(
        &app,
        post_json("/api/timesheet/draft?date=2026-11-06", &json!({})),
    )
    .await;
    let v = body_json(res).await;
    assert_eq!(v["status"]["count"], 1);
    let id = v["drafts"][0]["id"].as_i64().expect("draft id");

    // answer-style question loop is covered elsewhere; here: edit + approve
    let res = run(
        &app,
        put_json(
            &format!("/api/timesheet/draft/{id}"),
            &edit_body(
                "A143P010 - Frasers - H2 FH - BU Core",
                "Consulting",
                "Client cube refactor",
                "owner note",
            ),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = run(
        &app,
        post_json(
            "/api/timesheet/approve",
            &json!({ "date": "2026-11-06", "draft_id": id }),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);

    // the month export carries the approved row with the owner's values
    let res = run(&app, get("/api/timesheet/export?month=2026-11")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    let mut book: calamine::Xlsx<_> =
        calamine::open_workbook_from_rs(std::io::Cursor::new(bytes)).expect("workbook opens");
    let upload = book.worksheet_range("upload").expect("upload sheet");
    assert_eq!(upload.height(), 2, "header + the one approved row");
    assert_eq!(
        upload.get_value((1, 0)),
        Some(&calamine::Data::String(
            "A143P010 - Frasers - H2 FH - BU Core".to_string()
        ))
    );
    assert_eq!(
        upload.get_value((1, 4)),
        Some(&calamine::Data::String("Client cube refactor".to_string()))
    );
    assert_eq!(
        upload.get_value((1, 5)),
        Some(&calamine::Data::String("owner note".to_string()))
    );
    // 2 h, client code -> Billable column
    assert_eq!(upload.get_value((1, 6)), Some(&calamine::Data::Float(2.0)));
    assert!(
        book.worksheet_range("Warnings").is_err(),
        "an in-vocabulary approved row produces no warnings"
    );
}
