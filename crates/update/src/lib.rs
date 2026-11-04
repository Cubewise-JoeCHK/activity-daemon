//! Shared auto-update primitives for activity-daemon (issues #17 client,
//! #32 hub): GitHub `releases/latest` check, semver tag compare and
//! sha256-verified asset download.
//!
//! One implementation of the release contract serves two consumers:
//!
//! - the Windows client (`activity-cli` `update` command) swaps its own exe
//!   and restarts;
//! - the hub updater (`activity-server-update`, on the WSL host) rebuilds a
//!   thin container image from the verified `activity-server-linux-x86_64`
//!   release binary and swaps the podman container.
//!
//! The HTTP endpoints are unauthenticated (`User-Agent: activity-daemon/<version>`,
//! `Accept: application/vnd.github+json`; GitHub's 60 req/h limit - a daily
//! check is comfortably inside it). All network goes through the
//! [`ReleaseFetcher`] / [`AssetDownloader`] traits so consumers unit-test
//! against fakes without any network.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use semver::{BuildMetadata, Version};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// GitHub API endpoint for the latest published release (drafts and
/// pre-releases are excluded by the API itself).
pub const RELEASE_API_URL: &str =
    "https://api.github.com/repos/Cubewise-JoeCHK/activity-daemon/releases/latest";

/// Connection timeout shared by the API client and the downloader.
const CONNECT_TIMEOUT_S: u64 = 10;
/// Whole-request timeout for the small `releases/latest` JSON call.
const API_REQUEST_TIMEOUT_S: u64 = 30;
/// Whole-request timeout for binary downloads (a few MB over slow links).
const DOWNLOAD_TIMEOUT_S: u64 = 600;

/// User-Agent for all GitHub requests (API + asset downloads).
pub fn user_agent() -> String {
    format!("activity-daemon/{}", env!("CARGO_PKG_VERSION"))
}

/// The running crate version as semver (every workspace crate shares the
/// workspace version, so this is the released version whatever crate calls).
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is valid semver")
}

// ---------------------------------------------------------------------------
// Release metadata: trait, GitHub implementation, parsing
// ---------------------------------------------------------------------------

/// One downloadable release asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// The latest published release, as the updater sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// Release tag, e.g. `v0.2.0`.
    pub tag: String,
    pub assets: Vec<Asset>,
}

/// Source of release metadata. The real implementation talks to the GitHub
/// API; tests inject fakes (all network is behind this trait).
pub trait ReleaseFetcher {
    fn fetch_latest(&self) -> Result<ReleaseInfo>;
}

/// Moves bytes from a URL into a local file; see [`ReleaseFetcher`].
pub trait AssetDownloader {
    /// Download `url` into `dest` (created/overwritten); returns the byte
    /// count. Errors on HTTP failure or an empty response body.
    fn download_to(&self, url: &str, dest: &Path) -> Result<u64>;
}

/// `releases/latest` JSON wire format (only the fields we need).
#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// Parse the `releases/latest` JSON body into a [`ReleaseInfo`].
pub fn parse_release_json(body: &str) -> Result<ReleaseInfo> {
    let raw: GitHubRelease = serde_json::from_str(body)
        .with_context(|| format!("malformed release JSON: {}", clip(body)))?;
    Ok(ReleaseInfo {
        tag: raw.tag_name,
        assets: raw
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
            })
            .collect(),
    })
}

/// Trim + clip a (possibly huge) body for log/error lines.
fn clip(s: &str) -> String {
    s.trim().chars().take(200).collect()
}

fn http_error(e: ureq::Error, what: &str) -> anyhow::Error {
    match e {
        // 403 = unauthenticated rate limit, 404 = no releases published yet.
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            if code == 404 {
                anyhow!("{what}: HTTP 404 (no releases published yet)")
            } else {
                anyhow!("{what}: HTTP {code}: {}", clip(&body))
            }
        }
        ureq::Error::Transport(t) => anyhow!("{what}: transport error: {t}"),
    }
}

/// Real [`ReleaseFetcher`]: the unauthenticated `releases/latest` call.
pub struct GitHubFetcher {
    agent: ureq::Agent,
}

impl GitHubFetcher {
    pub fn new() -> GitHubFetcher {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(CONNECT_TIMEOUT_S))
            .timeout(Duration::from_secs(API_REQUEST_TIMEOUT_S))
            .build();
        GitHubFetcher { agent }
    }
}

impl Default for GitHubFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ReleaseFetcher for GitHubFetcher {
    fn fetch_latest(&self) -> Result<ReleaseInfo> {
        let resp = self
            .agent
            .get(RELEASE_API_URL)
            .set("User-Agent", &user_agent())
            .set("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| http_error(e, "cannot fetch the latest release"))?;
        let body = resp
            .into_string()
            .context("cannot read the release API response body")?;
        parse_release_json(&body)
    }
}

/// Real [`AssetDownloader`]: streaming GET into the destination file.
pub struct HttpDownloader {
    agent: ureq::Agent,
}

impl HttpDownloader {
    pub fn new() -> HttpDownloader {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(CONNECT_TIMEOUT_S))
            .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_S))
            .build();
        HttpDownloader { agent }
    }
}

impl Default for HttpDownloader {
    fn default() -> Self {
        Self::new()
    }
}

impl AssetDownloader for HttpDownloader {
    fn download_to(&self, url: &str, dest: &Path) -> Result<u64> {
        let mut reader = self
            .agent
            .get(url)
            .set("User-Agent", &user_agent())
            .call()
            .map_err(|e| http_error(e, "cannot download the asset"))?
            .into_reader();
        let mut file = std::fs::File::create(dest)
            .with_context(|| format!("cannot create {}", dest.display()))?;
        let bytes = std::io::copy(&mut reader, &mut file)
            .with_context(|| format!("cannot write {}", dest.display()))?;
        if bytes == 0 {
            bail!(
                "downloaded {} but the response body is empty",
                dest.display()
            );
        }
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// Version compare
// ---------------------------------------------------------------------------

/// Outcome of comparing the latest release tag with the running version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDecision {
    /// Remote tag parses to `<=` the running version.
    UpToDate,
    /// Remote tag is not valid semver (with optional leading `v`): skipped.
    MalformedTag(String),
    /// Remote tag parses to a strictly higher version.
    Available(Version),
}

/// Strip an optional leading `v`/`V` and surrounding whitespace, then parse
/// the rest as semver.
pub fn parse_tag(tag: &str) -> Result<Version> {
    let t = tag.trim();
    let t = t.strip_prefix(['v', 'V']).unwrap_or(t);
    Version::parse(t).with_context(|| format!("malformed release tag {tag:?}"))
}

/// Spec precedence comparison: build metadata ignored. (`Version`'s own
/// `Ord` includes build metadata as a final tiebreaker to stay consistent
/// with its derived `Eq`, which would order `0.1.0+build.7 > 0.1.0` - not
/// what semver precedence says. Pre-release ordering itself is spec-correct
/// in the crate: a pre-release orders below its release.)
fn is_newer_than(current: &Version, remote: &Version) -> bool {
    fn precedence(v: &Version) -> Version {
        Version {
            major: v.major,
            minor: v.minor,
            patch: v.patch,
            pre: v.pre.clone(),
            build: BuildMetadata::EMPTY,
        }
    }
    precedence(remote) > precedence(current)
}

/// Compare the running version against the release tag (build metadata is
/// ignored and pre-releases order below their release, per semver).
pub fn decide_update(current: &Version, tag: &str) -> UpdateDecision {
    match parse_tag(tag) {
        Ok(remote) if is_newer_than(current, &remote) => UpdateDecision::Available(remote),
        Ok(_) => UpdateDecision::UpToDate,
        Err(_) => UpdateDecision::MalformedTag(tag.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Asset selection
// ---------------------------------------------------------------------------

/// A release asset and its `.sha256` companion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedAssets {
    pub asset: Asset,
    pub sha256: Asset,
}

/// Find the asset named `name` plus its `<name>.sha256` companion - the
/// exact pair the release workflow uploads for every binary.
pub fn select_asset_pair(assets: &[Asset], name: &str) -> Result<SelectedAssets> {
    let asset = assets
        .iter()
        .find(|a| a.name == name)
        .with_context(|| format!("no {name} asset in the release"))?
        .clone();
    let sha_name = format!("{name}.sha256");
    let sha256 = assets
        .iter()
        .find(|a| a.name == sha_name)
        .with_context(|| format!("no {sha_name} companion asset in the release"))?
        .clone();
    Ok(SelectedAssets { asset, sha256 })
}

// ---------------------------------------------------------------------------
// sha256 verification
// ---------------------------------------------------------------------------

/// Lowercase hex sha256 of a file (streamed).
pub fn file_sha256_hex(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("cannot open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .with_context(|| format!("cannot read {} for hashing", path.display()))?;
    Ok(hex(&hasher.finalize()))
}

/// Lowercase hex digest of raw bytes.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Verify a file against a `sha256sum`-style digest line (the digest is the
/// first whitespace token; hex compare is case-insensitive).
pub fn verify_sha256(path: &Path, digest_line: &str) -> Result<()> {
    let expected = digest_line
        .split_whitespace()
        .next()
        .context("the .sha256 file is empty")?
        .to_ascii_lowercase();
    let actual = file_sha256_hex(path)?;
    if actual != expected {
        bail!(
            "sha256 mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tag naming
// ---------------------------------------------------------------------------

/// Tag -> safe path stem (tags are `vX.Y.Z` in practice; anything outside
/// `[A-Za-z0-9._-]` is replaced with `_`).
pub fn sanitize_tag(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sample `releases/latest` body with the exact asset names the release
    /// workflow uploads: the Windows client pair AND the Linux server pair.
    const SAMPLE_RELEASE_JSON: &str = r#"{
        "tag_name": "v0.2.0",
        "assets": [
            {"name": "activity-daemon-windows-x86_64.exe",
             "browser_download_url": "https://example.test/dl/activity-daemon-windows-x86_64.exe"},
            {"name": "activity-daemon-windows-x86_64.exe.sha256",
             "browser_download_url": "https://example.test/dl/activity-daemon-windows-x86_64.exe.sha256"},
            {"name": "activity-server-linux-x86_64",
             "browser_download_url": "https://example.test/dl/activity-server-linux-x86_64"},
            {"name": "activity-server-linux-x86_64.sha256",
             "browser_download_url": "https://example.test/dl/activity-server-linux-x86_64.sha256"}
        ]
    }"#;

    #[test]
    fn parse_release_json_maps_tag_and_assets() {
        let info = parse_release_json(SAMPLE_RELEASE_JSON).expect("parse sample");
        assert_eq!(info.tag, "v0.2.0");
        assert_eq!(info.assets.len(), 4);
        assert_eq!(info.assets[0].name, "activity-daemon-windows-x86_64.exe");
        assert_eq!(
            info.assets[0].url,
            "https://example.test/dl/activity-daemon-windows-x86_64.exe"
        );
        assert_eq!(
            info.assets[1].url,
            "https://example.test/dl/activity-daemon-windows-x86_64.exe.sha256"
        );
        assert_eq!(info.assets[2].name, "activity-server-linux-x86_64");
        // Malformed JSON (wrong types) must be an error, not a panic.
        assert!(parse_release_json("{\"tag_name\": 12}").is_err());
        assert!(parse_release_json("not json at all").is_err());
    }

    #[test]
    fn select_asset_pair_finds_the_exact_pair() {
        let info = parse_release_json(SAMPLE_RELEASE_JSON).expect("parse sample");
        let win = select_asset_pair(&info.assets, "activity-daemon-windows-x86_64.exe")
            .expect("windows pair");
        assert_eq!(win.asset.name, "activity-daemon-windows-x86_64.exe");
        assert_eq!(win.sha256.name, "activity-daemon-windows-x86_64.exe.sha256");
        let srv =
            select_asset_pair(&info.assets, "activity-server-linux-x86_64").expect("server pair");
        assert_eq!(srv.asset.name, "activity-server-linux-x86_64");
        assert_eq!(srv.sha256.name, "activity-server-linux-x86_64.sha256");

        // Missing asset / missing sha256 companion -> errors.
        assert!(select_asset_pair(&info.assets, "no-such-asset").is_err());
        let exe_only = vec![Asset {
            name: "activity-server-linux-x86_64".to_string(),
            url: "u".to_string(),
        }];
        assert!(select_asset_pair(&exe_only, "activity-server-linux-x86_64").is_err());
        assert!(select_asset_pair(&[], "activity-server-linux-x86_64").is_err());
    }

    #[test]
    fn parse_tag_strips_v_and_whitespace() {
        assert_eq!(
            parse_tag("v1.2.3").expect("v prefix"),
            Version::parse("1.2.3").expect("plain")
        );
        assert_eq!(
            parse_tag("  V2.0.0 ").expect("V prefix + spaces"),
            Version::parse("2.0.0").expect("plain")
        );
        assert_eq!(
            parse_tag("0.2.0-alpha.1").expect("bare semver"),
            Version::parse("0.2.0-alpha.1").expect("plain")
        );
        assert!(parse_tag("v1.2").is_err(), "missing patch");
        assert!(parse_tag("latest").is_err());
    }

    #[test]
    fn decide_update_semver_and_prerelease_edges() {
        let cur = Version::parse("0.1.0").expect("current");
        // Strictly newer -> available.
        assert_eq!(
            decide_update(&cur, "v0.2.0"),
            UpdateDecision::Available(Version::parse("0.2.0").expect("remote"))
        );
        assert_eq!(
            decide_update(&cur, "0.2.0-alpha.1"),
            UpdateDecision::Available(Version::parse("0.2.0-alpha.1").expect("remote"))
        );
        // Equal or older -> up to date.
        assert_eq!(decide_update(&cur, "v0.1.0"), UpdateDecision::UpToDate);
        assert_eq!(decide_update(&cur, "0.0.9"), UpdateDecision::UpToDate);
        // Build metadata is ignored: 0.1.0+build == 0.1.0.
        assert_eq!(
            decide_update(&cur, "v0.1.0+build.7"),
            UpdateDecision::UpToDate
        );
        // Pre-release of the current version orders below it.
        assert_eq!(decide_update(&cur, "v0.1.0-rc.1"), UpdateDecision::UpToDate);
        // Malformed tags are reported, never guessed.
        assert_eq!(
            decide_update(&cur, "not-a-version"),
            UpdateDecision::MalformedTag("not-a-version".to_string())
        );
        assert_eq!(
            decide_update(&cur, "v1.2"),
            UpdateDecision::MalformedTag("v1.2".to_string())
        );
    }

    #[test]
    fn sha256_verify_ok_mismatch_and_case_insensitive() {
        let dir = std::env::temp_dir().join(format!(
            "activity-update-sha-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("blob.bin");
        std::fs::write(&file, b"new exe bytes").expect("write blob");
        let digest = file_sha256_hex(&file).expect("hash");
        assert_eq!(digest.len(), 64);
        // sha256sum-style line: digest is the first whitespace token.
        verify_sha256(&file, &format!("{digest}  blob.bin\n")).expect("digest line ok");
        // Case-insensitive hex compare.
        verify_sha256(&file, &digest.to_uppercase()).expect("uppercase ok");
        // Mismatch -> error.
        let err = verify_sha256(&file, "deadbeef").expect_err("mismatch must fail");
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");
        // Canonical empty-file digest (independent constant).
        let empty = dir.join("empty.bin");
        std::fs::write(&empty, b"").expect("write empty");
        assert_eq!(
            file_sha256_hex(&empty).expect("hash empty"),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // Empty digest line -> error.
        assert!(verify_sha256(&file, "   \n").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_tag_replaces_unsafe_characters() {
        assert_eq!(sanitize_tag("v0.2.0"), "v0.2.0");
        assert_eq!(sanitize_tag("v0.2.0-beta.1+build"), "v0.2.0-beta.1_build");
    }
}
