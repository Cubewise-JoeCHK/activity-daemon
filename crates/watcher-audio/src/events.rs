//! Event payload construction for the two buckets this plugin writes
//! (issue #6 / ADR #16):
//!
//! - `media` index rows: `{kind:"audio", path, bytes, sha16, producer}` -
//!   one per finished audio file. ADR rev 5: file first, index row after.
//! - `meeting-audio` plain events: `{state:"start"|"stop", trigger, ...}` -
//!   the v0.2 timesheet consumes these spans.
//!
//! `path` in `media` rows is relative to the data dir with forward slashes,
//! matching the ADR layout `media/<kind>/<YYYY>/<MM>/<DD>/...`.
//! `sha16` is the first 16 hex chars of the SHA-256 over the file content.

use activity_core::{EventEnvelope, BUCKET_MEDIA};
use sha2::{Digest, Sha256};

/// Producer tag on every media index row from this plugin.
pub const PRODUCER: &str = "watcher-audio";

/// Well-known bucket for meeting audio spans.
pub const BUCKET_MEETING_AUDIO: &str = "meeting-audio";

/// First 16 hex chars of SHA-256 over `bytes` (the `sha16` contract).
pub fn sha16_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex = format!("{digest:x}");
    hex[..16].to_string()
}

/// Stream a file from disk and compute its `sha16` without loading it into
/// memory.
pub fn sha16_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hex = format!("{:x}", hasher.finalize());
    Ok(hex[..16].to_string())
}

/// Build a `media` bucket index row for one finished audio file.
pub fn media_event(
    started_rfc3339: &str,
    duration_s: f64,
    rel_path: &str,
    bytes: u64,
    sha16: &str,
) -> EventEnvelope {
    EventEnvelope {
        bucket: BUCKET_MEDIA.to_string(),
        timestamp: started_rfc3339.to_string(),
        duration_s,
        data: serde_json::json!({
            "kind": "audio",
            "path": rel_path,
            "bytes": bytes,
            "sha16": sha16,
            "producer": PRODUCER,
        }),
    }
}

/// Build a `meeting-audio` bucket event for a recording start or stop.
#[allow(clippy::too_many_arguments)]
pub fn meeting_event(
    timestamp_rfc3339: &str,
    state: &str,
    trigger_label: &str,
    source: &str,
    process: Option<&str>,
    detect_mode: Option<&str>,
    duration_s: f64,
) -> EventEnvelope {
    EventEnvelope {
        bucket: BUCKET_MEETING_AUDIO.to_string(),
        timestamp: timestamp_rfc3339.to_string(),
        duration_s,
        data: serde_json::json!({
            "state": state,
            "trigger": trigger_label,
            "source": source,
            "process": process,
            "detect_mode": detect_mode,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha16_matches_sha256_prefix() {
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223...
        assert_eq!(sha16_bytes(b"abc"), "ba7816bf8f01cfea");
        assert_eq!(sha16_bytes(b""), "e3b0c44298fc1c14");
    }

    #[test]
    fn sha16_file_streams_content() {
        let dir = std::env::temp_dir().join(format!("watcher-audio-sha-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("f.bin");
        std::fs::write(&path, b"abc").expect("write");
        assert_eq!(sha16_file(&path).expect("sha"), "ba7816bf8f01cfea");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn media_event_payload_matches_adr() {
        let env = media_event(
            "2026-11-04T09:14:32+08:00",
            123.5,
            "media/audio/x.wav",
            48000,
            "deadbeefdeadbeef",
        );
        assert_eq!(env.bucket, "media");
        assert_eq!(env.timestamp, "2026-11-04T09:14:32+08:00");
        assert!((env.duration_s - 123.5).abs() < 1e-9);
        assert_eq!(env.data["kind"], "audio");
        assert_eq!(env.data["path"], "media/audio/x.wav");
        assert_eq!(env.data["bytes"], 48000);
        assert_eq!(env.data["sha16"], "deadbeefdeadbeef");
        assert_eq!(env.data["producer"], "watcher-audio");
    }

    #[test]
    fn meeting_event_start_and_stop_payloads() {
        let start = meeting_event(
            "2026-11-04T09:14:32+08:00",
            "start",
            "auto_ms-teams",
            "auto",
            Some("ms-teams.exe"),
            Some("audio-session"),
            0.0,
        );
        assert_eq!(start.bucket, "meeting-audio");
        assert_eq!(start.data["state"], "start");
        assert_eq!(start.data["trigger"], "auto_ms-teams");
        assert_eq!(start.data["process"], "ms-teams.exe");
        assert_eq!(start.data["detect_mode"], "audio-session");

        let stop = meeting_event(
            "2026-11-04T10:00:00+08:00",
            "stop",
            "manual",
            "cli",
            None,
            None,
            2728.0,
        );
        assert_eq!(stop.data["state"], "stop");
        assert_eq!(stop.data["trigger"], "manual");
        assert_eq!(stop.data["process"], serde_json::Value::Null);
        assert_eq!(stop.data["detect_mode"], serde_json::Value::Null);
        assert!((stop.duration_s - 2728.0).abs() < 1e-9);
    }
}
