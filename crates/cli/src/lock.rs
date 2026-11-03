//! Single-instance guard: `{data_dir}/daemon.lock` holding the owning pid.
//!
//! The lock is created with `create_new` (O_EXCL semantics) so exactly one
//! process can claim it. If the file already exists, the pid inside decides:
//! a live pid blocks the start, a stale one (crashed daemon, reboot) is
//! taken over by deleting and re-creating the file. Release verifies the
//! file still records *our* pid before unlinking, so a blocked or race-lost
//! acquisition can never delete a live owner's lock.
//!
//! Pid liveness: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` on
//! Windows, `/proc/<pid>` on Linux, assume-alive elsewhere.

use std::path::{Path, PathBuf};

/// Outcome of trying to claim the daemon lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockOutcome {
    /// Freshly created for the current process.
    Acquired,
    /// A dead owner's lock was deleted and re-created.
    TookOver { stale_pid: u32 },
    /// Another live process owns the lock; do not start.
    Blocked { pid: u32 },
}

/// Held lock; removes the file on `release()` / `Drop`.
pub struct DaemonLock {
    path: PathBuf,
}

impl DaemonLock {
    /// Path of the lock file for `data_dir`.
    pub fn lock_path(data_dir: &Path) -> PathBuf {
        data_dir.join("daemon.lock")
    }

    /// Try to acquire the single-instance lock in `data_dir`. The data dir
    /// must exist. I/O failures (unwritable dir) surface as errors; the
    /// `Blocked` / takeover decisions are values, not errors. The returned
    /// `DaemonLock` is always safe to drop: `release` only unlinks while
    /// this process is still the recorded owner.
    pub fn acquire(data_dir: &Path) -> std::io::Result<(DaemonLock, LockOutcome)> {
        let path = DaemonLock::lock_path(data_dir);
        match create_lock(&path) {
            Ok(()) => Ok((DaemonLock { path }, LockOutcome::Acquired)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let pid = read_pid(&path).unwrap_or(0);
                if pid_alive(pid) {
                    return Ok((DaemonLock { path }, LockOutcome::Blocked { pid }));
                }
                // Stale owner: remove and reclaim.
                std::fs::remove_file(&path)?;
                match create_lock(&path) {
                    Ok(()) => Ok((
                        DaemonLock { path },
                        LockOutcome::TookOver { stale_pid: pid },
                    )),
                    // Another process won the race; report it as blocked.
                    Err(_) => Ok((DaemonLock { path }, LockOutcome::Blocked { pid })),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// The pid recorded in the lock file, if it parses (tests).
    #[cfg(test)]
    pub fn holder_pid(data_dir: &Path) -> Option<u32> {
        let content = std::fs::read_to_string(DaemonLock::lock_path(data_dir)).ok()?;
        parse_pid(&content)
    }

    /// Remove the lock file, but only while it still records this process.
    pub fn release(&self) {
        if let Some(pid) = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|c| parse_pid(&c))
        {
            if pid != std::process::id() {
                return; // someone else owns the lock now
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        self.release();
    }
}

fn create_lock(path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    writeln!(file, "{}", std::process::id())
}

/// Pid from the first whitespace-separated token.
fn parse_pid(content: &str) -> Option<u32> {
    content.split_whitespace().next()?.parse().ok()
}

/// Raw pid of the lock file; garbage counts as pid 0 (never a live process)
/// so a corrupt lock is taken over, not a deadlock.
fn read_pid(path: &Path) -> Option<u32> {
    parse_pid(&std::fs::read_to_string(path).ok()?)
}

/// Does a process with this pid exist right now?
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return false; // System Idle Process; never an owner
    }
    // SAFETY: plain Win32 queries; the handle (if any) is closed immediately.
    unsafe {
        // OpenProcess alone is not enough: a *terminated* process whose
        // handle is still open somewhere (parent shell, job object) keeps
        // its pid resolvable until every handle closes. The exit code
        // decides: STILL_ACTIVE (259) only for a genuinely running process.
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut exit_code = 0u32;
        let alive = GetExitCodeProcess(handle, &mut exit_code).is_ok()
            && exit_code == STILL_ACTIVE.0 as u32;
        let _ = CloseHandle(handle);
        alive
    }
}

#[cfg(target_os = "linux")]
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn pid_alive(_pid: u32) -> bool {
    // No portable probe; assume alive (fail safe: never steal a live lock).
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "activity-cli-lock-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn self_pid_is_alive_and_max_pid_is_not() {
        assert!(pid_alive(std::process::id()), "our own pid must be alive");
        assert!(
            !pid_alive(u32::MAX),
            "pid u32::MAX cannot exist on any platform"
        );
    }

    #[test]
    fn acquire_write_pid_release_roundtrip() {
        let dir = temp_dir("roundtrip");
        let path = DaemonLock::lock_path(&dir);
        let (lock, outcome) = DaemonLock::acquire(&dir).expect("acquire");
        assert_eq!(outcome, LockOutcome::Acquired);
        let pid: String = std::fs::read_to_string(&path)
            .expect("lock exists")
            .trim()
            .to_string();
        assert_eq!(pid, std::process::id().to_string());
        lock.release();
        assert!(!path.exists(), "released lock is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_owner_blocks_a_second_instance() {
        let dir = temp_dir("blocked");
        // Simulate another (live) owner: our own pid (the only live pid we
        // can fabricate portably; real foreign pids are unique).
        std::fs::write(
            DaemonLock::lock_path(&dir),
            format!("{}\n", std::process::id()),
        )
        .expect("write foreign lock");
        let (_keepalive, outcome) = DaemonLock::acquire(&dir).expect("acquire");
        assert_eq!(
            outcome,
            LockOutcome::Blocked {
                pid: std::process::id()
            }
        );
        assert_eq!(DaemonLock::holder_pid(&dir), Some(std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn release_never_unlinks_a_foreign_lock() {
        let dir = temp_dir("foreign-release");
        let (lock, outcome) = DaemonLock::acquire(&dir).expect("acquire");
        assert_eq!(outcome, LockOutcome::Acquired);
        // The owner changed behind our back (simulated: a dead pid we would
        // never claim as ours). Release must leave the file alone.
        std::fs::write(DaemonLock::lock_path(&dir), format!("{}\n", u32::MAX)).expect("swap owner");
        lock.release();
        assert!(DaemonLock::lock_path(&dir).exists(), "foreign lock kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_owner_is_taken_over() {
        let dir = temp_dir("stale");
        // A pid that cannot exist anywhere (u32::MAX) simulates a crashed
        // daemon left behind by a reboot.
        std::fs::write(DaemonLock::lock_path(&dir), format!("{}\n", u32::MAX))
            .expect("write stale lock");
        let (lock, outcome) = DaemonLock::acquire(&dir).expect("acquire");
        assert_eq!(
            outcome,
            LockOutcome::TookOver {
                stale_pid: u32::MAX
            }
        );
        assert_eq!(
            DaemonLock::holder_pid(&dir),
            Some(std::process::id()),
            "new lock holds our pid"
        );
        lock.release();
        assert!(!DaemonLock::lock_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_lock_file_is_taken_over_not_deadlock() {
        let dir = temp_dir("garbage");
        std::fs::write(DaemonLock::lock_path(&dir), "not-a-pid at all")
            .expect("write garbage lock");
        let (lock, outcome) = DaemonLock::acquire(&dir).expect("acquire");
        assert_eq!(outcome, LockOutcome::TookOver { stale_pid: 0 });
        lock.release();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
