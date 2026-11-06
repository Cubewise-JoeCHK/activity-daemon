//! Pure planning logic for the forwarder, unit-tested without any real
//! I/O beyond temp files: media path parsing, row planning, batch building
//! (file-before-index ordering), ack-to-id mapping, backoff schedule and
//! URL joining.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use activity_core::{Event, EventRow, BUCKET_MEDIA};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// First backoff delay.
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Backoff cap (5 minutes).
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// The pieces of a `media/<kind>/<YYYY>/<MM>/<DD>/<filename>` relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRef {
    pub kind: String,
    /// `YYYY-MM-DD` (the server route takes a single date segment).
    pub date: String,
    pub filename: String,
}

/// Single path component: non-empty ASCII alphanumerics plus `.` `-` `_`,
/// never `.` or `..` (mirrors the server's validation).
fn valid_component(s: &str) -> bool {
    if s.is_empty() || s == "." || s == ".." {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Parse the `data.path` of a media index row into the route parameters for
/// `PUT /ingest/media/{kind}/{date}/{filename}`. Only the exact
/// `media/<kind>/<YYYY>/<MM>/<DD>/<filename>` shape is accepted - anything
/// else (traversal, wrong depth, non-numeric date) is unusable.
pub fn parse_media_rel_path(rel: &str) -> Option<MediaRef> {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() != 6 || parts[0] != "media" {
        return None;
    }
    let (kind, y, m, d, filename) = (parts[1], parts[2], parts[3], parts[4], parts[5]);
    if !valid_component(kind) || !valid_component(filename) {
        return None;
    }
    let numeric = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if !(y.len() == 4 && m.len() == 2 && d.len() == 2 && numeric(y) && numeric(m) && numeric(d)) {
        return None;
    }
    Some(MediaRef {
        kind: kind.to_string(),
        date: format!("{y}-{m}-{d}"),
        filename: filename.to_string(),
    })
}

/// One unsynced row turned into a forwardable unit.
#[derive(Debug, Clone, PartialEq)]
pub enum PlannedRow {
    /// A plain event: goes straight into the next POST batch.
    Plain { row_id: i64, event: Event },
    /// A media index event: its file must be uploaded first; only after a
    /// successful upload does the event join the POST batch.
    Media {
        row_id: i64,
        event: Event,
        file: PathBuf,
        media: MediaRef,
    },
}

/// Turn unsynced rows into planned units. Media rows take their file path
/// from `data.path` (relative to `data_dir`); a row that cannot be parsed
/// is a local-storage integrity problem and fails the whole round (logged
/// by the caller, retried next interval - never marked synced).
pub fn plan_rows(rows: &[EventRow], data_dir: &Path) -> Result<Vec<PlannedRow>, String> {
    rows.iter()
        .map(|row| {
            let event = Event::try_from(row)
                .map_err(|e| format!("row {}: corrupt stored event: {e}", row.id))?;
            if row.bucket != BUCKET_MEDIA {
                return Ok(PlannedRow::Plain {
                    row_id: row.id,
                    event,
                });
            }
            let rel = event
                .data
                .get("path")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("row {}: media event without data.path", row.id))?;
            let media = parse_media_rel_path(&rel)
                .ok_or_else(|| format!("row {}: unusable media path '{rel}'", row.id))?;
            Ok(PlannedRow::Media {
                row_id: row.id,
                event,
                file: data_dir.join(rel),
                media,
            })
        })
        .collect()
}

/// Build the POST batch: all plain rows plus exactly those media rows whose
/// files were uploaded this round (`uploaded_ids`), preserving row order.
/// This is where "file first, index row after" is enforced.
pub fn build_post_batch(planned: &[PlannedRow], uploaded_ids: &[i64]) -> Vec<(i64, Event)> {
    planned
        .iter()
        .filter_map(|p| match p {
            PlannedRow::Plain { row_id, event } => Some((*row_id, event.clone())),
            PlannedRow::Media { row_id, event, .. } => {
                if uploaded_ids.contains(row_id) {
                    Some((*row_id, event.clone()))
                } else {
                    None
                }
            }
        })
        .collect()
}

/// Server ack for `POST /ingest/events`:
/// `{"accepted": n, "rejected": [{"index": i, "reason": "..."}]}` where
/// `index` refers to positions in the posted `events` array.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IngestAck {
    pub accepted: usize,
    #[serde(default)]
    pub rejected: Vec<RejectedItem>,
}

/// One rejected event in the ack.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RejectedItem {
    pub index: usize,
    pub reason: String,
}

/// Map the ack's rejected indices back to local row ids: everything posted
/// that was not rejected counts as accepted and may be marked synced.
pub fn accepted_row_ids(posted: &[(i64, Event)], ack: &IngestAck) -> Vec<i64> {
    let rejected: HashSet<usize> = ack.rejected.iter().map(|r| r.index).collect();
    posted
        .iter()
        .enumerate()
        .filter(|(i, _)| !rejected.contains(i))
        .map(|(_, (id, _))| *id)
        .collect()
}

/// Server ack for `PUT /ingest/media/{kind}/{date}/{filename}` (sync
/// contract v2, issue #40):
/// `{"stored": "media/...", "bytes": n, "sha256": "<lowercase hex>"}`.
/// `sha256` echoes the checksum of the bytes the hub durably wrote (the
/// ack is sent only after the fsync). It is optional so a pre-v2 hub (ack
/// without a checksum) still parses - such an ack is never treated as
/// verified, so nothing is deleted locally.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MediaPutAck {
    pub stored: String,
    pub bytes: u64,
    #[serde(default)]
    pub sha256: Option<String>,
}

/// True when the media PUT ack proves the hub durably stored exactly the
/// bytes the client hashed: the ack carries a checksum and it matches
/// `local_sha256_hex` (case-insensitive compare; both sides emit lowercase,
/// a hand-rolled hub may not).
pub fn ack_verifies_upload(ack: &MediaPutAck, local_sha256_hex: &str) -> bool {
    ack.sha256
        .as_deref()
        .is_some_and(|sha| sha.eq_ignore_ascii_case(local_sha256_hex))
}

/// Exponential backoff: `base * 2^attempt`, saturated and capped.
/// `attempt` starts at 0 (= `base`).
pub fn backoff_delay(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let mut delay = base;
    for _ in 0..attempt {
        delay = delay.saturating_mul(2);
        if delay >= cap {
            return cap;
        }
    }
    delay.min(cap)
}

/// Add jitter to a backoff delay: the result is uniformly reduced into
/// `[delay/2, delay]` given a caller-supplied `rand01` in `[0, 1)`
/// (deterministic and unit-testable; the loop feeds it a weak entropy
/// source).
pub fn apply_jitter(delay: Duration, rand01: f64) -> Duration {
    let r = rand01.clamp(0.0, 1.0);
    delay.mul_f64(0.5 + 0.5 * r)
}

/// Join a base URL and a path fragment (`/ingest/...`), tolerating a
/// trailing slash on the base.
pub fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// URL for a media upload.
pub fn media_put_url(base: &str, media: &MediaRef) -> String {
    join_url(
        base,
        &format!(
            "ingest/media/{}/{}/{}",
            media.kind, media.date, media.filename
        ),
    )
}

/// Lowercase hex of `bytes`.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Size and sha256 (lowercase hex) of a file in one pass; the hash goes out
/// in the `X-SHA256` header so the server can verify the stored bytes.
pub fn sha256_file_hex(path: &Path) -> std::io::Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut len = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    Ok((len, to_hex(&hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media_row(id: i64, rel: &str) -> EventRow {
        EventRow {
            id,
            bucket: BUCKET_MEDIA.to_string(),
            ts_start: "2026-11-04T09:14:32+08:00".to_string(),
            duration_s: 0.0,
            data: serde_json::json!({ "kind": "screen", "path": rel, "bytes": 10, "sha16": "aa", "producer": "watcher-screen" })
                .to_string(),
            device: "laptop".to_string(),
            synced: false,
        }
    }

    fn plain_row(id: i64) -> EventRow {
        EventRow {
            id,
            bucket: "window-watcher".to_string(),
            ts_start: "2026-11-04T09:14:32+08:00".to_string(),
            duration_s: 1.0,
            data: serde_json::json!({ "app": "Code.exe" }).to_string(),
            device: "laptop".to_string(),
            synced: false,
        }
    }

    #[test]
    fn parse_media_rel_path_accepts_the_canonical_shape() {
        let m = parse_media_rel_path("media/screen/2026/11/03/frame-000123.jpg")
            .expect("canonical path");
        assert_eq!(m.kind, "screen");
        assert_eq!(m.date, "2026-11-03");
        assert_eq!(m.filename, "frame-000123.jpg");
    }

    #[test]
    fn parse_media_rel_path_rejects_everything_else() {
        for bad in [
            "",
            "media",
            "media/screen",                           // too short
            "media/screen/2026/11/frame.jpg",         // 5 segments
            "media/screen/2026/11/03/sub/frame.jpg",  // 7 segments
            "notmedia/screen/2026/11/03/frame.jpg",   // wrong root
            "media/../2026/11/03/passwd.jpg",         // traversal in kind
            "media/../etc/passwd",                    // traversal, short
            "media/screen/2026-11/03/04/frame.jpg",   // wrong date shape
            "media/screen/20a6/11/03/frame.jpg",      // non-numeric year
            "media/screen/2026/11/03/../frame.jpg",   // traversal in filename
            "media/screen/2026/11/03/",               // empty filename
            "/etc/passwd",                            // absolute path
            "C:/data/media/screen/2026/11/03/f.jpg",  // windows absolute
            "media\\screen\\2026\\11\\03\\frame.jpg", // backslashes: 1 segment
            "media/screen/2026/11/03/.hidden",        // hmm - dotfiles? see below
        ] {
            let got = parse_media_rel_path(bad);
            if bad.ends_with(".hidden") {
                // '.' and '-' and '_' are legal filename characters
                assert!(got.is_some(), "dotfile names are legal: {bad}");
            } else {
                assert!(got.is_none(), "must reject: {bad}");
            }
        }
    }

    #[test]
    fn plan_rows_builds_plain_and_media_units() {
        let dir = Path::new("/data");
        let rows = vec![
            plain_row(1),
            media_row(2, "media/audio/2026/11/03/meet.m4a"),
        ];
        let planned = plan_rows(&rows, dir).expect("plan");
        assert_eq!(planned.len(), 2);
        assert!(matches!(&planned[0], PlannedRow::Plain { row_id: 1, .. }));
        match &planned[1] {
            PlannedRow::Media {
                row_id,
                file,
                media,
                ..
            } => {
                assert_eq!(*row_id, 2);
                assert_eq!(file, &dir.join("media/audio/2026/11/03/meet.m4a"));
                assert_eq!(media.date, "2026-11-03");
            }
            _ => panic!("expected media row"),
        }
    }

    #[test]
    fn plan_rows_rejects_media_rows_without_path() {
        let mut row = media_row(3, "media/screen/2026/11/03/x.jpg");
        row.data = serde_json::json!({ "kind": "screen" }).to_string();
        assert!(plan_rows(&[row], Path::new("/data")).is_err());
    }

    #[test]
    fn build_post_batch_orders_by_row_and_excludes_failed_uploads() {
        let rows = vec![
            plain_row(1),
            media_row(2, "media/screen/2026/11/03/a.jpg"), // file missing
            plain_row(3),
            media_row(4, "media/screen/2026/11/03/b.jpg"), // uploaded ok
        ];
        let planned = plan_rows(&rows, Path::new("/data")).expect("plan");
        let batch = build_post_batch(&planned, &[4]);
        let ids: Vec<i64> = batch.iter().map(|(id, _)| *id).collect();
        // id 2 excluded: its file was not uploaded (file first, index after)
        assert_eq!(ids, vec![1, 3, 4]);
    }

    #[test]
    fn accepted_row_ids_maps_indices_back() {
        let planned = plan_rows(
            &[plain_row(7), plain_row(8), plain_row(9), plain_row(10)],
            Path::new("/data"),
        )
        .expect("plan");
        let posted = build_post_batch(&planned, &[]);
        let ack = IngestAck {
            accepted: 3,
            rejected: vec![RejectedItem {
                index: 2,
                reason: "bad timestamp".to_string(),
            }],
        };
        let ids = accepted_row_ids(&posted, &ack);
        assert_eq!(ids, vec![7, 8, 10]);
    }

    #[test]
    fn media_put_ack_parses_v2_and_v1_bodies() {
        let v2: MediaPutAck = serde_json::from_str(
            r#"{"stored":"media/screen/2026/11/03/f.jpg","bytes":10,
                "sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}"#,
        )
        .expect("v2 ack");
        assert_eq!(
            v2.sha256.as_deref(),
            Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
        );
        // pre-v2 hub: no checksum in the ack
        let v1: MediaPutAck =
            serde_json::from_str(r#"{"stored":"media/x","bytes":1}"#).expect("v1 ack");
        assert_eq!(v1.sha256, None);
        assert!(serde_json::from_str::<MediaPutAck>("{\"bytes\":1}").is_err());
    }

    #[test]
    fn ack_verifies_upload_rules() {
        let local = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let ack = |sha: Option<&str>| MediaPutAck {
            stored: "media/x".to_string(),
            bytes: 5,
            sha256: sha.map(str::to_string),
        };
        assert!(ack_verifies_upload(&ack(Some(local)), local));
        assert!(ack_verifies_upload(
            &ack(Some(&local.to_ascii_uppercase())),
            local
        ));
        // wrong checksum, missing checksum: never verified
        assert!(!ack_verifies_upload(&ack(Some(&"0".repeat(64))), local));
        assert!(!ack_verifies_upload(&ack(None), local));
    }

    #[test]
    fn backoff_schedule_doubles_then_caps() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(300);
        assert_eq!(backoff_delay(0, base, cap), Duration::from_secs(1));
        assert_eq!(backoff_delay(1, base, cap), Duration::from_secs(2));
        assert_eq!(backoff_delay(2, base, cap), Duration::from_secs(4));
        assert_eq!(backoff_delay(5, base, cap), Duration::from_secs(32));
        assert_eq!(backoff_delay(9, base, cap), cap); // 512s capped to 300s
        assert_eq!(backoff_delay(5000, base, cap), cap); // no overflow, no spin
    }

    #[test]
    fn jitter_shrinks_into_half_open_range() {
        let d = Duration::from_secs(100);
        assert_eq!(apply_jitter(d, 0.0), Duration::from_secs(50));
        assert_eq!(apply_jitter(d, 1.0), d);
        assert_eq!(apply_jitter(d, 0.5), Duration::from_secs(75));
        assert_eq!(apply_jitter(d, 42.0), d); // clamped
        assert_eq!(apply_jitter(d, -1.0), Duration::from_secs(50));
    }

    #[test]
    fn urls_are_joined_and_tolerate_slashes() {
        assert_eq!(
            join_url("http://hub:8080", "/ingest/events"),
            "http://hub:8080/ingest/events"
        );
        assert_eq!(
            join_url("http://hub:8080/", "ingest/events"),
            "http://hub:8080/ingest/events"
        );
        let m = parse_media_rel_path("media/screen/2026/11/03/f.jpg").expect("media");
        assert_eq!(
            media_put_url("http://hub:8080", &m),
            "http://hub:8080/ingest/media/screen/2026-11-03/f.jpg"
        );
    }

    #[test]
    fn sha256_file_hex_matches_known_vector() {
        let dir = std::env::temp_dir().join(format!(
            "activity-sync-sha-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("hello.bin");
        std::fs::write(&path, b"hello").expect("write");
        let (len, hex) = sha256_file_hex(&path).expect("hash");
        assert_eq!(len, 5);
        // well-known sha256 of "hello"
        assert_eq!(
            hex,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn to_hex_is_lowercase() {
        assert_eq!(to_hex(&[0x0a, 0xff, 0x00]), "0aff00");
    }
}
