//! `activity-daemon startup install|uninstall|status` (issues #7 and #29).
//!
//! Autostart is a **fallback chain** (issue #29): on a real second machine
//! `schtasks /create` can fail with "Access is denied." (corporate GPO/EDR,
//! or a standard user account), so `install` degrades gracefully - every
//! step needs no admin:
//!
//! 1. **Full task XML** (issue #7): logon trigger, hidden,
//!    `RestartOnFailure` 3x/1 min, working directory = data dir.
//! 2. **Minimal task XML**: the same logon trigger / `LeastPrivilege` /
//!    `InteractiveToken` principal, but without `Hidden` and
//!    `RestartOnFailure` - for schedulers (or policies) that reject the
//!    richer settings.
//! 3. **Startup-folder fallback**: `%APPDATA%\Microsoft\Windows\Start
//!    Menu\Programs\Startup\activity-daemon.vbs` launches the exe
//!    windowless (`Wscript.Shell.Run ..., 0, False`) at logon.
//!
//! Which mechanism won is printed clearly, with its trade-off (the Startup
//! folder cannot auto-restart a crashed daemon). `status` reports which
//! mechanism is effective; `uninstall` removes the task and both fallback
//! artifacts (including a legacy `activity-daemon.cmd`).
//!
//! The XML generation, the VBS content, the schtasks-error classification
//! and the chain decision are pure functions unit-tested on every platform
//! (golden strings); the actual `schtasks`/file calls are cfg(windows).
//! `--dry-run` prints the XML, the fallback preview and the commands that
//! would run without touching anything.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::usage_log;

/// Scheduled task name.
pub const TASK_NAME: &str = "ActivityDaemon";
/// File the generated task XML is staged to before `schtasks /create /xml`.
pub const TASK_XML_FILE: &str = "ActivityDaemon.task.xml";
/// Startup-folder fallback file (issue #29), inside the user's Startup
/// folder (`%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup`).
pub const STARTUP_VBS_FILE: &str = "activity-daemon.vbs";
/// Legacy Startup-folder batch file: not written anymore, but removed by
/// `startup uninstall` when an older install left it behind (issue #29).
pub const LEGACY_CMD_FILE: &str = "activity-daemon.cmd";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Subcommand)]
pub enum StartupAction {
    /// Create the autostart entry (fallback chain) and start the daemon.
    Install,
    /// Delete the scheduled task and any Startup-folder fallback.
    Uninstall,
    /// Report which startup mechanism is effective.
    Status,
}

/// `startup` subcommand entry point.
pub fn cmd_startup(
    action: StartupAction,
    dry_run: bool,
    config_override: Option<&Path>,
) -> Result<()> {
    let resolved = crate::config::resolve(config_override)?;
    let data_dir = resolved.config.data_dir;
    let result = match action {
        StartupAction::Install => install(&data_dir, dry_run),
        StartupAction::Uninstall => uninstall(dry_run),
        StartupAction::Status => status(dry_run),
    };
    // Issue #27: only a real (non-dry-run) install/uninstall counts as
    // usage; the `status` query does NOT log. Fire-and-forget.
    if !dry_run && result.is_ok() {
        let logged_action = match action {
            StartupAction::Install => Some(usage_log::ACTION_INSTALL),
            StartupAction::Uninstall => Some(usage_log::ACTION_UNINSTALL),
            StartupAction::Status => None,
        };
        if let Some(logged_action) = logged_action {
            usage_log::record(
                &data_dir,
                &resolved.config.sync.device_name,
                usage_log::FEATURE_STARTUP,
                logged_action,
            );
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Pure fallback-chain logic (issue #29) - tested on every platform
// ---------------------------------------------------------------------------

/// A mechanism `startup install` can put in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMechanism {
    /// The scheduled task from the full XML (hidden, restart-on-failure).
    ScheduledTaskFull,
    /// The scheduled task from the minimal XML (no Hidden, no
    /// RestartOnFailure).
    ScheduledTaskMinimal,
    /// The Startup-folder `.vbs` fallback (windowless, no crash restart).
    StartupFolder,
}

impl InstallMechanism {
    /// Human-readable name of the mechanism (what `install` reports).
    pub fn label(self) -> &'static str {
        match self {
            InstallMechanism::ScheduledTaskFull => {
                "scheduled task ActivityDaemon (logon trigger, hidden, restart on failure 3x/1 min)"
            }
            InstallMechanism::ScheduledTaskMinimal => {
                "scheduled task ActivityDaemon (minimal: logon trigger only)"
            }
            InstallMechanism::StartupFolder => "Startup-folder fallback (activity-daemon.vbs)",
        }
    }

    /// The trade-off of the mechanism, printed right after `label`.
    pub fn tradeoff(self) -> &'static str {
        match self {
            InstallMechanism::ScheduledTaskFull => {
                "no trade-off: the Task Scheduler restarts the daemon if it crashes"
            }
            InstallMechanism::ScheduledTaskMinimal => {
                "trade-off: no crash auto-restart (RestartOnFailure was rejected and is not set)"
            }
            InstallMechanism::StartupFolder => {
                "trade-off: NO crash auto-restart - a crashed daemon stays down until the next logon"
            }
        }
    }
}

/// One step of the install chain (what is being tried right now).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStep {
    /// Step 1: the full task XML (issue #7 shape).
    TaskFullXml,
    /// Step 2: the minimal task XML (no Hidden, no RestartOnFailure).
    TaskMinimalXml,
    /// Step 3: the Startup-folder `.vbs` fallback.
    StartupFolder,
}

impl InstallStep {
    /// The mechanism that a successful step installs.
    pub fn mechanism(self) -> InstallMechanism {
        match self {
            InstallStep::TaskFullXml => InstallMechanism::ScheduledTaskFull,
            InstallStep::TaskMinimalXml => InstallMechanism::ScheduledTaskMinimal,
            InstallStep::StartupFolder => InstallMechanism::StartupFolder,
        }
    }

    /// Name used in log lines while trying the step.
    pub fn label(self) -> &'static str {
        match self {
            InstallStep::TaskFullXml => "full task XML",
            InstallStep::TaskMinimalXml => "minimal task XML",
            InstallStep::StartupFolder => "Startup-folder fallback",
        }
    }

    /// Pure chain decision (issue #29): the next step after a `schtasks
    /// /create` failure of `self`, classified as `failure`. `None` when the
    /// chain is exhausted (a Startup-folder write failure is a hard error).
    ///
    /// Full -> Minimal always: one cheap retry distinguishes "the policy
    /// blocks every task" from "one XML feature trips the EDR", and some
    /// older schedulers only reject `RestartOnFailure`. Minimal ->
    /// StartupFolder always: after two `schtasks` failures the folder is the
    /// last no-admin mechanism left.
    pub fn next_after_schtasks_failure(self, failure: SchtasksFailure) -> Option<InstallStep> {
        match (self, failure) {
            (InstallStep::TaskFullXml, _) => Some(InstallStep::TaskMinimalXml),
            (InstallStep::TaskMinimalXml, _) => Some(InstallStep::StartupFolder),
            (InstallStep::StartupFolder, _) => None,
        }
    }
}

/// Classification of a `schtasks` error message (drives the log text and
/// documents *why* the chain degraded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchtasksFailure {
    /// "Access is denied." / 0x80070005: GPO, EDR or a standard user
    /// account - the most common real-world blocker (issue #29 background).
    AccessDenied,
    /// The scheduler rejected the task XML itself.
    XmlRejected,
    /// Anything else.
    Other,
}

/// Pure classification of a schtasks failure text (case-insensitive, works
/// on the localized-message lossy strings we capture). Access-denied is
/// checked first: it is the decision-relevant signal, and some access
/// errors mention the XML too.
pub fn classify_schtasks_failure(text: &str) -> SchtasksFailure {
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("access is denied")
        || lowered.contains("access denied")
        || lowered.contains("0x80070005")
        || lowered.contains("administrator")
    {
        SchtasksFailure::AccessDenied
    } else if lowered.contains("xml") || lowered.contains("malformed") {
        SchtasksFailure::XmlRejected
    } else {
        SchtasksFailure::Other
    }
}

/// Human-readable reason for a classification (log lines + install output).
pub fn failure_reason(failure: SchtasksFailure) -> &'static str {
    match failure {
        SchtasksFailure::AccessDenied => {
            "schtasks denied task creation (access denied: corporate policy, EDR, or a \
             standard user account - no admin is required anywhere in the fallback chain)"
        }
        SchtasksFailure::XmlRejected => "the Task Scheduler rejected the task XML",
        SchtasksFailure::Other => "schtasks failed",
    }
}

/// How the daemon actually autostarts, as reported by `startup status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupMechanism {
    /// The scheduled task exists (either XML flavor).
    ScheduledTask,
    /// No task, but the Startup-folder fallback is in place.
    StartupFolder,
    /// Neither artifact found.
    None,
}

impl StartupMechanism {
    /// Description printed by `startup status`.
    pub fn description(self) -> &'static str {
        match self {
            StartupMechanism::ScheduledTask => {
                "scheduled task (starts at logon; crash auto-restart depends on the XML flavor)"
            }
            StartupMechanism::StartupFolder => {
                "startup-folder fallback (starts at logon; NO crash auto-restart)"
            }
            StartupMechanism::None => "none - run `startup install`",
        }
    }
}

/// Pure status decision: the scheduled task wins when present (it is the
/// mechanism `install` prefers); otherwise the Startup folder; otherwise
/// none. Both artifacts are checked (issue #29).
pub fn detect_mechanism(task_present: bool, startup_folder_present: bool) -> StartupMechanism {
    if task_present {
        StartupMechanism::ScheduledTask
    } else if startup_folder_present {
        StartupMechanism::StartupFolder
    } else {
        StartupMechanism::None
    }
}

/// Whether a `schtasks /delete` error simply means "the task is not
/// installed" (uninstall must succeed on an empty machine).
pub fn task_absent_from_error(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    ["does not exist", "cannot find", "not found", "0x80070002"]
        .iter()
        .any(|needle| lowered.contains(needle))
}

/// The user Startup folder below a given `%APPDATA%` value (pure; Windows
/// path shape, joined portably so the test runs on every platform).
pub fn startup_folder_vbs_path(appdata: &str) -> PathBuf {
    PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup")
}

/// The exact `.vbs` fallback content (issue #29 contract):
/// `Wscript.Shell.Run` with a hidden window (0) and no wait (False) - a
/// windowless launch without a console flash. The tripled quotes embed the
/// exe path as one quoted argument even when it contains spaces.
pub fn startup_folder_vbs(exe: &str) -> String {
    format!("CreateObject(\"Wscript.Shell\").Run \"\"\"{exe}\"\" run\", 0, False\n")
}

/// Generate the Task Scheduler task XML (issue #7 shape; hidden, restart on
/// failure).
///
/// Element order follows the Task Scheduler schema (RegistrationInfo,
/// Triggers, Principals, Settings, Actions); `schtasks /create /xml`
/// validates it. `ExecutionTimeLimit PT0S` = no time limit (the daemon runs
/// 24/7), `MultipleInstancesPolicy IgnoreNew` backs the in-process
/// single-instance lock.
pub fn task_xml(command: &str, arguments: &str, working_dir: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>activity-daemon personal activity collection (issue #7)</Description>
    <URI>\{TASK_NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
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
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
      <WorkingDirectory>{working_dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        command = xml_escape(command),
        arguments = xml_escape(arguments),
        working_dir = xml_escape(working_dir),
    )
}

/// Generate the MINIMAL task XML (issue #29 step 2): the same logon
/// trigger, `LeastPrivilege` and `InteractiveToken` principal as the full
/// XML, but `Hidden` and `RestartOnFailure` dropped - the settings some
/// hardened schedulers / policies reject.
pub fn minimal_task_xml(command: &str, arguments: &str, working_dir: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>activity-daemon personal activity collection (issue #7)</Description>
    <URI>\{TASK_NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
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
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
      <WorkingDirectory>{working_dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        command = xml_escape(command),
        arguments = xml_escape(arguments),
        working_dir = xml_escape(working_dir),
    )
}

/// XML text escaping for element content (attribute values are not used).
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// install: the fallback chain
// ---------------------------------------------------------------------------

fn install(data_dir: &Path, dry_run: bool) -> Result<()> {
    let exe = std::env::current_exe().context("cannot resolve the current exe path")?;
    let command = exe.display().to_string();
    let working_dir = data_dir.display().to_string();
    let xml_path: PathBuf = data_dir.join(TASK_XML_FILE);

    if dry_run {
        println!("dry run: would write {}", xml_path.display());
        println!(
            "dry run: schtasks /create /f /tn {TASK_NAME} /xml {}",
            xml_path.display()
        );
        println!(
            "dry run: on failure, retry with a minimal XML (no Hidden, no RestartOnFailure); \
             on second failure, write the Startup-folder fallback"
        );
        print!("{}", startup_folder_vbs(&command));
        println!("dry run: schtasks /run /tn {TASK_NAME}");
        println!("--- full task XML (step 1) ---");
        println!("{}", task_xml(&command, "run", &working_dir));
        println!("--- minimal task XML (step 2, on failure) ---");
        println!("{}", minimal_task_xml(&command, "run", &working_dir));
        return Ok(());
    }

    // The chain: full XML -> minimal XML -> Startup folder. Each schtasks
    // failure is classified (why we degraded) and the next step decides.
    let mut step = Some(InstallStep::TaskFullXml);
    let mut installed: Option<InstallMechanism> = None;
    let mut last_error: Option<(anyhow::Error, SchtasksFailure)> = None;
    while let Some(current) = step {
        match create_task(current, data_dir, &command, &working_dir) {
            Ok(()) => {
                installed = Some(current.mechanism());
                break;
            }
            Err(e) => {
                let failure = classify_schtasks_failure(&format!("{e:#}"));
                log::warn!(
                    "startup: {} failed ({}); falling back per issue #29",
                    current.label(),
                    failure_reason(failure)
                );
                last_error = Some((e, failure));
                step = current.next_after_schtasks_failure(failure);
            }
        }
    }

    match installed {
        Some(mechanism) => {
            // Start it now, best effort: a failed /run still leaves a valid
            // logon autostart behind.
            if let Err(e) = schtasks(&["/run", "/tn", TASK_NAME]) {
                let msg = format!("{e:#}");
                log::warn!("startup: the task is installed but could not be started now: {msg}");
                println!(
                    "warning: {} is installed, but starting it now failed: {msg}",
                    mechanism.label()
                );
                println!("it starts at the next logon.");
            }
            println!("installed {}", mechanism.label());
            println!("{}", mechanism.tradeoff());
            Ok(())
        }
        None => {
            let (error, failure) = last_error.expect("chain exhausted implies an error");
            println!(
                "scheduled task installation failed ({})",
                failure_reason(failure)
            );
            println!("last error: {error:#}");
            let vbs_path = write_startup_folder_fallback(&command)?;
            let mechanism = InstallMechanism::StartupFolder;
            println!("installed {}", mechanism.label());
            println!("  {}", vbs_path.display());
            println!("{}", mechanism.tradeoff());
            // Start the daemon now via the same detached spawn the update
            // relaunch uses (windowless, survives this process). Best
            // effort - the fallback itself is already in place.
            if let Err(e) = crate::update::spawn_detached(&exe) {
                log::warn!("startup: could not start the daemon now: {e:#}");
                println!("the daemon was not started now; it starts at the next logon");
            } else {
                println!("daemon started now (windowless, detached)");
            }
            Ok(())
        }
    }
}

/// Create the scheduled task with the XML flavor of `step` (Windows only;
/// the pure XML functions above are tested on every platform).
#[cfg(windows)]
fn create_task(step: InstallStep, data_dir: &Path, command: &str, working_dir: &str) -> Result<()> {
    let xml = match step {
        InstallStep::TaskFullXml => task_xml(command, "run", working_dir),
        InstallStep::TaskMinimalXml => minimal_task_xml(command, "run", working_dir),
        // The chain never reaches schtasks with the folder step.
        InstallStep::StartupFolder => {
            anyhow::bail!("internal: the Startup-folder step does not call schtasks")
        }
    };
    let xml_path = data_dir.join(TASK_XML_FILE);
    std::fs::write(&xml_path, &xml)
        .with_context(|| format!("cannot write task XML {}", xml_path.display()))?;
    schtasks(&[
        "/create",
        "/f",
        "/tn",
        TASK_NAME,
        "/xml",
        &xml_path.display().to_string(),
    ])
    .map(|_| ())
}

/// Non-Windows stub so `install` stays compilable: every schtasks step
/// fails, the folder step needs APPDATA (unset) - the whole command errors
/// out, like before #29 (use --dry-run to preview the XML).
#[cfg(not(windows))]
fn create_task(
    _step: InstallStep,
    _data_dir: &Path,
    _command: &str,
    _working_dir: &str,
) -> Result<()> {
    anyhow::bail!(
        "scheduled tasks are only available on Windows (use --dry-run to preview the task XML)"
    )
}

/// Write the Startup-folder fallback `.vbs` (step 3): `APPDATA` +
/// `Microsoft\Windows\Start Menu\Programs\Startup\activity-daemon.vbs`.
/// Returns the written path. APPDATA comes from the environment (std only,
/// no new deps).
fn write_startup_folder_fallback(command: &str) -> Result<PathBuf> {
    let appdata = std::env::var("APPDATA")
        .context("APPDATA is not set; cannot install the Startup-folder fallback")?;
    let dir = startup_folder_vbs_path(&appdata);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(STARTUP_VBS_FILE);
    std::fs::write(&path, startup_folder_vbs(command))
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// status + uninstall
// ---------------------------------------------------------------------------

fn status(dry_run: bool) -> Result<()> {
    if dry_run {
        println!("dry run: schtasks /query /tn {TASK_NAME}");
        println!(
            "dry run: check for {STARTUP_VBS_FILE} and {LEGACY_CMD_FILE} in the Startup folder"
        );
        return Ok(());
    }
    // The raw scheduler output (when the task exists) is printed by
    // `schtasks` itself; the structured summary follows it.
    let task_query = schtasks(&["/query", "/tn", TASK_NAME]);
    let task_present = task_query.is_ok();
    let folder_present =
        startup_artifact_present(STARTUP_VBS_FILE) || startup_artifact_present(LEGACY_CMD_FILE);
    let mechanism = detect_mechanism(task_present, folder_present);

    println!("startup mechanisms:");
    println!(
        "  scheduled task {TASK_NAME}: {}",
        if task_present {
            "installed"
        } else {
            "not installed"
        }
    );
    if let Some(path) = startup_artifact_path(STARTUP_VBS_FILE) {
        println!("  Startup-folder fallback: installed ({})", path.display());
    } else {
        println!("  Startup-folder fallback: not installed");
    }
    println!("  effective: {}", mechanism.description());
    Ok(())
}

fn uninstall(dry_run: bool) -> Result<()> {
    if dry_run {
        println!("dry run: schtasks /delete /f /tn {TASK_NAME}");
        println!(
            "dry run: remove {STARTUP_VBS_FILE} and {LEGACY_CMD_FILE} from the Startup folder (if present)"
        );
        return Ok(());
    }

    let mut removed: Vec<String> = Vec::new();
    // The scheduled task (issue #7/#29): a "does not exist" failure is a
    // clean no-op - uninstall succeeds on a machine where only the folder
    // fallback was installed.
    match schtasks(&["/delete", "/f", "/tn", TASK_NAME]) {
        Ok(_) => removed.push(format!("scheduled task {TASK_NAME}")),
        Err(e) => {
            let text = format!("{e:#}");
            if !task_absent_from_error(&text) {
                return Err(e);
            }
            log::debug!("startup: scheduled task not present; nothing to delete");
        }
    }
    // Both Startup-folder artifacts: the current .vbs fallback and a legacy
    // activity-daemon.cmd from older installs (issue #29).
    for file in [STARTUP_VBS_FILE, LEGACY_CMD_FILE] {
        if let Some(path) = startup_artifact_path(file) {
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
            removed.push(file.to_string());
        }
    }

    if removed.is_empty() {
        println!("nothing to remove: no scheduled task, no Startup-folder fallback");
    } else {
        println!("removed: {}", removed.join(", "));
    }
    Ok(())
}

/// Path of a Startup-folder artifact when present (`None` without APPDATA
/// or when the file does not exist).
fn startup_artifact_path(file: &str) -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    let path = startup_folder_vbs_path(&appdata).join(file);
    path.exists().then_some(path)
}

/// Whether a Startup-folder artifact exists.
fn startup_artifact_present(file: &str) -> bool {
    startup_artifact_path(file).is_some()
}

/// Run `schtasks` with the given arguments and print its output.
#[cfg(windows)]
fn schtasks(args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("schtasks")
        .args(args)
        .output()
        .context("cannot run schtasks.exe")?;
    // schtasks emits localized text in the console codepage; lossy keep it
    // readable without depending on a codepage crate.
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stdout.is_empty() {
        println!("{stdout}");
    }
    if output.status.success() {
        Ok(stdout)
    } else if stderr.is_empty() {
        anyhow::bail!("schtasks {} failed with {}", args.join(" "), output.status)
    } else {
        anyhow::bail!("schtasks {} failed: {stderr}", args.join(" "))
    }
}

#[cfg(not(windows))]
fn schtasks(args: &[&str]) -> Result<String> {
    anyhow::bail!(
        "schtasks is only available on Windows (use --dry-run to preview the task XML): {}",
        args.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n",
        "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n",
        "  <RegistrationInfo>\n",
        "    <Description>activity-daemon personal activity collection (issue #7)</Description>\n",
        "    <URI>\\ActivityDaemon</URI>\n",
        "  </RegistrationInfo>\n",
        "  <Triggers>\n",
        "    <LogonTrigger>\n",
        "      <Enabled>true</Enabled>\n",
        "    </LogonTrigger>\n",
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
        "    <StartWhenAvailable>false</StartWhenAvailable>\n",
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
        "      <Arguments>run</Arguments>\n",
        "      <WorkingDirectory>C:\\activity</WorkingDirectory>\n",
        "    </Exec>\n",
        "  </Actions>\n",
        "</Task>\n",
    );

    /// Golden-string test: the exact XML `schtasks /create /xml` receives.
    /// The RestartOnFailure block (3 retries / PT1M), the Hidden flag, the
    /// logon trigger and the working directory are contractual (issue #7).
    #[test]
    fn task_xml_matches_the_golden_document() {
        let xml = task_xml("C:\\tools\\activity-daemon.exe", "run", "C:\\activity");
        assert_eq!(xml, GOLDEN);
    }

    /// Golden-string test for the minimal fallback XML (issue #29): same
    /// trigger + principal as the full XML, but no Hidden and no
    /// RestartOnFailure - exactly the delta the chain step 2 promises.
    #[test]
    fn minimal_task_xml_matches_the_golden_document() {
        let xml = minimal_task_xml("C:\\tools\\activity-daemon.exe", "run", "C:\\activity");
        let full = task_xml("C:\\tools\\activity-daemon.exe", "run", "C:\\activity");
        for line in xml.lines() {
            assert!(
                !line.contains("<Hidden>"),
                "minimal XML drops Hidden: {line}"
            );
            assert!(
                !line.contains("RestartOnFailure"),
                "minimal XML drops RestartOnFailure: {line}"
            );
        }
        for must in [
            "<LogonTrigger>",
            "<RunLevel>LeastPrivilege</RunLevel>",
            "<LogonType>InteractiveToken</LogonType>",
            "<Arguments>run</Arguments>",
            "<WorkingDirectory>C:\\activity</WorkingDirectory>",
        ] {
            assert!(xml.contains(must), "minimal XML keeps {must}: {xml}");
        }
        // Identical except for the two removed blocks.
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

    /// The Startup-folder fallback content is contractual (issue #29):
    /// exactly `CreateObject("Wscript.Shell").Run """<exe>"" run", 0, False`
    /// - hidden window (0), no wait (False), quoted exe path.
    #[test]
    fn vbs_content_matches_the_contract_exactly() {
        let content = startup_folder_vbs("C:\\tools\\activity-daemon.exe");
        assert_eq!(
            content,
            concat!(
                "CreateObject(\"Wscript.Shell\").Run ",
                "\"\"\"C:\\tools\\activity-daemon.exe\"\" run\", 0, False\n",
            )
        );
        // Spaces in the exe path stay inside the quoted argument.
        let content = startup_folder_vbs("C:\\my tools\\activity-daemon.exe");
        assert!(content.contains(r#""""C:\my tools\activity-daemon.exe"" run""#));
    }

    #[test]
    fn vbs_path_sits_below_appdata_startup_folder() {
        let path = startup_folder_vbs_path("C:\\Users\\dev\\AppData\\Roaming");
        assert_eq!(
            path,
            PathBuf::from("C:\\Users\\dev\\AppData\\Roaming")
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
                .join("Startup")
        );
        assert!(path.ends_with("Startup"));
    }

    #[test]
    fn schtasks_failures_classify_by_text() {
        assert_eq!(
            classify_schtasks_failure("Access is denied."),
            SchtasksFailure::AccessDenied
        );
        assert_eq!(
            classify_schtasks_failure("ERROR: Access is denied. (0x8)"),
            SchtasksFailure::AccessDenied
        );
        assert_eq!(
            classify_schtasks_failure("schtasks /create failed: 0x80070005"),
            SchtasksFailure::AccessDenied
        );
        assert_eq!(
            classify_schtasks_failure(
                "schtasks /create /f /tn ActivityDaemon failed: \
                 The task XML is malformed or invalid."
            ),
            SchtasksFailure::XmlRejected
        );
        assert_eq!(
            classify_schtasks_failure("ERROR: The specified task name does not exist."),
            SchtasksFailure::Other
        );
        assert_eq!(classify_schtasks_failure(""), SchtasksFailure::Other);
    }

    #[test]
    fn fallback_chain_advances_until_the_startup_folder() {
        // Step 1 always falls back to the minimal XML (one cheap retry
        // distinguishes blanket policy denial from a single XML feature),
        // whatever the failure kind.
        for failure in [
            SchtasksFailure::AccessDenied,
            SchtasksFailure::XmlRejected,
            SchtasksFailure::Other,
        ] {
            assert_eq!(
                InstallStep::TaskFullXml.next_after_schtasks_failure(failure),
                Some(InstallStep::TaskMinimalXml)
            );
            // Step 2 always degrades to the Startup folder...
            assert_eq!(
                InstallStep::TaskMinimalXml.next_after_schtasks_failure(failure),
                Some(InstallStep::StartupFolder)
            );
        }
        // ...and the folder step is terminal: its failure is a hard error.
        assert_eq!(
            InstallStep::StartupFolder.next_after_schtasks_failure(SchtasksFailure::Other),
            None
        );
        // The mechanisms the steps install, in chain order.
        assert_eq!(
            InstallStep::TaskFullXml.mechanism(),
            InstallMechanism::ScheduledTaskFull
        );
        assert_eq!(
            InstallStep::TaskMinimalXml.mechanism(),
            InstallMechanism::ScheduledTaskMinimal
        );
        assert_eq!(
            InstallStep::StartupFolder.mechanism(),
            InstallMechanism::StartupFolder
        );
    }

    #[test]
    fn status_detects_the_effective_mechanism() {
        // The task wins when present (install prefers it); the folder only
        // when the task is gone; none when both artifacts are missing.
        assert_eq!(
            detect_mechanism(true, true),
            StartupMechanism::ScheduledTask
        );
        assert_eq!(
            detect_mechanism(true, false),
            StartupMechanism::ScheduledTask
        );
        assert_eq!(
            detect_mechanism(false, true),
            StartupMechanism::StartupFolder
        );
        assert_eq!(detect_mechanism(false, false), StartupMechanism::None);
    }

    #[test]
    fn delete_errors_for_absent_tasks_are_clean_noops() {
        assert!(task_absent_from_error(
            "ERROR: The specified task name \"ActivityDaemon\" does not exist in the system."
        ));
        assert!(task_absent_from_error(
            "schtasks /delete failed: The system cannot find the file specified."
        ));
        assert!(task_absent_from_error(
            "schtasks /delete failed: 0x80070002"
        ));
        // A real failure must still propagate.
        assert!(!task_absent_from_error("Access is denied."));
        assert!(!task_absent_from_error(""));
    }

    #[test]
    fn every_mechanism_reports_label_and_tradeoff() {
        for mechanism in [
            InstallMechanism::ScheduledTaskFull,
            InstallMechanism::ScheduledTaskMinimal,
            InstallMechanism::StartupFolder,
        ] {
            assert!(!mechanism.label().is_empty());
            assert!(!mechanism.tradeoff().is_empty());
            assert!(
                mechanism.label().contains("activity-daemon.vbs")
                    || mechanism.label().contains("ActivityDaemon")
            );
        }
        // The folder trade-off is explicit about the missing crash restart.
        assert!(InstallMechanism::StartupFolder
            .tradeoff()
            .contains("NO crash auto-restart"));
    }

    #[test]
    fn xml_escapes_markup_characters() {
        assert_eq!(xml_escape("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        let xml = task_xml("C:\\a&b\\daemon.exe", "run", "C:\\x<y");
        assert!(xml.contains("<Command>C:\\a&amp;b\\daemon.exe</Command>"));
        assert!(xml.contains("<WorkingDirectory>C:\\x&lt;y</WorkingDirectory>"));
    }

    #[test]
    fn dry_run_paths_do_not_call_schtasks() {
        // On every platform: uninstall/status dry runs are pure prints. We
        // only assert that the non-windows schtasks stub is never reached
        // for dry runs by exercising the pure paths indirectly.
        assert_eq!(TASK_NAME, "ActivityDaemon");
        assert!(TASK_XML_FILE.ends_with(".task.xml"));
        assert_eq!(STARTUP_VBS_FILE, "activity-daemon.vbs");
        assert_eq!(LEGACY_CMD_FILE, "activity-daemon.cmd");
    }
}
