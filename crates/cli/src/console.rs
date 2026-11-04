//! Console-window hiding for `run` mode (issue #30).
//!
//! The owner want: the daemon must never show a console window; presence is
//! shown as a system tray icon instead (see [`crate::tray`]). But a
//! developer starting `activity-daemon run` from cmd.exe wants to see the
//! usual startup lines - so `run.hide_console` config (`auto` default)
//! decides:
//!
//! - `auto`: hide unless the **parent process** is a visible terminal
//!   (cmd.exe / powershell.exe / pwsh.exe / WindowsTerminal.exe). Explorer
//!   double-click, the Startup-folder fallback (#29), the Task Scheduler and
//!   detached relaunches all launch with a non-terminal parent -> hidden.
//! - `always` / `never`: force the decision.
//!
//! The parent image name comes from a Toolhelp32 snapshot (same pattern as
//! watcher-audio's conferencing detector, re-implemented locally so the cli
//! crate stays decoupled). The classification is a pure function tested on
//! every platform; only the final `ShowWindow(SW_HIDE)` call is cfg(windows).

use activity_core::HideConsole;

/// Image names that count as a *visible* developer terminal in `auto` mode
/// (lowercase; matched against the parent process image name).
const VISIBLE_TERMINALS: [&str; 4] = [
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "windowsterminal.exe",
];

/// Pure `auto` decision: should the console window be hidden, given the
/// parent process image name (`None` = parent unknown or already exited)?
/// A missing parent means the daemon was launched by a short-lived process
/// (explorer shell invoke, wscript from the Startup-folder fallback, the
/// Task Scheduler, a detached relaunch) - definitely not a visible terminal.
pub fn should_hide(mode: HideConsole, parent_image: Option<&str>) -> bool {
    match mode {
        HideConsole::Always => true,
        HideConsole::Never => false,
        HideConsole::Auto => match parent_image {
            None => true,
            Some(name) => !is_visible_terminal(name),
        },
    }
}

/// Whether `image` is one of the visible developer terminals (pure,
/// case-insensitive, ignores any directory prefix).
pub fn is_visible_terminal(image: &str) -> bool {
    let name = image.rsplit(['\\', '/']).next().unwrap_or(image);
    VISIBLE_TERMINALS
        .iter()
        .any(|term| name.eq_ignore_ascii_case(term))
}

/// Apply the configured hide-console behavior for `run` mode. Returns
/// whether the console window was hidden (always `false` on non-Windows,
/// where there is no console window to hide). Never fails: a Win32 call
/// failing just means the decision could not be applied.
pub fn apply_hide_console(mode: HideConsole) -> bool {
    let parent = parent_process_image();
    let hide = should_hide(mode, parent.as_deref());
    if !hide {
        return false;
    }
    #[cfg(windows)]
    {
        hide_console_window()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Hide the current console window (`GetConsoleWindow` + `ShowWindow`).
#[cfg(windows)]
fn hide_console_window() -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Console::GetConsoleWindow;
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

    // SAFETY: GetConsoleWindow and ShowWindow take no pointers and are safe
    // to call on a thread that owns a console; a null hwnd means "no
    // console" and ShowWindow is skipped.
    unsafe {
        let hwnd = GetConsoleWindow();
        if hwnd == HWND::default() {
            return false;
        }
        // The BOOL result ("was previously visible") carries no signal we
        // care about: a failed hide is indistinguishable from an already
        // hidden window and harmless either way.
        let _ = ShowWindow(hwnd, SW_HIDE);
        true
    }
}

/// Image name of this process's parent, via the Toolhelp32 snapshot pattern
/// (`None` when the parent pid cannot be resolved or the parent already
/// exited - the snapshot race behind the wscript/explorer launches).
#[cfg(windows)]
fn parent_process_image() -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: the snapshot handle is closed on all paths; the entry struct
    // is zeroed via default() and dwSize set, as the Toolhelp contract
    // requires (same pattern as watcher-audio's process-name snapshot).
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let me = std::process::id();
        let mut entry = PROCESSENTRY32W {
            dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(u32::MAX),
            ..Default::default()
        };
        // One pass collects every (pid, parent_pid, image) tuple; the parent
        // entry can appear anywhere in the snapshot (order is not
        // guaranteed), so resolve it afterwards.
        let mut processes: Vec<(u32, u32, String)> = Vec::new();
        if Process32FirstW(HANDLE(snapshot.0), &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                processes.push((
                    entry.th32ProcessID,
                    entry.th32ParentProcessID,
                    String::from_utf16_lossy(&entry.szExeFile[..len]),
                ));
                if Process32NextW(HANDLE(snapshot.0), &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(HANDLE(snapshot.0));
        let parent_pid = processes
            .iter()
            .find(|(pid, _, _)| *pid == me)
            .map(|(_, parent, _)| *parent)?;
        processes
            .into_iter()
            .find(|(pid, _, _)| *pid == parent_pid)
            .map(|(_, _, name)| name)
    }
}

#[cfg(not(windows))]
fn parent_process_image() -> Option<String> {
    // Console-window hiding is a Windows concern; non-Windows always reports
    // "no parent image" (and `apply_hide_console` is a no-op anyway).
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_hides_only_for_non_terminal_parents() {
        // Visible terminals keep the console.
        assert!(!should_hide(HideConsole::Auto, Some("cmd.exe")));
        assert!(!should_hide(HideConsole::Auto, Some("powershell.exe")));
        assert!(!should_hide(HideConsole::Auto, Some("pwsh.exe")));
        assert!(!should_hide(HideConsole::Auto, Some("WindowsTerminal.exe")));
        assert!(!should_hide(
            HideConsole::Auto,
            Some(r"C:\WINDOWS\system32\cmd.exe")
        ));

        // Anything else hides: explorer, the scheduler, wscript (#29
        // fallback), a dead parent pid, detached relaunch parents.
        assert!(should_hide(HideConsole::Auto, Some("explorer.exe")));
        assert!(should_hide(HideConsole::Auto, Some("wscript.exe")));
        assert!(should_hide(HideConsole::Auto, Some("svchost.exe")));
        assert!(should_hide(HideConsole::Auto, Some("conhost.exe")));
        assert!(should_hide(HideConsole::Auto, Some("activity-daemon.exe")));
        assert!(should_hide(HideConsole::Auto, None));

        // always / never force the decision regardless of the parent.
        assert!(should_hide(HideConsole::Always, Some("cmd.exe")));
        assert!(should_hide(HideConsole::Always, None));
        assert!(!should_hide(HideConsole::Never, Some("explorer.exe")));
        assert!(!should_hide(HideConsole::Never, None));
    }

    #[test]
    fn visible_terminal_match_is_case_and_path_insensitive() {
        assert!(is_visible_terminal("CMD.EXE"));
        assert!(is_visible_terminal("Powershell.EXE"));
        assert!(is_visible_terminal(
            r"C:\Program Files\PowerShell\7\pwsh.exe"
        ));
        assert!(is_visible_terminal("windowsterminal.exe"));
        assert!(!is_visible_terminal("cmder.exe"));
        assert!(!is_visible_terminal("powershell_ise.exe"));
        assert!(!is_visible_terminal("cmd.exe.bak"));
    }
}
