//! System tray icon for the daemon (issue #30, Windows only).
//!
//! The owner want: the daemon never shows a console window; its presence is
//! the tray icon in the bottom-right corner instead.
//!
//! - A message-only window + `Shell_NotifyIconW` on a dedicated thread with
//!   its own Win32 message loop (the `windows` crate; no new dependencies).
//! - Tooltip = version + health: `activity-daemon v0.1.2: active` /
//!   `recording` (the latest `meeting-audio` event says a recording is
//!   running) / `heartbeat stale` (no or stale heartbeat file - refreshed
//!   every second).
//! - Icon = the same health as a color, from the repo icon set (issue #35):
//!   the eye's pupil is teal when active, red when recording, amber when
//!   the heartbeat is stale (assets/README.md). The taskbar theme picks the
//!   dark- or light-bg variant once at startup; the size follows the
//!   system small-icon metric.
//! - Right-click menu: `Open dashboard` (the hub web UI at the configured
//!   `[sync] server_url`, opened in the default browser via ShellExecuteW -
//!   no URL to remember), `Update now` (a one-shot flag the run loop
//!   consumes - the same update path as the manual `update` command) and
//!   `Quit` (sets the same [`SHUTDOWN`] flag as Ctrl+C - graceful worker
//!   join). Left click is the same one-click dashboard shortcut (the pause
//!   switch stays with issue #14).
//! - The tray only ever *reads* daemon state and *writes* the two atomics;
//!   it never touches the workers directly, so a tray failure can never
//!   take the daemon down (spawn failures are logged and skipped).
//!
//! Pure logic ([`TrayState`], [`tray_state`], [`tray_tooltip`],
//! [`latest_meeting_audio_is_recording`], the embedded PNG set and its
//! helpers) is cfg-free and unit-tested on every platform; every Win32 call
//! is cfg(windows) so the ubuntu CI leg stays green.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use activity_core::Event;

/// Decoded icon handle for the tray slots (null on decode failure).
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::HICON;

/// One-shot `Update now` request from the tray menu; the run loop consumes
/// it with [`take_update_request`] and runs the same update path as a
/// manual `update`.
static TRAY_UPDATE_REQUEST: AtomicBool = AtomicBool::new(false);

/// Take the pending tray update request (exactly once; `false` = none).
pub fn take_update_request() -> bool {
    TRAY_UPDATE_REQUEST.swap(false, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Pure logic (tested on every platform)
// ---------------------------------------------------------------------------

/// The health state the tray tooltip reports. On non-Windows the tray is a
/// compile-clean no-op, so these pure helpers exist for the tests only
/// there (the bin target would otherwise flag them as dead code).
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// Running normally, no recording active.
    Active,
    /// A meeting recording is running (latest `meeting-audio` = `start`).
    Recording,
    /// No heartbeat file, or it is older than the staleness threshold.
    HeartbeatStale,
}

/// Pure health decision: recording wins over a stale heartbeat (a recording
/// is the more actionable signal; the heartbeat is the daemon's base proof
/// of life, reported when no recording runs).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn tray_state(recording: bool, heartbeat_stale: bool) -> TrayState {
    if recording {
        TrayState::Recording
    } else if heartbeat_stale {
        TrayState::HeartbeatStale
    } else {
        TrayState::Active
    }
}

/// The daemon version the tray reports (`CARGO_PKG_VERSION` of the CLI
/// crate, the same string `--version` prints).
#[cfg_attr(not(windows), allow(dead_code))]
pub const TRAY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tooltip text for a tray state, carrying the daemon version (kept short:
/// the Shell tooltip budget is 127 characters).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn tray_tooltip(state: TrayState, version: &str) -> String {
    let label = match state {
        TrayState::Active => "active",
        TrayState::Recording => "recording",
        TrayState::HeartbeatStale => "heartbeat stale",
    };
    format!("activity-daemon v{version}: {label}")
}

/// Whether the latest `meeting-audio` event (events ordered oldest-first,
/// as `query_events` returns them) says a recording is running: the newest
/// event's `state` is `start`. Malformed payloads are skipped.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn latest_meeting_audio_is_recording(events: &[Event]) -> bool {
    events
        .iter()
        .filter_map(|event| event.data.get("state").and_then(|s| s.as_str()))
        .next_back()
        .is_some_and(|state| state == "start")
}

/// The dashboard URL the tray opens, derived from `[sync] server_url` (the
/// hub serves the web UI at its root). `None` when the setting is blank or
/// whitespace (sync not configured - no menu item, left click stays a
/// no-op). A trailing slash is trimmed so the browser lands on `/` cleanly.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn dashboard_url(server_url: &str) -> Option<String> {
    let trimmed = server_url.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.trim_end_matches('/').to_string())
    }
}

// ---------------------------------------------------------------------------
// Embedded state icons (issue #35). The repo icon set (assets/README.md) is
// a watchful-eye glyph whose pupil color carries the tray health state -
// teal active, red recording, amber stale - in a dark-bg and a light-bg
// variant per taskbar theme. The 16/20/24/32 px PNGs are embedded verbatim;
// at tray startup the size closest to the system small-icon metric is
// decoded with CreateIconFromResourceEx, which takes a PNG payload verbatim
// (the Vista+ RT_ICON resource format) - still the old zero-dependency
// decode path, just fed the new art.
// ---------------------------------------------------------------------------

/// PNG sizes present in the asset set (assets/tray/*-{16,20,24,32}.png).
#[cfg_attr(not(windows), allow(dead_code))]
const TRAY_SIZES: [usize; 4] = [16, 20, 24, 32];

/// The dark-bg tray PNGs, `[state][size]`: Active / Recording /
/// HeartbeatStale x 16/20/24/32 px.
#[cfg_attr(not(windows), allow(dead_code))]
const TRAY_PNG_DARK: [[&[u8]; 4]; 3] = [
    [
        include_bytes!("../../../assets/tray/dark-bg/active-16.png"),
        include_bytes!("../../../assets/tray/dark-bg/active-20.png"),
        include_bytes!("../../../assets/tray/dark-bg/active-24.png"),
        include_bytes!("../../../assets/tray/dark-bg/active-32.png"),
    ],
    [
        include_bytes!("../../../assets/tray/dark-bg/recording-16.png"),
        include_bytes!("../../../assets/tray/dark-bg/recording-20.png"),
        include_bytes!("../../../assets/tray/dark-bg/recording-24.png"),
        include_bytes!("../../../assets/tray/dark-bg/recording-32.png"),
    ],
    [
        include_bytes!("../../../assets/tray/dark-bg/stale-16.png"),
        include_bytes!("../../../assets/tray/dark-bg/stale-20.png"),
        include_bytes!("../../../assets/tray/dark-bg/stale-24.png"),
        include_bytes!("../../../assets/tray/dark-bg/stale-32.png"),
    ],
];

/// The light-bg tray PNGs, same layout as [`TRAY_PNG_DARK`].
#[cfg_attr(not(windows), allow(dead_code))]
const TRAY_PNG_LIGHT: [[&[u8]; 4]; 3] = [
    [
        include_bytes!("../../../assets/tray/light-bg/active-16.png"),
        include_bytes!("../../../assets/tray/light-bg/active-20.png"),
        include_bytes!("../../../assets/tray/light-bg/active-24.png"),
        include_bytes!("../../../assets/tray/light-bg/active-32.png"),
    ],
    [
        include_bytes!("../../../assets/tray/light-bg/recording-16.png"),
        include_bytes!("../../../assets/tray/light-bg/recording-20.png"),
        include_bytes!("../../../assets/tray/light-bg/recording-24.png"),
        include_bytes!("../../../assets/tray/light-bg/recording-32.png"),
    ],
    [
        include_bytes!("../../../assets/tray/light-bg/stale-16.png"),
        include_bytes!("../../../assets/tray/light-bg/stale-20.png"),
        include_bytes!("../../../assets/tray/light-bg/stale-24.png"),
        include_bytes!("../../../assets/tray/light-bg/stale-32.png"),
    ],
];

/// Icon array slot for a tray state (the tray keeps one decoded icon per
/// state and swaps between them on transitions).
#[cfg_attr(not(windows), allow(dead_code))]
fn state_index(state: TrayState) -> usize {
    match state {
        TrayState::Active => 0,
        TrayState::Recording => 1,
        TrayState::HeartbeatStale => 2,
    }
}

/// Asset-set size closest to `metric` (ties pick the smaller side: the
/// shell scales a small icon up more gracefully than it scales a big one
/// down).
#[cfg_attr(not(windows), allow(dead_code))]
fn nearest_tray_size(metric: usize) -> usize {
    let mut best = TRAY_SIZES[0];
    for size in TRAY_SIZES {
        if metric.abs_diff(size) < metric.abs_diff(best) {
            best = size;
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

/// Tray icon id inside our message-only window.
#[cfg(windows)]
const TRAY_ICON_ID: u32 = 1;
/// Callback message the Shell sends for tray mouse events (WM_APP + 1).
#[cfg(windows)]
const WM_TRAY_CALLBACK: u32 = 0x8000 + 1;
/// Teardown request posted from the shutdown path (removes the icon before
/// the process exits instead of leaving a ghost icon until mouse-over).
#[cfg(windows)]
const WM_TRAY_TEARDOWN: u32 = 0x8000 + 2;
/// Health refresh / shutdown-watch timer id and cadence.
#[cfg(windows)]
const TIMER_ID: usize = 1;
#[cfg(windows)]
const TIMER_INTERVAL_MS: u32 = 1000;
/// Popup menu command ids.
#[cfg(windows)]
const MENU_OPEN_DASHBOARD: u32 = 1;
#[cfg(windows)]
const MENU_UPDATE_NOW: u32 = 2;
#[cfg(windows)]
const MENU_QUIT: u32 = 3;

/// HWND of the live tray window (0 = none). Set by the wndproc on
/// WM_CREATE, cleared on WM_DESTROY; lets the daemon shutdown path ask the
/// tray thread to remove the icon cleanly.
#[cfg(windows)]
static TRAY_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Per-tray state, kept in a thread_local (the wndproc and the message loop
/// run on the same thread - no locking needed, and `Db` is !Send across
/// threads by contract anyway).
#[cfg(windows)]
struct TrayCtx {
    data_dir: PathBuf,
    device: String,
    /// Hub web UI base URL (`[sync] server_url`); `None` = no dashboard
    /// menu item and left click stays a no-op.
    dashboard_url: Option<String>,
    shutdown: &'static AtomicBool,
    /// Own Db connection (same threading contract as the four workers:
    /// each thread opens its own; recording-state reads never block them).
    db: Option<activity_core::Db>,
    last_state: Option<TrayState>,
    /// One decoded icon per state (slot = [`state_index`]); the refresh
    /// swaps the icon together with the tooltip on a state transition.
    icons: [HICON; 3],
}

#[cfg(windows)]
thread_local! {
    static TRAY_CTX: std::cell::RefCell<Option<TrayCtx>> =
        const { std::cell::RefCell::new(None) };
}

/// Spawn the tray thread (Windows). `None` everywhere else; any Win32
/// failure inside is logged and the daemon runs on without a tray.
pub fn spawn_tray(
    data_dir: PathBuf,
    device: String,
    dashboard_url: Option<String>,
    shutdown: &'static AtomicBool,
) -> Option<JoinHandle<()>> {
    #[cfg(windows)]
    {
        std::thread::Builder::new()
            .name("tray".to_string())
            .spawn(move || {
                if let Err(e) =
                    win_impl::run_message_loop(data_dir, device, dashboard_url, shutdown)
                {
                    log::warn!("tray icon unavailable (continuing without it): {e:#}");
                }
            })
            .ok()
    }
    #[cfg(not(windows))]
    {
        // No tray on non-Windows; parameters exist for signature parity.
        let _ = (data_dir, device, dashboard_url, shutdown);
        None
    }
}

/// Best-effort icon removal during daemon shutdown: posts a teardown
/// message so the tray thread destroys its window (which deletes the icon)
/// before the process exits.
pub fn teardown_window() {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

        let value = TRAY_HWND.swap(0, Ordering::SeqCst);
        if value != 0 {
            // SAFETY: posting a plain message to the stored hwnd; the value
            // is only ever set by the tray thread's own WM_CREATE.
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(value as *mut core::ffi::c_void)),
                    WM_TRAY_TEARDOWN,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

#[cfg(windows)]
mod win_impl {
    //! Every Win32 call of the tray lives here so nothing leaks into the
    //! cross-platform module above (ubuntu CI compiles this module away).

    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::anyhow;
    use chrono::{Local, SecondsFormat};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    use windows::Win32::UI::Shell::{
        ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
        NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
        KillTimer, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer,
        TrackPopupMenu, TranslateMessage, HICON, HWND_MESSAGE, LR_DEFAULTCOLOR, MF_STRING, MSG,
        SM_CXSMICON, SM_CYSMICON, SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
        WM_CREATE, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
    };

    use super::{
        latest_meeting_audio_is_recording, nearest_tray_size, state_index, tray_state,
        tray_tooltip, MenuCommand, TrayCtx, TrayState, MENU_OPEN_DASHBOARD, MENU_QUIT,
        MENU_UPDATE_NOW, TIMER_ID, TIMER_INTERVAL_MS, TRAY_CTX, TRAY_HWND, TRAY_ICON_ID,
        TRAY_PNG_DARK, TRAY_PNG_LIGHT, TRAY_SIZES, TRAY_UPDATE_REQUEST, TRAY_VERSION,
        WM_TRAY_CALLBACK, WM_TRAY_TEARDOWN,
    };
    use crate::heartbeat::{self, HeartbeatState};
    use crate::usage_log;
    use activity_core::Db;

    /// Run the tray thread body: register the class, create the
    /// message-only window, add the icon, then pump messages until the
    /// teardown path ends the loop.
    pub(super) fn run_message_loop(
        data_dir: std::path::PathBuf,
        device: String,
        dashboard_url: Option<String>,
        shutdown: &'static AtomicBool,
    ) -> anyhow::Result<()> {
        // Own Db connection for recording-state queries (best effort; the
        // same threading contract as the four workers).
        let db = open_tray_db(&data_dir.join("activity.db"));
        if db.is_none() {
            log::warn!("tray: cannot open activity.db; tooltip falls back to the heartbeat signal");
        }

        // One decoded icon per state (theme + small-icon metric picked here,
        // once; a theme/DPI change applies on the next daemon start).
        let icons = tray_icon_handles();

        TRAY_CTX.with(|cell| {
            *cell.borrow_mut() = Some(TrayCtx {
                data_dir,
                device,
                dashboard_url,
                shutdown,
                db,
                last_state: None,
                icons,
            })
        });

        let result = pump_messages(icons);
        TRAY_CTX.with(|cell| *cell.borrow_mut() = None);
        result
    }

    fn pump_messages(icons: [HICON; 3]) -> anyhow::Result<()> {
        // Register the window class. A zero atom usually means the class
        // already exists (a previous daemon generation in this process);
        // CreateWindowExW below is the real gate.
        let class_name = wide("ActivityDaemonTrayWindow");
        let class = WNDCLASSW {
            lpfnWndProc: Some(tray_wndproc),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };
        // SAFETY: `class` and the name buffer outlive the registration call.
        if unsafe { RegisterClassW(&class) } == 0 {
            log::debug!("tray: window class already registered; continuing");
        }

        // Message-only window: never visible, hosts the tray icon messages.
        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                PCWSTR::from_raw(class_name.as_ptr()),
                PCWSTR::from_raw(wide("activity-daemon tray").as_ptr()),
                Default::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .map_err(|e| anyhow!("cannot create the tray window: {e}"))?;

        // Add the icon. A NIM_ADD failure (e.g. Explorer not running) ends
        // the tray thread - the daemon itself never depends on the tray.
        // The active icon leads; the first timer tick swaps in the real
        // state within a second.
        let mut nid = base_notify_icon_data(hwnd);
        nid.uFlags |= NIF_ICON;
        nid.hIcon = icons[state_index(TrayState::Active)];
        // SAFETY: nid references our own window/id; no pointers kept.
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool() {
            let _ = unsafe { DestroyWindow(hwnd) };
            return Err(anyhow!(
                "Shell_NotifyIconW(NIM_ADD) failed (Explorer not running?)"
            ));
        }

        // Initial tooltip + version 3 semantics (the callback lParam carries
        // the mouse message), then the once-per-second health timer.
        nid.uFlags |= NIF_TIP;
        set_tip(
            &mut nid,
            &tray_tooltip(tray_state(false, false), TRAY_VERSION),
        );
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
            nid.Anonymous.uVersion = 3;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
            SetTimer(Some(hwnd), TIMER_ID, TIMER_INTERVAL_MS, None);
        }
        log::info!(
            "tray icon installed (Open dashboard / Update now / Quit; left click = dashboard)"
        );

        let mut msg = MSG::default();
        loop {
            // SAFETY: plain message-loop calls on our own thread.
            let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if r.0 <= 0 {
                // 0 = WM_QUIT (clean); negative = error. Either way: stop.
                break;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }

    /// The tray window procedure: tray callback (left click no-op, right
    /// click menu), per-second timer (health refresh + shutdown watch),
    /// teardown request, icon cleanup on destroy.
    unsafe extern "system" fn tray_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            TRAY_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        match msg {
            WM_TRAY_CALLBACK => {
                // lParam carries the mouse message (version 3 semantics).
                let mouse = (lparam.0 & 0xFFFF) as u32;
                if mouse == WM_RBUTTONUP {
                    show_context_menu(hwnd);
                } else if mouse == WM_LBUTTONUP {
                    // One-click dashboard shortcut (the menu item is the
                    // discoverable twin; the pause switch stays with #14).
                    open_dashboard();
                }
                LRESULT(0)
            }
            WM_TRAY_TEARDOWN => {
                // Graceful removal requested by the daemon shutdown path.
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_TIMER => {
                // The RefCell borrow must NOT span DestroyWindow (WM_DESTROY
                // re-enters this wndproc synchronously on the same thread).
                let wants_destroy = TRAY_CTX.with(|cell| {
                    let mut slot = cell.borrow_mut();
                    match slot.as_mut() {
                        // Menu open (ctx taken out): skip this tick.
                        None => false,
                        Some(ctx) if ctx.shutdown.load(Ordering::Relaxed) => true,
                        Some(ctx) => {
                            refresh_tray(ctx);
                            false
                        }
                    }
                });
                if wants_destroy {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                // Delete the icon (hWnd + uID identify it; deliberately no
                // thread_local access - WM_DESTROY can re-enter from the
                // paths above), stop the timer, end the message loop.
                let nid = base_notify_icon_data(hwnd);
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                    let _ = KillTimer(Some(hwnd), TIMER_ID);
                }
                TRAY_HWND.store(0, Ordering::SeqCst);
                PostQuitMessage(0);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    /// Build + track the right-click context menu (`Open dashboard` when a
    /// dashboard URL is configured / `Update now` / `Quit`). The tray
    /// context is taken out of its thread_local for the modal
    /// `TrackPopupMenu` call so a concurrent timer tick cannot double-borrow
    /// it; the menu action runs after the menu returns.
    unsafe fn show_context_menu(hwnd: HWND) {
        let Some(ctx) = TRAY_CTX.with(|cell| cell.borrow_mut().take()) else {
            return;
        };

        let menu = match CreatePopupMenu() {
            Ok(menu) => menu,
            Err(e) => {
                log::warn!("tray: cannot create the context menu: {e}");
                TRAY_CTX.with(|cell| *cell.borrow_mut() = Some(ctx));
                return;
            }
        };
        if ctx.dashboard_url.is_some() {
            let dashboard_label = wide("Open dashboard");
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                MENU_OPEN_DASHBOARD as usize,
                PCWSTR::from_raw(dashboard_label.as_ptr()),
            );
        }
        let update_label = wide("Update now");
        let quit_label = wide("Quit");
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_UPDATE_NOW as usize,
            PCWSTR::from_raw(update_label.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_QUIT as usize,
            PCWSTR::from_raw(quit_label.as_ptr()),
        );

        // The SetForegroundWindow + WM_NULL dance is the documented way to
        // make TrackPopupMenu dismiss on an outside click.
        let _ = SetForegroundWindow(hwnd);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let choice = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));

        match MenuCommand::from_id(u32::try_from(choice.0).unwrap_or(0)) {
            Some(MenuCommand::OpenDashboard) => {
                // User-initiated: logs {tray, open-dashboard}
                // (fire-and-forget), then opens the hub web UI. Uses the
                // taken-out ctx directly - TRAY_CTX is empty while the
                // menu is open.
                open_dashboard_from_ctx(&ctx);
            }
            Some(MenuCommand::UpdateNow) => {
                // User-initiated: logs {tray, update-now} (fire-and-forget),
                // then asks the run loop for an update via the one-shot flag
                // (same code path as a manual `update`).
                usage_log::record(
                    &ctx.data_dir,
                    &ctx.device,
                    usage_log::FEATURE_TRAY,
                    usage_log::ACTION_UPDATE_NOW,
                );
                TRAY_UPDATE_REQUEST.store(true, Ordering::Relaxed);
                log::info!("tray: update requested");
            }
            Some(MenuCommand::Quit) => {
                // User-initiated: logs {tray, quit}, then requests the same
                // graceful shutdown as Ctrl+C (workers join, sessions flush).
                usage_log::record(
                    &ctx.data_dir,
                    &ctx.device,
                    usage_log::FEATURE_TRAY,
                    usage_log::ACTION_QUIT,
                );
                ctx.shutdown.store(true, Ordering::Relaxed);
                log::info!("tray: quit requested");
            }
            None => {}
        }

        TRAY_CTX.with(|cell| *cell.borrow_mut() = Some(ctx));
    }

    /// Open the hub web UI in the default browser (ShellExecuteW "open" -
    /// zero dependencies, the same verb Explorer uses). Shared by both
    /// entry points (menu item + left click); records the usage row here so
    /// each activation logs exactly once. A blank `[sync] server_url` is a
    /// logged no-op.
    fn open_dashboard_from_ctx(ctx: &TrayCtx) {
        let Some(url) = &ctx.dashboard_url else {
            log::info!("tray: no dashboard URL configured ([sync] server_url is empty)");
            return;
        };
        usage_log::record(
            &ctx.data_dir,
            &ctx.device,
            usage_log::FEATURE_TRAY,
            usage_log::ACTION_OPEN_DASHBOARD,
        );
        let verb = wide("open");
        let target = wide(url);
        // SAFETY: all strings are NUL-terminated buffers that outlive the
        // call; the returned HINSTANCE (DDE handle) is discarded.
        unsafe {
            let _ = ShellExecuteW(
                None,
                PCWSTR::from_raw(verb.as_ptr()),
                PCWSTR::from_raw(target.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            );
        }
        log::info!("tray: dashboard opened ({url})");
    }

    /// Left-click entry point: forwards to [`open_dashboard_from_ctx`] via
    /// the thread_local (a no-op if the tray context is gone).
    fn open_dashboard() {
        let handled = TRAY_CTX.with(|cell| cell.borrow().as_ref().map(open_dashboard_from_ctx));
        if handled.is_none() {
            log::info!("tray: no dashboard URL configured ([sync] server_url is empty)");
        }
    }

    /// Null-terminated UTF-16 for Win32 string parameters (NUL included).
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Minimal NOTIFYICONDATAW identifying our icon (hWnd + uID are the
    /// primary key; everything else is only sent with the right uFlags).
    fn base_notify_icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>()).unwrap_or(u32::MAX),
            hWnd: hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_MESSAGE,
            uCallbackMessage: WM_TRAY_CALLBACK,
            ..Default::default()
        }
    }

    /// Decode one icon per tray state (issue #35): the taskbar theme picks
    /// the dark- or light-bg set and the system small-icon metric picks the
    /// embedded size. A decode failure leaves that state's slot blank (the
    /// menu and tooltip still work - the same degrade contract as before).
    fn tray_icon_handles() -> [HICON; 3] {
        // SAFETY: plain system-metric read, no preconditions.
        let metric = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(0) as usize;
        let size = nearest_tray_size(metric);
        let set = if taskbar_prefers_light() {
            &TRAY_PNG_LIGHT
        } else {
            &TRAY_PNG_DARK
        };
        let size_idx = TRAY_SIZES.iter().position(|s| *s == size).unwrap_or(0);
        let mut handles = [HICON::default(); 3];
        for (state, pngs) in set.iter().enumerate() {
            handles[state] = ico_from_png(pngs[size_idx]);
        }
        handles
    }

    /// Decode one embedded PNG at the system small-icon size (blank
    /// default on failure; the menu still works, only the slot art is
    /// empty). A PNG is the Vista+ RT_ICON resource format verbatim - the
    /// shell decodes it natively, no BMP conversion needed.
    pub(super) fn ico_from_png(png: &[u8]) -> HICON {
        // SAFETY: pure resource decode from our own bytes.
        match unsafe {
            CreateIconFromResourceEx(
                png,
                true,
                0x0003_0000,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            )
        } {
            Ok(icon) => icon,
            Err(e) => {
                log::warn!("tray: cannot decode the embedded icon ({e}); showing a blank slot");
                HICON::default()
            }
        }
    }

    /// Whether the system (taskbar) theme prefers light. Read once at
    /// startup: the tray sits on the taskbar, which follows
    /// SystemUsesLightTheme. Dark - the Windows default - is also the
    /// answer on any read failure or missing value.
    fn taskbar_prefers_light() -> bool {
        let subkey = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        let value = wide("SystemUsesLightTheme");
        let mut dword: u32 = 0;
        let mut len = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(0);
        // SAFETY: all buffers outlive the call; RRF_RT_REG_DWORD pins the
        // value type so a wrong-typed value fails instead of overflowing.
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR::from_raw(subkey.as_ptr()),
                PCWSTR::from_raw(value.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut dword as *mut u32).cast()),
                Some(&mut len),
            )
        };
        result == ERROR_SUCCESS && dword == 1
    }

    /// Copy `tip` into the NOTIFYICONDATAW tooltip slot (NUL-terminated,
    /// clipped to the 128-u16 field; our tooltips are ~26 characters).
    fn set_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
        let wide_tip = wide(tip);
        let len = wide_tip.len().min(nid.szTip.len());
        nid.szTip[..len].copy_from_slice(&wide_tip[..len]);
    }

    /// Refresh the tray from current daemon health (once per second from
    /// the timer; logs transitions): a state change swaps both the icon
    /// (pupil color = the state, issue #35) and the tooltip in one
    /// NIM_MODIFY.
    fn refresh_tray(ctx: &mut TrayCtx) {
        let state = current_tray_state(ctx);
        if ctx.last_state == Some(state) {
            return;
        }
        ctx.last_state = Some(state);
        log::info!("tray: {}", tray_tooltip(state, TRAY_VERSION));
        let value = TRAY_HWND.load(Ordering::SeqCst);
        if value == 0 {
            return;
        }
        let mut nid = base_notify_icon_data(HWND(value as *mut core::ffi::c_void));
        nid.uFlags |= NIF_ICON | NIF_TIP;
        nid.hIcon = ctx.icons[state_index(state)];
        set_tip(&mut nid, &tray_tooltip(state, TRAY_VERSION));
        // SAFETY: nid references our own icon (hWnd/uID); no pointers kept.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    /// Current health: recording (latest meeting-audio event = start) beats
    /// a stale heartbeat; both beat `active`. A missing heartbeat file also
    /// reads as stale - inside a running daemon that means the heartbeat
    /// writer died, which is exactly the stale condition.
    fn current_tray_state(ctx: &TrayCtx) -> TrayState {
        let recording = ctx.db.as_ref().map(latest_recording_now).unwrap_or(false);
        let stale = matches!(
            heartbeat::status(&ctx.data_dir),
            HeartbeatState::Stale { .. } | HeartbeatState::NotRunning
        );
        tray_state(recording, stale)
    }

    /// Latest `meeting-audio` state within the last week (one small query
    /// per second is negligible next to the watchers' own cadence).
    fn latest_recording_now(db: &Db) -> bool {
        let from =
            (Local::now() - chrono::Duration::days(7)).to_rfc3339_opts(SecondsFormat::Secs, false);
        match db.query_events(
            watcher_audio::events::BUCKET_MEETING_AUDIO,
            Some(&from),
            None,
        ) {
            Ok(events) => latest_meeting_audio_is_recording(&events),
            Err(_) => false,
        }
    }

    /// Open the tray's own Db connection with the same short retry loop the
    /// workers use (first-boot WAL contention); `None` = degraded tooltip.
    fn open_tray_db(db_path: &Path) -> Option<Db> {
        for _ in 0..20 {
            match Db::open_db(db_path) {
                Ok(db) => return Some(db),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
        None
    }
}

/// Which popup menu command `TrackPopupMenu` returned (`None` = dismissed
/// without a selection).
#[cfg(windows)]
#[derive(Debug, PartialEq)]
enum MenuCommand {
    OpenDashboard,
    UpdateNow,
    Quit,
}

#[cfg(windows)]
impl MenuCommand {
    fn from_id(id: u32) -> Option<MenuCommand> {
        match id {
            MENU_OPEN_DASHBOARD => Some(MenuCommand::OpenDashboard),
            MENU_UPDATE_NOW => Some(MenuCommand::UpdateNow),
            MENU_QUIT => Some(MenuCommand::Quit),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meeting_event(state: &str) -> Event {
        Event {
            bucket: "meeting-audio".to_string(),
            timestamp: "2026-11-04T09:14:32+08:00".to_string(),
            duration_s: 0.0,
            data: json!({"state": state, "trigger": "manual"}),
            device: "dev-laptop".to_string(),
        }
    }

    #[test]
    fn tray_states_and_tooltips() {
        assert_eq!(tray_state(false, false), TrayState::Active);
        assert_eq!(tray_state(true, false), TrayState::Recording);
        assert_eq!(tray_state(false, true), TrayState::HeartbeatStale);
        // Recording wins over a stale heartbeat.
        assert_eq!(tray_state(true, true), TrayState::Recording);

        assert_eq!(
            tray_tooltip(TrayState::Active, "1.2.3"),
            "activity-daemon v1.2.3: active"
        );
        assert_eq!(
            tray_tooltip(TrayState::Recording, "1.2.3"),
            "activity-daemon v1.2.3: recording"
        );
        assert_eq!(
            tray_tooltip(TrayState::HeartbeatStale, "1.2.3"),
            "activity-daemon v1.2.3: heartbeat stale"
        );
        // The production tooltip reports the CLI crate version (what
        // `--version` prints).
        assert_eq!(TRAY_VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!TRAY_VERSION.is_empty());
        // Shell tooltip budget is 127 characters.
        for state in [
            TrayState::Active,
            TrayState::Recording,
            TrayState::HeartbeatStale,
        ] {
            assert!(
                tray_tooltip(state, TRAY_VERSION).chars().count() < 128,
                "tooltip must fit the Shell budget: {state:?}"
            );
        }
    }

    #[test]
    fn latest_meeting_audio_event_decides_recording() {
        assert!(!latest_meeting_audio_is_recording(&[]));

        assert!(latest_meeting_audio_is_recording(&[meeting_event("start")]));
        assert!(!latest_meeting_audio_is_recording(&[meeting_event("stop")]));
        assert!(latest_meeting_audio_is_recording(&[
            meeting_event("start"),
            meeting_event("stop"),
            meeting_event("start"),
        ]));
        assert!(!latest_meeting_audio_is_recording(&[
            meeting_event("start"),
            meeting_event("stop"),
        ]));

        // Events without a parseable `state` are skipped; the newest event
        // that carries one decides.
        let mut no_state = meeting_event("start");
        no_state.data = json!({"trigger": "manual"});
        assert!(!latest_meeting_audio_is_recording(&[
            no_state.clone(),
            meeting_event("stop"),
        ]));
        assert!(latest_meeting_audio_is_recording(&[
            meeting_event("stop"),
            no_state.clone(),
            meeting_event("start"),
        ]));
        let garbage = Event {
            bucket: "meeting-audio".to_string(),
            timestamp: "2026-11-04T09:14:32+08:00".to_string(),
            duration_s: 0.0,
            data: json!("not-an-object"),
            device: "dev-laptop".to_string(),
        };
        assert!(latest_meeting_audio_is_recording(&[
            meeting_event("start"),
            garbage
        ]));
    }

    #[test]
    fn update_request_flag_is_one_shot() {
        assert!(!take_update_request());
        TRAY_UPDATE_REQUEST.store(true, Ordering::Relaxed);
        assert!(take_update_request());
        assert!(!take_update_request(), "exactly once");
    }

    #[test]
    fn dashboard_url_derives_from_sync_server_url() {
        assert_eq!(
            dashboard_url("http://activity-hub:8080"),
            Some("http://activity-hub:8080".to_string())
        );
        // trailing slash is trimmed so the browser lands on `/` cleanly
        assert_eq!(
            dashboard_url("http://activity-hub:8080/"),
            Some("http://activity-hub:8080".to_string())
        );
        // surrounding whitespace tolerated, interior kept verbatim
        assert_eq!(
            dashboard_url("  http://hub:8080/tailscale  "),
            Some("http://hub:8080/tailscale".to_string())
        );
        // blank / whitespace-only: no dashboard (no menu item, no-op click)
        assert_eq!(dashboard_url(""), None);
        assert_eq!(dashboard_url("   "), None);
    }

    #[cfg(windows)]
    #[test]
    fn menu_command_ids_map() {
        use super::{MENU_OPEN_DASHBOARD, MENU_QUIT, MENU_UPDATE_NOW};
        assert_eq!(
            MenuCommand::from_id(MENU_OPEN_DASHBOARD),
            Some(MenuCommand::OpenDashboard)
        );
        assert_eq!(
            MenuCommand::from_id(MENU_UPDATE_NOW),
            Some(MenuCommand::UpdateNow)
        );
        assert_eq!(MenuCommand::from_id(MENU_QUIT), Some(MenuCommand::Quit));
        assert_eq!(MenuCommand::from_id(0), None, "dismissed menu");
        assert_eq!(MenuCommand::from_id(99), None, "unknown id");
    }

    #[test]
    fn tray_png_set_is_complete() {
        // The whole asset set is embedded, laid out [state][size] per the
        // constants, and every entry is a real PNG.
        for set in [&TRAY_PNG_DARK, &TRAY_PNG_LIGHT] {
            assert_eq!(set.len(), 3, "three states: active / recording / stale");
            for pngs in set {
                assert_eq!(pngs.len(), TRAY_SIZES.len());
                for png in pngs {
                    assert!(png.len() > 8);
                    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
                }
            }
        }
    }

    #[test]
    fn nearest_tray_size_picks_the_closest_side() {
        // Midpoints 18 / 22 / 28; ties pick the smaller side (strict <).
        assert_eq!(nearest_tray_size(0), 16);
        assert_eq!(nearest_tray_size(16), 16);
        assert_eq!(nearest_tray_size(18), 16);
        assert_eq!(nearest_tray_size(19), 20);
        assert_eq!(nearest_tray_size(22), 20);
        assert_eq!(nearest_tray_size(23), 24);
        assert_eq!(nearest_tray_size(28), 24);
        assert_eq!(nearest_tray_size(29), 32);
        assert_eq!(nearest_tray_size(1000), 32);
    }

    #[cfg(windows)]
    #[test]
    fn all_state_icons_decode() {
        // The real gate for the tray: every embedded PNG, both themes, all
        // sizes, decodes through the exact production path (the shell's
        // CreateIconFromResourceEx) into a non-null icon.
        for set in [&TRAY_PNG_DARK, &TRAY_PNG_LIGHT] {
            for pngs in set {
                for png in pngs {
                    let icon = win_impl::ico_from_png(png);
                    assert!(!icon.is_invalid(), "PNG must decode into an icon");
                }
            }
        }
    }
}
