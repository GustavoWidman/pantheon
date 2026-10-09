//! Retry classification uses typed failures, never provider error-message text.
use anyhow::{Result, ensure};
use reqwest::StatusCode;
use std::{fmt, time::Duration};

#[derive(Clone)]
pub(super) struct RequestRetries {
    pub max_attempts: usize,
    pub backoff: Duration,
}

impl Default for RequestRetries {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            backoff: Duration::from_secs(2),
        }
    }
}

pub(crate) fn validate(max_attempts: usize, backoff_seconds: u64) -> Result<()> {
    ensure!(
        (1..=10).contains(&max_attempts),
        "request_max_attempts must be between 1 and 10 (including the initial request)"
    );
    ensure!(
        (1..=60).contains(&backoff_seconds),
        "request_backoff_seconds must be between 1 and 60"
    );
    Ok(())
}

#[derive(Debug)]
pub(super) struct HttpFailure {
    pub status: StatusCode,
    pub provider: &'static str,
    pub retry_after: Option<Duration>,
}

impl fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} returned HTTP {}", self.provider, self.status)
    }
}
impl std::error::Error for HttpFailure {}

#[derive(Debug)]
pub(super) enum StreamFailure {
    PrematureEof,
    Transient,
}
impl fmt::Display for StreamFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PrematureEof => "provider stream ended without a completed response; no tools from this response executed",
            Self::Transient => "provider stream reported a transient failure; no tools from this response executed",
        })
    }
}
impl std::error::Error for StreamFailure {}

pub(super) fn transient_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Numeric seconds and IMF-fixdate. Invalid values use the linear delay; a valid
/// delay above sixty seconds is retained so the policy can fail without retrying.
pub(super) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(Duration::from_secs(
        (date.timestamp() - chrono::Utc::now().timestamp()).max(0) as u64,
    ))
}

impl RequestRetries {
    pub fn delay(&self, error: &anyhow::Error, attempt: usize) -> Option<Duration> {
        if attempt >= self.max_attempts {
            return None;
        }
        let linear = self.backoff.checked_mul(attempt.try_into().ok()?)?;
        if let Some(http) = error.downcast_ref::<HttpFailure>() {
            if !transient_status(http.status.as_u16())
                || http
                    .retry_after
                    .is_some_and(|delay| delay > Duration::from_secs(60))
            {
                return None;
            }
            return Some(linear.max(http.retry_after.unwrap_or_default()));
        }
        if error.downcast_ref::<StreamFailure>().is_some() {
            return Some(linear);
        }
        let transient = error.chain().any(|cause| {
            cause
                .downcast_ref::<reqwest::Error>()
                // JSON/SSE syntax is decoded separately with serde. At this
                // boundary reqwest decode errors describe broken HTTP bodies.
                .is_some_and(|e| e.is_connect() || e.is_timeout() || e.is_body() || e.is_decode())
                || cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::TimedOut
                    )
                })
        });
        transient.then_some(linear)
    }
}

/// Only recognized transient codes/statuses qualify. Arbitrary message strings,
/// incomplete output/token limits and malformed SSE are deliberately excluded.
pub(super) fn transient_event(event: &serde_json::Value) -> bool {
    if !matches!(event["type"].as_str(), Some("error" | "response.failed")) {
        return false;
    }
    let error = event
        .get("error")
        .or_else(|| event["response"].get("error"))
        .or_else(|| (event["type"] == "error").then_some(event));
    let Some(error) = error else { return false };
    error["status"]
        .as_u64()
        .is_some_and(|s| u16::try_from(s).is_ok_and(transient_status))
        || [
            "server_error",
            "internal_error",
            "internal_server_error",
            "overloaded_error",
            "rate_limit_error",
            "rate_limit_exceeded",
        ]
        .iter()
        .any(|code| error["code"] == *code || error["type"] == *code)
}
