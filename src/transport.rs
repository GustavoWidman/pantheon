//! Failure diagnostics contain transport metadata, never request or response bodies.
use anyhow::Result;
use serde_json::{Value, json};
use std::time::Instant;

pub(crate) struct Exchange {
    id: String,
    model: String,
    attempt: usize,
    timeout_seconds: Option<u64>,
    started: Instant,
    headers_ms: Option<u64>,
    status: Option<u16>,
    http_version: Option<String>,
    request_id: Option<String>,
    cf_ray: Option<String>,
    content_type: &'static str,
    content_length: Option<u64>,
    pub stage: &'static str,
    streaming: bool,
    bytes: usize,
    chunks: usize,
    first_byte_ms: Option<u64>,
    last_byte: Option<Instant>,
    events: usize,
    last_event: Option<&'static str>,
    completed_items: usize,
}

fn milliseconds(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

impl Exchange {
    pub fn new(model: &str, attempt: usize, timeout_seconds: Option<u64>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            model: if model.len() <= 200
                && model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._:-".contains(&b))
            {
                model.to_owned()
            } else {
                "[invalid model ID]".into()
            },
            attempt,
            timeout_seconds,
            started: Instant::now(),
            headers_ms: None,
            status: None,
            http_version: None,
            request_id: None,
            cf_ray: None,
            content_type: "unknown",
            content_length: None,
            stage: "headers",
            streaming: false,
            bytes: 0,
            chunks: 0,
            first_byte_ms: None,
            last_byte: None,
            events: 0,
            last_event: None,
            completed_items: 0,
        }
    }

    pub fn headers(&mut self, response: &reqwest::Response) {
        self.headers_ms = Some(milliseconds(self.started.elapsed()));
        self.status = Some(response.status().as_u16());
        self.http_version = Some(format!("{:?}", response.version()));
        // Only bounded opaque identifiers are allowed; no arbitrary headers or URLs.
        let identifier = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .filter(|v| {
                    !v.is_empty()
                        && v.len() <= 256
                        && v.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
                })
                .map(str::to_owned)
        };
        self.request_id = identifier("x-request-id").or_else(|| identifier("request-id"));
        self.cf_ray = identifier("cf-ray");
        self.content_type = match response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
        {
            Some("text/event-stream") => "text/event-stream",
            Some("application/json") => "application/json",
            Some(_) => "other",
            None => "unknown",
        };
        self.content_length = response.content_length();
        self.stage = "http_status";
    }

    pub fn streaming(&mut self, streaming: bool) {
        self.streaming = streaming;
        self.stage = "body";
    }

    pub fn chunk(&mut self, bytes: usize) {
        self.first_byte_ms
            .get_or_insert_with(|| milliseconds(self.started.elapsed()));
        self.last_byte = Some(Instant::now());
        self.bytes += bytes;
        self.chunks += 1;
    }

    pub fn events(&mut self, count: usize, last: Option<&'static str>, completed: usize) {
        self.events = count;
        self.last_event = last;
        self.completed_items = completed;
    }

    fn report(&self, error: &anyhow::Error) -> Value {
        let transport = error
            .chain()
            .find_map(|e| e.downcast_ref::<reqwest::Error>());
        let io = error
            .chain()
            .find_map(|e| e.downcast_ref::<std::io::Error>());
        // Reqwest URLs have been removed at both send and body-read boundaries.
        // Decode/protocol errors may contain payload fragments, so never render those chains.
        let causes = transport.map(|e| {
            let mut chain: Vec<&(dyn std::error::Error + 'static)> = vec![e];
            while chain.len() < 12 {
                match chain.last().and_then(|e| e.source()) {
                    Some(source) => chain.push(source),
                    None => break,
                }
            }
            chain
                .into_iter()
                .map(|e| safe_cause(&e.to_string()))
                .collect::<Vec<_>>()
        });
        json!({
            "id":self.id,"model":self.model,"attempt":self.attempt,
            "stage":self.stage,"elapsed_ms":milliseconds(self.started.elapsed()),
            "timeout_seconds":self.timeout_seconds,"headers_ms":self.headers_ms,
            "status":self.status,"http_version":self.http_version,
            "provider_request_id":self.request_id,"cf_ray":self.cf_ray,
            "content_type":self.content_type,"content_length":self.content_length,
            "streaming":self.streaming,"bytes_received":self.bytes,"chunks_received":self.chunks,
            "first_byte_ms":self.first_byte_ms,
            "since_last_byte_ms":self.last_byte.map(|t|milliseconds(t.elapsed())),
            "sse_events":self.events,"last_sse_event":self.last_event,
            "completed_output_items":self.completed_items,
            "is_timeout":transport.map(reqwest::Error::is_timeout),
            "is_connect":transport.map(reqwest::Error::is_connect),
            "is_body":transport.map(reqwest::Error::is_body),
            "is_decode":transport.map(reqwest::Error::is_decode),
            "is_request":transport.map(reqwest::Error::is_request),
            "io_kind":io.map(|e|format!("{:?}",e.kind())),
            "os_error":io.and_then(std::io::Error::raw_os_error),
            "causes":causes,
        })
    }

    pub fn finish<T>(&self, result: Result<T>) -> Result<T> {
        result.map_err(|error| {
            tracing::warn!(diagnostic_id=%self.id, diagnostic=%self.report(&error), "provider request failed");
            let message = format!("{error} [diagnostic {}]", self.id);
            error.context(message)
        })
    }
}

fn safe_cause(text: &str) -> String {
    use std::sync::OnceLock;
    static URL: OnceLock<regex::Regex> = OnceLock::new();
    let regex =
        URL.get_or_init(|| regex::Regex::new(r#"(?i)(?:https?|wss?)://[^\s\)\]\"']+"#).unwrap());
    regex
        .replace_all(text, "[redacted URL]")
        .chars()
        .filter(|c| !c.is_control())
        .take(1024)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_causes_are_bounded_and_urls_and_controls_are_removed() {
        let text = safe_cause(
            "failed at https://user:private-password@example.test/path?token=private-token\n\tconnection reset",
        );
        assert_eq!(text, "failed at [redacted URL]connection reset");
        assert_eq!(safe_cause(&"a".repeat(2000)).len(), 1024);
    }
    #[test]
    fn non_transport_errors_do_not_render_potential_body_fragments() {
        let trace = Exchange::new("openai/test", 1, Some(300));
        let report = trace.report(&anyhow::anyhow!("invalid value private-provider-payload"));
        assert!(report["causes"].is_null());
        assert_eq!(report["timeout_seconds"], 300);
        assert!(!report.to_string().contains("private-provider-payload"));
    }
}
