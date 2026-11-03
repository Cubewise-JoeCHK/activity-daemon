//! Foreground sampling sources: the OS seam between the desktop and the
//! pure [`SessionCoalescer`](crate::SessionCoalescer).
//!
//! The trait is implemented by [`WindowsSource`] (Win32) on Windows. On
//! every platform the end-to-end test drives the production loop with a
//! scripted fake source (see [`run_with_source`](crate::run_with_source)).

use crate::sample::Sample;

/// Produces one [`Sample`] per poll. Implementations must never panic and
/// must degrade gracefully: a failing OS call logs a warning and yields a
/// degraded sample (empty app / title, idle 0) rather than aborting the
/// capture loop - a flaky API call must never kill the daemon.
pub trait ForegroundSource {
    /// Take one sample now.
    fn sample(&mut self) -> Sample;
}

#[cfg(windows)]
mod win32 {
    use std::time::SystemTime;

    use log::{trace, warn};
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HWND};
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };

    use crate::sample::Sample;
    use crate::source::ForegroundSource;

    /// Polls the real Windows desktop: on each call it returns the current
    /// foreground window's process name + title and the user input idle
    /// time, stamped with the wall-clock time (`SystemTime::now`) so sleep /
    /// hibernate gaps stay visible to the coalescer.
    ///
    /// Sampling cadence (1 s by convention) is owned by the capture loop in
    /// [`run`](crate::run), not by the source.
    pub struct WindowsSource;

    impl WindowsSource {
        pub fn new() -> Self {
            WindowsSource
        }
    }

    impl Default for WindowsSource {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ForegroundSource for WindowsSource {
        fn sample(&mut self) -> Sample {
            let ts = SystemTime::now();
            let (app, title) = foreground_window();
            Sample {
                ts,
                app,
                title,
                input_idle_s: input_idle_s(),
            }
        }
    }

    /// Foreground window title + process exe name. Both degrade to empty
    /// strings on failure. UIPI may truncate titles of elevated windows -
    /// accepted and traced, not an error.
    fn foreground_window() -> (String, String) {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_invalid() {
            // No foreground window right now (locked workstation, secure
            // desktop, ...).
            trace!("no foreground window (secure desktop or locked session)");
            return (String::new(), String::new());
        }
        (process_image_name(hwnd), window_title(hwnd))
    }

    fn window_title(hwnd: HWND) -> String {
        let mut buf = [0u16; 512];
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if len <= 0 {
            // Empty title, or UIPI blocked the read (elevated window).
            trace!("GetWindowTextW returned no title for the foreground window");
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }

    /// Exe basename of the window's process via
    /// OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) + QueryFullProcessImageNameW.
    /// Falls back to the empty string on any failure (e.g. access denied for
    /// protected processes).
    fn process_image_name(hwnd: HWND) -> String {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0 {
            warn!("GetWindowThreadProcessId failed for the foreground window");
            return String::new();
        }

        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(handle) if !handle.is_invalid() => handle,
            Ok(_) => {
                warn!("OpenProcess returned an invalid handle for pid {pid}");
                return String::new();
            }
            Err(e) => {
                warn!("OpenProcess failed for pid {pid}: {e}");
                return String::new();
            }
        };

        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let result = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        };
        if let Err(e) = unsafe { CloseHandle(handle) } {
            warn!("CloseHandle failed for pid {pid}: {e}");
        }

        match result {
            Ok(()) => {
                let full = String::from_utf16_lossy(&buf[..len as usize]);
                full.rsplit(['\\', '/'])
                    .next()
                    .unwrap_or_default()
                    .to_string()
            }
            Err(e) => {
                warn!("QueryFullProcessImageNameW failed for pid {pid}: {e}");
                String::new()
            }
        }
    }

    /// Seconds since the last keyboard/mouse input via GetLastInputInfo.
    /// Failure degrades to 0 (active) - a broken measurement must not invent
    /// AFK time.
    fn input_idle_s() -> u64 {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            warn!("GetLastInputInfo failed; treating input as active");
            return 0;
        }
        let now_ms = unsafe { GetTickCount() };
        // u32 millisecond ticks wrap every ~49.7 days; wrapping_sub keeps the
        // difference correct across one wrap.
        let idle_ms = now_ms.wrapping_sub(info.dwTime);
        u64::from(idle_ms / 1000)
    }
}

#[cfg(windows)]
pub use win32::WindowsSource;
