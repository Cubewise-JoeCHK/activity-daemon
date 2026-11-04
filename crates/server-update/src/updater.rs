//! The hub update flow (issue #32): keep the `activity-server` container on
//! the latest GitHub Release, mirroring the client's safety order (#17):
//!
//! 1. Probe the running version: `/health` `version` first (what actually
//!    runs), then the current image's `org.opencontainers.image.version`
//!    label. Unknown = adoption of a pre-updater deployment: proceed (the
//!    data volume is never touched; the previous image id is kept as the
//!    rollback target).
//! 2. Fetch `releases/latest` and compare (shared `activity-update` crate).
//! 3. Download `activity-server-linux-x86_64` + `.sha256` into the staging
//!    dir and verify: a mismatch deletes the download and aborts - the old
//!    container keeps running.
//! 4. Build a thin image from the verified binary (`release.containerfile`
//!    below, same runtime/env contract as the source `Containerfile`; no
//!    compilation happens on the hub), tagged `activity-server:<tag>` and
//!    `activity-server:latest`, labeled with the release tag.
//! 5. Smoke test: `podman run --rm --network none <image> --version` must
//!    report the release version - the container is not touched until the
//!    new image provably boots.
//! 6. Swap: `podman stop` (SIGTERM, server drains gracefully) -> `rm` ->
//!    `run -d` with the args re-derived from the inspect state (ports,
//!    binds, ACTIVITY_* env) plus `--restart=on-failure` for crash
//!    resilience. Any failure here restores the previous image id.
//! 7. Health gate: `/health` must answer within the budget, else the
//!    previous image is started again (automatic rollback).
//!
//! The saved state (run args + rollback image id) lets the `watch` loop
//! self-heal a container that vanished mid-swap (e.g. a `systemctl stop`
//! that killed the updater between the `stop` and `run` steps).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use activity_update::{
    decide_update, parse_tag, sanitize_tag, select_asset_pair, verify_sha256, AssetDownloader,
    ReleaseFetcher, ReleaseInfo, UpdateDecision,
};
use anyhow::{anyhow, bail, Context, Result};
use semver::Version;

use crate::podman::{
    derive_run_args, health_port, image_ref, ContainerInspect, Podman, CONTAINER_NAME, IMAGE_NAME,
    STOP_TIMEOUT_S, VERSION_LABEL,
};
use crate::state::{self, UpdateState};

/// Asset name the release workflow uploads for the Linux x86_64 server
/// binary (see .github/workflows/release.yml).
pub const SERVER_BINARY_ASSET: &str = "activity-server-linux-x86_64";
/// Health endpoint on the hub host.
pub const HEALTH_PATH: &str = "/health";
/// Default health probe port (docs/sync.md contract).
pub const DEFAULT_PORT: u16 = 8080;
/// Total health-gate budget after a swap.
const GATE_TIMEOUT: Duration = Duration::from_secs(20);

/// Thin release image: built from the sha256-verified release binary - no
/// compilation on the hub. Runtime base matches the source `Containerfile`
/// (ubuntu:24.04 keeps glibc parity with the ubuntu-latest CI build; the
/// bookworm-built binary is forward-compatible). Same env / volume / port /
/// entrypoint contract.
pub const RELEASE_CONTAINERFILE: &str = r#"# Thin release image built by activity-server-update (issue #32) from the
# sha256-verified GitHub Release binary in this build context.
# Source builds: see the Containerfile in the repo root.
FROM docker.io/library/ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY activity-server-linux-x86_64 /usr/local/bin/activity-server
ENV ACTIVITY_PORT=8080 \
    ACTIVITY_DB=/data/activity.db \
    ACTIVITY_DATA=/data
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/activity-server"]
"#;

/// `/health` body, as the updater sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthInfo {
    pub ok: bool,
    pub version: Option<String>,
}

/// Liveness/version probe against the running container. All HTTP is behind
/// this trait; tests inject fakes.
pub trait HealthProbe {
    fn probe(&self, port: u16) -> Result<HealthInfo>;
}

/// Real probe: `GET http://127.0.0.1:<port>/health` with a short timeout -
/// the hub host talks to its own published port.
pub struct RealHealthProbe {
    agent: ureq::Agent,
}

impl RealHealthProbe {
    pub fn new() -> RealHealthProbe {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .build();
        RealHealthProbe { agent }
    }
}

impl Default for RealHealthProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthProbe for RealHealthProbe {
    fn probe(&self, port: u16) -> Result<HealthInfo> {
        let body = self
            .agent
            .get(&format!("http://127.0.0.1:{port}{HEALTH_PATH}"))
            .set("User-Agent", &activity_update::user_agent())
            .call()
            .map_err(|e| anyhow!("health probe failed: {e}"))?
            .into_string()
            .map_err(|e| anyhow!("health probe body error: {e}"))?;
        let value: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow!("health probe: malformed JSON: {e}"))?;
        Ok(HealthInfo {
            ok: value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            version: value
                .get("version")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        })
    }
}

/// The wired-up updater. Tests inject fakes for every dependency; the
/// production wiring is [`HubUpdater::real`].
pub struct HubUpdater<'a> {
    pub fetcher: &'a dyn ReleaseFetcher,
    pub downloader: &'a dyn AssetDownloader,
    pub podman: &'a dyn Podman,
    pub health: &'a dyn HealthProbe,
    /// State dir (`state.json`, `update/<tag>/` staging).
    pub state_dir: PathBuf,
    /// Health-gate tuning (defaults: 20 s budget, 1 s poll; tests shrink
    /// both).
    pub gate_timeout: Duration,
    pub gate_poll: Duration,
}

impl<'a> HubUpdater<'a> {
    pub fn new(
        fetcher: &'a dyn ReleaseFetcher,
        downloader: &'a dyn AssetDownloader,
        podman: &'a dyn Podman,
        health: &'a dyn HealthProbe,
        state_dir: PathBuf,
    ) -> HubUpdater<'a> {
        HubUpdater {
            fetcher,
            downloader,
            podman,
            health,
            state_dir,
            gate_timeout: GATE_TIMEOUT,
            gate_poll: Duration::from_secs(1),
        }
    }

    /// Staging dir for a tag's downloads + image build context.
    pub fn staging_dir(&self, tag: &str) -> PathBuf {
        self.state_dir.join("update").join(sanitize_tag(tag))
    }

    /// Running version string of the deployment: /health first, then the
    /// current image's version label. `Ok(None)` = unknown (adoption case).
    /// Unparseable versions count as unknown - never a hard failure.
    pub fn detect_current_version(&self, inspect: &ContainerInspect) -> Result<Option<String>> {
        let port = health_port(inspect);
        if let Ok(info) = self.health.probe(port) {
            if let Some(v) = info.version {
                if parse_tag(&v).is_ok() {
                    return Ok(Some(v));
                }
            }
        }
        let labels = self
            .podman
            .image_labels_json(&image_ref(inspect, IMAGE_NAME))?;
        if let Some(json) = labels {
            let map: BTreeMap<String, String> = serde_json::from_str(json.trim())
                .with_context(|| format!("malformed image labels JSON: {}", json.trim()))?;
            if let Some(tag) = map.get(VERSION_LABEL) {
                if parse_tag(tag).is_ok() {
                    return Ok(Some(tag.clone()));
                }
            }
        }
        Ok(None)
    }

    /// The inspect state + current version + latest release + the decision,
    /// without changing anything. Shared by `check` and `apply`.
    fn decide(
        &self,
    ) -> Result<(
        ContainerInspect,
        Option<String>,
        ReleaseInfo,
        UpdateDecision,
    )> {
        let inspect = self
            .podman
            .container_inspect(CONTAINER_NAME)?
            .ok_or_else(|| {
                anyhow!(
                    "no {CONTAINER_NAME} container found - deploy it once per docs/sync.md \
                 (`podman build -t {IMAGE_NAME} .` + `podman run -d ...`); \
                 the updater adopts it from there"
                )
            })?;
        let current = self.detect_current_version(&inspect)?;
        let release = self.fetcher.fetch_latest()?;
        let current_version = current.as_deref().and_then(|v| parse_tag(v).ok());
        let decision = match current_version {
            Some(v) => decide_update(&v, &release.tag),
            None => match parse_tag(&release.tag) {
                Ok(remote) => UpdateDecision::Available(remote),
                Err(_) => UpdateDecision::MalformedTag(release.tag.clone()),
            },
        };
        Ok((inspect, current, release, decision))
    }

    /// `check`: report only; never touches files or containers.
    pub fn check_only(&self) -> Result<()> {
        let (_, current, release, decision) = self.decide()?;
        let current = current.unwrap_or_else(|| "unknown version".to_string());
        match decision {
            UpdateDecision::UpToDate => println!(
                "up to date: running {current}, no newer release (latest: {})",
                release.tag
            ),
            UpdateDecision::MalformedTag(t) => println!(
                "running {current}; latest release tag {t:?} is not valid semver - skipped"
            ),
            UpdateDecision::Available(remote) => println!(
                "update available: {current} -> {remote} (release {}; \
                 run `activity-server-update apply`)",
                release.tag
            ),
        }
        Ok(())
    }

    /// `apply`: check now and swap the container when a newer release
    /// exists. `Ok(None)` = up to date or skipped; `Ok(Some(tag))` = the
    /// container now runs `<tag>`.
    pub fn check_and_apply(&self) -> Result<Option<String>> {
        match self.check_and_apply_inner() {
            Ok(result) => {
                let message = match &result {
                    Some(tag) => format!("applied {tag}"),
                    None => "up to date".to_string(),
                };
                self.record_result(&message, result.clone());
                Ok(result)
            }
            Err(e) => {
                self.record_result(&format!("error: {e}"), None);
                Err(e)
            }
        }
    }

    fn check_and_apply_inner(&self) -> Result<Option<String>> {
        let (inspect, current, release, decision) = self.decide()?;
        match decision {
            UpdateDecision::UpToDate => {
                log::debug!(
                    "latest release {} <= running {}; up to date",
                    release.tag,
                    current.unwrap_or_else(|| "unknown".to_string())
                );
                Ok(None)
            }
            UpdateDecision::MalformedTag(t) => {
                log::warn!("release tag {t:?} is not valid semver; skipping");
                Ok(None)
            }
            UpdateDecision::Available(remote) => {
                log::info!(
                    "update available: {} -> {remote} (release {})",
                    current.unwrap_or_else(|| "unknown version (adoption)".to_string()),
                    release.tag
                );
                let tag = release.tag.clone();
                self.apply_release(&inspect, &release, &tag, &remote)
                    .map(Some)
            }
        }
    }

    /// Steps (3)-(7): download, verify, build, smoke test, swap, gate.
    fn apply_release(
        &self,
        inspect: &ContainerInspect,
        release: &ReleaseInfo,
        tag: &str,
        remote: &Version,
    ) -> Result<String> {
        let selected = select_asset_pair(&release.assets, SERVER_BINARY_ASSET)?;
        let dir = self.staging_dir(tag);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create the staging dir {}", dir.display()))?;
        let binary = dir.join(SERVER_BINARY_ASSET);
        let digest_file = dir.join(format!("{SERVER_BINARY_ASSET}.sha256"));

        // (3) download + verify: on mismatch delete everything and abort -
        // the old container keeps running.
        let bytes = self
            .downloader
            .download_to(&selected.asset.url, &binary)
            .with_context(|| format!("download of {} failed", selected.asset.name))?;
        log::info!("downloaded {} ({} bytes)", selected.asset.name, bytes);
        self.downloader
            .download_to(&selected.sha256.url, &digest_file)
            .with_context(|| format!("download of {} failed", selected.sha256.name))?;
        let digest = std::fs::read_to_string(&digest_file)
            .with_context(|| format!("cannot read {}", digest_file.display()))?;
        if let Err(e) = verify_sha256(&binary, &digest) {
            let _ = std::fs::remove_dir_all(&dir);
            bail!("{e}; deleted the download, the old container keeps running");
        }
        log::info!("sha256 verified for {}", selected.asset.name);

        // (4) thin image from the verified binary.
        let containerfile = dir.join("release.containerfile");
        std::fs::write(&containerfile, RELEASE_CONTAINERFILE)
            .with_context(|| format!("cannot write {}", containerfile.display()))?;
        let image_tag = format!("{IMAGE_NAME}:{tag}");
        let image_latest = format!("{IMAGE_NAME}:latest");
        self.podman
            .build_image(
                &containerfile,
                &[image_tag.clone(), image_latest],
                tag,
                &dir,
            )
            .context("building the release image")?;

        // (5) smoke test - nothing is swapped until the new image boots.
        let smoke = self
            .podman
            .run_version(&image_tag)
            .context("smoke test of the new image")?;
        if !smoke.contains(&remote.to_string()) {
            let _ = self.podman.remove_image(&image_tag);
            bail!(
                "smoke test failed: `{image_tag} --version` reported {smoke:?}, \
                 expected {} - the old container keeps running",
                remote
            );
        }
        log::info!("smoke test ok: {smoke}");

        // (6) swap, restoring the previous image on any failure.
        let args = derive_run_args(inspect);
        let old_image_id = inspect.image_id.clone();
        let port = health_port(inspect);
        self.podman
            .stop_container(CONTAINER_NAME, STOP_TIMEOUT_S)
            .context("stopping the old container")?;
        self.podman
            .remove_container(CONTAINER_NAME)
            .context("removing the old container")?;
        if let Err(e) = self.podman.run_detached(&args, &image_tag) {
            // Restore: the previous image id still exists.
            match self.podman.run_detached(&args, &old_image_id) {
                Ok(()) => {
                    let _ = self.gate(port);
                    bail!(
                        "cannot start the new container ({e}); \
                         the previous image {old_image_id} is running again"
                    )
                }
                Err(restore) => bail!(
                    "cannot start the new container ({e}); and the rollback failed too \
                     ({restore}): `watch` self-heals from the saved state on its next tick"
                ),
            }
        }

        // (7) health gate with automatic rollback.
        if let Err(e) = self.gate(port) {
            match self.restore_old(&args, &old_image_id) {
                Ok(()) => {
                    let note = self
                        .gate(port)
                        .unwrap_or_else(|_| "still unhealthy".to_string());
                    bail!(
                        "update to {tag} failed the health gate ({e}); rolled back to the \
                         previous image {old_image_id} (gate after rollback: {note})"
                    )
                }
                Err(rb) => bail!(
                    "update to {tag} failed the health gate ({e}); AND the rollback \
                     failed ({rb}): `watch` self-heals from the saved state on its next tick"
                ),
            }
        }
        log::info!("health gate ok: {CONTAINER_NAME} now runs {tag}");

        // Success: persist enough state to self-heal and keep exactly one
        // rollback image; prune the older generation best-effort.
        let previous = state::load(&self.state_dir)?;
        state::save(
            &self.state_dir,
            &UpdateState {
                last_check: Some(now_rfc3339()),
                last_applied_tag: Some(tag.to_string()),
                rollback_image_id: Some(old_image_id.clone()),
                run_args: args.clone(),
                last_result: Some(format!("applied {tag}")),
            },
        )?;
        if let Some(previous_rollback) = previous.rollback_image_id {
            if previous_rollback != old_image_id {
                let _ = self.podman.remove_image(&previous_rollback);
            }
        }
        if let Some(previous_tag) = previous.last_applied_tag {
            if previous_tag != tag {
                // Untag the previous release image (its image id may still
                // be the rollback target; only the name goes away).
                let _ = self
                    .podman
                    .remove_image(&format!("{IMAGE_NAME}:{previous_tag}"));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        Ok(tag.to_string())
    }

    /// Stop + remove + start `image` with `args`, then health-gate.
    fn swap_to(&self, args: &[String], image: &str, port: u16) -> Result<()> {
        self.restore_old(args, image)?;
        self.gate(port)?;
        Ok(())
    }

    /// Stop + remove + start `image` with `args` - no gate (the callers
    /// gate separately and decide what a failure means).
    fn restore_old(&self, args: &[String], image: &str) -> Result<()> {
        let _ = self.podman.stop_container(CONTAINER_NAME, STOP_TIMEOUT_S);
        let _ = self.podman.remove_container(CONTAINER_NAME);
        self.podman.run_detached(args, image)?;
        Ok(())
    }

    /// Poll /health until it answers ok (or the budget runs out). Returns
    /// the reported version when the server provides one.
    fn gate(&self, port: u16) -> Result<String> {
        let attempts =
            (self.gate_timeout.as_secs_f64() / self.gate_poll.as_secs_f64()).ceil() as u32;
        for attempt in 0..attempts {
            if attempt > 0 {
                std::thread::sleep(self.gate_poll);
            }
            match self.health.probe(port) {
                Ok(info) if info.ok => {
                    return Ok(info.version.unwrap_or_else(|| "unknown".to_string()))
                }
                _ => continue,
            }
        }
        bail!(
            "health gate failed: http://127.0.0.1:{port}{HEALTH_PATH} did not answer ok within {:?}",
            self.gate_timeout
        )
    }

    /// Self-heal (watch tick): when the container vanished mid-swap,
    /// recreate it from the saved state (rollback image + run args).
    pub fn ensure_container(&self) -> Result<()> {
        if self.podman.container_inspect(CONTAINER_NAME)?.is_some() {
            return Ok(());
        }
        let st = state::load(&self.state_dir)?;
        match (st.rollback_image_id, st.run_args.is_empty()) {
            (Some(image), false) => {
                log::warn!(
                    "{CONTAINER_NAME} is missing - self-healing from the saved state \
                     (image {image})"
                );
                self.podman.run_detached(&st.run_args, &image)?;
                match self.gate(DEFAULT_PORT) {
                    Ok(version) => log::info!("self-healed: /health ok (version {version})"),
                    Err(e) => log::warn!("self-heal started the container but {e}"),
                }
                Ok(())
            }
            _ => bail!(
                "no {CONTAINER_NAME} container found and no saved state to self-heal from - \
                 deploy it once per docs/sync.md (`podman build -t {IMAGE_NAME} .` + \
                 `podman run -d ...`); the updater adopts it from there"
            ),
        }
    }

    /// `rollback`: stop whatever runs and start the saved rollback image
    /// (the deployment that ran before the last update). The image that was
    /// running just now becomes the new rollback target.
    pub fn rollback(&self) -> Result<()> {
        let st = state::load(&self.state_dir)?;
        let target = st
            .rollback_image_id
            .clone()
            .filter(|id| !id.is_empty())
            .context("no rollback image recorded in the state - nothing to roll back to")?;
        let args = if st.run_args.is_empty() {
            let inspect = self
                .podman
                .container_inspect(CONTAINER_NAME)?
                .context("no container and no saved run args - cannot roll back")?;
            derive_run_args(&inspect)
        } else {
            st.run_args.clone()
        };
        let previous_image = self
            .podman
            .container_inspect(CONTAINER_NAME)?
            .map(|i| i.image_id);
        self.swap_to(&args, &target, DEFAULT_PORT)?;
        log::info!("rolled back to {target}");
        state::save(
            &self.state_dir,
            &UpdateState {
                last_check: Some(now_rfc3339()),
                last_applied_tag: None,
                rollback_image_id: previous_image,
                run_args: args,
                last_result: Some(format!("rolled back to {target}")),
            },
        )?;
        println!("rolled back to {target}");
        Ok(())
    }

    /// Fire-and-forget bookkeeping for `status` (never fails the caller).
    fn record_result(&self, message: &str, applied: Option<String>) {
        let mut st = state::load(&self.state_dir).unwrap_or_default();
        st.last_check = Some(now_rfc3339());
        st.last_result = Some(message.to_string());
        if let Some(tag) = applied {
            st.last_applied_tag = Some(tag);
        }
        let _ = state::save(&self.state_dir, &st);
    }
}

/// RFC3339 local timestamp for the state file (seconds precision).
fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_containerfile_matches_the_env_contract() {
        // Same contract as the source Containerfile / docs/sync.md.
        for needle in [
            "FROM docker.io/library/ubuntu:24.04",
            "COPY activity-server-linux-x86_64 /usr/local/bin/activity-server",
            "ACTIVITY_PORT=8080",
            "ACTIVITY_DB=/data/activity.db",
            "ACTIVITY_DATA=/data",
            "VOLUME /data",
            "EXPOSE 8080",
            "ENTRYPOINT [\"/usr/local/bin/activity-server\"]",
        ] {
            assert!(
                RELEASE_CONTAINERFILE.contains(needle),
                "release containerfile must contain {needle:?}"
            );
        }
    }
}
