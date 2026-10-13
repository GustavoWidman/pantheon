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
pub(crate) enum StreamFailure {
    PrematureEof,
    Transient(StreamEventDetails),
    Terminal(StreamEventDetails),
}

/// Never preserve provider free-text or arbitrary identifiers: providers may echo
/// prompt text or credentials in even ostensibly structured error fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamEventDetails {
    pub event: &'static str,
    pub code: Option<&'static str>,
    pub code_state: &'static str,
    pub error_type: Option<&'static str>,
    pub type_state: &'static str,
    pub error_source: &'static str,
    pub status: Option<u16>,
    pub incomplete_reason: Option<&'static str>,
    pub kind: &'static str,
    pub retryable: bool,
}

/// Null placeholders are not error objects. Preserve the precedence of actual
/// objects (including empty ones); never combine fields from conflicting errors.
fn event_error(event: &serde_json::Value) -> (Option<&serde_json::Value>, &'static str) {
    if let Some(error) = event.get("error").filter(|e| e.is_object()) {
        (Some(error), "error")
    } else if let Some(error) = event["response"].get("error").filter(|e| e.is_object()) {
        (Some(error), "response.error")
    } else if event["type"] == "error" {
        (Some(event), "event")
    } else {
        (None, "none")
    }
}

fn code_state(value: Option<&serde_json::Value>) -> &'static str {
    match value {
        None => "missing",
        Some(serde_json::Value::Null) => "null",
        Some(value) if safe_code(value).is_some() => "recognized",
        Some(serde_json::Value::String(_)) => "unrecognized",
        Some(_) => "invalid_type",
    }
}

fn safe_code(value: &serde_json::Value) -> Option<&'static str> {
    match value.as_str()? {
        "server_error" => Some("server_error"),
        "internal_error" => Some("internal_error"),
        "internal_server_error" => Some("internal_server_error"),
        "overloaded_error" => Some("overloaded_error"),
        "rate_limit_error" => Some("rate_limit_error"),
        "rate_limit_exceeded" => Some("rate_limit_exceeded"),
        "invalid_request_error" => Some("invalid_request_error"),
        "context_length_exceeded" => Some("context_length_exceeded"),
        "input_tokens_exceeded" => Some("input_tokens_exceeded"),
        "authentication_error" => Some("authentication_error"),
        "permission_error" => Some("permission_error"),
        "insufficient_quota" => Some("insufficient_quota"),
        "model_not_found" => Some("model_not_found"),
        "unsupported_parameter" => Some("unsupported_parameter"),
        _ => None,
    }
}

pub(crate) fn event_details(event: &serde_json::Value) -> StreamEventDetails {
    let name = match event["type"].as_str() {
        Some("error") => "error",
        Some("response.failed") => "response.failed",
        Some("response.incomplete") => "response.incomplete",
        _ => "unknown",
    };
    // Diagnostics and retry decisions must inspect the same error object.
    let (error, error_source) = event_error(event);
    let code = error.and_then(|e| safe_code(&e["code"]));
    // A flat event's `type: error` identifies the event, not the error category.
    let type_value = error
        .filter(|_| error_source != "event")
        .and_then(|e| e.get("type"));
    let error_type = type_value.and_then(safe_code);
    let status = error
        .and_then(|e| e["status"].as_u64())
        .and_then(|s| u16::try_from(s).ok())
        .filter(|s| (100..=599).contains(s));
    let incomplete_reason = match event["response"]["incomplete_details"]["reason"].as_str() {
        Some("max_output_tokens") => Some("max_output_tokens"),
        Some("content_filter") => Some("content_filter"),
        _ => None,
    };
    let retryable = transient_event(event);
    let kind = if name == "response.incomplete" {
        "incomplete"
    } else if [code, error_type]
        .into_iter()
        .flatten()
        .any(|value| matches!(value, "context_length_exceeded" | "input_tokens_exceeded"))
    {
        "context"
    } else if code == Some("rate_limit_error")
        || code == Some("rate_limit_exceeded")
        || error_type == Some("rate_limit_error")
        || error_type == Some("rate_limit_exceeded")
        || status == Some(429)
    {
        "rate_limit"
    } else if status.is_some_and(transient_status)
        || [code, error_type].into_iter().flatten().any(|value| {
            matches!(
                value,
                "server_error" | "internal_error" | "internal_server_error" | "overloaded_error"
            )
        })
    {
        "upstream"
    } else if matches!(status, Some(401 | 403))
        || [code, error_type].into_iter().flatten().any(|value| {
            matches!(
                value,
                "authentication_error" | "permission_error" | "insufficient_quota"
            )
        })
    {
        "auth"
    } else if matches!(status, Some(400 | 404 | 422))
        || [code, error_type].into_iter().flatten().any(|value| {
            matches!(
                value,
                "invalid_request_error" | "model_not_found" | "unsupported_parameter"
            )
        })
    {
        "validation"
    } else {
        "unknown"
    };
    StreamEventDetails {
        event: name,
        code,
        code_state: code_state(error.and_then(|e| e.get("code"))),
        error_type,
        type_state: if error_source == "event" {
            "not_applicable"
        } else {
            code_state(type_value)
        },
        error_source,
        status,
        incomplete_reason,
        kind,
        retryable,
    }
}
impl fmt::Display for StreamFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let details = match self {
            Self::PrematureEof => return f.write_str("provider stream ended without a completed response; no tools from this response executed"),
            Self::Transient(details) => {
                f.write_str("provider stream reported a transient failure")?;
                details
            }
            Self::Terminal(details) => {
                f.write_str("provider stream failed or was incomplete")?;
                details
            }
        };
        write!(f, " ({}; category={}", details.event, details.kind)?;
        if let Some(code) = details.code.or(details.error_type) {
            write!(f, "; code={code}")?;
        } else {
            write!(f, "; code={}", details.code_state)?;
        }
        if let Some(status) = details.status {
            write!(f, "; status={status}")?;
        }
        if let Some(reason) = details.incomplete_reason {
            write!(f, "; reason={reason}")?;
        }
        f.write_str("); no tools from this response executed")
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
        if error
            .downcast_ref::<StreamFailure>()
            .is_some_and(|e| matches!(e, StreamFailure::PrematureEof | StreamFailure::Transient(_)))
        {
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
    let (error, _) = event_error(event);
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
