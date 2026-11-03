//! WASAPI capture worker (Windows only, `#[cfg(windows)]`).
//!
//! Uses the `wasapi` crate (safe WASAPI bindings, v0.26):
//! - **Mic** (manual + auto trigger): default capture device, 16-bit PCM,
//!   48 kHz, mono (AUTOCONVERT lets WASAPI resample/downmix whatever the
//!   device offers).
//! - **Loopback** (auto trigger only): default render device opened in
//!   loopback mode, 16-bit PCM, 48 kHz, stereo (remote voices arrive via
//!   the speakers).
//!
//! Both streams use shared-mode polling (`StreamMode::PollingShared`):
//! event-driven mode never fires for loopback when the system plays
//! silence, so polling is the robust choice for meeting recording.
//!
//! Each recording writes `<base>_mic.wav` / `<base>_loopback.wav` via
//! [`crate::sink::WavSink`] (`.part` file, renamed on finalize).
//!
//! Robustness contract (issue #6): every device/COM error is contained.
//! The worker answers every `Start` with exactly one `FinishedRecording`
//! and keeps running; a flaky device can end a recording, never the daemon.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

use crate::logging::Log;
use crate::recording::{CaptureCommand, FinishedRecording, StartSpec, StreamKind};
use crate::sink::{AudioSink, WavSink};

/// Polling cadence of the capture loop.
const PUMP_INTERVAL_MS: u64 = 50;
/// Device buffer length in 100 ns units (200 ms).
const BUFFER_DURATION_HNS: i64 = 2_000_000;
/// Guard against pathological read loops per pump tick.
const MAX_READS_PER_PUMP: usize = 64;

const MIC_SAMPLE_RATE: usize = 48_000;
const LOOPBACK_SAMPLE_RATE: usize = 48_000;

/// One live WASAPI stream wired to its sink.
struct ActiveStream {
    client: wasapi::AudioClient,
    capture: wasapi::AudioCaptureClient,
    sink: WavSink,
    deque: VecDeque<u8>,
}

impl ActiveStream {
    fn open(
        direction: Direction,
        sample_rate: usize,
        channels: usize,
        final_path: &Path,
    ) -> anyhow::Result<ActiveStream> {
        let enumerator = DeviceEnumerator::new()?;
        let device = enumerator.get_default_device(&direction)?;
        let mut client = device.get_iaudioclient()?;
        let format = WaveFormat::new(16, 16, &SampleType::Int, sample_rate, channels, None);
        // Note: passing Direction::Capture for a render device makes the
        // wasapi crate set AUDCLNT_STREAMFLAGS_LOOPBACK (system loopback).
        client.initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::PollingShared {
                autoconvert: true,
                buffer_duration_hns: BUFFER_DURATION_HNS,
            },
        )?;
        let capture = client.get_audiocaptureclient()?;
        let sink = WavSink::create(
            final_path.to_path_buf(),
            sample_rate as u32,
            channels as u16,
            16,
        )?;
        client.start_stream()?;
        Ok(ActiveStream {
            client,
            capture,
            sink,
            deque: VecDeque::new(),
        })
    }

    /// Read everything currently buffered by WASAPI into the sink.
    /// Returns Err on stream failure (device removed, COM error, WAV cap).
    fn pump(&mut self) -> anyhow::Result<()> {
        for _ in 0..MAX_READS_PER_PUMP {
            let padding = self.client.get_current_padding()?;
            if padding == 0 {
                return Ok(());
            }
            let info = self.capture.read_from_device_to_deque(&mut self.deque)?;
            let len = self.deque.len();
            if len > 0 {
                if info.flags.silent {
                    // Buffer is silence: write zeros (content is undefined).
                    let zeros = vec![0u8; len];
                    self.sink.write(&zeros)?;
                } else {
                    let (a, b) = self.deque.as_slices();
                    self.sink.write(a)?;
                    self.sink.write(b)?;
                }
                self.deque.clear();
            }
        }
        Ok(())
    }

    fn finalize(mut self) -> anyhow::Result<u64> {
        let _ = self.client.stop_stream();
        self.sink.finalize().map_err(anyhow::Error::from)
    }
}

/// One recording in flight.
struct ActiveRecording {
    spec: StartSpec,
    started_at: Instant,
    mic: Option<ActiveStream>,
    loopback: Option<ActiveStream>,
}

/// The worker: owns WASAPI objects on its own thread. Replies with exactly
/// one `FinishedRecording` per accepted `Start` on `done_tx`.
fn worker_loop(
    cmd_rx: Receiver<CaptureCommand>,
    done_tx: Sender<FinishedRecording>,
    data_dir: &Path,
    log: Log,
) {
    if !wasapi::initialize_mta().is_ok() {
        log.warn("COM (MTA) initialization failed; capture may still work via auto-init");
    }
    let mut active: Option<ActiveRecording> = None;

    loop {
        // 1. Commands first so Stop is honored promptly.
        let mut stop_requested = false;
        loop {
            match cmd_rx.try_recv() {
                Ok(CaptureCommand::Start(spec)) => {
                    if active.is_some() {
                        log.warn(&format!(
                            "start ignored, already recording ({})",
                            spec.trigger_label
                        ));
                    } else {
                        match open_recording(&spec, &log) {
                            Ok(rec) => active = Some(rec),
                            Err(e) => {
                                log.error(&format!(
                                    "capture start failed ({}): {e}",
                                    spec.trigger_label
                                ));
                                let _ = done_tx.send(FinishedRecording {
                                    spec,
                                    duration_s: 0.0,
                                    files: Vec::new(),
                                    error: Some(format!("{e:#}")),
                                });
                            }
                        }
                    }
                }
                Ok(CaptureCommand::Stop) => {
                    stop_requested = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Run loop gone: finalize and exit.
                    if let Some(rec) = active.take() {
                        finish_recording(rec, &done_tx, &log, data_dir, "shutdown");
                    }
                    wasapi::deinitialize();
                    return;
                }
            }
        }

        // 2. Pump active streams.
        if let Some(mut rec) = active.take() {
            let mut stream_error: Option<String> = None;
            for (label, slot) in [("mic", &mut rec.mic), ("loopback", &mut rec.loopback)] {
                if let Some(stream) = slot.as_mut() {
                    if let Err(e) = stream.pump() {
                        let msg = format!("{label} stream failed: {e:#}");
                        log.error(&msg);
                        if stream_error.is_none() {
                            stream_error = Some(msg);
                        }
                        // Dead stream: finalize it now to save what we have.
                        if let Some(dead) = slot.take() {
                            let _ = dead.finalize();
                        }
                    }
                }
            }
            if stream_error.is_some() && rec.mic.is_none() {
                // Mic is the essential stream: without it the recording is
                // over (loopback-only audio is not useful for meetings).
                finish_recording(rec, &done_tx, &log, data_dir, "capture failure");
            } else {
                active = Some(rec);
            }
        }

        // 3. Stop finalization.
        if stop_requested && active.is_some() {
            if let Some(rec) = active.take() {
                finish_recording(rec, &done_tx, &log, data_dir, "stop request");
            }
        }

        std::thread::sleep(Duration::from_millis(PUMP_INTERVAL_MS));
    }
}

/// Open the streams for a new recording. The mic is essential: if it cannot
/// be opened the start fails outright. Loopback is best-effort.
fn open_recording(spec: &StartSpec, log: &Log) -> anyhow::Result<ActiveRecording> {
    let mic_path = crate::naming::stream_file(&spec.base_path, StreamKind::Mic);
    let mic = ActiveStream::open(Direction::Capture, MIC_SAMPLE_RATE, 1, &mic_path)
        .map_err(|e| anyhow::anyhow!("mic open failed ({}): {e:#}", mic_path.display()))?;
    log.info(&format!("mic capture open: {}", mic_path.display()));

    let loopback = if spec.with_loopback {
        let loop_path = crate::naming::stream_file(&spec.base_path, StreamKind::Loopback);
        match ActiveStream::open(Direction::Render, LOOPBACK_SAMPLE_RATE, 2, &loop_path) {
            Ok(stream) => {
                log.info(&format!("loopback capture open: {}", loop_path.display()));
                Some(stream)
            }
            Err(e) => {
                log.warn(&format!("loopback open failed, recording mic-only: {e:#}"));
                None
            }
        }
    } else {
        None
    };

    Ok(ActiveRecording {
        spec: spec.clone(),
        started_at: Instant::now(),
        mic: Some(mic),
        loopback,
    })
}

/// Stop streams, finalize sinks (rename .part -> final), hash files and
/// report on the done channel.
fn finish_recording(
    rec: ActiveRecording,
    done_tx: &Sender<FinishedRecording>,
    log: &Log,
    data_dir: &Path,
    why: &str,
) {
    let duration_s = rec.started_at.elapsed().as_secs_f64();
    let mut files = Vec::new();
    for (stream, slot) in [
        (StreamKind::Mic, rec.mic),
        (StreamKind::Loopback, rec.loopback),
    ] {
        let Some(stream_obj) = slot else { continue };
        match stream_obj.finalize() {
            Ok(bytes) if bytes > 0 => {
                let path = crate::naming::stream_file(&rec.spec.base_path, stream);
                match crate::events::sha16_file(&path) {
                    Ok(sha) => files.push(crate::recording::FinalizedFile {
                        rel_path: crate::naming::relative_media_path(data_dir, &path),
                        path,
                        bytes: bytes + 44,
                        sha16: sha,
                        stream,
                    }),
                    Err(e) => log.warn(&format!("hash failed for {}: {e}", path.display())),
                }
            }
            Ok(_) => {
                log.warn(&format!(
                    "{} stream produced no data (device silent/absent); file dropped",
                    stream.label()
                ));
                let path = crate::naming::stream_file(&rec.spec.base_path, stream);
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => {
                log.error(&format!(
                    "finalize failed for {} stream: {e:#}",
                    stream.label()
                ));
            }
        }
    }
    log.info(&format!(
        "recording finished ({why}): {} file(s), {:.1}s, trigger {}",
        files.len(),
        duration_s,
        rec.spec.trigger_label
    ));
    let _ = done_tx.send(FinishedRecording {
        spec: rec.spec,
        duration_s,
        files,
        error: None,
    });
}

/// Spawn the worker thread. Returns its join handle.
pub fn spawn_worker(
    cmd_rx: Receiver<CaptureCommand>,
    done_tx: Sender<FinishedRecording>,
    data_dir: &Path,
    log: Log,
) -> JoinHandle<()> {
    let data_dir = data_dir.to_path_buf();
    std::thread::Builder::new()
        .name("watcher-audio-capture".to_string())
        .spawn(move || worker_loop(cmd_rx, done_tx, &data_dir, log))
        .expect("spawn capture worker thread")
}
