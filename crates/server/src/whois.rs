//! Tailscale device identity: resolve a peer IP to a device name through
//! the tailscaled local API socket (`/localapi/v0/whois`). Used only when
//! the socket is mounted (see crate docs, auth mode 1); the parsing helpers
//! are pure and unit-tested on every platform.

use std::net::IpAddr;

#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use crate::TAILSCALE_SOCKET;

/// Resolve the peer IP to a device name via tailscaled whois, or `None`
/// when the socket is unavailable / the request fails / no name is found.
#[cfg(unix)]
pub fn device_for_ip(ip: IpAddr) -> Option<String> {
    request_whois(Path::new(TAILSCALE_SOCKET), ip)
}

/// Non-unix platforms never see a tailscaled socket in this deployment.
#[cfg(not(unix))]
pub fn device_for_ip(_ip: IpAddr) -> Option<String> {
    None
}

/// Issue the whois request over the tailscaled HTTP-on-unix-socket API and
/// extract the device name. Kept defensive: any failure is `None`, and the
/// caller falls back to the request body's device.
#[cfg(unix)]
fn request_whois(socket: &Path, ip: IpAddr) -> Option<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let request = format!(
        "GET /localapi/v0/whois?ip={ip} HTTP/1.1\r\nHost: local-tailscaled.sock\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw);
    parse_whois_device(http_ok_body(&text)?)
}

/// Body of an HTTP/1.x response with status 200, else `None`.
pub fn http_ok_body(response: &str) -> Option<&str> {
    let (head, body) = response.split_once("\r\n\r\n")?;
    let status = head.lines().next()?;
    if status.contains(" 200 ") {
        Some(body)
    } else {
        None
    }
}

/// Device name from a tailscaled whois JSON body: `Node.HostName`, falling
/// back to `UserProfile.DisplayName`.
pub fn parse_whois_device(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value["Node"]["HostName"]
        .as_str()
        .or_else(|| value["UserProfile"]["DisplayName"].as_str())
        .map(str::to_string)
}
