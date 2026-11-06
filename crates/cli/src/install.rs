//! `activity-daemon install` + `ensure-run` + the watchdog service (issue
//! #36).
//!
//! One command takes a machine from bare install to fully supervised
//! capture:
//!
//! 1. **Config** - `resolve` writes the template on first use; `--device`,
//!    `--server-url` and `--disable-auto-update` patch single lines
//!    (`config.toml.bak-install` backup written before the first change).
//!    The config is otherwise never touched (the auto-update choice stays
//!    with the owner; see the 2026-11-06 incident below).
//! 2. **Autostart** - the existing `startup install` fallback chain
//!    (issues #7, #29) is reused unchanged.
//! 3. **Power** - `powercfg /change standby-timeout-ac 0` by default: a
//!    sleeping machine is the one failure a supervisor cannot heal.
//!    `--keep-power-plan` skips it.
//! 4. **Supervisor** - the watchdog, in order of preference:
//!    - **Windows service `ActivityDaemonWatchdog`** (LocalSystem): every
//!      15s it reads `{data_dir}/heartbeat`; fresh = leave, stale with a
//!      dead or absent lock holder = start, stale with a live holder whose
//!      pid matches the last beater = kill and start (wedged). The restart
//!      happens in the active console session (`WTSQueryUserToken` +
//!      `CreateEnvironmentBlock` + `CreateProcessAsUserW` into
//!      `winsta0\default`): the daemon itself always runs in the user
//!      session (session 0 cannot capture anything); the service only
//!      checks and decides. SCM failure recovery (`sc failure
//!      actions= restart/5000`) restarts the watchdog itself. The binPath
//!      carries `service --data-dir <dir>` because LocalSystem resolves a
//!      different `%APPDATA%`. This layer also heals a failed auto-update
//!      relaunch (the 2026-11-06 incident class) within about 30s.
//!    - **Fallback task `ActivityDaemonKeepAlive`**: logon trigger plus a
//!      PT1M repetition running `ensure-run` - the same pure decision as a
//!      one shot, in the user session, no admin. Installed automatically
//!      when the service creation is denied (no admin, corporate GPO/EDR).
//!    - Last resort either way: the autostart VBS (logon only, no
//!      supervision).
//!
//! `install status` reports every layer; `install uninstall` removes the
//! service, the task and the autostart entries (the power change is left
//! in place, with the undo command printed).
//!
//! Pure logic (the supervision decision, the sc command builders, the
//! watchdog task XML, the degrade chain) is unit-tested on every platform
//! with golden strings; all `sc`/Win32 execution is cfg(windows) with
//! non-Windows stubs, mirroring startup.rs (issue #29).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::PatchOutcome;
use crate::heartbeat::HeartbeatState;
use crate::lock::DaemonLock;
use crate::startup::StartupAction;
use crate::usage_log;

/// Watchdog service name (the SCM registry key).
pub const SERVICE_NAME: &str = "ActivityDaemonWatchdog";
/// Friendly name shown in services.msc.
pub const SERVICE_DISPLAY_NAME: &str = "Activity Daemon Watchdog";
/// Fallback scheduled task name.
pub const WATCHDOG_TASK_NAME: &str = "ActivityDaemonKeepAlive";
/// File the fallback task XML is staged to (in the data dir).
#[cfg_attr(not(windows), allow(dead_code))]
pub const WATCHDOG_TASK_XML_FILE: &str = "ActivityDaemonKeepAlive.task.xml";
/// Service supervision cadence (the heartbeat is stale after
/// `heartbeat::MAX_AGE_S` = 15s, so one tick sees every failure fast).
#[cfg_attr(not(windows), allow(dead_code))]
pub const TICK_MS: u64 = 15_000;
/// Pause between the kill and the respawn of a wedged daemon (handles
/// close asynchronously).
pub const KILL_GRACE_MS: u64 = 1_000;
/// `WTSGetActiveConsoleSessionId()` return value when no console session
/// is active (logon screen).
#[cfg_attr(not(windows), allow(dead_code))]
const NO_CONSOLE_SESSION: u32 = 0xFFFF_FFFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Subcommand)]
pub enum InstallAction {
    /// Report every layer: heartbeat, lock, supervisor, autostart, config.
    Status,
    /// Remove the service, the fallback task and the autostart entries.
    Uninstall,
}

/// `install` subcommand entry point.
#[allow(clippy::too_many_arguments)]
pub fn cmd_install(
    action: Option<InstallAction>,
    device: Option<String>,
    server_url: Option<String>,
    disable_auto_update: bool,
    keep_power_plan: bool,
    supervisor_only: bool,
    dry_run: bool,
    config_override: Option<&Path>,
) -> Result<()> {
    let resolved = crate::config::resolve(config_override)?;
    let data_dir = resolved.config.data_dir.clone();

    match action {
        Some(InstallAction::Status) => {
            return status_report(&data_dir, &resolved.config, dry_run, config_override)
        }
        Some(InstallAction::Uninstall) => {
            return uninstall_flow(dry_run, &resolved, config_override)
        }
        None => {}
    }

    // 1. Config patches (never implicit; only the owner's flags touch it).
    if !supervisor_only {
        apply_config_patches(
            &resolved.path,
            device.as_deref(),
            server_url.as_deref(),
            disable_auto_update,
            dry_run,
        )?;
        // 2. Autostart - the #29 chain, unchanged.
        crate::startup::cmd_startup(StartupAction::Install, dry_run, config_override)?;
        // 3. Power default.
        apply_power(dry_run, keep_power_plan)?;
    }

    // 4. Supervisor layer (service preferred, task fallback).
    supervisor_stage(&data_dir, dry_run, supervisor_only, config_override)?;

    if !supervisor_only && !dry_run {
        usage_log::record(
            &data_dir,
            &resolved.config.sync.device_name,
            usage_log::FEATURE_INSTALL,
            usage_log::ACTION_INSTALL,
        );
    }
    Ok(())
}

/// `ensure-run` entry point: one supervision decision, no supervisor of its
/// own. Called by the fallback task every minute; automatic calls never log
/// usage (README contract) and stay silent while the daemon is healthy.
pub fn cmd_ensure_run(config_override: Option<&Path>) -> Result<()> {
    let resolved = crate::config::resolve(config_override)?;
    let data_dir = resolved.config.data_dir.clone();
    let exe = std::env::current_exe().context("cannot resolve the current exe path")?;
    match supervise_once(&data_dir, &exe, false)? {
        Supervision::Leave => {}
        Supervision::Start => println!("ensure-run: daemon started"),
        Supervision::KillAndStart => {
            println!("ensure-run: wedged daemon killed and restarted")
        }
    }
    Ok(())
}

/// `service` entry point: the SCM starts the exe with
/// `service --data-dir <dir>`; never meant for interactive use.
pub fn cmd_service(data_dir: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        service_impl::dispatch(data_dir)
    }
    #[cfg(not(windows))]
    {
        let _ = data_dir;
        anyhow::bail!("the watchdog service is Windows-only")
    }
}

// ---------------------------------------------------------------------------
// Pure supervision decision (tested on every platform)
// ---------------------------------------------------------------------------

/// The supervision decision for one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Supervision {
    /// Heartbeat fresh - nothing to do.
    Leave,
    /// Daemon gone (stale heartbeat, dead or absent lock holder) - start.
    Start,
    /// Wedged (stale heartbeat, live holder whose pid matches the last
    /// beater) - kill it, then start.
    KillAndStart,
}

/// Pure decision (issue #36 contract).
///
/// - fresh heartbeat (`< heartbeat::MAX_AGE_S`) => [`Supervision::Leave`];
/// - stale or missing heartbeat with a dead/absent lock holder =>
///   [`Supervision::Start`] (crashed daemon; also covers a lost lock file);
/// - stale with a live holder whose pid matches the last beater =>
///   [`Supervision::KillAndStart`] (wedged run loop);
/// - stale with a live holder whose pid DIFFERS => [`Supervision::Leave`]:
///   a fresh daemon owns the lock but has not beaten yet - never touch it.
///
/// The pid-match guard is what makes the kill safe: the heartbeat pid is
/// minutes-to-days old by the time it looks wedged, and pids get reused.
pub fn decide_supervision(
    heartbeat: Option<(f64, u32)>,
    lock_holder: Option<u32>,
    alive: impl Fn(u32) -> bool,
) -> Supervision {
    if let Some((age_s, _)) = heartbeat {
        if age_s < crate::heartbeat::MAX_AGE_S {
            return Supervision::Leave;
        }
    }
    match lock_holder {
        Some(pid) if alive(pid) => match heartbeat {
            Some((_, hb_pid)) if hb_pid == pid => Supervision::KillAndStart,
            _ => Supervision::Leave,
        },
        _ => Supervision::Start,
    }
}

/// Flatten the heartbeat state to `(age_s, pid)` for [`decide_supervision`].
fn heartbeat_beat(data_dir: &Path) -> Option<(f64, u32)> {
    match crate::heartbeat::status(data_dir) {
        HeartbeatState::Running { heartbeat } | HeartbeatState::Stale { heartbeat } => {
            Some((heartbeat.age_s, heartbeat.pid))
        }
        HeartbeatState::NotRunning => None,
    }
}

/// One supervision decision + its actions. `session_aware` selects the
/// spawn path: the service (session 0) must cross into the active console
/// session; `ensure-run` already runs inside it and uses the plain
/// detached spawn.
fn supervise_once(data_dir: &Path, exe: &Path, session_aware: bool) -> Result<Supervision> {
    let heartbeat = heartbeat_beat(data_dir);
    let decision = decide_supervision(
        heartbeat,
        DaemonLock::holder_pid(data_dir),
        crate::lock::pid_alive,
    );
    match decision {
        Supervision::Leave => {}
        Supervision::Start => spawn_daemon(exe, data_dir, session_aware)?,
        Supervision::KillAndStart => {
            let pid = heartbeat
                .map(|(_, pid)| pid)
                .expect("wedge implies a beater");
            kill_pid(pid)?;
            std::thread::sleep(Duration::from_millis(KILL_GRACE_MS));
            spawn_daemon(exe, data_dir, session_aware)?;
        }
    }
    Ok(decision)
}

fn spawn_daemon(exe: &Path, data_dir: &Path, session_aware: bool) -> Result<()> {
    if session_aware {
        spawn_into_session(exe, data_dir)
    } else {
        crate::update::spawn_detached(exe)
    }
}

// ---------------------------------------------------------------------------
// sc.exe command builders (pure; the binPath quoting is contractual)
// ---------------------------------------------------------------------------

/// Image path value the service registry entry carries: the quoted exe plus
/// `service --data-dir`. LocalSystem resolves a different `%APPDATA%`, so
/// the data dir travels inside the binPath.
pub fn service_bin_path(exe: &str, data_dir: &str) -> String {
    format!("\"{exe}\" service --data-dir \"{data_dir}\"")
}

/// `sc create ...` arguments (start= auto; DisplayName for services.msc).
pub fn sc_create_args(exe: &str, data_dir: &str) -> Vec<String> {
    vec![
        "create".into(),
        SERVICE_NAME.into(),
        "binPath=".into(),
        service_bin_path(exe, data_dir),
        "start=".into(),
        "auto".into(),
        "DisplayName=".into(),
        SERVICE_DISPLAY_NAME.into(),
    ]
}

/// SCM failure recovery: the watchdog restarts itself when it crashes
/// (reset the failure counter daily).
pub fn sc_failure_args() -> Vec<String> {
    vec![
        "failure".into(),
        SERVICE_NAME.into(),
        "reset=".into(),
        "86400".into(),
        "actions=".into(),
        "restart/5000".into(),
    ]
}

pub fn sc_query_args() -> Vec<String> {
    vec!["query".into(), SERVICE_NAME.into()]
}

pub fn sc_start_args() -> Vec<String> {
    vec!["start".into(), SERVICE_NAME.into()]
}

pub fn sc_stop_args() -> Vec<String> {
    vec!["stop".into(), SERVICE_NAME.into()]
}

pub fn sc_delete_args() -> Vec<String> {
    vec!["delete".into(), SERVICE_NAME.into()]
}

/// Whether an `sc` error text means "nothing to do" for uninstall: the
/// service is not installed (1060) or is already stopped (1062).
pub fn service_absent_from_error(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    ["1060", "does not exist", "1062", "has not been started"]
        .iter()
        .any(|needle| lowered.contains(needle))
}

// ---------------------------------------------------------------------------
// Watchdog fallback task XML (pure; golden-tested, startup.rs schema order)
// ---------------------------------------------------------------------------

/// Task flavors for the fallback chain (issue #29 shape): some hardened
/// schedulers reject `Hidden` / `RestartOnFailure`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogTaskFlavor {
    /// Hidden + RestartOnFailure.
    Full,
    /// Neither - the minimal fallback.
    Minimal,
}

/// The watchdog task XML: logon trigger + a PT1M repetition
/// (`ScheduleByDay` anchor; no Repetition Duration = indefinite), running
/// `ensure-run` in the user session. `StartWhenAvailable` true so ticks
/// missed during sleep run right after wake.
pub fn watchdog_task_xml(
    flavor: WatchdogTaskFlavor,
    command: &str,
    arguments: &str,
    working_dir: &str,
) -> String {
    let hidden = flavor == WatchdogTaskFlavor::Full;
    let restart = flavor == WatchdogTaskFlavor::Full;
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>activity-daemon keepalive: ensure-run every minute (issue #36)</Description>
    <URI>\{WATCHDOG_TASK_NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
    <CalendarTrigger>
      <Repetition>
        <Interval>PT1M</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
      <StartBoundary>2000-01-01T00:00:00</StartBoundary>
      <Enabled>true</Enabled>
      <ScheduleByDay>
        <DaysInterval>1</DaysInterval>
      </ScheduleByDay>
    </CalendarTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
{hidden_line}    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
{restart_block}  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
      <WorkingDirectory>{working_dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        WATCHDOG_TASK_NAME = WATCHDOG_TASK_NAME,
        hidden_line = if hidden {
            "    <Hidden>true</Hidden>\n"
        } else {
            ""
        },
        restart_block = if restart {
            "    <RestartOnFailure>\n      <Interval>PT1M</Interval>\n      <Count>3</Count>\n    </RestartOnFailure>\n"
        } else {
            ""
        },
        command = crate::startup::xml_escape(command),
        arguments = crate::startup::xml_escape(arguments),
        working_dir = crate::startup::xml_escape(working_dir),
    )
}

// ---------------------------------------------------------------------------
// Supervisor degrade chain (pure)
// ---------------------------------------------------------------------------

/// Supervisor host steps, best first (issue #36): the service needs admin
/// once at install, the task never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorStep {
    Service,
    WatchdogTaskFull,
    WatchdogTaskMinimal,
}

impl SupervisorStep {
    /// Name printed when this step installs.
    pub fn label(self) -> &'static str {
        match self {
            SupervisorStep::Service => {
                "watchdog service ActivityDaemonWatchdog (15s heartbeat checks, SCM restart on failure)"
            }
            SupervisorStep::WatchdogTaskFull => {
                "watchdog task ActivityDaemonKeepAlive (logon + every 1 min, hidden, ensure-run)"
            }
            SupervisorStep::WatchdogTaskMinimal => {
                "watchdog task ActivityDaemonKeepAlive (logon + every 1 min, minimal, ensure-run)"
            }
        }
    }

    /// The next step after `self` failed. Unconditional (mirrors the #29
    /// chain: one cheap retry per level tells policy denial from XML
    /// rejection).
    pub fn next_step(self) -> Option<SupervisorStep> {
        match self {
            SupervisorStep::Service => Some(SupervisorStep::WatchdogTaskFull),
            SupervisorStep::WatchdogTaskFull => Some(SupervisorStep::WatchdogTaskMinimal),
            SupervisorStep::WatchdogTaskMinimal => None,
        }
    }
}

// ---------------------------------------------------------------------------
// install apply: supervisor stage
// ---------------------------------------------------------------------------

fn supervisor_stage(
    data_dir: &Path,
    dry_run: bool,
    supervisor_only: bool,
    config_override: Option<&Path>,
) -> Result<()> {
    let exe = std::env::current_exe().context("cannot resolve the current exe path")?;

    if dry_run {
        let args = sc_create_args(&exe.display().to_string(), &data_dir.display().to_string());
        println!("dry run (preferred supervisor): sc {}", args.join(" "));
        println!("dry run: sc {}", sc_failure_args().join(" "));
        println!("dry run: sc {}", sc_start_args().join(" "));
        println!("--- watchdog task XML (fallback step 1) ---");
        println!(
            "{}",
            watchdog_task_xml(
                WatchdogTaskFlavor::Full,
                &exe.display().to_string(),
                "ensure-run",
                &data_dir.display().to_string(),
            )
        );
        println!("--- minimal watchdog task XML (fallback step 2, on failure) ---");
        println!(
            "{}",
            watchdog_task_xml(
                WatchdogTaskFlavor::Minimal,
                &exe.display().to_string(),
                "ensure-run",
                &data_dir.display().to_string(),
            )
        );
        return Ok(());
    }

    if is_elevated() {
        return install_supervisor(data_dir, &exe);
    }
    if supervisor_only {
        // Launched manually without elevation: try anyway (the chain will
        // degrade to the task, which needs no admin).
        return install_supervisor(data_dir, &exe);
    }
    match elevate_install(config_override) {
        Ok(()) => {
            println!("supervisor: setup continues in the elevated process (service preferred)");
            println!("  verify afterwards with: activity-daemon install status");
            Ok(())
        }
        Err(e) => {
            log::warn!("install: elevation refused ({e:#}); using the no-admin task supervisor");
            install_supervisor(data_dir, &exe)
        }
    }
}

/// The degrade chain itself: service -> task full -> task minimal. Each
/// failure is logged with its reason; the effective layer is printed.
fn install_supervisor(data_dir: &Path, exe: &Path) -> Result<()> {
    let mut step = Some(SupervisorStep::Service);
    let mut installed: Option<&'static str> = None;
    while let Some(current) = step {
        let outcome = match current {
            SupervisorStep::Service => install_service(data_dir, exe),
            SupervisorStep::WatchdogTaskFull => {
                install_watchdog_task(data_dir, exe, WatchdogTaskFlavor::Full)
            }
            SupervisorStep::WatchdogTaskMinimal => {
                install_watchdog_task(data_dir, exe, WatchdogTaskFlavor::Minimal)
            }
        };
        match outcome {
            Ok(()) => {
                installed = Some(current.label());
                break;
            }
            Err(e) => {
                log::warn!(
                    "install: {} failed ({e:#}); degrading per issue #36",
                    current.label()
                );
                step = current.next_step();
            }
        }
    }
    match installed {
        Some(label) => println!("supervisor: {label}"),
        None => println!(
            "warning: no supervisor could be installed - only the logon autostart protects the daemon"
        ),
    }
    Ok(())
}

/// Create + configure + start the watchdog service. Idempotent: an
/// existing service is reconfigured (failure recovery) and started.
#[cfg(windows)]
fn install_service(data_dir: &Path, exe: &Path) -> Result<()> {
    let args = sc_create_args(&exe.display().to_string(), &data_dir.display().to_string());
    match sc(&sc_query_args()) {
        Ok(_) => {
            log::info!("install: service {SERVICE_NAME} already exists; reconfiguring");
        }
        Err(_) => {
            sc(&args)?;
        }
    }
    sc(&sc_failure_args())?;
    // Best effort: a failed start still leaves a valid boot autostart.
    if let Err(e) = sc(&sc_start_args()) {
        log::warn!("install: service start failed ({e:#}); it starts at the next boot");
        println!("warning: the service is installed but could not be started now: {e:#}");
    }
    Ok(())
}

#[cfg(not(windows))]
fn install_service(_data_dir: &Path, _exe: &Path) -> Result<()> {
    anyhow::bail!("the watchdog service is Windows-only (use --dry-run to preview)")
}

/// Create the fallback task (no admin needed) and fire one `ensure-run`
/// tick immediately (best effort).
#[cfg(windows)]
fn install_watchdog_task(data_dir: &Path, exe: &Path, flavor: WatchdogTaskFlavor) -> Result<()> {
    let xml = watchdog_task_xml(
        flavor,
        &exe.display().to_string(),
        "ensure-run",
        &data_dir.display().to_string(),
    );
    let xml_path = data_dir.join(WATCHDOG_TASK_XML_FILE);
    std::fs::write(&xml_path, &xml)
        .with_context(|| format!("cannot write watchdog task XML {}", xml_path.display()))?;
    crate::startup::schtasks(&[
        "/create",
        "/f",
        "/tn",
        WATCHDOG_TASK_NAME,
        "/xml",
        &xml_path.display().to_string(),
    ])?;
    if let Err(e) = crate::startup::schtasks(&["/run", "/tn", WATCHDOG_TASK_NAME]) {
        log::warn!(
            "install: watchdog task start failed ({e:#}); it fires at the next logon/minute"
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn install_watchdog_task(_data_dir: &Path, _exe: &Path, _flavor: WatchdogTaskFlavor) -> Result<()> {
    anyhow::bail!("scheduled tasks are only available on Windows (use --dry-run to preview)")
}

// ---------------------------------------------------------------------------
// status + uninstall
// ---------------------------------------------------------------------------

fn status_report(
    data_dir: &Path,
    config: &activity_core::Config,
    dry_run: bool,
    config_override: Option<&Path>,
) -> Result<()> {
    if dry_run {
        println!(
            "dry run: read heartbeat + lock, sc {}",
            sc_query_args().join(" ")
        );
        println!("dry run: schtasks /query /tn {WATCHDOG_TASK_NAME}");
        println!("dry run: startup status");
        return Ok(());
    }
    println!("install status (issue #36):");
    match crate::heartbeat::status(data_dir) {
        HeartbeatState::Running { heartbeat } => println!(
            "  heartbeat: running (age {:.0}s, pid {})",
            heartbeat.age_s, heartbeat.pid
        ),
        HeartbeatState::Stale { heartbeat } => println!(
            "  heartbeat: STALE (age {:.0}s, pid {}) - the run loop is not beating",
            heartbeat.age_s, heartbeat.pid
        ),
        HeartbeatState::NotRunning => println!("  heartbeat: not running (no file)"),
    }
    match DaemonLock::holder_pid(data_dir) {
        Some(pid) => println!(
            "  lock holder: pid {pid} ({})",
            if crate::lock::pid_alive(pid) {
                "alive"
            } else {
                "dead"
            }
        ),
        None => println!("  lock holder: none"),
    }

    let mut effective = "none - only the logon autostart protects the daemon";
    let service_state = sc(&sc_query_args());
    let service_line = match &service_state {
        Ok(out) if out.contains("RUNNING") => {
            effective = "service (15s checks, SCM self-recovery)";
            "running".to_string()
        }
        Ok(_) => "installed, stopped".to_string(),
        Err(_) => "not installed".to_string(),
    };
    println!("  supervisor service {SERVICE_NAME}: {service_line}");
    let task_state = crate::startup::schtasks(&["/query", "/tn", WATCHDOG_TASK_NAME]);
    if task_state.is_ok() && effective == "none - only the logon autostart protects the daemon" {
        effective = "fallback task (60s worst case)";
    }
    println!(
        "  fallback task {WATCHDOG_TASK_NAME}: {}",
        if task_state.is_ok() {
            "installed"
        } else {
            "not installed"
        }
    );
    println!("  effective supervisor: {effective}");

    // The autostart layer (prints its own block, issue #29 style).
    println!("autostart:");
    crate::startup::cmd_startup(StartupAction::Status, false, config_override)?;

    println!(
        "  config: device={} server={} auto_update={}",
        config.sync.device_name, config.sync.server_url, config.sync.auto_update
    );
    println!(
        "  power: standby-timeout-ac=0 applied by install (not read back; undo: \
         powercfg /change standby-timeout-ac <minutes>)"
    );
    Ok(())
}

fn uninstall_flow(
    dry_run: bool,
    resolved: &crate::config::Resolved,
    config_override: Option<&Path>,
) -> Result<()> {
    if dry_run {
        println!("dry run: sc {}", sc_stop_args().join(" "));
        println!("dry run: sc {}", sc_delete_args().join(" "));
        println!("dry run: schtasks /delete /f /tn {WATCHDOG_TASK_NAME}");
        println!("dry run: startup uninstall (task + Startup folder)");
        return Ok(());
    }
    let mut removed: Vec<String> = Vec::new();
    match sc(&sc_stop_args()) {
        Ok(_) => removed.push(format!("stopped service {SERVICE_NAME}")),
        Err(e) => {
            let text = format!("{e:#}");
            if !service_absent_from_error(&text) {
                return Err(anyhow::anyhow!("sc stop failed: {text}"));
            }
            log::debug!("install: service not running; nothing to stop");
        }
    }
    match sc(&sc_delete_args()) {
        Ok(_) => removed.push(format!("service {SERVICE_NAME}")),
        Err(e) => {
            let text = format!("{e:#}");
            if !service_absent_from_error(&text) {
                return Err(anyhow::anyhow!("sc delete failed: {text}"));
            }
            log::debug!("install: service not present; nothing to delete");
        }
    }
    match crate::startup::schtasks(&["/delete", "/f", "/tn", WATCHDOG_TASK_NAME]) {
        Ok(_) => removed.push(format!("scheduled task {WATCHDOG_TASK_NAME}")),
        Err(e) => {
            let text = format!("{e:#}");
            if !crate::startup::task_absent_from_error(&text) {
                return Err(e);
            }
            log::debug!("install: watchdog task not present; nothing to delete");
        }
    }
    // The autostart chain (prints its own removed list; logs
    // {startup, uninstall} itself).
    crate::startup::cmd_startup(StartupAction::Uninstall, false, config_override)?;

    if removed.is_empty() {
        println!("nothing to remove at the supervisor layer");
    } else {
        println!("removed: {}", removed.join(", "));
    }
    println!(
        "note: the power plan is not restored (undo manually: powercfg /change standby-timeout-ac <minutes>)"
    );
    usage_log::record(
        &resolved.config.data_dir,
        &resolved.config.sync.device_name,
        usage_log::FEATURE_INSTALL,
        usage_log::ACTION_UNINSTALL,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// config patches + power (cross-platform)
// ---------------------------------------------------------------------------

fn apply_config_patches(
    path: &Path,
    device: Option<&str>,
    server_url: Option<&str>,
    disable_auto_update: bool,
    dry_run: bool,
) -> Result<()> {
    let mut planned: Vec<&str> = Vec::new();
    if let Some(name) = device {
        planned.push("device_name");
        if !dry_run {
            patch_and_print(path, "device_name", &format!("device_name = \"{name}\""))?;
        }
    }
    if let Some(url) = server_url {
        planned.push("server_url");
        if !dry_run {
            patch_and_print(path, "server_url", &format!("server_url = \"{url}\""))?;
        }
    }
    if disable_auto_update {
        planned.push("auto_update=false");
        if !dry_run {
            patch_and_print(path, "auto_update", "auto_update = false")?;
        }
    }
    if dry_run && !planned.is_empty() {
        println!(
            "dry run: would patch {} ({}) with a config.toml.bak-install backup first",
            path.display(),
            planned.join(", ")
        );
    }
    Ok(())
}

fn patch_and_print(path: &Path, key: &str, new_line: &str) -> Result<()> {
    match crate::config::patch_config_line(path, key, new_line)? {
        PatchOutcome::Changed => println!("config: {key} updated (backup: {})", {
            let mut bak = path.as_os_str().to_owned();
            bak.push(".bak-install");
            bak.to_string_lossy().into_owned()
        }),
        PatchOutcome::Already => println!("config: {key} already set"),
        PatchOutcome::LineMissing => println!(
            "config: the '{key}' line is not present in {} - add it manually",
            path.display()
        ),
    }
    Ok(())
}

fn apply_power(dry_run: bool, keep_power_plan: bool) -> Result<()> {
    if keep_power_plan {
        println!("power: left untouched (--keep-power-plan)");
        return Ok(());
    }
    if dry_run {
        println!("dry run: powercfg /change standby-timeout-ac 0");
        return Ok(());
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("powercfg")
            .args(["/change", "standby-timeout-ac", "0"])
            .output()
            .context("cannot run powercfg.exe")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!("powercfg /change standby-timeout-ac 0 failed: {stderr}");
        }
        println!(
            "power: standby-timeout-ac = 0 (AC never sleeps; a sleeping machine cannot capture)"
        );
        Ok(())
    }
    #[cfg(not(windows))]
    {
        println!("power: standby-timeout-ac is a Windows setting; nothing done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Windows execution: service protocol, session-aware spawn, sc, elevation
// ---------------------------------------------------------------------------

/// Run `sc.exe` with the given arguments, echoing its stdout (same shape as
/// startup::schtasks).
#[cfg(windows)]
fn sc(args: &[String]) -> Result<String> {
    let output = std::process::Command::new("sc")
        .args(args)
        .output()
        .context("cannot run sc.exe")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stdout.is_empty() {
        println!("{stdout}");
    }
    if output.status.success() {
        Ok(stdout)
    } else if stderr.is_empty() {
        anyhow::bail!("sc {} failed with {}", args.join(" "), output.status)
    } else {
        anyhow::bail!("sc {} failed: {stderr}", args.join(" "))
    }
}

#[cfg(not(windows))]
fn sc(args: &[String]) -> Result<String> {
    anyhow::bail!("sc.exe is Windows-only: {}", args.join(" "))
}

/// Force-kill a wedged daemon pid. The supervisor only kills when the lock
/// holder matches the last beater (see [`decide_supervision`]).
#[cfg(windows)]
fn kill_pid(pid: u32) -> Result<()> {
    let output = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output()
        .context("cannot run taskkill.exe")?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!("taskkill /F /PID {pid} failed: {stderr}")
    }
}

#[cfg(not(windows))]
fn kill_pid(_pid: u32) -> Result<()> {
    anyhow::bail!("the watchdog kill path is Windows-only")
}

/// Whether the current process token has admin rights (the service install
/// needs them once).
#[cfg(windows)]
fn is_elevated() -> bool {
    // SAFETY: a plain shell query; no handles to free.
    unsafe { windows::Win32::UI::Shell::IsUserAnAdmin().as_bool() }
}

#[cfg(not(windows))]
fn is_elevated() -> bool {
    false
}

/// Relaunch `exe install --service-only` elevated (ShellExecuteW "runas").
/// The elevated child performs the supervisor stage (service preferred,
/// task fallback) against the same config.
#[cfg(windows)]
fn elevate_install(config_override: Option<&Path>) -> Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_DEFAULT, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = std::env::current_exe().context("cannot resolve the current exe path")?;
    let file: Vec<u16> = exe
        .display()
        .to_string()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut params = String::from("install --service-only");
    if let Some(cfg) = config_override {
        params.push_str(&format!(" --config \"{}\"", cfg.display()));
    }
    let params_w: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_DEFAULT,
        lpVerb: PCWSTR::from_raw(verb.as_ptr()),
        lpFile: PCWSTR::from_raw(file.as_ptr()),
        lpParameters: PCWSTR::from_raw(params_w.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: all NUL-terminated buffers outlive the call; the struct is
    // ours. A refused UAC prompt surfaces as an error here.
    unsafe {
        ShellExecuteExW(&mut info)
            .map_err(|e| anyhow::anyhow!("elevation was refused or failed (runas): {e}"))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn elevate_install(_config_override: Option<&Path>) -> Result<()> {
    anyhow::bail!("elevation is a Windows concept (run the service install as root instead)")
}

/// Spawn `exe run` in the ACTIVE CONSOLE USER SESSION (the daemon needs the
/// interactive desktop; session 0 sees nothing). Returns quietly when no
/// console session is active (logon screen) - nothing to capture, nothing
/// to spawn. Called by the service only.
#[cfg(windows)]
fn spawn_into_session(exe: &Path, working_dir: &Path) -> Result<()> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};

    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == NO_CONSOLE_SESSION {
        log::info!("watchdog: no active console session; nothing to spawn");
        return Ok(());
    }
    let mut token = HANDLE::default();
    // SAFETY: token is closed on every path below; the env block is
    // destroyed after CreateProcessAsUserW copied it.
    unsafe {
        WTSQueryUserToken(session, &mut token)
            .map_err(|e| anyhow::anyhow!("WTSQueryUserToken(session {session}) failed: {e}"))?;
    }
    let result = spawn_with_token(token, exe, working_dir);
    // SAFETY: token is ours, opened above.
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

/// The spawn itself, with the already-opened user token (split out so the
/// token close stays unconditional in [`spawn_into_session`]).
#[cfg(windows)]
fn spawn_with_token(
    token: windows::Win32::Foundation::HANDLE,
    exe: &Path,
    working_dir: &Path,
) -> Result<()> {
    use windows::core::PCWSTR;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::Threading::{
        CreateProcessAsUserW, CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    };
    let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
    // The user environment block is what makes the spawned daemon
    // resolve the USER's %APPDATA% (and therefore their config).
    let have_env = unsafe { CreateEnvironmentBlock(&mut env, Some(token), false) }.is_ok();
    let mut cmdline: Vec<u16> = format!("\"{}\" run", exe.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut desktop: Vec<u16> = "winsta0\\default"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let workdir: Vec<u16> = working_dir
        .display()
        .to_string()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR::from_raw(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    let spawned = unsafe {
        CreateProcessAsUserW(
            Some(token),
            None,
            Some(PWSTR::from_raw(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS | CREATE_UNICODE_ENVIRONMENT,
            if have_env { Some(env) } else { None },
            PCWSTR::from_raw(workdir.as_ptr()),
            &si,
            &mut pi,
        )
    };
    if let Ok(()) = spawned {
        // SAFETY: the two handles are ours, returned by CreateProcess.
        unsafe {
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
        }
    }
    if have_env {
        unsafe {
            let _ = DestroyEnvironmentBlock(env);
        }
    }
    spawned.map_err(|e| anyhow::anyhow!("CreateProcessAsUserW failed: {e}"))
}

#[cfg(not(windows))]
fn spawn_into_session(_exe: &Path, _working_dir: &Path) -> Result<()> {
    anyhow::bail!("session-aware spawning is Windows-only")
}

// ---------------------------------------------------------------------------
// The service itself (cfg windows)
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod service_impl {
    //! Minimal SCM protocol: dispatcher -> ServiceMain -> supervision loop
    //! with a cooperative stop flag. No log sink here (services have no
    //! console) - failures are best effort and the next tick retries.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use windows::core::PWSTR;
    use windows::Win32::System::Services::{
        RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
        SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_SHUTDOWN,
        SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
        SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING,
        SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    };

    use super::{supervise_once, SERVICE_NAME, TICK_MS};

    static SERVICE_STOP: AtomicBool = AtomicBool::new(false);
    /// `SERVICE_STATUS_HANDLE` backing value (0 = none yet).
    static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);

    /// Dispatcher: must run on the process main thread (the SCM contract).
    pub fn dispatch(_data_dir: &Path) -> anyhow::Result<()> {
        // service_main re-derives --data-dir from the process command line
        // (the SCM started us with: "<exe>" service --data-dir "<dir>").
        let name: Vec<u16> = SERVICE_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut table = [
            SERVICE_TABLE_ENTRYW {
                lpServiceName: PWSTR::from_raw(name.as_ptr() as *mut u16),
                lpServiceProc: Some(service_main),
            },
            SERVICE_TABLE_ENTRYW::default(), // list terminator
        ];
        // SAFETY: the table lives across the call; the SCM owns dispatch.
        unsafe {
            StartServiceCtrlDispatcherW(table.as_mut_ptr())
                .map_err(|e| anyhow::anyhow!("StartServiceCtrlDispatcher failed: {e}"))
        }
    }

    /// SAFETY: called by the SCM on its own thread; touches only the static
    /// flag and the status handle below.
    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
        let name: Vec<u16> = SERVICE_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let Ok(handle) = RegisterServiceCtrlHandlerExW(
            windows::core::PCWSTR::from_raw(name.as_ptr()),
            Some(control_handler),
            None,
        ) else {
            return; // nothing sensible left; SCM will time us out
        };
        STATUS_HANDLE.store(handle.0 as usize, Ordering::Release);
        set_state(SERVICE_START_PENDING, 0);

        let data_dir = std::env::args()
            .skip_while(|arg| arg != "--data-dir")
            .nth(1)
            .map(PathBuf::from);
        let exe = std::env::current_exe().ok();
        let (Some(data_dir), Some(exe)) = (data_dir, exe) else {
            set_state(SERVICE_STOPPED, 0);
            return;
        };

        set_state(
            SERVICE_RUNNING,
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
        );
        loop {
            if SERVICE_STOP.load(Ordering::Acquire) {
                break;
            }
            // A supervision error never kills the service; retry next tick.
            let _ = supervise_once(&data_dir, &exe, true);
            let mut waited = 0u64;
            while waited < TICK_MS && !SERVICE_STOP.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(250));
                waited += 250;
            }
        }
        set_state(SERVICE_STOP_PENDING, 0);
        set_state(SERVICE_STOPPED, 0);
    }

    /// SAFETY: SCM callback; only flips the stop flag.
    unsafe extern "system" fn control_handler(
        control: u32,
        _event_type: u32,
        _event_data: *mut core::ffi::c_void,
        _context: *mut core::ffi::c_void,
    ) -> u32 {
        if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
            SERVICE_STOP.store(true, Ordering::Release);
            set_state(SERVICE_STOP_PENDING, 0);
        }
        0 // NO_ERROR
    }

    fn set_state(state: SERVICE_STATUS_CURRENT_STATE, controls_accepted: u32) {
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: controls_accepted,
            dwWin32ExitCode: 0,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: 3_000,
        };
        let raw = STATUS_HANDLE.load(Ordering::Acquire);
        if raw == 0 {
            return;
        }
        // SAFETY: the handle was returned by RegisterServiceCtrlHandlerExW.
        unsafe {
            let _ = SetServiceStatus(
                SERVICE_STATUS_HANDLE(raw as *mut core::ffi::c_void),
                &status,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// tests (pure logic; golden strings; no shell-outs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn alive_if_42(pid: u32) -> bool {
        pid == 42
    }

    /// The full supervision decision table (issue #36 contract): fresh
    /// beats win, stale+dead starts, stale+wedged kills, and a live holder
    /// with a differing pid (fresh start, not beaten yet) is left alone.
    #[test]
    fn supervision_decision_table() {
        use Supervision::{KillAndStart, Leave, Start};
        // Fresh heartbeat: always leave.
        assert_eq!(
            decide_supervision(Some((1.0, 42)), Some(42), alive_if_42),
            Leave
        );
        assert_eq!(
            decide_supervision(Some((1.0, 42)), None, alive_if_42),
            Leave
        );
        // Stale heartbeat + live matching holder: wedged.
        assert_eq!(
            decide_supervision(Some((99.0, 42)), Some(42), alive_if_42),
            KillAndStart
        );
        // Stale heartbeat + live differing holder: a fresh daemon owns the
        // lock but has not beaten yet - leave, re-check next tick. (The
        // alive probe here accepts both pids; a holder we cannot see as
        // alive behaves like a dead one -> Start, already covered above.)
        assert_eq!(
            decide_supervision(Some((99.0, 42)), Some(43), |pid| pid == 42 || pid == 43),
            Leave
        );
        // Stale heartbeat + dead holder: crashed daemon.
        assert_eq!(
            decide_supervision(Some((99.0, 42)), Some(42), |_| false),
            Start
        );
        assert_eq!(
            decide_supervision(Some((99.0, 42)), None, alive_if_42),
            Start
        );
        // Missing heartbeat + live holder: never kill without beat-pid
        // evidence - leave (it may be starting or the file was removed).
        assert_eq!(decide_supervision(None, Some(43), |pid| pid == 43), Leave);
        assert_eq!(decide_supervision(None, Some(42), |_| false), Start);
        assert_eq!(decide_supervision(None, None, alive_if_42), Start);
        // The freshness threshold is heartbeat::MAX_AGE_S (15s): just under
        // is Leave, at it is already stale.
        let max = crate::heartbeat::MAX_AGE_S;
        assert_eq!(
            decide_supervision(Some((max - 0.5, 42)), Some(42), alive_if_42),
            Leave
        );
        assert_eq!(
            decide_supervision(Some((max, 42)), Some(42), alive_if_42),
            KillAndStart
        );
    }

    /// The kill safety guard: a stale heartbeat pid that does NOT match the
    /// live lock holder is never killed (pid reuse protection).
    #[test]
    fn kill_requires_heartbeat_and_lock_pid_match() {
        // heartbeat pid 7 (dead), holder 42 (alive): leave - 42 may be
        // starting; pid 7 is long gone and must not be "killed".
        assert_eq!(
            decide_supervision(Some((999.0, 7)), Some(42), alive_if_42),
            Supervision::Leave
        );
    }

    /// The sc argument builders are contractual: the binPath must carry the
    /// quoted exe plus `service --data-dir` (LocalSystem cannot resolve the
    /// user config), and the failure recovery is restart/5000.
    #[test]
    fn sc_command_builders_match_the_contract() {
        assert_eq!(
            service_bin_path("C:\\tools\\activity-daemon.exe", "C:\\ad\\data"),
            "\"C:\\tools\\activity-daemon.exe\" service --data-dir \"C:\\ad\\data\""
        );
        assert_eq!(
            sc_create_args("C:\\tools\\activity-daemon.exe", "C:\\ad\\data"),
            vec![
                "create".to_string(),
                "ActivityDaemonWatchdog".to_string(),
                "binPath=".to_string(),
                "\"C:\\tools\\activity-daemon.exe\" service --data-dir \"C:\\ad\\data\""
                    .to_string(),
                "start=".to_string(),
                "auto".to_string(),
                "DisplayName=".to_string(),
                "Activity Daemon Watchdog".to_string(),
            ]
        );
        assert_eq!(
            sc_failure_args(),
            vec![
                "failure".to_string(),
                "ActivityDaemonWatchdog".to_string(),
                "reset=".to_string(),
                "86400".to_string(),
                "actions=".to_string(),
                "restart/5000".to_string(),
            ]
        );
        assert_eq!(
            sc_query_args(),
            vec!["query".to_string(), "ActivityDaemonWatchdog".to_string()]
        );
        assert_eq!(
            sc_start_args(),
            vec!["start".to_string(), "ActivityDaemonWatchdog".to_string()]
        );
        assert_eq!(
            sc_stop_args(),
            vec!["stop".to_string(), "ActivityDaemonWatchdog".to_string()]
        );
        assert_eq!(
            sc_delete_args(),
            vec!["delete".to_string(), "ActivityDaemonWatchdog".to_string()]
        );
    }

    /// Uninstall must be a clean no-op on machines without the service
    /// (1060) or with it already stopped (1062).
    #[test]
    fn sc_absent_errors_classify_as_noop() {
        assert!(service_absent_from_error(
            "sc query ActivityDaemonWatchdog failed: [SC] EnumQueryServicesWStatus:1060:"
        ));
        assert!(service_absent_from_error(
            "The specified service does not exist as an installed service."
        ));
        assert!(service_absent_from_error(
            "sc stop failed: [SC] ControlService:1062: The service has not been started."
        ));
        assert!(!service_absent_from_error(
            "sc delete failed: Access is denied."
        ));
        assert!(!service_absent_from_error(""));
    }

    /// The supervisor degrade chain: service -> task full -> task minimal
    /// -> none.
    #[test]
    fn supervisor_chain_degrades_to_the_minimal_task() {
        assert_eq!(
            SupervisorStep::Service.next_step(),
            Some(SupervisorStep::WatchdogTaskFull)
        );
        assert_eq!(
            SupervisorStep::WatchdogTaskFull.next_step(),
            Some(SupervisorStep::WatchdogTaskMinimal)
        );
        assert_eq!(SupervisorStep::WatchdogTaskMinimal.next_step(), None);
        for step in [
            SupervisorStep::Service,
            SupervisorStep::WatchdogTaskFull,
            SupervisorStep::WatchdogTaskMinimal,
        ] {
            assert!(!step.label().is_empty());
        }
    }

    /// Golden-string test: the exact XML `schtasks /create /xml` receives
    /// for the fallback watchdog task (logon + PT1M repetition, hidden,
    /// restart-on-failure, ensure-run).
    #[test]
    fn watchdog_task_xml_matches_the_golden_document() {
        let xml = watchdog_task_xml(
            WatchdogTaskFlavor::Full,
            "C:\\tools\\activity-daemon.exe",
            "ensure-run",
            "C:\\activity",
        );
        assert_eq!(xml, GOLDEN_WATCHDOG);
    }

    /// The minimal fallback XML is the full one minus `Hidden` and
    /// `RestartOnFailure` - everything else byte-identical (issue #29
    /// delta pattern).
    #[test]
    fn watchdog_minimal_task_xml_is_the_full_minus_hidden_and_restart() {
        let xml = watchdog_task_xml(
            WatchdogTaskFlavor::Minimal,
            "C:\\tools\\activity-daemon.exe",
            "ensure-run",
            "C:\\activity",
        );
        let full = watchdog_task_xml(
            WatchdogTaskFlavor::Full,
            "C:\\tools\\activity-daemon.exe",
            "ensure-run",
            "C:\\activity",
        );
        for line in xml.lines() {
            assert!(!line.contains("<Hidden>"), "minimal drops Hidden: {line}");
            assert!(
                !line.contains("RestartOnFailure"),
                "minimal drops RestartOnFailure: {line}"
            );
        }
        for must in [
            "<LogonTrigger>",
            "<CalendarTrigger>",
            "<Interval>PT1M</Interval>",
            "<ScheduleByDay>",
            "<StartWhenAvailable>true</StartWhenAvailable>",
            "<Arguments>ensure-run</Arguments>",
            "<WorkingDirectory>C:\\activity</WorkingDirectory>",
        ] {
            assert!(xml.contains(must), "minimal keeps {must}: {xml}");
        }
        let strip = |xml: &str| {
            xml.lines()
                .filter(|line| {
                    !line.contains("<Hidden>")
                        && !line.contains("RestartOnFailure")
                        && !line.contains("<Interval>")
                        && !line.contains("<Count>")
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(strip(&xml), strip(&full));
    }

    const GOLDEN_WATCHDOG: &str = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n",
        "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n",
        "  <RegistrationInfo>\n",
        "    <Description>activity-daemon keepalive: ensure-run every minute (issue #36)</Description>\n",
        "    <URI>\\ActivityDaemonKeepAlive</URI>\n",
        "  </RegistrationInfo>\n",
        "  <Triggers>\n",
        "    <LogonTrigger>\n",
        "      <Enabled>true</Enabled>\n",
        "    </LogonTrigger>\n",
        "    <CalendarTrigger>\n",
        "      <Repetition>\n",
        "        <Interval>PT1M</Interval>\n",
        "        <StopAtDurationEnd>false</StopAtDurationEnd>\n",
        "      </Repetition>\n",
        "      <StartBoundary>2000-01-01T00:00:00</StartBoundary>\n",
        "      <Enabled>true</Enabled>\n",
        "      <ScheduleByDay>\n",
        "        <DaysInterval>1</DaysInterval>\n",
        "      </ScheduleByDay>\n",
        "    </CalendarTrigger>\n",
        "  </Triggers>\n",
        "  <Principals>\n",
        "    <Principal id=\"Author\">\n",
        "      <LogonType>InteractiveToken</LogonType>\n",
        "      <RunLevel>LeastPrivilege</RunLevel>\n",
        "    </Principal>\n",
        "  </Principals>\n",
        "  <Settings>\n",
        "    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n",
        "    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n",
        "    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n",
        "    <AllowHardTerminate>true</AllowHardTerminate>\n",
        "    <StartWhenAvailable>true</StartWhenAvailable>\n",
        "    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\n",
        "    <IdleSettings>\n",
        "      <StopOnIdleEnd>false</StopOnIdleEnd>\n",
        "      <RestartOnIdle>false</RestartOnIdle>\n",
        "    </IdleSettings>\n",
        "    <AllowStartOnDemand>true</AllowStartOnDemand>\n",
        "    <Enabled>true</Enabled>\n",
        "    <Hidden>true</Hidden>\n",
        "    <RunOnlyIfIdle>false</RunOnlyIfIdle>\n",
        "    <WakeToRun>false</WakeToRun>\n",
        "    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n",
        "    <Priority>7</Priority>\n",
        "    <RestartOnFailure>\n",
        "      <Interval>PT1M</Interval>\n",
        "      <Count>3</Count>\n",
        "    </RestartOnFailure>\n",
        "  </Settings>\n",
        "  <Actions Context=\"Author\">\n",
        "    <Exec>\n",
        "      <Command>C:\\tools\\activity-daemon.exe</Command>\n",
        "      <Arguments>ensure-run</Arguments>\n",
        "      <WorkingDirectory>C:\\activity</WorkingDirectory>\n",
        "    </Exec>\n",
        "  </Actions>\n",
        "</Task>\n",
    );
}
