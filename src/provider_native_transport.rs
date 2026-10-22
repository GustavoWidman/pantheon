//! Pinned Agent.step/send lifecycle for native public OpenAI/Anthropic turns.
//! Not the configurable generic/search/subscription policy. No import rooms.
use super::{HttpFailure, StreamFailure};
use std::{fmt, time::Duration};

pub(super) const ATTEMPTS: usize = 8;

#[derive(Debug)]
pub(super) struct Http {
    pub status: reqwest::StatusCode,
    passing: bool,
    after: Option<Duration>,
}
impl Http {
    pub fn new(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> Self {
        let should = headers.get("x-should-retry").and_then(|s| s.to_str().ok());
        let passing = should == Some("true")
            || (should != Some("false")
                && (matches!(status.as_u16(), 408 | 409 | 429) || status.as_u16() >= 500));
        let after = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(number)
            .filter(|n| *n > 0.)
            .map(timer);
        Self {
            status,
            passing,
            after,
        }
    }
}
impl fmt::Display for Http {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native provider returned HTTP {}", self.status)
    }
}
impl std::error::Error for Http {}

/// Retry-After belongs to the HTTP response even when its stream fails later.
#[derive(Debug)]
pub(super) struct After(Option<Duration>);
impl After {
    pub fn new(headers: &reqwest::header::HeaderMap) -> Self {
        Self(
            headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(number)
                .filter(|n| *n > 0.)
                .map(timer),
        )
    }
}
impl fmt::Display for After {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("native provider response stream failed")
    }
}
impl std::error::Error for After {}

/// JS Number(string) for Retry-After, including radix forms. Dates are NaN.
/// Preserve Number's case-sensitive Infinity; signed hex is not a JS number.
fn number(s: &str) -> Option<f64> {
    let s = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if s.is_empty() {
        return Some(0.);
    }
    if s == "Infinity" || s == "+Infinity" {
        return Some(f64::INFINITY);
    }
    if s == "-Infinity" {
        return Some(f64::NEG_INFINITY);
    }
    for (prefixes, radix) in [(["0x", "0X"], 16), (["0b", "0B"], 2), (["0o", "0O"], 8)] {
        if let Some(digits) = prefixes.iter().find_map(|prefix| s.strip_prefix(prefix)) {
            if digits.is_empty() {
                return None;
            }
            return digits.chars().try_fold(0., |n, c| {
                c.to_digit(radix).map(|d| n * radix as f64 + d as f64)
            });
        }
    }
    // Rust accepts inf/NaN spellings which JS Number does not accept as positive.
    if s.chars()
        .any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E')
    {
        return None;
    }
    s.parse().ok()
}
/// Bun/Node timers normalize overflow, infinity and <1ms to 1ms, truncating
/// fractional milliseconds. Saturation to an infinite Rust Duration is wrong.
fn timer(seconds: f64) -> Duration {
    let ms = seconds * 1000.;
    if !(1. ..=2_147_483_647.).contains(&ms) || !ms.is_finite() {
        Duration::from_millis(1)
    } else {
        Duration::from_millis(ms as u64)
    }
}

#[derive(Debug)]
pub(super) struct LocalBoundary;
impl fmt::Display for LocalBoundary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("local native submission/admission failed; not a provider retry")
    }
}
impl std::error::Error for LocalBoundary {}

pub(super) fn refusal(value: &serde_json::Value) -> bool {
    value["output"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| {
            item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|b| b["type"] == "refusal")
        })
}
/// Native entry normalization and Agent.parse tool-argument behavior. Semantic
/// classification is separate so observations survive fail/again outcomes.
pub(super) fn ordinary_response(
    vendor: &str,
    value: &super::NativeValue,
) -> anyhow::Result<super::Response> {
    use anyhow::Context;
    let blocks = value
        .field(if vendor == "openai" {
            "output"
        } else {
            "content"
        })
        .context("missing provider output")?
        .elements();
    let mut response = super::Response {
        native: Vec::new(),
        texts: Vec::new(),
        calls: Vec::new(),
        usage: value["usage"].clone(),
    };
    for item in &blocks {
        if (vendor == "openai" && item["type"] == "function_call")
            || (vendor == "anthropic" && item["type"] == "tool_use")
        {
            let input = if vendor == "openai" {
                super::ToolInput::agent_parse_units(item.units("arguments"))
            } else {
                item.source
                    .get("input")
                    .cloned()
                    .map(super::ToolInput::native)
                    .unwrap_or_else(super::ToolInput::absent)
            };
            response.calls.push(super::ToolCall {
                id: super::CallId::from_json(item.source.get(if vendor == "openai" {
                    "call_id"
                } else {
                    "id"
                }))
                .context("missing call id")?,
                name: match item.source.get("name") {
                    Some(super::entry_json::Json::String(units)) => String::from_utf16_lossy(units),
                    _ => anyhow::bail!("missing tool name"),
                },
                arguments: input,
            });
        }
    }
    response.native = if vendor == "anthropic" {
        super::anthropic_stream::normalize_native(blocks)
    } else {
        blocks
    };
    response.texts = super::completed_entries::read(vendor, value, &response)
        .into_iter()
        .filter_map(|e| match e.kind {
            super::ObservationKind::Reply => Some((e.text, false)),
            super::ObservationKind::Thought => Some((e.text, true)),
            _ => None,
        })
        .collect();
    Ok(response)
}

pub(super) fn delay(error: &anyhow::Error, attempt: usize) -> Option<Duration> {
    if error.downcast_ref::<LocalBoundary>().is_some() {
        return None;
    }
    if attempt >= ATTEMPTS {
        return None;
    }
    if let Some(http) = error.downcast_ref::<Http>() {
        return http.passing.then(|| {
            http.after
                .unwrap_or_else(|| Duration::from_secs(1 << attempt))
        });
    }
    // A completed semantic failure is answered, not a transport outage. The
    // compactor handles source fallback outside this inner retry loop.
    if matches!(
        error.downcast_ref::<StreamFailure>(),
        Some(StreamFailure::Terminal(_))
    ) {
        return None;
    }
    // HttpFailure belongs to non-native callers; never silently reinterpret it.
    if error.downcast_ref::<HttpFailure>().is_some() {
        return None;
    }
    // Agent.send/stream classify thrown network/JSON/UTF8/EOF/idle failures busy.
    Some(
        error
            .downcast_ref::<After>()
            .and_then(|after| after.0)
            .unwrap_or_else(|| Duration::from_secs(1 << attempt)),
    )
}

pub(super) fn idle(model: &str) -> Duration {
    Duration::from_secs(if model.starts_with("anthropic/") {
        300
    } else {
        1200
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_header_policy_matches_pinned_agent_passing_and_number() {
        for status in [400, 401, 408, 409, 429, 500, 501, 599] {
            for value in [None, Some("true"), Some("false"), Some("TRUE")] {
                let mut headers = reqwest::header::HeaderMap::new();
                if let Some(v) = value {
                    headers.insert("x-should-retry", v.parse().unwrap());
                }
                let err: anyhow::Error =
                    Http::new(reqwest::StatusCode::from_u16(status).unwrap(), &headers).into();
                assert_eq!(
                    delay(&err, 1).is_some(),
                    value == Some("true")
                        || (value != Some("false")
                            && (matches!(status, 408 | 409 | 429) || status >= 500))
                );
                assert!(delay(&err, 8).is_none());
            }
        }
        for (value, ms) in [
            ("0", 2000),
            ("-1", 2000),
            ("0.25", 250),
            ("NaN", 2000),
            ("Wed, 21 Oct 2030 07:28:00 GMT", 2000),
            ("0x10", 16000),
            ("1e2", 100000),
            ("Infinity", 1),
            ("2147484", 1),
            ("0.0001", 1),
            ("inf", 2000),
            ("+0x10", 2000),
        ] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("retry-after", value.parse().unwrap());
            let err = Http::new(reqwest::StatusCode::TOO_MANY_REQUESTS, &headers).into();
            assert_eq!(delay(&err, 1), Some(Duration::from_millis(ms)), "{value}");
        }
        let error = anyhow::anyhow!("synthetic broken JSON");
        assert_eq!(
            (1..8)
                .map(|n| delay(&error, n).unwrap().as_secs())
                .collect::<Vec<_>>(),
            vec![2, 4, 8, 16, 32, 64, 128]
        );
        assert!(delay(&error, 8).is_none());
    }
}
