//! systemd user-service plumbing for the hub updater (issue #32): the
//! auto-update trigger. `install` writes a single unit (watch loop,
//! `Restart=on-failure`), enables it and enables linger so the service runs
//! without an interactive login - the hub equivalent of the client's
//! `startup install` fallback chain (#29), one mechanism, no fallbacks.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Unit name (also the journalctl target: `journalctl --user -u
/// activity-server-update`).
pub const SERVICE_NAME: &str = "activity-server-update.service";
/// Default check interval (daily; GitHub's unauthenticated 60 req/h limit
/// makes this comfortably cheap).
pub const DEFAULT_INTERVAL_SECS: u64 = 86_400;

/// The unit file content. `WantedBy=default.target` is the user-service
/// equivalent of multi-user.target; `Restart=on-failure` brings the updater
/// back after a crash (its `watch` tick then self-heals a missing
/// container from the saved state).
pub fn unit_content(exe: &Path, interval_secs: u64) -> String {
    format!(
        "[Unit]\n\
         Description=activity-daemon hub auto-updater (GitHub Releases check)\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart=\"{}\" watch --interval-secs {}\n\
         Restart=on-failure\n\
         RestartSec=60\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display(),
        interval_secs.max(1)
    )
}

/// `~/.config/systemd/user` (XDG_CONFIG_HOME honored).
pub fn user_unit_dir() -> Result<PathBuf> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(home_dir()?).join(".config"),
    };
    Ok(base.join("systemd").join("user"))
}

fn home_dir() -> Result<String> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .context("cannot resolve the home directory (no HOME set)")
}

/// Run a command, returning trimmed stdout; failures include stderr.
fn run_stdout(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("cannot run {program}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} {} failed: {}", args.join(" "), stderr.trim())
    }
    Ok(stdout)
}

/// Best-effort variant (linger, disable): failures surface as warnings.
fn run_ignoring_errors(program: &str, args: &[&str]) -> Result<String> {
    run_stdout(program, args)
}

/// Install + enable the user service. `dry_run` prints the unit and the
/// commands without touching anything (same spirit as the client's
/// `startup install --dry-run`, issue #29).
pub fn install(interval_secs: u64, dry_run: bool) -> Result<()> {
    let exe = std::env::current_exe().context("cannot resolve the current exe path")?;
    let content = unit_content(&exe, interval_secs);
    let unit_dir = user_unit_dir()?;
    let unit_path = unit_dir.join(SERVICE_NAME);

    if dry_run {
        println!("--- {} ---", unit_path.display());
        println!("{content}");
        println!("--- commands ---");
        println!("systemctl --user daemon-reload");
        println!("systemctl --user enable --now {SERVICE_NAME}");
        println!("loginctl enable-linger");
        return Ok(());
    }

    std::fs::create_dir_all(&unit_dir)
        .with_context(|| format!("cannot create {}", unit_dir.display()))?;
    std::fs::write(&unit_path, content)
        .with_context(|| format!("cannot write {}", unit_path.display()))?;
    run_stdout("systemctl", &["--user", "daemon-reload"])?;
    run_stdout("systemctl", &["--user", "enable", "--now", SERVICE_NAME])?;
    // Linger: run without an interactive login. On a failure the service
    // still works - it just stops when the user logs out everywhere.
    if run_ignoring_errors("loginctl", &["enable-linger"]).is_err() {
        log::warn!("could not enable linger: the updater only runs while you are logged in");
    }
    println!("installed: systemd user service {SERVICE_NAME} (Restart=on-failure, linger)");
    println!(
        "checks GitHub Releases every {} s and swaps the {} container on a newer release",
        interval_secs.max(1),
        crate::podman::CONTAINER_NAME
    );
    println!("trade-offs:");
    println!("  - needs systemd inside WSL (wsl.conf: [boot] systemd=true)");
    println!("  - the hub runs while the WSL VM runs: it starts with the first session and stops when idle-shutdown hits");
    println!("  - logs: journalctl --user -u {SERVICE_NAME}");
    Ok(())
}

/// Disable + remove the unit (linger is deliberately left as-is - it is a
/// user-level setting other services may rely on).
pub fn uninstall(dry_run: bool) -> Result<()> {
    let unit_path = user_unit_dir()?.join(SERVICE_NAME);
    if dry_run {
        println!("systemctl --user disable --now {SERVICE_NAME}");
        println!("rm {}", unit_path.display());
        println!("systemctl --user daemon-reload");
        return Ok(());
    }
    let _ = run_ignoring_errors("systemctl", &["--user", "disable", "--now", SERVICE_NAME]);
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("cannot remove {}", unit_path.display()))?;
    }
    let _ = run_ignoring_errors("systemctl", &["--user", "daemon-reload"]);
    println!("uninstalled: {SERVICE_NAME}");
    Ok(())
}

/// `(enabled, active)` from systemctl, or None when systemd is not usable
/// (e.g. WSL without systemd - install would fail the same way).
pub fn service_state() -> Result<Option<(String, String)>> {
    let enabled = match run_stdout("systemctl", &["--user", "is-enabled", SERVICE_NAME]) {
        Ok(state) => state,
        Err(_) => return Ok(None),
    };
    let active =
        run_stdout("systemctl", &["--user", "is-active", SERVICE_NAME]).unwrap_or_default();
    Ok(Some((enabled, active)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_content_is_a_valid_user_service() {
        let content = unit_content(Path::new("/usr/local/bin/activity-server-update"), 3600);
        for needle in [
            "[Unit]",
            "After=network-online.target",
            "[Service]",
            "ExecStart=\"/usr/local/bin/activity-server-update\" watch --interval-secs 3600",
            "Restart=on-failure",
            "RestartSec=60",
            "[Install]",
            "WantedBy=default.target",
        ] {
            assert!(
                content.contains(needle),
                "unit must contain {needle:?}:\n{content}"
            );
        }
    }

    #[test]
    fn interval_is_never_zero() {
        let content = unit_content(Path::new("/x"), 0);
        assert!(
            content.contains("--interval-secs 1"),
            "zero clamped:\n{content}"
        );
    }
}
