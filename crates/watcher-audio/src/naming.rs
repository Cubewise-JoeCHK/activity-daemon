//! File naming and layout for recordings (ADR #16: media lives under
//! `media/<kind>/<YYYY>/<MM>/<DD>/`, so audio goes to
//! `{data_dir}/media/audio/<YYYY>/<MM>/<DD>/<start_ts>_<trigger>_<stream>.wav`).
//!
//! Pure and cross-platform; unit-tested everywhere.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Local};

use crate::machine::Trigger;
use crate::recording::StreamKind;

/// Sanitize a process name for use in file names / event labels:
/// strip a trailing `.exe`, lowercase, map anything outside `[a-z0-9]` to
/// `-`, collapse repeats, trim. Empty results become `app`.
pub fn sanitize_process(raw: &str) -> String {
    let stem = raw.strip_suffix(".exe").unwrap_or(raw);
    let mut out = String::with_capacity(stem.len());
    let mut last_dash = false;
    for ch in stem.chars() {
        let mapped = if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            Some(ch)
        } else if ch.is_ascii_uppercase() {
            Some(ch.to_ascii_lowercase())
        } else {
            None
        };
        match mapped {
            Some(c) => {
                out.push(c);
                last_dash = false;
            }
            None if !last_dash && !out.is_empty() => {
                out.push('-');
                last_dash = true;
            }
            None => {}
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "app".to_string()
    } else {
        trimmed
    }
}

/// The trigger label used in file names and events:
/// `manual` or `auto_<sanitized process>`.
pub fn trigger_label(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Manual => "manual".to_string(),
        Trigger::Auto(process) => format!("auto_{}", sanitize_process(process)),
    }
}

/// `{media_root}/<YYYY>/<MM>/<DD>` for the given local timestamp.
pub fn date_dir(media_root: &Path, local: &DateTime<Local>) -> PathBuf {
    media_root
        .join(format!("{:04}", local.year()))
        .join(format!("{:02}", local.month()))
        .join(format!("{:02}", local.day()))
}

/// File-name-safe local timestamp: `20261104T091432`.
pub fn timestamp_slug(local: &DateTime<Local>) -> String {
    local.format("%Y%m%dT%H%M%S").to_string()
}

/// Build the recording base path (without stream suffix and extension):
/// `{media_root}/<YYYY>/<MM>/<DD>/<start_ts>_<trigger>`.
pub fn base_path(media_root: &Path, start_local: &DateTime<Local>, trigger: &Trigger) -> PathBuf {
    date_dir(media_root, start_local).join(format!(
        "{}_{}",
        timestamp_slug(start_local),
        trigger_label(trigger)
    ))
}

/// Final file path for one stream of a recording:
/// `<base>_<mic|loopback>.wav`.
pub fn stream_file(base: &Path, stream: StreamKind) -> PathBuf {
    let name = base
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    base.with_file_name(format!("{name}_{}.wav", stream.label()))
}

/// The in-progress twin of a final file: `<final>.part`.
pub fn part_path(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Path of a finished file relative to the data dir, with forward slashes
/// (this is what goes into the `media` index rows).
pub fn relative_media_path(data_dir: &Path, final_path: &Path) -> String {
    final_path
        .strip_prefix(data_dir)
        .unwrap_or(final_path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn sanitize_handles_config_style_names() {
        assert_eq!(sanitize_process("Teams.exe"), "teams");
        assert_eq!(sanitize_process("ms-teams.exe"), "ms-teams");
        assert_eq!(sanitize_process("Zoom.exe"), "zoom");
        assert_eq!(sanitize_process("Cisco Webex.exe"), "cisco-webex");
        assert_eq!(sanitize_process("Discord.exe"), "discord");
        assert_eq!(sanitize_process("123 App.exe"), "123-app");
        assert_eq!(sanitize_process("***.exe"), "app");
        assert_eq!(sanitize_process(""), "app");
    }

    #[test]
    fn trigger_labels_for_files_and_events() {
        assert_eq!(trigger_label(&Trigger::Manual), "manual");
        assert_eq!(
            trigger_label(&Trigger::Auto("ms-teams.exe".into())),
            "auto_ms-teams"
        );
    }

    #[test]
    fn date_dir_follows_adr_layout() {
        let root = Path::new("/data/media/audio");
        let d = date_dir(root, &ts(2026, 11, 4, 9, 14, 32));
        assert_eq!(d, Path::new("/data/media/audio/2026/11/04"));
    }

    #[test]
    fn base_and_stream_files_match_adr_pattern() {
        let root = Path::new("/data/media/audio");
        let start = ts(2026, 11, 4, 9, 14, 32);
        let base = base_path(root, &start, &Trigger::Manual);
        assert_eq!(
            base,
            Path::new("/data/media/audio/2026/11/04/20261104T091432_manual")
        );
        assert_eq!(
            stream_file(&base, StreamKind::Mic),
            Path::new("/data/media/audio/2026/11/04/20261104T091432_manual_mic.wav")
        );
        let base_auto = base_path(root, &start, &Trigger::Auto("Zoom.exe".into()));
        assert_eq!(
            stream_file(&base_auto, StreamKind::Loopback),
            Path::new("/data/media/audio/2026/11/04/20261104T091432_auto_zoom_loopback.wav")
        );
    }

    #[test]
    fn part_path_appends_suffix() {
        let p = part_path(Path::new("/data/x/rec_mic.wav"));
        assert_eq!(p, Path::new("/data/x/rec_mic.wav.part"));
    }

    #[test]
    fn relative_media_path_uses_forward_slashes() {
        let data = Path::new("/data");
        let f = Path::new("/data/media/audio/2026/11/04/rec_mic.wav");
        assert_eq!(
            relative_media_path(data, f),
            "media/audio/2026/11/04/rec_mic.wav"
        );
        // Outside the data dir: falls back to the full path string.
        assert_eq!(
            relative_media_path(data, Path::new("/elsewhere/x.wav")),
            "/elsewhere/x.wav"
        );
    }
}
