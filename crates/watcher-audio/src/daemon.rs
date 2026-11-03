//! The watcher-audio run loop (`daemon`): owns the trigger state machine,
//! the two trigger channels (hotkey + control file), the conferencing
//! detector, the capture worker and the retention sweeper.
//!
//! Blocking loop with a ~500 ms tick; every subsystem failure is logged and
//! absorbed - the daemon must survive flaky devices, broken hotkeys and
//! missing audio hardware (issue #6 robustness contract).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use activity_core::{Config, Db, EventEnvelope};

use crate::control::{poll_control_file, ControlAction, ControlFileOutcome};
use crate::detect::ConferenceWatcher;
use crate::logging::Log;
use crate::machine::{Source, StopReason, Transition, Trigger, TriggerMachine};
use crate::recording::{CaptureCommand, FinishedRecording, StartSpec};
use crate::retention::{sweep, RetentionBudget};

/// Grace period between the conferencing session ending and the auto
/// recording stopping (seconds), default per issue #6.
pub const DEFAULT_GRACE_STOP_S: u64 = 10;
/// Environment override for the grace period (`audio.grace_stop_s` is not
/// part of core's `AudioConfig` yet; reported as a core wish in #6).
pub const ENV_GRACE_STOP_S: &str = "ACTIVITY_AUDIO_GRACE_STOP_S";
/// Environment override for the data dir (matches the CLI's `ACTIVITY_DB`
/// convention of resolving data under the working directory otherwise).
pub const ENV_DATA_DIR: &str = "ACTIVITY_DATA_DIR";
/// Main loop tick.
const TICK_MS: u64 = 500;
/// Conferencing detector cadence (issue #6: 2 s).
const DETECT_INTERVAL: Duration = Duration::from_secs(2);
/// Retention sweep cadence (issue #6: hourly).
const RETENTION_INTERVAL: Duration = Duration::from_secs(3600);
/// Backoff after a failed capture start so a broken device does not cause a
/// hot start/fail loop while a conference is detected.
const CAPTURE_RETRY_BACKOFF: Duration = Duration::from_secs(30);
/// Max wait for capture finalization on shutdown.
const SHUTDOWN_FINALIZE_TIMEOUT: Duration = Duration::from_secs(5);

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Resolve the daemon data dir: `ACTIVITY_DATA_DIR` or the process working
/// directory (consistent with the CLI's default `activity.db` location).
pub fn data_dir() -> PathBuf {
    std::env::var_os(ENV_DATA_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Effective grace period: `ACTIVITY_AUDIO_GRACE_STOP_S` if it parses,
/// otherwise [`DEFAULT_GRACE_STOP_S`].
pub fn grace_stop_s() -> u64 {
    std::env::var(ENV_GRACE_STOP_S)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|&v| v <= 3600)
        .unwrap_or(DEFAULT_GRACE_STOP_S)
}

/// Run the meeting-audio watcher until `shutdown` is set. **Blocking**;
/// call from a dedicated thread in the daemon process:
///
/// ```no_run
/// # use activity_core::{Config, Db};
/// # use std::sync::atomic::AtomicBool;
/// # fn demo(cfg: Config, db: Db, shutdown: AtomicBool) -> anyhow::Result<()> {
/// let shutdown_flag = std::sync::Arc::new(shutdown);
/// watcher_audio::run(&cfg, &db, &shutdown_flag)?;
/// # Ok(())
/// # }
/// ```
///
/// Responsibilities per issue #6: manual trigger (hotkey + `record.json`
/// control file, mic only), conferencing auto-trigger (active audio
/// session, mic + loopback, stop after a grace period), rolling retention
/// (hourly), and event rows in the `media` + `meeting-audio` buckets.
/// Subsystem errors are logged and absorbed; this function returns only
/// when `shutdown` is set.
pub fn run(cfg: &Config, db: &Db, shutdown: &AtomicBool) -> anyhow::Result<()> {
    let data_dir = data_dir();
    let media_root = data_dir.join("media").join("audio");
    let control_path = data_dir.join("record.json");
    let log = Log::open(&data_dir.join("logs").join("audio.log"));

    log.info(&format!(
        "starting; data_dir={} device={}",
        data_dir.display(),
        cfg.sync.device_name
    ));
    if let Err(e) = std::fs::create_dir_all(&media_root) {
        log.warn(&format!(
            "cannot create media root {}: {e}",
            media_root.display()
        ));
    }

    // WASAPI needs COM on this thread (detector polls from the run loop).
    // Without it, session enumeration fails and detection silently degrades
    // to the process-running fallback.
    #[cfg(windows)]
    {
        if !wasapi::initialize_mta().is_ok() {
            log.warn("COM (MTA) initialization failed on the run loop thread");
        }
    }

    let grace_s = grace_stop_s();
    log.info(&format!(
        "auto-trigger grace period: {grace_s}s (override with {ENV_GRACE_STOP_S})"
    ));
    log.info(&format!(
        "control file channel: {} ({{\"action\":\"start\"|\"stop\",\"trigger\":\"cli\"}})",
        control_path.display()
    ));

    // Capture worker.
    let (cmd_tx, cmd_rx) = mpsc::channel::<CaptureCommand>();
    let (done_tx, done_rx) = mpsc::channel::<FinishedRecording>();
    let worker = spawn_capture_worker(cmd_rx, done_tx, &data_dir, log.clone());
    let capture_available = worker.is_some();
    if !capture_available {
        log.warn("recording is only implemented on Windows; manual and auto triggers are ignored");
    }

    // Hotkey channel (thread exits when hotkey_shutdown flips to true).
    let (hotkey_tx, hotkey_rx) = mpsc::channel::<()>();
    let hotkey_shutdown = Arc::new(AtomicBool::new(false));
    let hotkey_thread =
        spawn_hotkey_listener(&cfg.audio.hotkey, hotkey_tx, hotkey_shutdown.clone(), &log);

    // Conferencing detector.
    let mut detector: Box<dyn ConferenceWatcher> = make_detector(&cfg.audio.process_list, &log);

    let mut machine = TriggerMachine::new();
    let mut last_detect = Instant::now() - DETECT_INTERVAL; // poll immediately
    let mut last_retention = Instant::now() - RETENTION_INTERVAL; // sweep immediately
    let mut auto_suppressed_until: Option<Instant> = None;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // 1. Hotkey toggles.
        while let Ok(()) = hotkey_rx.try_recv() {
            if machine.is_recording() {
                handle_manual_stop(&mut machine, &cmd_tx, db, &cfg.sync.device_name, &log);
            } else {
                handle_manual_request(
                    &mut machine,
                    Source::Hotkey,
                    capture_available,
                    &cmd_tx,
                    db,
                    &cfg.sync.device_name,
                    &media_root,
                    &log,
                );
            }
        }

        // 2. Control file (CLI channel), every tick.
        match poll_control_file(&control_path, true) {
            ControlFileOutcome::Command(cmd) => match cmd.action {
                ControlAction::Start => handle_manual_request(
                    &mut machine,
                    Source::Cli,
                    capture_available,
                    &cmd_tx,
                    db,
                    &cfg.sync.device_name,
                    &media_root,
                    &log,
                ),
                ControlAction::Stop => {
                    handle_manual_stop(&mut machine, &cmd_tx, db, &cfg.sync.device_name, &log);
                }
            },
            ControlFileOutcome::Invalid(note) => {
                log.warn(&format!("invalid record.json ({note}); file moved aside"));
            }
            ControlFileOutcome::Absent => {}
        }

        // 3. Conferencing detection every 2 s.
        if last_detect.elapsed() >= DETECT_INTERVAL {
            last_detect = Instant::now();
            match detector.poll() {
                Some(hit) => handle_conference_active(
                    &mut machine,
                    &hit.process,
                    hit.mode.label(),
                    capture_available,
                    &cmd_tx,
                    db,
                    &cfg.sync.device_name,
                    &media_root,
                    auto_suppressed_until,
                    &log,
                ),
                None => {
                    // No conference seen: the grace window of an ongoing
                    // auto recording keeps ticking (nothing to do here;
                    // auto_seen is only refreshed on positive polls).
                }
            }
        }

        // 4. Grace expiry for auto recordings.
        if machine.auto_grace_expired(now_unix(), grace_s) {
            if let Some(Transition::Stopped { recording, .. }) =
                machine.stop(StopReason::AutoSessionLost, now_unix())
            {
                log.info(&format!(
                    "auto recording stopped after {grace_s}s grace: {}",
                    recording.trigger.label()
                ));
                let _ = cmd_tx.send(CaptureCommand::Stop);
                insert_event_logged(
                    db,
                    &crate::events::meeting_event(
                        &now_rfc3339(),
                        "stop",
                        &recording.trigger.label(),
                        "auto",
                        recording.process.as_deref(),
                        recording.detect_mode,
                        now_unix().saturating_sub(recording.started_at_unix) as f64,
                    ),
                    &cfg.sync.device_name,
                    &log,
                );
            }
        }

        // 5. Finished recordings -> media index rows.
        while let Ok(finished) = done_rx.try_recv() {
            let failed_auto =
                handle_finished_recording(&mut machine, finished, db, &cfg.sync.device_name, &log);
            if failed_auto {
                auto_suppressed_until = Some(Instant::now() + CAPTURE_RETRY_BACKOFF);
            }
        }

        // 6. Retention sweep every hour.
        if last_retention.elapsed() >= RETENTION_INTERVAL {
            last_retention = Instant::now();
            run_retention_sweep(&data_dir, &media_root, cfg, db, &log);
        }

        std::thread::sleep(Duration::from_millis(TICK_MS));
    }

    // ---- shutdown: stop any recording, flush its files, stop threads ----
    if machine.is_recording() {
        if let Some(Transition::Stopped { recording, .. }) =
            machine.stop(StopReason::Shutdown, now_unix())
        {
            let source = if matches!(recording.trigger, Trigger::Auto(_)) {
                "auto".to_string()
            } else {
                recording.source.label().to_string()
            };
            let _ = cmd_tx.send(CaptureCommand::Stop);
            insert_event_logged(
                db,
                &crate::events::meeting_event(
                    &now_rfc3339(),
                    "stop",
                    &recording.trigger.label(),
                    &source,
                    recording.process.as_deref(),
                    recording.detect_mode,
                    now_unix().saturating_sub(recording.started_at_unix) as f64,
                ),
                &cfg.sync.device_name,
                &log,
            );
        }
    }
    drop(cmd_tx); // worker finalizes any in-flight recording and exits
    hotkey_shutdown.store(true, Ordering::Relaxed);

    let deadline = Instant::now() + SHUTDOWN_FINALIZE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            log.warn("capture worker did not report back in time during shutdown");
            break;
        }
        match done_rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(finished) => insert_media_rows(&finished, db, &cfg.sync.device_name, &log),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some(handle) = worker {
        let _ = handle.join();
    }
    if let Some(handle) = hotkey_thread {
        let _ = handle.join();
    }
    log.info("watcher-audio stopped");
    Ok(())
}

/// Handle a manual start request (hotkey toggle when idle / control-file
/// start).
#[allow(clippy::too_many_arguments)]
fn handle_manual_request(
    machine: &mut TriggerMachine,
    source: Source,
    capture_available: bool,
    cmd_tx: &mpsc::Sender<CaptureCommand>,
    db: &Db,
    device: &str,
    media_root: &Path,
    log: &Log,
) {
    if !capture_available {
        log.warn("start request ignored: capture unavailable on this platform");
        return;
    }
    let Some(Transition::Started(rec)) = machine.start_manual(source, now_unix()) else {
        log.info(&format!(
            "start request ignored: already recording ({})",
            machine
                .active()
                .map(|a| a.trigger.label())
                .unwrap_or_default()
        ));
        return;
    };
    log.info("manual recording started (mic only)");
    send_start(cmd_tx, &rec, media_root, log);
    insert_event_logged(
        db,
        &crate::events::meeting_event(
            &now_rfc3339(),
            "start",
            "manual",
            source.label(),
            None,
            None,
            0.0,
        ),
        device,
        log,
    );
}

/// Handle an explicit manual stop (hotkey toggle while recording / control
/// file stop). Manual stop wins over auto recordings.
fn handle_manual_stop(
    machine: &mut TriggerMachine,
    cmd_tx: &mpsc::Sender<CaptureCommand>,
    db: &Db,
    device: &str,
    log: &Log,
) {
    let Some(Transition::Stopped { recording, .. }) = machine.stop(StopReason::Manual, now_unix())
    else {
        log.info("stop request ignored: not recording");
        return;
    };
    let _ = cmd_tx.send(CaptureCommand::Stop);
    let source = if matches!(recording.trigger, Trigger::Auto(_)) {
        "auto".to_string()
    } else {
        recording.source.label().to_string()
    };
    log.info(&format!("manual stop: {}", recording.trigger.label()));
    insert_event_logged(
        db,
        &crate::events::meeting_event(
            &now_rfc3339(),
            "stop",
            &recording.trigger.label(),
            &source,
            recording.process.as_deref(),
            recording.detect_mode,
            now_unix().saturating_sub(recording.started_at_unix) as f64,
        ),
        device,
        log,
    );
}

/// Handle a positive detector poll.
#[allow(clippy::too_many_arguments)]
fn handle_conference_active(
    machine: &mut TriggerMachine,
    process: &str,
    detect_mode: &'static str,
    capture_available: bool,
    cmd_tx: &mpsc::Sender<CaptureCommand>,
    db: &Db,
    device: &str,
    media_root: &Path,
    auto_suppressed_until: Option<Instant>,
    log: &Log,
) {
    match machine.active() {
        Some(rec) if matches!(rec.trigger, Trigger::Auto(_)) => {
            // Ongoing auto recording: refresh the grace window.
            machine.auto_seen(now_unix());
        }
        Some(_rec) => {
            // Manual recording in progress: no double start, nothing to do.
        }
        None => {
            if !capture_available {
                return;
            }
            if auto_suppressed_until.is_some_and(|until| Instant::now() < until) {
                log.warn("auto start suppressed briefly: last capture attempt failed");
                return;
            }
            let Some(Transition::Started(rec)) =
                machine.start_auto(process.to_string(), detect_mode, now_unix())
            else {
                return;
            };
            log.info(&format!(
                "auto recording started: {} (detect: {detect_mode})",
                rec.trigger.label()
            ));
            send_start(cmd_tx, &rec, media_root, log);
            insert_event_logged(
                db,
                &crate::events::meeting_event(
                    &now_rfc3339(),
                    "start",
                    &rec.trigger.label(),
                    "auto",
                    rec.process.as_deref(),
                    rec.detect_mode,
                    0.0,
                ),
                device,
                log,
            );
        }
    }
}

/// Build the worker StartSpec and hand it to the capture worker.
fn send_start(
    cmd_tx: &mpsc::Sender<CaptureCommand>,
    rec: &crate::machine::ActiveRecording,
    media_root: &Path,
    log: &Log,
) {
    let base = crate::naming::base_path(media_root, &chrono::Local::now(), &rec.trigger);
    let spec = StartSpec {
        trigger_label: rec.trigger.label(),
        process: rec.process.clone(),
        source: if matches!(rec.trigger, Trigger::Auto(_)) {
            "auto"
        } else {
            rec.source.label()
        },
        detect_mode: rec.detect_mode,
        base_path: base,
        with_loopback: rec.trigger.uses_loopback(),
        started_rfc3339: now_rfc3339(),
    };
    if let Err(e) = cmd_tx.send(CaptureCommand::Start(spec)) {
        log.error(&format!("capture worker unreachable: {e}"));
    }
}

/// A recording finished: insert `media` index rows (one per file). If the
/// worker reported an outright failure while the machine still thinks we
/// are recording, stop the machine (capture failure must not kill the
/// daemon). Returns true if an auto-triggered capture failed (caller
/// applies a retry backoff).
fn handle_finished_recording(
    machine: &mut TriggerMachine,
    finished: FinishedRecording,
    db: &Db,
    device: &str,
    log: &Log,
) -> bool {
    let FinishedRecording {
        spec,
        duration_s,
        files,
        error,
    } = finished;

    if let Some(err) = &error {
        log.error(&format!("capture failed ({}): {err}", spec.trigger_label));
        let was_auto = spec.source == "auto";
        if machine.is_recording() {
            let _ = machine.stop(StopReason::CaptureFailed, now_unix());
            insert_event_logged(
                db,
                &crate::events::meeting_event(
                    &now_rfc3339(),
                    "stop",
                    &spec.trigger_label,
                    spec.source,
                    spec.process.as_deref(),
                    spec.detect_mode,
                    duration_s,
                ),
                device,
                log,
            );
        }
        return was_auto;
    }

    insert_media_rows_impl(&spec, duration_s, &files, db, device, log);
    false
}

fn insert_media_rows(finished: &FinishedRecording, db: &Db, device: &str, log: &Log) {
    if finished.error.is_some() {
        return;
    }
    insert_media_rows_impl(
        &finished.spec,
        finished.duration_s,
        &finished.files,
        db,
        device,
        log,
    );
}

fn insert_media_rows_impl(
    spec: &StartSpec,
    duration_s: f64,
    files: &[crate::recording::FinalizedFile],
    db: &Db,
    device: &str,
    log: &Log,
) {
    for file in files {
        log.info(&format!(
            "recording finalized: {} ({} bytes, sha16 {})",
            file.rel_path, file.bytes, file.sha16
        ));
        insert_event_logged(
            db,
            &crate::events::media_event(
                &spec.started_rfc3339,
                duration_s,
                &file.rel_path,
                file.bytes,
                &file.sha16,
            ),
            device,
            log,
        );
    }
    if files.is_empty() {
        log.warn(&format!(
            "recording finished with no files ({})",
            spec.trigger_label
        ));
    }
}

/// One hourly retention sweep: walk `media/audio`, delete oldest-first
/// within the configured budgets, sparing files whose `media` index row is
/// not synced yet (ADR rev 5).
fn run_retention_sweep(data_dir: &Path, media_root: &Path, cfg: &Config, db: &Db, log: &Log) {
    let budget = RetentionBudget {
        max_age_days: cfg.audio.max_age_days,
        max_total_mb: cfg.audio.max_total_mb,
    };
    let protected = unsynced_media_paths(db, log);
    let outcome = sweep(data_dir, media_root, &budget, &protected);
    for path in &outcome.deleted {
        log.info(&format!("retention deleted: {}", path.display()));
    }
    for path in &outcome.removed_part_files {
        log.info(&format!(
            "retention removed stale part file: {}",
            path.display()
        ));
    }
    for err in &outcome.errors {
        log.warn(&format!("retention delete failed: {err}"));
    }
    if outcome.protected_unsynced > 0 {
        log.info(&format!(
            "retention spared {} not-yet-synced file(s)",
            outcome.protected_unsynced
        ));
    }
    if !outcome.deleted.is_empty() || !outcome.removed_part_files.is_empty() {
        log.info(&format!(
            "retention sweep done: {} file(s) deleted, {} part file(s) removed",
            outcome.deleted.len(),
            outcome.removed_part_files.len()
        ));
    }
}

/// Relative paths (forward slashes) of audio files whose `media` index row
/// is not synced to the hub yet. These are never pruned (ADR rev 5).
fn unsynced_media_paths(db: &Db, log: &Log) -> HashSet<String> {
    const LIMIT: u32 = 100_000;
    let mut out = HashSet::new();
    let rows = match db.fetch_unsynced_events(LIMIT) {
        Ok(rows) => rows,
        Err(e) => {
            log.warn(&format!(
                "cannot read unsynced events ({e}); retention disabled this sweep"
            ));
            return out;
        }
    };
    if rows.len() as u32 >= LIMIT {
        log.warn("unsynced set truncated at limit; some files may be spared longer than needed");
    }
    for row in rows {
        if row.bucket != activity_core::BUCKET_MEDIA {
            continue;
        }
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&row.data) else {
            continue;
        };
        if data["kind"] == "audio" {
            if let Some(path) = data["path"].as_str() {
                out.insert(path.to_string());
            }
        }
    }
    out
}

fn insert_event_logged(db: &Db, event: &EventEnvelope, device: &str, log: &Log) {
    if let Err(e) = db.insert_event(event, device) {
        log.error(&format!(
            "DB insert failed for bucket {}: {e}",
            event.bucket
        ));
    }
}

// ---------------------------------------------------------------------------
// Platform seams (Windows real implementations, no-op elsewhere)
// ---------------------------------------------------------------------------

/// Spawn the capture worker. `None` on non-Windows (no capture support).
#[cfg(windows)]
fn spawn_capture_worker(
    cmd_rx: mpsc::Receiver<CaptureCommand>,
    done_tx: mpsc::Sender<FinishedRecording>,
    data_dir: &Path,
    log: Log,
) -> Option<std::thread::JoinHandle<()>> {
    Some(crate::win_capture::spawn_worker(
        cmd_rx, done_tx, data_dir, log,
    ))
}

#[cfg(not(windows))]
fn spawn_capture_worker(
    _cmd_rx: mpsc::Receiver<CaptureCommand>,
    _done_tx: mpsc::Sender<FinishedRecording>,
    _data_dir: &Path,
    _log: Log,
) -> Option<std::thread::JoinHandle<()>> {
    None
}

/// Spawn the hotkey listener thread. `None` if unavailable/disabled.
#[cfg(windows)]
fn spawn_hotkey_listener(
    hotkey_cfg: &str,
    tx: mpsc::Sender<()>,
    shutdown: Arc<AtomicBool>,
    log: &Log,
) -> Option<std::thread::JoinHandle<()>> {
    match crate::hotkey::HotKeySpec::parse(hotkey_cfg) {
        Ok(spec) => Some(crate::win_hotkey::spawn_hotkey(
            spec,
            tx,
            shutdown,
            log.clone(),
        )),
        Err(e) => {
            log.warn(&format!(
                "hotkey config '{hotkey_cfg}' invalid ({e}); hotkey disabled, control file still works"
            ));
            None
        }
    }
}

#[cfg(not(windows))]
fn spawn_hotkey_listener(
    _hotkey_cfg: &str,
    _tx: mpsc::Sender<()>,
    _shutdown: Arc<AtomicBool>,
    _log: &Log,
) -> Option<std::thread::JoinHandle<()>> {
    None
}

/// Build the conferencing detector for this platform.
#[cfg(windows)]
fn make_detector(process_list: &[String], log: &Log) -> Box<dyn ConferenceWatcher> {
    if process_list.is_empty() {
        log.info("auto-trigger disabled: audio.process_list is empty");
    } else {
        log.info("detection mode: audio-session (preferred), process-running fallback on errors");
    }
    Box::new(crate::win_detector::ConferencingDetector::new(
        process_list,
        log.clone(),
    ))
}

#[cfg(not(windows))]
fn make_detector(_process_list: &[String], log: &Log) -> Box<dyn ConferenceWatcher> {
    log.warn("conferencing auto-trigger unavailable on this platform");
    Box::new(crate::detect::NullWatcher)
}
