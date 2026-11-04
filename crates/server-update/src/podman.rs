//! podman access for the hub updater (issue #32): the exact calls needed to
//! inspect, rebuild and swap the `activity-server` container, every one of
//! them behind the [`Podman`] trait so the whole flow is unit-tested
//! against a scripted fake - no podman, no containers, no network.
//!
//! Run-argument derivation is the safety-critical pure part: the swap
//! recreates the container from `podman container inspect` state (ports,
//! binds, ACTIVITY_* env, network mode) so an update never silently changes
//! the deployment contract of docs/sync.md.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// Container the updater manages (docs/sync.md deploy naming).
pub const CONTAINER_NAME: &str = "activity-server";
/// Image the updater builds/retags (docs/sync.md build naming).
pub const IMAGE_NAME: &str = "activity-server";
/// Label stamped on every updater-built image: the release tag it came
/// from. Read back when /health cannot answer (server too old to report a
/// version, or down).
pub const VERSION_LABEL: &str = "org.opencontainers.image.version";
/// Graceful stop budget for the swap (SIGTERM -> the server drains;
/// see crates/server main.rs shutdown_signal).
pub const STOP_TIMEOUT_S: u64 = 30;

/// The fields of `podman container inspect` the updater needs (the command
/// returns a JSON array with one object; only what we read is declared, so
/// unrelated podman fields never break the parse).
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerInspect {
    pub name: String,
    /// Image id the container was created from - the rollback target of a
    /// swap (works even when the old image carries no version label).
    pub image_id: String,
    pub config: ConfigSection,
    pub host_config: HostConfigSection,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigSection {
    /// `K=V` entries (`Config.Env` in podman JSON).
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostConfigSection {
    /// `host`, `bridge` or empty (podman default = bridge).
    pub network_mode: String,
    /// `"8080/tcp" -> [{HostIp, HostPort}]`.
    pub port_bindings: BTreeMap<String, Vec<PortBinding>>,
    /// Volume binds, verbatim (`/srv/activity:/data`, the whois socket, ...).
    pub binds: Vec<String>,
    /// Restart policy name: "", "no", "on-failure", "always", ...
    pub restart_policy: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PortBinding {
    pub host_ip: String,
    pub host_port: String,
}

// Wire types: podman's docker-compatible inspect JSON (capitalized keys).
#[derive(Debug, Deserialize)]
struct RawInspect {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "Config")]
    config: RawConfig,
    #[serde(rename = "HostConfig")]
    host_config: RawHostConfig,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(rename = "Env", default)]
    env: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawHostConfig {
    #[serde(rename = "NetworkMode", default)]
    network_mode: Option<String>,
    #[serde(rename = "PortBindings", default)]
    port_bindings: Option<BTreeMap<String, Option<Vec<RawPortBinding>>>>,
    #[serde(rename = "Binds", default)]
    binds: Option<Vec<String>>,
    #[serde(rename = "RestartPolicy", default)]
    restart_policy: Option<RawRestartPolicy>,
}

#[derive(Debug, Deserialize)]
struct RawPortBinding {
    #[serde(rename = "HostIp", default)]
    host_ip: Option<String>,
    #[serde(rename = "HostPort", default)]
    host_port: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRestartPolicy {
    #[serde(rename = "Name", default)]
    name: Option<String>,
}

/// Parse the stdout of `podman container inspect <name>` (a JSON array with
/// exactly one object).
pub fn parse_container_inspect(stdout: &str) -> Result<ContainerInspect> {
    let raw: Vec<RawInspect> = serde_json::from_str(stdout)
        .with_context(|| format!("malformed container inspect JSON: {}", clip(stdout)))?;
    let raw = raw
        .into_iter()
        .next()
        .context("container inspect returned an empty array")?;
    Ok(ContainerInspect {
        name: raw.name,
        image_id: raw.image,
        config: ConfigSection {
            env: raw.config.env.unwrap_or_default(),
        },
        host_config: HostConfigSection {
            network_mode: raw.host_config.network_mode.unwrap_or_default(),
            port_bindings: raw
                .host_config
                .port_bindings
                .unwrap_or_default()
                .into_iter()
                .map(|(key, bindings)| {
                    let bindings = bindings
                        .unwrap_or_default()
                        .into_iter()
                        .map(|b| PortBinding {
                            host_ip: b.host_ip.unwrap_or_default(),
                            host_port: b.host_port.unwrap_or_default(),
                        })
                        .collect();
                    (key, bindings)
                })
                .collect(),
            binds: raw.host_config.binds.unwrap_or_default(),
            restart_policy: raw
                .host_config
                .restart_policy
                .and_then(|p| p.name)
                .unwrap_or_default(),
        },
    })
}

/// Every podman interaction the updater performs. The real implementation
/// shells out to the `podman` CLI (rootless podman runs as the same user as
/// the updater); tests inject a scripted fake.
pub trait Podman {
    /// Inspect the container; `Ok(None)` when it does not exist.
    fn container_inspect(&self, name: &str) -> Result<Option<ContainerInspect>>;
    /// Labels of an image as raw JSON (`{"k": "v"}`), or None when the
    /// image is unknown.
    fn image_labels_json(&self, image: &str) -> Result<Option<String>>;
    /// `podman build` a thin image: one or more `-t` tags plus a version
    /// label, build context = `context` with `containerfile` inside.
    fn build_image(
        &self,
        containerfile: &Path,
        tags: &[String],
        version_label: &str,
        context: &Path,
    ) -> Result<()>;
    /// Smoke test: run the image once (no network) with `--version` and
    /// return stdout.
    fn run_version(&self, image: &str) -> Result<String>;
    /// Graceful stop with an explicit timeout budget.
    fn stop_container(&self, name: &str, timeout_s: u64) -> Result<()>;
    /// Remove the (stopped) container.
    fn remove_container(&self, name: &str) -> Result<()>;
    /// `podman run -d <args...> <image>` - args as derived by
    /// [`derive_run_args`].
    fn run_detached(&self, args: &[String], image: &str) -> Result<()>;
    /// Remove an image reference (rollback pruning); best effort.
    fn remove_image(&self, image: &str) -> Result<()>;
    /// Whether a usable `podman` CLI is on PATH.
    fn available(&self) -> bool;
}

/// The real `podman` CLI.
pub struct RealPodman;

impl Podman for RealPodman {
    fn container_inspect(&self, name: &str) -> Result<Option<ContainerInspect>> {
        let output = Command::new("podman")
            .args(["container", "inspect", name])
            .output()
            .context("cannot run podman (is it installed and on PATH?)")?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Ok(Some(parse_container_inspect(&stdout)?));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no such") {
            return Ok(None);
        }
        bail!("podman container inspect {name} failed: {}", stderr.trim())
    }

    fn image_labels_json(&self, image: &str) -> Result<Option<String>> {
        let output = Command::new("podman")
            .args(["image", "inspect", image])
            .output()
            .context("cannot run podman")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("no such") {
                return Ok(None);
            }
            bail!("podman image inspect {image} failed: {}", stderr.trim())
        }
        // Full inspect array -> [0].Config.Labels (absent = empty map).
        let stdout = String::from_utf8_lossy(&output.stdout);
        let value: serde_json::Value = serde_json::from_str(stdout.trim())
            .with_context(|| format!("malformed image inspect JSON: {}", clip(&stdout)))?;
        let labels = value
            .as_array()
            .and_then(|a| a.first())
            .and_then(|i| i.get("Config"))
            .and_then(|c| c.get("Labels"))
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));
        if labels.is_null() {
            return Ok(Some("{}".to_string()));
        }
        Ok(Some(labels.to_string()))
    }

    fn build_image(
        &self,
        containerfile: &Path,
        tags: &[String],
        version_label: &str,
        context: &Path,
    ) -> Result<()> {
        let mut cmd = Command::new("podman");
        cmd.arg("build").arg("-f").arg(containerfile);
        for tag in tags {
            cmd.arg("-t").arg(tag);
        }
        cmd.arg("--label")
            .arg(format!("{VERSION_LABEL}={version_label}"));
        cmd.arg(context);
        let output = cmd.output().context("cannot run podman")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            bail!(
                "podman build failed: {}{}",
                stderr.trim(),
                if stderr.trim().is_empty() {
                    stdout.trim().to_string()
                } else {
                    String::new()
                }
            );
        }
        Ok(())
    }

    fn run_version(&self, image: &str) -> Result<String> {
        let output = Command::new("podman")
            .args(["run", "--rm", "--network", "none", image, "--version"])
            .output()
            .context("cannot run podman")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("smoke test `{image} --version` failed: {}", stderr.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn stop_container(&self, name: &str, timeout_s: u64) -> Result<()> {
        run_ok(&["stop", "-t", &timeout_s.to_string(), name])
    }

    fn remove_container(&self, name: &str) -> Result<()> {
        run_ok(&["rm", name])
    }

    fn run_detached(&self, args: &[String], image: &str) -> Result<()> {
        let mut cmd = Command::new("podman");
        cmd.arg("run").arg("-d");
        for arg in args {
            cmd.arg(arg);
        }
        cmd.arg(image);
        let output = cmd.output().context("cannot run podman")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("podman run -d {image} failed: {}", stderr.trim());
        }
        Ok(())
    }

    fn remove_image(&self, image: &str) -> Result<()> {
        run_ok(&["rmi", image])
    }

    fn available(&self) -> bool {
        Command::new("podman")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Run podman with args, fail on non-zero exit (stderr in the error).
fn run_ok(args: &[&str]) -> Result<()> {
    let output = Command::new("podman")
        .args(args)
        .output()
        .context("cannot run podman")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("podman {} failed: {}", args.join(" "), stderr.trim())
}

/// Rebuild the `podman run` args of an existing container (without the
/// image, which the caller appends): name, host networking or explicit port
/// bindings, volume binds, ACTIVITY_* env carry-over and the restart
/// policy. Contract (issue #32): an update must never silently change the
/// deployment of docs/sync.md - and containers updated by the updater get
/// `on-failure` restart when they had none (crash resilience on the hub).
pub fn derive_run_args(inspect: &ContainerInspect) -> Vec<String> {
    let mut args = vec!["--name".to_string(), CONTAINER_NAME.to_string()];

    if inspect.host_config.network_mode.trim() == "host" {
        args.push("--network".to_string());
        args.push("host".to_string());
    } else {
        for (key, bindings) in &inspect.host_config.port_bindings {
            let (container_port, proto) = split_port_key(key);
            for binding in bindings {
                let mut spec = String::new();
                // Empty or 0.0.0.0 host ip = podman's default: omit it.
                if !binding.host_ip.is_empty() && binding.host_ip != "0.0.0.0" {
                    spec.push_str(&binding.host_ip);
                    spec.push(':');
                }
                spec.push_str(&binding.host_port);
                spec.push(':');
                spec.push_str(&container_port);
                if let Some(ref proto) = proto {
                    if proto != "tcp" {
                        spec.push('/');
                        spec.push_str(proto);
                    }
                }
                args.push("-p".to_string());
                args.push(spec);
            }
        }
    }
    for bind in &inspect.host_config.binds {
        args.push("--volume".to_string());
        args.push(bind.clone());
    }
    // Env contract (docs/sync.md): ACTIVITY_PORT / ACTIVITY_DB /
    // ACTIVITY_DATA / ACTIVITY_TOKEN. Carrying those forward preserves a
    // user-set `ACTIVITY_TOKEN` across the swap; image-internal env (PATH,
    // LANG, ...) is deliberately not re-passed.
    for entry in &inspect.config.env {
        if entry.starts_with("ACTIVITY_") {
            args.push("--env".to_string());
            args.push(entry.clone());
        }
    }
    match inspect.host_config.restart_policy.as_str() {
        "" | "no" => {
            args.push("--restart".to_string());
            args.push("on-failure".to_string());
        }
        other => {
            args.push("--restart".to_string());
            args.push(other.to_string());
        }
    }
    args
}

/// `"8080/tcp"` -> `("8080", Some("tcp"))`; a bare port stays bare.
fn split_port_key(key: &str) -> (String, Option<String>) {
    match key.split_once('/') {
        Some((port, proto)) => (port.to_string(), Some(proto.to_string())),
        None => (key.to_string(), None),
    }
}

/// The port to health-probe: ACTIVITY_PORT when set, else the first
/// host-side port binding, else the documented default 8080.
pub fn health_port(inspect: &ContainerInspect) -> u16 {
    for entry in &inspect.config.env {
        if let Some(value) = entry.strip_prefix("ACTIVITY_PORT=") {
            if let Ok(port) = value.trim().parse::<u16>() {
                return port;
            }
        }
    }
    for bindings in inspect.host_config.port_bindings.values() {
        for binding in bindings {
            if let Ok(port) = binding.host_port.trim().parse::<u16>() {
                return port;
            }
        }
    }
    8080
}

/// Image reference to introspect: the symbolic name when podman reported
/// one, else the image id (older podman leaves ImageName empty).
pub fn image_ref(inspect: &ContainerInspect, image_name: &str) -> String {
    if image_name.is_empty() {
        inspect.image_id.clone()
    } else {
        image_name.to_string()
    }
}

fn clip(s: &str) -> String {
    s.trim().chars().take(200).collect()
}

/// `anyhow!` lives in anyhow's prelude; keep one local helper so error
/// construction sites stay short.
pub fn podman_missing() -> anyhow::Error {
    anyhow!("podman CLI is not available - install podman (see docs/sync.md)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style_a_json() -> String {
        // docs/sync.md style A: explicit port mapping + /srv/activity volume.
        r#" [{
            "Name": "activity-server",
            "Image": "a1b2c3d4e5f6",
            "ImageName": "localhost/activity-server:latest",
            "Config": { "Env": ["PATH=/usr/local/bin:/usr/bin", "ACTIVITY_PORT=8080", "ACTIVITY_DB=/data/activity.db", "ACTIVITY_DATA=/data", "LANG=C.UTF-8"] },
            "HostConfig": {
                "NetworkMode": "bridge",
                "PortBindings": { "8080/tcp": [ { "HostIp": "0.0.0.0", "HostPort": "8080" } ] },
                "Binds": ["/srv/activity:/data"],
                "RestartPolicy": { "Name": "" }
            }
        }]"#
        .to_string()
    }

    fn style_b_json() -> String {
        // docs/sync.md style B: host networking + whois socket mount + a
        // user-set token env + an explicit restart policy to preserve.
        r#" [{
            "Name": "activity-server",
            "Image": "f6e5d4c3b2a1",
            "ImageName": "localhost/activity-server:v0.1.2",
            "Config": { "Env": ["ACTIVITY_TOKEN=hunter2", "ACTIVITY_PORT=9000"] },
            "HostConfig": {
                "NetworkMode": "host",
                "PortBindings": {},
                "Binds": ["/srv/activity:/data", "/run/tailscale/tailscaled.sock:/var/run/tailscale/tailscaled.sock"],
                "RestartPolicy": { "Name": "always" }
            }
        }]"#
        .to_string()
    }

    #[test]
    fn parse_inspect_reads_the_docker_compatible_json() {
        let inspect = parse_container_inspect(&style_a_json()).expect("parse style A");
        assert_eq!(inspect.name, "activity-server");
        assert_eq!(inspect.image_id, "a1b2c3d4e5f6");
        assert_eq!(inspect.host_config.network_mode, "bridge");
        assert_eq!(inspect.host_config.binds, vec!["/srv/activity:/data"]);
        assert_eq!(inspect.host_config.restart_policy, "");
        let bindings = inspect
            .host_config
            .port_bindings
            .get("8080/tcp")
            .expect("8080/tcp bindings");
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].host_ip, "0.0.0.0");
        assert_eq!(bindings[0].host_port, "8080");
        assert!(inspect
            .config
            .env
            .contains(&"ACTIVITY_PORT=8080".to_string()));
    }

    #[test]
    fn parse_inspect_tolerates_missing_optional_fields() {
        // Minimal object: no PortBindings/Binds/RestartPolicy/Env at all.
        let minimal = r#"[{ "Name": "x", "Image": "id", "Config": {}, "HostConfig": {} }]"#;
        let inspect = parse_container_inspect(minimal).expect("parse minimal");
        assert!(inspect.config.env.is_empty());
        assert!(inspect.host_config.port_bindings.is_empty());
        assert!(inspect.host_config.binds.is_empty());
        assert_eq!(inspect.host_config.restart_policy, "");
        // Garbage must error, not panic.
        assert!(parse_container_inspect("not json").is_err());
    }

    #[test]
    fn derive_run_args_rebuilds_style_a() {
        let inspect = parse_container_inspect(&style_a_json()).expect("parse");
        let args = derive_run_args(&inspect);
        let expected = vec![
            "--name",
            "activity-server",
            "-p",
            "8080:8080",
            "--volume",
            "/srv/activity:/data",
            "--env",
            "ACTIVITY_PORT=8080",
            "--env",
            "ACTIVITY_DB=/data/activity.db",
            "--env",
            "ACTIVITY_DATA=/data",
            "--restart",
            "on-failure",
        ];
        assert_eq!(
            args, expected,
            "style A args (0.0.0.0 omitted, ACTIVITY_* only)"
        );
    }

    #[test]
    fn derive_run_args_rebuilds_style_b_and_preserves_policy() {
        let inspect = parse_container_inspect(&style_b_json()).expect("parse");
        let args = derive_run_args(&inspect);
        let expected = vec![
            "--name",
            "activity-server",
            "--network",
            "host",
            "--volume",
            "/srv/activity:/data",
            "--volume",
            "/run/tailscale/tailscaled.sock:/var/run/tailscale/tailscaled.sock",
            "--env",
            "ACTIVITY_TOKEN=hunter2",
            "--env",
            "ACTIVITY_PORT=9000",
            "--restart",
            "always",
        ];
        assert_eq!(
            args, expected,
            "style B args (host net, binds, token, always)"
        );
    }

    #[test]
    fn health_port_prefers_env_then_binding_then_default() {
        let style_a = parse_container_inspect(&style_a_json()).expect("parse");
        assert_eq!(health_port(&style_a), 8080);
        let style_b = parse_container_inspect(&style_b_json()).expect("parse");
        assert_eq!(health_port(&style_b), 9000, "ACTIVITY_PORT env wins");
        let mut no_env = style_a.clone();
        no_env.config.env.clear();
        assert_eq!(health_port(&no_env), 8080, "host-side binding fallback");
        no_env.host_config.port_bindings.clear();
        assert_eq!(health_port(&no_env), 8080, "documented default");
    }

    #[test]
    fn image_ref_prefers_symbolic_name() {
        let inspect = parse_container_inspect(&style_a_json()).expect("parse");
        assert_eq!(
            image_ref(&inspect, "localhost/activity-server:latest"),
            "localhost/activity-server:latest"
        );
        assert_eq!(image_ref(&inspect, ""), "a1b2c3d4e5f6");
        assert!(podman_missing().to_string().contains("podman"));
    }
}
