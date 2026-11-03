//! Frame storage: JPEG encoding, sha16 dedup, on-disk layout and `media`
//! bucket index rows (issue #5, ADR media contract).
//!
//! Layout: `{data_dir}/media/frames/<YYYY>/<MM>/<DD>/<HHMMSS>_<sha16>.jpg`
//! (local time). Each written file is indexed as one event in the `media`
//! bucket with `{ kind, path, bytes, sha16, producer }`; `path` is relative
//! to the data dir with forward slashes so records stay machine-portable.

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
    /// JPEG size in bytes (index row `bytes`).
    pub bytes: u64,
    /// Content hash prefix (index row `sha16`).
    pub sha16: String,
}

/// Deduplicating frame store: encode -> hash -> skip or write + index.
pub struct FrameStore {
    /// The `media` root, e.g. `<data_dir>/media`.
    media_root: PathBuf,
    quality: u8,
    last_sha16: Option<String>,
}

impl FrameStore {
    pub fn new(media_root: PathBuf, quality: u8) -> FrameStore {
        FrameStore {
            media_root,
            quality,
            last_sha16: None,
        }
    }

    pub fn media_root(&self) -> &Path {
        &self.media_root
    }

    /// Encode `frame` and store it unless its `sha16` matches the previous
    /// frame (duplicate content is not written at all - no file, no index
    /// row). On write, returns the [`StoredFrame`]; on dedup, `Ok(None)`.
    pub fn store_frame(
        &mut self,
        frame: &Frame,
        at: DateTime<Local>,
        db: &Db,
        device: &str,
    ) -> anyhow::Result<Option<StoredFrame>> {
        let jpeg = encode_jpeg(frame, self.quality)?;
        let sha16 = sha16(&jpeg);
        if self.last_sha16.as_deref() == Some(sha16.as_str()) {
            return Ok(None);
        }

        let (year, month, day, hhmmss) = time_parts(at);
        let rel = format!("media/frames/{year}/{month}/{day}/{hhmmss}_{sha16}.jpg");
        let full = self
            .media_root
            .join("frames")
            .join(&year)
            .join(&month)
            .join(&day)
            .join(format!("{hhmmss}_{sha16}.jpg"));
        fs::create_dir_all(full.parent().expect("path has a parent"))?;
        fs::write(&full, &jpeg)?;

        let stored = StoredFrame {
            path: rel.clone(),
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
                "producer": PRODUCER,
            }),
        };
        db.insert_event(&event, device)?;
        self.last_sha16 = Some(sha16.clone());

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
    fn store_writes_layout_and_media_index_row() {
        let media = temp_media("layout");
        let db = Db::open_in_memory().expect("db");
        let mut store = FrameStore::new(media.clone(), DEFAULT_JPEG_QUALITY);

        let stored = store
            .store_frame(&frame(1), at(), &db, "dev-laptop")
            .expect("store")
            .expect("first frame is written, not deduped");

        // Layout: media/frames/YYYY/MM/DD/HHMMSS_<sha16>.jpg
        let sha = &stored.sha16;
        assert_eq!(
            stored.path,
            format!("media/frames/2026/11/04/091500_{sha}.jpg")
        );
        let full = media
            .join("frames")
            .join("2026")
            .join("11")
            .join("04")
            .join(format!("091500_{sha}.jpg"));
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
        assert_eq!(data["producer"], "watcher-screen");

        let _ = fs::remove_dir_all(media);
    }

    #[test]
    fn identical_frame_is_skipped_entirely() {
        let media = temp_media("dedup");
        let db = Db::open_in_memory().expect("db");
        let mut store = FrameStore::new(media.clone(), DEFAULT_JPEG_QUALITY);

        let first = store
            .store_frame(&frame(7), at(), &db, "dev-laptop")
            .expect("store 1")
            .expect("first written");
        let second = store
            .store_frame(&frame(7), at(), &db, "dev-laptop")
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
            .store_frame(&frame(7), at(), &db, "dev")
            .expect("store 1")
            .is_some());
        let second = store
            .store_frame(&frame(8), at(), &db, "dev")
            .expect("store 2")
            .expect("changed frame written");
        // Distinct sha16s must never collide on one file.
        assert_eq!(walk_jpegs(&media), 2);
        assert_eq!(db.event_count().expect("count"), 2);
        assert_eq!(
            second.path.split_once('_').expect("sha in name").1,
            format!("{}.jpg", second.sha16)
        );

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
