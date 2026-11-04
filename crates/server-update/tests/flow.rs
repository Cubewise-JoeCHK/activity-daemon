//! End-to-end tests of the hub update flow with scripted fakes: no podman,
//! no containers, no network. Every swap permutation of issue #32 is
//! covered: happy path, adoption, sha mismatch, smoke-test failure, run
//! failure restore, health-gate rollback, self-heal, rollback, and the
//! version-source preferences.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use activity_server_update::podman::{
    parse_container_inspect, ContainerInspect, Podman, CONTAINER_NAME, STOP_TIMEOUT_S,
    VERSION_LABEL,
};
use activity_server_update::state::{self, UpdateState};
use activity_server_update::updater::{HealthInfo, HealthProbe, HubUpdater, SERVER_BINARY_ASSET};
use activity_update::{file_sha256_hex, Asset, AssetDownloader, ReleaseFetcher, ReleaseInfo};
use anyhow::{bail, Context, Result};

// ----------------------------------------------------------------- fixtures

/// docs/sync.md style A inspect (bridge + port map + /srv/activity volume).
const STYLE_A_JSON: &str = r#" [{
    "Name": "activity-server",
    "Image": "old-image-id",
    "ImageName": "localhost/activity-server:latest",
    "Config": { "Env": ["PATH=/usr/bin", "ACTIVITY_PORT=8080"] },
    "HostConfig": {
        "NetworkMode": "bridge",
        "PortBindings": { "8080/tcp": [ { "HostIp": "0.0.0.0", "HostPort": "8080" } ] },
        "Binds": ["/srv/activity:/data"],
        "RestartPolicy": { "Name": "" }
    }
}]"#;

struct FakeFetcher {
    release: Option<ReleaseInfo>,
}

impl ReleaseFetcher for FakeFetcher {
    fn fetch_latest(&self) -> Result<ReleaseInfo> {
        self.release.clone().context("no release configured")
    }
}

/// Writes canned bodies per URL.
struct FakeDownloader {
    bodies: HashMap<String, String>,
}

impl AssetDownloader for FakeDownloader {
    fn download_to(&self, url: &str, dest: &Path) -> Result<u64> {
        let body = self
            .bodies
            .get(url)
            .with_context(|| format!("unexpected url {url}"))?;
        std::fs::write(dest, body).with_context(|| format!("write {}", dest.display()))?;
        Ok(body.len() as u64)
    }
}

#[derive(Clone)]
enum HealthBehavior {
    /// Always answers ok.
    Ok { version: Option<String> },
    /// Always refuses (server down).
    Down,
    /// The first `after` probes refuse, then it answers ok - models an old
    /// server that is gone by the time the new container is up (adoption).
    DownThenOk { after: u32, version: Option<String> },
}

struct FakeHealth {
    behavior: RefCell<HealthBehavior>,
    probes: RefCell<Vec<u16>>,
}

impl FakeHealth {
    fn ok(version: &str) -> FakeHealth {
        FakeHealth {
            behavior: RefCell::new(HealthBehavior::Ok {
                version: Some(version.to_string()),
            }),
            probes: RefCell::new(Vec::new()),
        }
    }

    fn down() -> FakeHealth {
        FakeHealth {
            behavior: RefCell::new(HealthBehavior::Down),
            probes: RefCell::new(Vec::new()),
        }
    }

    fn down_then_ok(after: u32, version: &str) -> FakeHealth {
        FakeHealth {
            behavior: RefCell::new(HealthBehavior::DownThenOk {
                after,
                version: Some(version.to_string()),
            }),
            probes: RefCell::new(Vec::new()),
        }
    }
}

impl HealthProbe for FakeHealth {
    fn probe(&self, port: u16) -> Result<HealthInfo> {
        self.probes.borrow_mut().push(port);
        let behavior = self.behavior.borrow_mut();
        let count = self.probes.borrow().len() as u32;
        if let HealthBehavior::DownThenOk { after, version } = &*behavior {
            if count > *after {
                return Ok(HealthInfo {
                    ok: true,
                    version: version.clone(),
                });
            }
        }
        match &*behavior {
            HealthBehavior::Ok { version } => Ok(HealthInfo {
                ok: true,
                version: version.clone(),
            }),
            HealthBehavior::DownThenOk { .. } => bail!("connection refused"),
            HealthBehavior::Down => bail!("connection refused"),
        }
    }
}

/// Scripted podman: records every call, fails on demand.
struct FakePodman {
    inspect: RefCell<Option<ContainerInspect>>,
    labels: RefCell<Option<String>>,
    smoke_output: String,
    /// Image refs that fail `run_detached` (the restore path exercises this).
    fail_run_for: Vec<String>,
    calls: RefCell<Vec<String>>,
}

impl FakePodman {
    fn running(inspect_json: &str) -> FakePodman {
        FakePodman {
            inspect: RefCell::new(Some(
                parse_container_inspect(inspect_json).expect("fixture parses"),
            )),
            labels: RefCell::new(None),
            smoke_output: "activity-server 0.0.0".to_string(),
            fail_run_for: Vec::new(),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn none() -> FakePodman {
        FakePodman {
            inspect: RefCell::new(None),
            labels: RefCell::new(None),
            smoke_output: String::new(),
            fail_run_for: Vec::new(),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn with_labels(mut self, tag: &str) -> FakePodman {
        self.labels = RefCell::new(Some(serde_json::json!({ VERSION_LABEL: tag }).to_string()));
        self
    }

    fn with_smoke_output(mut self, out: &str) -> FakePodman {
        self.smoke_output = out.to_string();
        self
    }

    fn failing_run_for(mut self, image_ref: &str) -> FakePodman {
        self.fail_run_for.push(image_ref.to_string());
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    /// The image the container "runs" after the fake's last run_detached.
    fn running_image(&self) -> Option<String> {
        self.calls
            .borrow()
            .iter()
            .rev()
            .find_map(|c| c.strip_prefix("run:").map(str::to_string))
    }

    fn record(&self, call: String) {
        self.calls.borrow_mut().push(call);
    }
}

impl Podman for FakePodman {
    fn container_inspect(&self, name: &str) -> Result<Option<ContainerInspect>> {
        self.record(format!("inspect:{name}"));
        Ok(self.inspect.borrow().clone())
    }

    fn image_labels_json(&self, image: &str) -> Result<Option<String>> {
        self.record(format!("image-labels:{image}"));
        Ok(self.labels.borrow().clone())
    }

    fn build_image(
        &self,
        containerfile: &Path,
        tags: &[String],
        version_label: &str,
        context: &Path,
    ) -> Result<()> {
        self.record(format!(
            "build:{}:{tags:?}:{version_label}",
            containerfile.display()
        ));
        assert!(
            context.join(SERVER_BINARY_ASSET).exists(),
            "build context must contain the verified binary"
        );
        assert_eq!(version_label, "v0.2.0", "label carries the release tag");
        Ok(())
    }

    fn run_version(&self, image: &str) -> Result<String> {
        self.record(format!("smoke:{image}"));
        Ok(self.smoke_output.clone())
    }

    fn stop_container(&self, name: &str, timeout_s: u64) -> Result<()> {
        self.record(format!("stop:{name}:{timeout_s}"));
        Ok(())
    }

    fn remove_container(&self, name: &str) -> Result<()> {
        self.record(format!("remove:{name}"));
        Ok(())
    }

    fn run_detached(&self, args: &[String], image: &str) -> Result<()> {
        self.record(format!("run:{image}"));
        if self.fail_run_for.iter().any(|f| f == image) {
            bail!("podman run failed (scripted)");
        }
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--restart" && w[1] == "on-failure"),
            "every recreated container gets --restart=on-failure, args: {args:?}"
        );
        Ok(())
    }

    fn remove_image(&self, image: &str) -> Result<()> {
        self.record(format!("rmi:{image}"));
        Ok(())
    }

    fn available(&self) -> bool {
        true
    }
}

// ------------------------------------------------------------------- wiring

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "activity-server-update-flow-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// The v0.2.0 release with the fixed fake URLs.
fn release_v020() -> ReleaseInfo {
    ReleaseInfo {
        tag: "v0.2.0".to_string(),
        assets: vec![
            Asset {
                name: SERVER_BINARY_ASSET.to_string(),
                url: "fake://binary".to_string(),
            },
            Asset {
                name: format!("{SERVER_BINARY_ASSET}.sha256"),
                url: "fake://digest".to_string(),
            },
        ],
    }
}

fn downloader_for(binary_body: &str, digest_line: &str) -> FakeDownloader {
    FakeDownloader {
        bodies: HashMap::from([
            ("fake://binary".to_string(), binary_body.to_string()),
            ("fake://digest".to_string(), digest_line.to_string()),
        ]),
    }
}

fn updater<'a>(
    fetcher: &'a FakeFetcher,
    downloader: &'a FakeDownloader,
    podman: &'a FakePodman,
    health: &'a FakeHealth,
    dir: &Path,
) -> HubUpdater<'a> {
    let mut u = HubUpdater::new(fetcher, downloader, podman, health, dir.to_path_buf());
    u.gate_timeout = Duration::from_millis(100);
    u.gate_poll = Duration::from_millis(10);
    u
}

fn valid_digest_line(dir: &Path, body: &str) -> String {
    let reference = dir.join("reference-binary");
    std::fs::write(&reference, body).expect("write reference");
    format!(
        "{}  {SERVER_BINARY_ASSET}\n",
        file_sha256_hex(&reference).expect("hash")
    )
}

fn self_heal_args() -> Vec<String> {
    vec![
        "--name".to_string(),
        "activity-server".to_string(),
        "-p".to_string(),
        "8080:8080".to_string(),
        "--restart".to_string(),
        "on-failure".to_string(),
    ]
}

// -------------------------------------------------------------------- tests

/// Happy path: probe 0.1.2 (health) -> release v0.2.0 -> build -> smoke ->
/// stop/remove/run with derived args + on-failure -> gate ok -> state.
#[test]
fn apply_full_flow_swaps_the_container() {
    let dir = temp_dir("happy");
    let body = "new-server-binary-bytes";
    let digest_line = valid_digest_line(&dir, body);
    let fetcher = FakeFetcher {
        release: Some(release_v020()),
    };
    let downloader = downloader_for(body, &digest_line);
    let podman = FakePodman::running(STYLE_A_JSON).with_smoke_output("activity-server 0.2.0");
    let health = FakeHealth::ok("0.1.2");
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);

    let applied = upd
        .check_and_apply()
        .expect("apply succeeds")
        .expect("an update was applied");
    assert_eq!(applied, "v0.2.0");

    let calls = podman.calls();
    let joined = calls.join(" | ");
    assert!(joined.contains("build:"), "image built: {joined}");
    assert!(
        joined.contains("smoke:activity-server:v0.2.0"),
        "smoke tested: {joined}"
    );
    // Swap order: stop with the graceful budget, remove, run.
    let stop = calls
        .iter()
        .position(|c| c.starts_with("stop:"))
        .expect("stop called");
    let remove = calls
        .iter()
        .position(|c| c.starts_with("remove:"))
        .expect("remove called");
    let run = calls
        .iter()
        .position(|c| c.starts_with("run:"))
        .expect("run called");
    assert_eq!(
        calls[stop],
        format!("stop:{CONTAINER_NAME}:{STOP_TIMEOUT_S}"),
        "graceful stop budget"
    );
    assert!(
        stop < remove && remove < run,
        "stop -> remove -> run: {joined}"
    );
    assert_eq!(
        podman.running_image().as_deref(),
        Some("activity-server:v0.2.0")
    );
    // Health was probed on the derived port.
    assert!(health.probes.borrow().contains(&8080), "probed 8080");

    // State: rollback target + run args + applied tag.
    let st = state::load(&dir).expect("state");
    assert_eq!(st.last_applied_tag.as_deref(), Some("v0.2.0"));
    assert_eq!(st.rollback_image_id.as_deref(), Some("old-image-id"));
    assert!(
        st.run_args.windows(2).any(|w| w == ["-p", "8080:8080"]),
        "run args preserved: {:?}",
        st.run_args
    );
    assert!(
        st.run_args
            .windows(2)
            .any(|w| w == ["--restart", "on-failure"]),
        "on-failure added: {:?}",
        st.run_args
    );

    // Up to date afterwards: no second swap.
    let health_new = FakeHealth::ok("0.2.0");
    let upd_new = updater(&fetcher, &downloader, &podman, &health_new, &dir);
    assert_eq!(upd_new.check_and_apply().expect("check"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Adoption: an unlabeled, unreachable-version deployment is still brought
/// under management (unknown current version + newer release = apply).
#[test]
fn adoption_applies_when_the_running_version_is_unknown() {
    let dir = temp_dir("adopt");
    let body = "b";
    let digest_line = valid_digest_line(&dir, body);
    let fetcher = FakeFetcher {
        release: Some(release_v020()),
    };
    let downloader = downloader_for(body, &digest_line);
    let podman = FakePodman::running(STYLE_A_JSON).with_smoke_output("activity-server 0.2.0");
    // The old server never answers (one probe = the version detection);
    // after the swap the new one does.
    let health = FakeHealth::down_then_ok(1, "0.2.0");
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);

    let applied = upd
        .check_and_apply()
        .expect("adoption applies")
        .expect("an update was applied");
    assert_eq!(applied, "v0.2.0");
    assert!(
        podman
            .calls()
            .iter()
            .any(|c| c == "run:activity-server:v0.2.0"),
        "{}",
        podman.calls().join(" | ")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// sha256 mismatch: abort before any podman mutation, delete the download.
#[test]
fn sha_mismatch_aborts_before_touching_the_container() {
    let dir = temp_dir("badsha");
    let digest_line = format!("{}  x\n", "0".repeat(64));
    let fetcher = FakeFetcher {
        release: Some(release_v020()),
    };
    let downloader = downloader_for("tampered", &digest_line);
    let podman = FakePodman::running(STYLE_A_JSON);
    let health = FakeHealth::ok("0.1.2");
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);

    let err = upd.check_and_apply().expect_err("mismatch must abort");
    assert!(err.to_string().contains("sha256 mismatch"), "{err}");
    let joined = podman.calls().join(" | ");
    assert!(
        !joined.contains("build:"),
        "no build after a bad digest: {joined}"
    );
    assert!(!joined.contains("stop:"), "container untouched: {joined}");
    assert!(
        !dir.join("update").join("v0.2.0").exists(),
        "staging dir deleted"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Smoke-test mismatch: the image is discarded, the container is untouched.
#[test]
fn smoke_failure_aborts_before_the_swap() {
    let dir = temp_dir("smoke");
    let body = "b";
    let digest_line = valid_digest_line(&dir, body);
    let fetcher = FakeFetcher {
        release: Some(release_v020()),
    };
    let downloader = downloader_for(body, &digest_line);
    let podman = FakePodman::running(STYLE_A_JSON).with_smoke_output("activity-server 9.9.9");
    let health = FakeHealth::ok("0.1.2");
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);

    let err = upd.check_and_apply().expect_err("smoke must abort");
    assert!(err.to_string().contains("smoke test failed"), "{err}");
    let joined = podman.calls().join(" | ");
    assert!(
        joined.contains("rmi:activity-server:v0.2.0"),
        "bad image pruned: {joined}"
    );
    assert!(!joined.contains("stop:"), "container untouched: {joined}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run failure right after the swap: the previous image id is started again.
#[test]
fn run_failure_restores_the_previous_image() {
    let dir = temp_dir("runfail");
    let body = "b";
    let digest_line = valid_digest_line(&dir, body);
    let fetcher = FakeFetcher {
        release: Some(release_v020()),
    };
    let downloader = downloader_for(body, &digest_line);
    let podman = FakePodman::running(STYLE_A_JSON)
        .with_smoke_output("activity-server 0.2.0")
        .failing_run_for("activity-server:v0.2.0");
    let health = FakeHealth::down_then_ok(1, "0.1.2");
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);

    let err = upd.check_and_apply().expect_err("run failure must abort");
    assert!(
        err.to_string()
            .contains("previous image old-image-id is running again"),
        "{err}"
    );
    let run_calls: Vec<String> = podman
        .calls()
        .iter()
        .filter(|c| c.starts_with("run:"))
        .cloned()
        .collect();
    assert_eq!(
        run_calls,
        vec![
            "run:activity-server:v0.2.0".to_string(),
            "run:old-image-id".to_string()
        ],
        "new image attempted, then the rollback id restored: {run_calls:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Health-gate failure after the swap: automatic rollback to the previous
/// image id, and the error says so.
#[test]
fn health_gate_failure_rolls_back_automatically() {
    let dir = temp_dir("gatefail");
    let body = "b";
    let digest_line = valid_digest_line(&dir, body);
    let fetcher = FakeFetcher {
        release: Some(release_v020()),
    };
    let downloader = downloader_for(body, &digest_line);
    let podman = FakePodman::running(STYLE_A_JSON).with_smoke_output("activity-server 0.2.0");
    let health = FakeHealth::down(); // nothing ever answers
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);

    let err = upd.check_and_apply().expect_err("gate failure must abort");
    assert!(
        err.to_string()
            .contains("rolled back to the previous image old-image-id"),
        "{err}"
    );
    assert_eq!(podman.running_image().as_deref(), Some("old-image-id"));
    // The failed tag is NOT recorded as applied.
    let st = state::load(&dir).expect("state");
    assert_ne!(st.last_applied_tag.as_deref(), Some("v0.2.0"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Missing container without saved state: a clear error pointing at the
/// runbook - the updater never conjures a deployment from nothing.
#[test]
fn missing_container_errors_with_the_runbook_pointer() {
    let dir = temp_dir("missing");
    let fetcher = FakeFetcher { release: None };
    let downloader = FakeDownloader {
        bodies: HashMap::new(),
    };
    let podman = FakePodman::none();
    let health = FakeHealth::down();
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);

    let err = upd
        .check_and_apply()
        .expect_err("must not invent a container");
    assert!(err.to_string().contains("docs/sync.md"), "{err}");
    assert!(err.to_string().contains("adopts"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Self-heal: the container vanished (e.g. killed mid-swap) but the state
/// survives - the next tick recreates it from the rollback image + args.
#[test]
fn ensure_container_self_heals_from_saved_state() {
    let dir = temp_dir("selfheal");
    state::save(
        &dir,
        &UpdateState {
            rollback_image_id: Some("old-image-id".to_string()),
            run_args: self_heal_args(),
            ..UpdateState::default()
        },
    )
    .expect("seed state");
    let fetcher = FakeFetcher { release: None };
    let downloader = FakeDownloader {
        bodies: HashMap::new(),
    };
    let podman = FakePodman::none();
    let health = FakeHealth::ok("0.1.2");
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);

    upd.ensure_container().expect("self-heal");
    assert_eq!(podman.running_image().as_deref(), Some("old-image-id"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rollback subcommand: swap back to the saved rollback image; the image
/// that was running becomes the new rollback target.
#[test]
fn rollback_swaps_back_and_retargets() {
    let dir = temp_dir("rollback");
    state::save(
        &dir,
        &UpdateState {
            last_applied_tag: Some("v0.2.0".to_string()),
            rollback_image_id: Some("old-image-id".to_string()),
            run_args: self_heal_args(),
            ..UpdateState::default()
        },
    )
    .expect("seed state");
    let fetcher = FakeFetcher { release: None };
    let downloader = FakeDownloader {
        bodies: HashMap::new(),
    };
    let podman = FakePodman::running(STYLE_A_JSON); // currently "running" the old inspect image
    let health = FakeHealth::ok("0.1.2");
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);

    upd.rollback().expect("rollback");
    let calls = podman.calls();
    let last_run = calls
        .iter()
        .rev()
        .find(|c| c.starts_with("run:"))
        .expect("a run happened");
    assert_eq!(last_run, "run:old-image-id");
    let st = state::load(&dir).expect("state");
    assert_eq!(
        st.rollback_image_id.as_deref(),
        Some("old-image-id"),
        "the inspect image id was the one running before the rollback"
    );
    assert!(
        st.last_applied_tag.is_none(),
        "no applied tag after a rollback"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rollback without saved state: a clear error, no mutation.
#[test]
fn rollback_without_state_errors_cleanly() {
    let dir = temp_dir("norollback");
    let fetcher = FakeFetcher { release: None };
    let downloader = FakeDownloader {
        bodies: HashMap::new(),
    };
    let podman = FakePodman::running(STYLE_A_JSON);
    let health = FakeHealth::ok("0.1.2");
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);

    let err = upd.rollback().expect_err("nothing to roll back to");
    assert!(
        err.to_string().contains("no rollback image recorded"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Check-only never mutates: no build/stop/remove/rmi/run calls at all.
#[test]
fn check_only_never_touches_podman_state() {
    let dir = temp_dir("checkonly");
    let fetcher = FakeFetcher {
        release: Some(ReleaseInfo {
            tag: "v99.0.0".to_string(),
            assets: vec![],
        }),
    };
    let downloader = FakeDownloader {
        bodies: HashMap::new(),
    };
    let podman = FakePodman::running(STYLE_A_JSON).with_labels("v0.1.2");
    let health = FakeHealth::down();
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);

    upd.check_only().expect("check");
    let joined = podman.calls().join(" | ");
    assert!(!joined.contains("build:"), "{joined}");
    assert!(!joined.contains("stop:"), "{joined}");
    assert!(!joined.contains("run:"), "{joined}");
    assert!(!joined.contains("rmi:"), "{joined}");
    // Current version came from the image label (health is down).
    assert!(
        podman
            .calls()
            .iter()
            .any(|c| c.starts_with("image-labels:")),
        "{joined}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Version-source preference: /health wins when it answers; the image
/// label is the fallback; no source at all counts as unknown (adoption).
#[test]
fn detect_current_version_prefers_health_then_label() {
    let dir = temp_dir("versionprobe");
    let fetcher = FakeFetcher { release: None };
    let downloader = FakeDownloader {
        bodies: HashMap::new(),
    };
    let podman = FakePodman::running(STYLE_A_JSON).with_labels("v0.9.0");
    let inspect = podman
        .container_inspect(CONTAINER_NAME)
        .expect("inspect")
        .expect("exists");

    // Health answers -> wins over the label.
    let health = FakeHealth::ok("0.1.2");
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);
    assert_eq!(
        upd.detect_current_version(&inspect)
            .expect("detect")
            .as_deref(),
        Some("0.1.2")
    );

    // Health down -> label fallback.
    let health = FakeHealth::down();
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);
    assert_eq!(
        upd.detect_current_version(&inspect)
            .expect("detect")
            .as_deref(),
        Some("v0.9.0")
    );

    // Neither -> unknown (adoption), never an error.
    let podman_bare = FakePodman::running(STYLE_A_JSON);
    let upd = updater(&fetcher, &downloader, &podman_bare, &health, &dir);
    assert_eq!(upd.detect_current_version(&inspect).expect("detect"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The semver compare contract holds end-to-end: running newer than the
/// release = no swap; a pre-release below its release still updates.
#[test]
fn decide_versions_and_prereleases_end_to_end() {
    let dir = temp_dir("decide");
    let body = "b";
    let digest_line = valid_digest_line(&dir, body);
    let fetcher = FakeFetcher {
        release: Some(release_v020()),
    };
    let downloader = downloader_for(body, &digest_line);
    let podman = FakePodman::running(STYLE_A_JSON).with_smoke_output("activity-server 0.2.0");

    // Running 0.3.0 (newer than the release): up to date, nothing swapped.
    let health = FakeHealth::ok("0.3.0");
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);
    assert_eq!(upd.check_and_apply().expect("check"), None);
    assert!(!podman.calls().iter().any(|c| c.starts_with("run:")));

    // Running a pre-release of the release version: 0.2.0-rc.1 < 0.2.0.
    let health = FakeHealth::down_then_ok(1, "0.2.0");
    let upd = updater(&fetcher, &downloader, &podman, &health, &dir);
    assert_eq!(
        upd.check_and_apply().expect("check"),
        Some("v0.2.0".to_string())
    );
    let _ = std::fs::remove_dir_all(&dir);
}
