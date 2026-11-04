//! Frame storage: JPEG encoding, sha16 dedup, on-disk layout and `media`
//! bucket index rows (issue #5, ADR media contract).
//!
//! Layout: `{data_dir}/media/frames/<YYYY>/<MM>/<DD>/<HHMMSS>_<monitor>_
//! <sha16>.jpg` (local time), one file per captured monitor. Each written
//! file is indexed as one event in the `media` bucket with
//! `{ kind, path, bytes, sha16, monitor, producer }`; `path` is relative
//! to the data dir with forward slashes so records stay machine-portable.
//! Files written before multi-monitor capture lack the `<monitor>` part;
//! retention and sync treat old and new names identically.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use activity_core::{Db, EventEnvelope, BUCKET_MEDIA};
use chrono::{DateTime, Local, SecondsFormat};
use jpeg_encoder::{ColorType, Encoder};
use log::info;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::capture::Frame;

/// Default JPEG quality. Mirrors core's `ScreenConfig::default().quality`;
/// the watcher reads the configured value (`screen.quality`), so this const
/// only documents the default and backs the tests.
pub const DEFAULT_JPEG_QUALITY: u8 = 70;

/// Producer identity recorded on every `media` index row.
pub const PRODUCER: &str = "watcher-screen";

/// First 16 hex chars of the SHA-256 of `bytes` - the frame content hash
/// (ADR: `sha16` lets the watcher skip duplicate frames).
pub fn sha16(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(16);
    for byte in &digest[..8] {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Reduce a raw monitor device name (`\\.\DISPLAY2`) to a safe file-name
/// component (`DISPLAY2`): only `[A-Za-z0-9._-]` survive; an empty result
/// falls back to `MONITOR`. Idempotent - labels are sanitized again by
/// [`FrameStore::store_frame`] so any caller stays on the layout contract.
pub fn sanitize_monitor_label(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('\0');
    let stripped = trimmed.strip_prefix(r"\\.\").unwrap_or(trimmed);
    let cleaned: String = stripped
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    if cleaned.is_empty() {
        "MONITOR".to_string()
    } else {
        cleaned
    }
}

/// Encode a BGRA [`Frame`] as baseline JPEG of the given quality (1-100).
pub fn encode_jpeg(frame: &Frame, quality: u8) -> anyhow::Result<Vec<u8>> {
    let expected = frame.width as usize * frame.height as usize * 4;
    anyhow::ensure!(
        frame.bgra.len() == expected,
        "BGRA buffer holds {} byte(s), expected {expected} for {}x{}",
        frame.bgra.len(),
        frame.width,
        frame.height
    );
    let mut jpeg = Vec::new();
    Encoder::new(&mut jpeg, quality.clamp(1, 100)).encode(
        &frame.bgra,
        frame.width,
        frame.height,
        ColorType::Bgra,
    )?;
    Ok(jpeg)
}

/// One frame written to disk and indexed in the `media` bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFrame {
    /// Data-dir-relative path with forward slashes (index row `path`).
    pub path: String,
    /// Sanitized monitor label the frame came from (index row `monitor`).
    pub monitor: String,
    /// JPEG size in bytes (index row `bytes`).
    pub bytes: u64,
    /// Content hash prefix (index row `sha16`).
    pub sha16: String,
}

/// Deduplicating frame store: encode -> hash -> skip or write + index.
///
/// Dedup is per monitor: each label keeps its own previous-frame hash, so
/// an unchanged monitor skips its frame even while another monitor's
/// content changes every tick.
pub struct FrameStore {
    /// The `media` root, e.g. `<data_dir>/media`.
    media_root: PathBuf,
    quality: u8,
    /// Previous frame's `sha16` per monitor label (the dedup chain).
    last_sha16: HashMap<String, String>,
}

impl FrameStore {
    pub fn new(media_root: PathBuf, quality: u8) -> FrameStore {
        FrameStore {
            media_root,
            quality,
            last_sha16: HashMap::new(),
        }
    }

    pub fn media_root(&self) -> &Path {
        &self.media_root
    }

    /// Encode `frame` from `monitor` and store it unless its `sha16`
    /// matches that monitor's previous frame (duplicate content is not
    /// written at all - no file, no index row). On write, returns the
    /// [`StoredFrame`]; on dedup, `Ok(None)`.
    pub fn store_frame(
        &mut self,
        monitor: &str,
        frame: &Frame,
        at: DateTime<Local>,
        db: &Db,
        device: &str,
    ) -> anyhow::Result<Option<StoredFrame>> {
        let monitor = sanitize_monitor_label(monitor);
        let jpeg = encode_jpeg(frame, self.quality)?;
        let sha16 = sha16(&jpeg);
        if self.last_sha16.get(&monitor).map(String::as_str) == Some(sha16.as_str()) {
            return Ok(None);
        }

        let (year, month, day, hhmmss) = time_parts(at);
        let rel = format!("media/frames/{year}/{month}/{day}/{hhmmss}_{monitor}_{sha16}.jpg");
        let full = self
            .media_root
            .join("frames")
            .join(&year)
            .join(&month)
            .join(&day)
            .join(format!("{hhmmss}_{monitor}_{sha16}.jpg"));
        fs::create_dir_all(full.parent().expect("path has a parent"))?;
        fs::write(&full, &jpeg)?;

        let stored = StoredFrame {
            path: rel.clone(),
            monitor: monitor.clone(),
            bytes: jpeg.len() as u64,
            sha16: sha16.clone(),
        };
        let event = EventEnvelope {
            bucket: BUCKET_MEDIA.to_string(),
            timestamp: at.to_rfc3339_opts(SecondsFormat::Secs, false),
            duration_s: 0.0,
            data: json!({
                "kind": "frame",
                "path": rel,
                "bytes": stored.bytes,
                "sha16": sha16,
                "monitor": monitor,
                "producer": PRODUCER,
            }),
        };
        db.insert_event(&event, device)?;
        self.last_sha16.insert(monitor.clone(), sha16.clone());

        info!(
            "frame stored: {} ({} bytes, sha16 {sha16})",
            stored.path, stored.bytes
        );
        Ok(Some(stored))
    }
}

/// (YYYY, MM, DD, HHMMSS) in local time, zero-padded strings.
fn time_parts(at: DateTime<Local>) -> (String, String, String, String) {
    (
        at.format("%Y").to_string(),
        at.format("%m").to_string(),
        at.format("%d").to_string(),
        at.format("%H%M%S").to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 11, 4, 9, 15, 0)
            .single()
            .expect("valid test time")
    }

    fn temp_media(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("watcher-screen-store-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn frame(seed: u8) -> Frame {
        Frame::synthetic(4, 4, seed)
    }

    #[test]
    fn sha16_is_prefix_of_sha256_hex() {
        let data = b"activity-daemon frame bytes";
        let full = Sha256::digest(data);
        let expected: String = full[..8].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(sha16(data), expected);
        assert_eq!(sha16(data).len(), 16);
    }

    #[test]
    fn default_quality_matches_core_config_default() {
        // The watcher encodes with `screen.quality`; this const only mirrors
        // core's default so the two cannot drift silently (issue #7).
        assert_eq!(
            activity_core::Config::default().screen.quality,
            DEFAULT_JPEG_QUALITY
        );
    }

    #[test]
    fn encode_jpeg_produces_jfif_bytes() {
        let jpeg = encode_jpeg(&frame(1), DEFAULT_JPEG_QUALITY).expect("encode");
        assert!(jpeg.len() > 2);
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "SOI marker");
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "EOI marker");
        // Same input, same settings: byte-identical output (dedup relies on it).
        let again = encode_jpeg(&frame(1), DEFAULT_JPEG_QUALITY).expect("encode 2");
        assert_eq!(jpeg, again);
        // Different input: different bytes.
        let other = encode_jpeg(&frame(2), DEFAULT_JPEG_QUALITY).expect("encode 3");
        assert_ne!(jpeg, other);
    }

    #[test]
    fn encode_jpeg_rejects_wrong_buffer_length() {
        let mut bad = Frame::synthetic(4, 4, 0);
        bad.bgra.pop();
        let err = encode_jpeg(&bad, 70).expect_err("must reject");
        assert!(err.to_string().contains("byte(s)"));
    }

    #[test]
    fn sanitize_monitor_label_strips_device_prefix() {
        assert_eq!(sanitize_monitor_label(r"\\.\DISPLAY1"), "DISPLAY1");
        assert_eq!(sanitize_monitor_label("DISPLAY2"), "DISPLAY2");
        // Idempotent on already-sanitized labels.
        assert_eq!(sanitize_monitor_label("DISPLAY2"), "DISPLAY2");
        // Path-hostile separators are dropped, so the result is always a
        // single safe file-name component (no traversal possible).
        assert_eq!(sanitize_monitor_label(r"..\..\evil"), "....evil");
        assert_eq!(sanitize_monitor_label(r"\\evil\..\x"), "evil..x");
        assert_eq!(sanitize_monitor_label("///"), "MONITOR");
        assert_eq!(sanitize_monitor_label(""), "MONITOR");
        // NUL padding from fixed-size device-name buffers is trimmed.
        assert_eq!(
            sanitize_monitor_label("DISPLAY3\u{0}\u{0}\u{0}"),
            "DISPLAY3"
        );
    }

    #[test]
    fn store_writes_layout_and_media_index_row() {
        let media = temp_media("layout");
        let db = Db::open_in_memory().expect("db");
        let mut store = FrameStore::new(media.clone(), DEFAULT_JPEG_QUALITY);

        let stored = store
            .store_frame("DISPLAY1", &frame(1), at(), &db, "dev-laptop")
            .expect("store")
            .expect("first frame is written, not deduped");

        // Layout: media/frames/YYYY/MM/DD/HHMMSS_<monitor>_<sha16>.jpg
        let sha = &stored.sha16;
        assert_eq!(
            stored.path,
            format!("media/frames/2026/11/04/091500_DISPLAY1_{sha}.jpg")
        );
        let full = media
            .join("frames")
            .join("2026")
            .join("11")
            .join("04")
            .join(format!("091500_DISPLAY1_{sha}.jpg"));
        let on_disk = fs::read(&full).expect("jpeg on disk");
        assert_eq!(on_disk.len() as u64, stored.bytes);
        assert_eq!(&on_disk[..2], &[0xFF, 0xD8]);

        // Index row in the media bucket.
        let rows = db.fetch_unsynced_events(10).expect("rows");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.bucket, BUCKET_MEDIA);
        // RFC3339 with the local offset, second precision (TZ-independent).
        assert_eq!(
            row.ts_start,
            at().to_rfc3339_opts(SecondsFormat::Secs, false)
        );
        assert_eq!(row.duration_s, 0.0);
        assert_eq!(row.device, "dev-laptop");
        let data: serde_json::Value = serde_json::from_str(&row.data).expect("json");
        assert_eq!(data["kind"], "frame");
        assert_eq!(data["path"], stored.path);
        assert_eq!(data["bytes"], stored.bytes);
        assert_eq!(data["sha16"], stored.sha16);
        assert_eq!(data["monitor"], "DISPLAY1");
        assert_eq!(data["producer"], "watcher-screen");

        let _ = fs::remove_dir_all(media);
    }

    #[test]
    fn identical_frame_is_skipped_entirely() {
        let media = temp_media("dedup");
        let db = Db::open_in_memory().expect("db");
        let mut store = FrameStore::new(media.clone(), DEFAULT_JPEG_QUALITY);

        let first = store
            .store_frame("DISPLAY1", &frame(7), at(), &db, "dev-laptop")
            .expect("store 1")
            .expect("first written");
        let second = store
            .store_frame("DISPLAY1", &frame(7), at(), &db, "dev-laptop")
            .expect("store 2");
        assert_eq!(second, None, "identical content must not be written");

        // One file, one index row.
        let jpeg_count = walk_jpegs(&media);
        assert_eq!(jpeg_count, 1);
        assert_eq!(db.event_count().expect("count"), 1);
        assert_eq!(first.sha16.len(), 16);

        let _ = fs::remove_dir_all(media);
    }

    #[test]
    fn changed_frame_is_written_again() {
        let media = temp_media("changed");
        let db = Db::open_in_memory().expect("db");
        let mut store = FrameStore::new(media.clone(), DEFAULT_JPEG_QUALITY);

        assert!(store
            .store_frame("DISPLAY1", &frame(7), at(), &db, "dev")
            .expect("store 1")
            .is_some());
        let second = store
            .store_frame("DISPLAY1", &frame(8), at(), &db, "dev")
            .expect("store 2")
            .expect("changed frame written");
        // Distinct sha16s must never collide on one file.
        assert_eq!(walk_jpegs(&media), 2);
        assert_eq!(db.event_count().expect("count"), 2);
        assert!(
            second
                .path
                .ends_with(&format!("_{}_{}.jpg", "DISPLAY1", second.sha16)),
            "file name must end with _<monitor>_<sha16>.jpg: {}",
            second.path
        );

        let _ = fs::remove_dir_all(media);
    }

    #[test]
    fn per_monitor_dedup_is_independent() {
        let media = temp_media("per-monitor");
        let db = Db::open_in_memory().expect("db");
        let mut store = FrameStore::new(media.clone(), DEFAULT_JPEG_QUALITY);

        // Same content, two monitors, same second: two files, two rows
        // (the monitor label keeps the names apart).
        let m1 = store
            .store_frame("DISPLAY1", &frame(7), at(), &db, "dev")
            .expect("store m1")
            .expect("m1 written");
        let m2 = store
            .store_frame("DISPLAY2", &frame(7), at(), &db, "dev")
            .expect("store m2")
            .expect("m2 written despite identical content");
        assert_ne!(m1.path, m2.path, "monitor labels must keep names apart");

        // Monitor 1 repeats its previous content: deduped. Monitor 2
        // changes: written. Their dedup chains must not interfere.
        let m1_again = store
            .store_frame("DISPLAY1", &frame(7), at(), &db, "dev")
            .expect("store m1 again");
        assert_eq!(m1_again, None, "m1 unchanged -> deduped against m1");
        let m2_changed = store
            .store_frame("DISPLAY2", &frame(8), at(), &db, "dev")
            .expect("store m2 changed")
            .expect("m2 change written");
        assert_ne!(m2_changed.sha16, m2.sha16);

        assert_eq!(walk_jpegs(&media), 3);
        assert_eq!(db.event_count().expect("count"), 3);
        // Every index row names its monitor.
        for row in db.fetch_unsynced_events(10).expect("rows") {
            let data: serde_json::Value = serde_json::from_str(&row.data).expect("json");
            assert!(
                data["monitor"] == "DISPLAY1" || data["monitor"] == "DISPLAY2",
                "unexpected monitor field: {data}"
            );
        }

        let _ = fs::remove_dir_all(media);
    }

    fn walk_jpegs(media: &Path) -> usize {
        let mut count = 0;
        let mut stack = vec![media.join("frames")];
        while let Some(dir) = stack.pop() {
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if p.extension().is_some_and(|e| e == "jpg") {
                        count += 1;
                    }
                }
            }
        }
        count
    }
}
