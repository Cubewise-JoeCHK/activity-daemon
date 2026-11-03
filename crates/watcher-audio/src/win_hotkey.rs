//! Global hotkey listener (Windows only, `#[cfg(windows)]`).
//!
//! Uses `RegisterHotKey` with no window: `WM_HOTKEY` messages are posted to
//! the thread's message queue and drained with `PeekMessageW`. **No window
//! is ever created** (issue #6: no visible UI ever).
//!
//! If registration fails (hotkey taken by another app, restricted session,
//! etc.) the thread logs a warning and exits - the daemon keeps running and
//! the `record.json` control-file channel stays fully functional.

use std::sync::mpsc::Sender;
use std::time::Duration;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_HOTKEY,
};

use crate::hotkey::HotKeySpec;
use crate::logging::Log;

/// Application-defined hotkey id (any nonzero value).
const HOTKEY_ID: i32 = 0xA06;
/// How often the message loop checks for shutdown.
const LOOP_SLEEP_MS: u64 = 150;

/// Map the parsed spec to Win32 modifier flags + virtual-key code.
fn to_win32(spec: &HotKeySpec) -> (HOT_KEY_MODIFIERS, u32) {
    let mut mods = HOT_KEY_MODIFIERS(0);
    if spec.ctrl {
        mods |= MOD_CONTROL;
    }
    if spec.shift {
        mods |= MOD_SHIFT;
    }
    if spec.alt {
        mods |= MOD_ALT;
    }
    if spec.win {
        mods |= MOD_WIN;
    }
    (mods, spec.vk_code())
}

/// Human-readable hotkey description for logs.
fn describe(spec: &HotKeySpec) -> String {
    let mut parts: Vec<String> = Vec::new();
    if spec.ctrl {
        parts.push("Ctrl".to_string());
    }
    if spec.shift {
        parts.push("Shift".to_string());
    }
    if spec.alt {
        parts.push("Alt".to_string());
    }
    if spec.win {
        parts.push("Win".to_string());
    }
    parts.push(spec.key.to_uppercase());
    parts.join("+")
}

/// Spawn the hotkey thread. Sends one message on every hotkey press.
/// Returns the sender handle to keep the channel alive (dropping it is also
/// fine; the thread exits on shutdown).
pub fn spawn_hotkey(
    spec: HotKeySpec,
    tx: Sender<()>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    log: Log,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("watcher-audio-hotkey".to_string())
        .spawn(move || run_hotkey(spec, tx, shutdown, log))
        .expect("spawn hotkey thread")
}

fn run_hotkey(
    spec: HotKeySpec,
    tx: Sender<()>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    log: Log,
) {
    let (mods, vk) = to_win32(&spec);
    // SAFETY: RegisterHotKey with a NULL hwnd posts WM_HOTKEY to this
    // thread's queue; the id is our own constant. No window is involved.
    let registered = unsafe { RegisterHotKey(None, HOTKEY_ID, mods, vk) };
    if let Err(e) = registered {
        log.warn(&format!(
            "hotkey registration failed for '{}' ({e}); hotkey disabled, the CLI control file still works",
            describe(&spec)
        ));
        return;
    }
    log.info(&format!(
        "hotkey registered: {} (ctrl={} shift={} alt={} win={})",
        describe(&spec),
        spec.ctrl,
        spec.shift,
        spec.alt,
        spec.win
    ));

    let mut msg = MSG::default();
    while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
        let has_message =
            unsafe { PeekMessageW(&mut msg, Some(HWND::default()), 0, 0, PM_REMOVE) }.as_bool();
        if has_message {
            if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == HOTKEY_ID {
                if tx.send(()).is_err() {
                    break; // run loop gone
                }
            } else {
                // SAFETY: standard message dispatch; queue can contain other
                // posted messages (e.g. WM_QUIT from the system).
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(LOOP_SLEEP_MS));
        }
    }
    // SAFETY: matching unregister for the registration above.
    unsafe {
        let _ = UnregisterHotKey(None, HOTKEY_ID);
    }
    log.info("hotkey listener stopped");
}
