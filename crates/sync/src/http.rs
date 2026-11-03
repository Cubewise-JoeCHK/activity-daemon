//! HTTP client abstraction for the forwarder plus the real `ureq`
//! implementation. The trait exists so the forwarder logic can be unit
//! tested against a fake client on every platform.

use std::fmt;
use std::io::Read;
use std::time::Duration;

/// Connection timeout for the real client.
pub const CONNECT_TIMEOUT_S: u64 = 10;
/// Whole-request timeout for the real client (media uploads can be large).
pub const REQUEST_TIMEOUT_S: u64 = 300;

/// How a failed HTTP interaction should be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// Transport-level failure or server-side (5xx) failure: back off
    /// exponentially and retry.
    Backoff,
    /// The server understood us and refused (4xx): retrying unchanged will
    /// not help - wait a normal interval and log it.
    SkipCycle,
}

/// A failed HTTP interaction, classified by the presence of a status code.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpError {
    /// HTTP status if the server responded, `None` for transport failures.
    pub status: Option<u16>,
    pub message: String,
}

impl HttpError {
    pub fn transport(message: impl Into<String>) -> HttpError {
        HttpError {
            status: None,
            message: message.into(),
        }
    }

    pub fn http(status: u16, message: impl Into<String>) -> HttpError {
        HttpError {
            status: Some(status),
            message: message.into(),
        }
    }

    /// Classify this failure for the retry loop.
    pub fn class(&self) -> RetryClass {
        match self.status {
            None => RetryClass::Backoff,
            Some(code) if code >= 500 => RetryClass::Backoff,
            _ => RetryClass::SkipCycle,
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(code) => write!(f, "HTTP {}: {}", code, self.message),
            None => write!(f, "transport error: {}", self.message),
        }
    }
}

impl std::error::Error for HttpError {}

/// The forwarder's view of the hub API (two endpoints).
pub trait HttpClient: Send + Sync {
    /// `POST {url}` with a JSON body; returns the response body on 2xx.
    fn post_events(&self, url: &str, body: &serde_json::Value) -> Result<String, HttpError>;

    /// `PUT {url}` with the given reader as body and an `X-SHA256` header;
    /// returns the response body on 2xx.
    fn put_media(
        &self,
        url: &str,
        sha256_hex: &str,
        reader: Box<dyn Read + '_>,
    ) -> Result<String, HttpError>;
}

/// Blocking HTTP client for the forwarder loop (no async runtime needed).
pub struct UreqClient {
    agent: ureq::Agent,
}

impl UreqClient {
    pub fn new() -> UreqClient {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(CONNECT_TIMEOUT_S))
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_S))
            .build();
        UreqClient { agent }
    }
}

impl Default for UreqClient {
    fn default() -> Self {
        UreqClient::new()
    }
}

/// Error bodies can be large; keep log lines sane.
fn clip(s: &str) -> String {
    s.trim().chars().take(400).collect()
}

fn body_text(resp: ureq::Response) -> String {
    resp.into_string()
        .map(|s| clip(&s))
        .unwrap_or_else(|e| format!("<unreadable body: {e}>"))
}

fn ok_body(resp: ureq::Response) -> Result<String, HttpError> {
    resp.into_string()
        .map_err(|e| HttpError::transport(format!("failed to read response body: {e}")))
}

impl HttpClient for UreqClient {
    fn post_events(&self, url: &str, body: &serde_json::Value) -> Result<String, HttpError> {
        match self.agent.post(url).send_json(body) {
            Ok(resp) => ok_body(resp),
            Err(ureq::Error::Status(code, resp)) => Err(HttpError::http(code, body_text(resp))),
            Err(ureq::Error::Transport(t)) => Err(HttpError::transport(t.to_string())),
        }
    }

    fn put_media(
        &self,
        url: &str,
        sha256_hex: &str,
        reader: Box<dyn Read + '_>,
    ) -> Result<String, HttpError> {
        let req = self
            .agent
            .put(url)
            .set("Content-Type", "application/octet-stream")
            .set("X-SHA256", sha256_hex);
        match req.send(reader) {
            Ok(resp) => ok_body(resp),
            Err(ureq::Error::Status(code, resp)) => Err(HttpError::http(code, body_text(resp))),
            Err(ureq::Error::Transport(t)) => Err(HttpError::transport(t.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_of_statuses() {
        assert_eq!(HttpError::transport("refused").class(), RetryClass::Backoff);
        assert_eq!(
            HttpError::http(503, "unavailable").class(),
            RetryClass::Backoff
        );
        assert_eq!(
            HttpError::http(400, "bad batch").class(),
            RetryClass::SkipCycle
        );
        assert_eq!(
            HttpError::http(401, "bad token").class(),
            RetryClass::SkipCycle
        );
    }

    #[test]
    fn display_includes_status_or_transport() {
        assert!(HttpError::http(404, "nope")
            .to_string()
            .contains("HTTP 404"));
        assert!(HttpError::transport("dns")
            .to_string()
            .contains("transport"));
    }
}
