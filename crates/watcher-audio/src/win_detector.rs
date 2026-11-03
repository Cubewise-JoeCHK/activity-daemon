//! Conferencing detector (Windows only, `#[cfg(windows)]`).
//!
//! Preferred mode (`audio-session`): enumerate the active WASAPI audio
//! sessions of every render device (`IAudioSessionManager2` /
//! `IAudioSessionEnumerator`, wrapped by the `wasapi` crate as
//! `AudioSessionManager` / `AudioSessionEnumerator`) and match the owning
//! process image name against `audio.process_list`. `SessionState::Active`
//! means the session actually has a running stream - the app is in a call,
//! not merely running.
//!
//! Fallback mode (`process-running`): when session enumeration fails for a
//! poll (no audio device, restricted COM, driver trouble), check running
//! processes (Toolhelp32 snapshot) against the same list. May over-trigger;
//! the mode in use is returned with every hit and logged by the caller so
//! the active mode is always visible.

use wasapi::{DeviceEnumerator, Direction, SessionState};

use crate::detect::{process_matches, ConferenceHit, ConferenceWatcher, DetectionMode};
use crate::logging::Log;

/// Guard so one poll cannot take forever on exotic session lists.
const MAX_SESSIONS_PER_DEVICE: i32 = 512;
const MAX_DEVICES_PER_POLL: usize = 32;

/// Conferencing detector for Windows.
pub struct ConferencingDetector {
    /// Normalized lowercase process names, all ending in `.exe`.
    wanted: Vec<String>,
    /// Tracks mode transitions so changes are logged once.
    last_mode: Option<DetectionMode>,
    log: Log,
}

impl ConferencingDetector {
    pub fn new(process_list: &[String], log: Log) -> ConferencingDetector {
        let mut wanted: Vec<String> = process_list
            .iter()
            .map(|n| crate::detect::normalize_config_name(n))
            .collect();
        wanted.sort();
        wanted.dedup();
        ConferencingDetector {
            wanted,
            last_mode: None,
            log,
        }
    }

    fn matches(&self, image_name: &str) -> bool {
        process_matches(&self.wanted, image_name)
    }

    /// Preferred: active audio sessions on the render devices.
    fn poll_audio_sessions(&mut self) -> anyhow::Result<Option<ConferenceHit>> {
        let enumerator = DeviceEnumerator::new()?;
        let collection = enumerator.get_device_collection(&Direction::Render)?;
        let mut checked = 0;
        for device in &collection {
            if checked >= MAX_DEVICES_PER_POLL {
                break;
            }
            let Ok(device) = device else { continue };
            checked += 1;
            let Ok(manager) = device.get_iaudiosessionmanager() else {
                continue;
            };
            let Ok(sessions) = manager.get_audiosessionenumerator() else {
                continue;
            };
            let Ok(count) = sessions.get_count() else {
                continue;
            };
            for i in 0..count.min(MAX_SESSIONS_PER_DEVICE) {
                let Ok(control) = sessions.get_session(i) else {
                    continue;
                };
                let Ok(state) = control.get_state() else {
                    continue;
                };
                if state != SessionState::Active {
                    continue;
                }
                let Ok(pid) = control.get_process_id() else {
                    continue;
                };
                if let Some(name) = process_image_name(pid) {
                    if self.matches(&name) {
                        return Ok(Some(ConferenceHit {
                            process: name,
                            mode: DetectionMode::AudioSession,
                        }));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Fallback: configured process is running (regardless of audio).
    fn poll_running_processes(&mut self) -> anyhow::Result<Option<ConferenceHit>> {
        for entry in running_process_names()? {
            if self.matches(&entry) {
                return Ok(Some(ConferenceHit {
                    process: entry,
                    mode: DetectionMode::ProcessFallback,
                }));
            }
        }
        Ok(None)
    }
}

impl ConferenceWatcher for ConferencingDetector {
    fn poll(&mut self) -> Option<ConferenceHit> {
        let hit = match self.poll_audio_sessions() {
            Ok(hit) => hit,
            Err(e) => {
                // Preferred mode unavailable for this poll: fall back and
                // make the degraded mode visible in the log.
                let fallback = self.poll_running_processes().unwrap_or(None);
                let mode = fallback.as_ref().map(|h| h.mode);
                if self.last_mode != mode {
                    self.log.warn(&format!(
                        "audio-session detection unavailable ({e:#}); \
                         using process-running fallback (may over-trigger)"
                    ));
                }
                self.last_mode = mode;
                return fallback;
            }
        };
        // Session enumeration worked this poll; remember healthy mode.
        self.last_mode = Some(DetectionMode::AudioSession);
        hit
    }
}

/// Query the image name (basename) of a process id via
/// `OpenProcess` + `QueryFullProcessImageNameW`. `None` if the process
/// exited meanwhile or access is denied.
pub fn process_image_name(pid: u32) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return None;
    }
    // SAFETY: plain Win32 handle use, handle closed on all paths.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return None;
        };
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(HANDLE(handle.0));
        if result.is_err() || size == 0 {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..size as usize]);
        let basename = full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string();
        Some(basename)
    }
}

/// All active audio sessions across render devices as (pid, image name).
/// Debugging helper backing `examples/detect_probe.rs`.
pub fn active_session_processes() -> anyhow::Result<Vec<(u32, String)>> {
    let enumerator = DeviceEnumerator::new()?;
    let collection = enumerator.get_device_collection(&Direction::Render)?;
    let mut out = Vec::new();
    for device in &collection {
        let Ok(device) = device else { continue };
        let Ok(manager) = device.get_iaudiosessionmanager() else {
            continue;
        };
        let Ok(sessions) = manager.get_audiosessionenumerator() else {
            continue;
        };
        let Ok(count) = sessions.get_count() else {
            continue;
        };
        for i in 0..count.min(MAX_SESSIONS_PER_DEVICE) {
            let Ok(control) = sessions.get_session(i) else {
                continue;
            };
            let Ok(state) = control.get_state() else {
                continue;
            };
            if state != SessionState::Active {
                continue;
            }
            let Ok(pid) = control.get_process_id() else {
                continue;
            };
            let name = process_image_name(pid).unwrap_or_else(|| "<unknown>".to_string());
            out.push((pid, name));
        }
    }
    Ok(out)
}

/// Snapshot of running process image basenames (Toolhelp32).
pub fn running_process_names() -> anyhow::Result<Vec<String>> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: snapshot handle is closed on all paths; entry struct is
    // initialized via default() then dwSize is set as required.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
        let mut names = Vec::new();
        let mut entry = PROCESSENTRY32W {
            dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(u32::MAX),
            ..Default::default()
        };
        if Process32FirstW(HANDLE(snapshot.0), &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                names.push(String::from_utf16_lossy(&entry.szExeFile[..len]));
                if Process32NextW(HANDLE(snapshot.0), &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(HANDLE(snapshot.0));
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_normalizes_and_matches_config_list() {
        let d = ConferencingDetector::new(
            &[
                "Teams.exe".to_string(),
                "teams".to_string(),
                "Zoom.exe".to_string(),
            ],
            Log::stderr_only(),
        );
        // Normalized duplicates collapse, keep sorted order.
        assert_eq!(
            d.wanted,
            vec!["teams.exe".to_string(), "zoom.exe".to_string()]
        );
        assert!(d.matches("Teams.exe"));
        assert!(!d.matches(r"C:\bin\ms-teams.exe"));
        assert!(!d.matches("Skype.exe"));
        assert_eq!(d.last_mode, None, "no poll yet");
    }

    #[test]
    fn empty_process_list_never_matches() {
        let d = ConferencingDetector::new(&[], Log::stderr_only());
        assert_eq!(d.wanted.len(), 0);
        assert!(!d.matches("Teams.exe"));
    }

    #[test]
    fn running_process_names_works_on_this_machine() {
        // Smoke test: the snapshot must list at least our own process.
        let names = running_process_names().expect("toolhelp snapshot");
        assert!(!names.is_empty());
    }
}
