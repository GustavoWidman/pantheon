//! Z.ai's explicitly selected Coding Chat Completions transport. Native reasoning
//! stays only in the active ephemeral transcript, never in display/history text.
use super::{Response, ToolCall, retry};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

pub const ENDPOINT: &str = "https://api.z.ai/api/coding/paas/v4/chat/completions";
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 131072;
pub const MODELS: [&str; 2] = ["glm-5.3", "glm-5.3-flash"];
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    /// None means no harness concurrency cap; finite caps are shared by clones.
    #[serde(
        deserialize_with = "deserialize_max_in_flight",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_in_flight: Option<usize>,
    pub max_output_tokens: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            max_in_flight: None,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }
}
fn deserialize_max_in_flight<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Limit {
        Count(usize),
        Name(String),
    }
    match Option::<Limit>::deserialize(deserializer)? {
        None | Some(Limit::Count(0)) => Ok(None),
        Some(Limit::Count(count)) => Ok(Some(count)),
        Some(Limit::Name(name)) if name == "none" => Ok(None),
        Some(Limit::Name(_)) => Err(serde::de::Error::custom(
            "zai.max_in_flight string must be \"none\"; otherwise use 0 or an integer 1–32",
        )),
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.max_in_flight
                .is_none_or(|limit| (1..=32).contains(&limit)),
            "zai.max_in_flight must be omitted, \"none\", 0, or 1–32"
        );
        ensure!(
            (1..=131072).contains(&self.max_output_tokens),
            "zai.max_output_tokens must be 1–131072"
        );
        Ok(())
    }
}
pub fn validate_model(id: &str) -> Result<()> {
    ensure!(
        MODELS.contains(&id),
        "Supported exact Z.ai Coding IDs: glm-5.3, glm-5.3-flash (legacy aliases are not pinned models)"
    );
    Ok(())
}
pub fn validate_effort(effort: &str) -> Result<()> {
    ensure!(
        ["low", "high", "max"].contains(&effort),
        "Z.ai reasoning must be low, high or max; thinking cannot be disabled"
    );
    Ok(())
}
pub fn supports_images(model: &str) -> bool {
    model == "zai/glm-5.3-flash"
}
pub fn catalog() -> Vec<crate::models::Model> {
    MODELS.into_iter().map(|id| crate::models::Model {
        id: id.into(), name: id.into(), efforts: vec!["low".into(), "high".into(), "max".into()],
        default_effort: Some("max".into()), adaptive_thinking: false, codex: None,
        source: "Z.ai current model documentation: https://docs.z.ai/devpack/latest-model (static allowlist, not account access)".into(),
        observed_at: None,
    }).collect()
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}
fn identifier(value: &Value) -> Result<&str> {
    value
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
        .context("missing/invalid Z.ai tool call id")
}
fn validate_content(value: &Value, images: bool) -> Result<()> {
    if value.is_string() || value.is_null() {
        return Ok(());
    }
    let blocks = value.as_array().context("Z.ai content must be text or supported content blocks; cannot replay another provider's transcript")?;
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                ensure!(block["text"].is_string(), "Z.ai text block is missing text");
            }
            Some("image_url") => {
                ensure!(
                    images,
                    "This Z.ai model is text-only: visual content is not supported; select an explicitly vision-capable model, no automatic fallback"
                );
                let url = block["image_url"]["url"]
                    .as_str()
                    .context("invalid Z.ai image block")?;
                ensure!(
                    url.starts_with("data:image/png;base64,")
                        || url.starts_with("data:image/jpeg;base64,")
                        || url.starts_with("data:image/webp;base64,")
                        || url.starts_with("data:image/gif;base64,"),
                    "Z.ai images must use local supported data URLs; no implicit remote fetching"
                );
            }
            _ => bail!(
                "Unsupported Z.ai content block; native Responses/Anthropic/file/media items cannot be replayed here"
            ),
        }
    }
    Ok(())
}
fn validate_calls(calls: &Value) -> Result<Vec<ToolCall>> {
    let calls = calls.as_array().context("invalid Z.ai tool_calls")?;
    ensure!(
        !calls.is_empty() && calls.len() <= 128,
        "invalid Z.ai tool count"
    );
    let mut ids = HashSet::new();
    calls
        .iter()
        .map(|call| {
            ensure!(call["type"] == "function", "unsupported Z.ai tool type");
            let id = identifier(&call["id"])?;
            ensure!(ids.insert(id), "duplicate Z.ai tool call id");
            let name = call["function"]["name"]
                .as_str()
                .filter(|n| valid_name(n))
                .context("missing/invalid Z.ai tool name")?;
            let arguments: Value = serde_json::from_str(
                call["function"]["arguments"]
                    .as_str()
                    .context("missing Z.ai tool arguments")?,
            )
            .context("Z.ai tool JSON incomplete; no tools from this response executed")?;
            ensure!(
                arguments.is_object(),
                "Z.ai tool arguments must be a JSON object; no tools from this response executed"
            );
            Ok(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            })
        })
        .collect()
}
fn validate_history(history: &[Value], images: bool) -> Result<()> {
    let mut pending = HashSet::new();
    let mut seen = HashSet::new();
    for item in history {
        let role = item["role"].as_str().context(
            "Z.ai requires Chat messages; another provider's native transcript cannot be replayed",
        )?;
        ensure!(
            item.get("type").is_none(),
            "Z.ai cannot replay native Responses items"
        );
        if role == "tool" {
            let id = identifier(&item["tool_call_id"])?;
            ensure!(
                pending.remove(id),
                "Z.ai tool result has no matching pending call"
            );
            ensure!(
                item["content"].is_string(),
                "Z.ai tool result content must be a string"
            );
            continue;
        }
        ensure!(
            pending.is_empty(),
            "Z.ai tool calls require every result before the next message"
        );
        ensure!(
            ["user", "assistant"].contains(&role),
            "Unexpected role in Z.ai transcript"
        );
        validate_content(&item["content"], images)?;
        if let Some(reasoning) = item.get("reasoning_content") {
            ensure!(
                role == "assistant" && reasoning.is_string(),
                "invalid Z.ai native reasoning content"
            );
        }
        if let Some(calls) = item.get("tool_calls") {
            ensure!(
                role == "assistant",
                "Z.ai tool calls must belong to assistant"
            );
            for call in validate_calls(calls)? {
                ensure!(seen.insert(call.id.clone()), "reused Z.ai tool call id");
                pending.insert(call.id);
            }
        }
    }
    ensure!(
        pending.is_empty(),
        "Z.ai transcript is missing tool results"
    );
    Ok(())
}
pub(super) fn request_body(
    id: &str,
    effort: &str,
    system: &str,
    history: &[Value],
    tools: &[Value],
) -> Result<Value> {
    validate_model(id)?;
    validate_effort(effort)?;
    validate_history(history, id == "glm-5.3-flash")?;
    ensure!(
        tools.len() <= 128,
        "Z.ai supports at most 128 function definitions"
    );
    let mut names = HashSet::new();
    let tools = tools.iter().map(|tool| {
        let name = tool["name"].as_str().filter(|n| valid_name(n)).context("invalid Z.ai function definition name")?;
        ensure!(names.insert(name), "duplicate Z.ai function definition");
        ensure!(tool["input_schema"].is_object(), "invalid Z.ai function schema");
        Ok(json!({"type":"function","function":{"name":name,"description":tool["description"],"parameters":tool["input_schema"]}}))
    }).collect::<Result<Vec<_>>>()?;
    let mut messages = vec![json!({"role":"system","content":system})];
    messages.extend_from_slice(history);
    let mut body = json!({"model":id,"messages":messages,"stream":true,"tool_stream":true,"stream_options":{"include_usage":true},"thinking":{"type":"enabled","clear_thinking":false},"reasoning_effort":effort,"max_tokens":DEFAULT_MAX_OUTPUT_TOKENS});
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
    }
    Ok(body)
}
pub(super) fn parse(value: Value) -> Result<Response> {
    if value.get("error").is_some_and(|e| !e.is_null()) {
        bail!("Z.ai returned a provider error; no tools from this response executed");
    }
    let choices = value["choices"]
        .as_array()
        .context("missing Z.ai choices")?;
    ensure!(
        choices.len() == 1,
        "Z.ai requires one complete choice; no tools from this response executed"
    );
    let choice = &choices[0];
    ensure!(
        choice["index"].as_u64() == Some(0),
        "invalid Z.ai choice index"
    );
    let finish = choice["finish_reason"]
        .as_str()
        .context("Z.ai response missing terminal reason; no tools from this response executed")?;
    ensure!(
        ["stop", "tool_calls"].contains(&finish),
        "Z.ai response incomplete/refused; no tools from this response executed"
    );
    let message = &choice["message"];
    ensure!(
        message["role"] == "assistant",
        "Z.ai response has invalid role"
    );
    let text = match &message["content"] {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        _ => bail!("invalid Z.ai assistant text"),
    };
    if let Some(reasoning) = message.get("reasoning_content") {
        ensure!(reasoning.is_string(), "invalid Z.ai reasoning content");
    }
    ensure!(
        message.get("refusal").is_none_or(|v| v.is_null()),
        "Z.ai refused response; no tools from this response executed"
    );
    let calls = if let Some(calls) = message
        .get("tool_calls")
        .filter(|c| !c.is_null() && c.as_array().is_none_or(|a| !a.is_empty()))
    {
        validate_calls(calls)?
    } else {
        vec![]
    };
    ensure!(
        (finish == "tool_calls") == !calls.is_empty(),
        "Z.ai finish reason/tool batch mismatch; no tools from this response executed"
    );
    // Deliberately exclude reasoning_content even when show_reasoning is enabled.
    // Keep the native field verbatim, and only legitimate assistant wire fields.
    let mut native = json!({"role":"assistant","content":message["content"]});
    if let Some(reasoning) = message.get("reasoning_content") {
        native["reasoning_content"] = reasoning.clone();
    }
    if !calls.is_empty() {
        native["tool_calls"] = message["tool_calls"].clone();
    }
    let mut usage = value["usage"].clone();
    if usage.is_object() {
        usage["input_tokens"] = usage["prompt_tokens"].clone();
        usage["output_tokens"] = usage["completion_tokens"].clone();
        if let Some(cached) = usage["prompt_tokens_details"].get("cached_tokens").cloned() {
            usage["input_tokens_details"] = json!({"cached_tokens":cached});
        }
    }
    Ok(Response {
        native: vec![native],
        texts: text
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(|s| (s, false))
            .collect(),
        calls,
        usage,
    })
}
#[derive(Default)]
pub(super) struct Decoder {
    pending: Vec<u8>,
    data: Vec<String>,
    pub events: usize,
    capture_errors: bool,
    last_error_event: Option<&'static str>,
    text: String,
    reasoning: String,
    has_reasoning: bool,
    calls: BTreeMap<u64, Value>,
    finish: Option<String>,
    usage: Value,
}
impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Option<Value>> {
        self.pending.extend_from_slice(bytes);
        ensure!(
            self.pending.len() <= 16 * 1024 * 1024,
            "oversized Z.ai SSE event"
        );
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line = self.pending.drain(..=end).collect::<Vec<_>>();
            let line = std::str::from_utf8(&line)
                .context("invalid Z.ai stream UTF-8")?
                .trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if self.data.is_empty() {
                    continue;
                }
                let payload = std::mem::take(&mut self.data).join("\n");
                self.events += 1;
                if payload == "[DONE]" {
                    return Ok(Some(self.complete()?));
                }
                let event: Value =
                    serde_json::from_str(&payload).context("invalid Z.ai SSE JSON")?;
                self.event(event)?;
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).into());
                ensure!(
                    self.data.iter().map(String::len).sum::<usize>() <= 16 * 1024 * 1024,
                    "oversized Z.ai SSE payload"
                );
            }
        }
        Ok(None)
    }
    fn event(&mut self, event: Value) -> Result<()> {
        if event.get("error").is_some_and(|e| !e.is_null()) || event["type"] == "error" {
            let details = retry::event_details_captured(&event, self.capture_errors);
            self.last_error_event = Some(details.event);
            return Err(if details.retryable {
                retry::StreamFailure::Transient(details)
            } else {
                retry::StreamFailure::Terminal(details)
            }
            .into());
        }
        if let Some(usage) = event.get("usage").filter(|v| !v.is_null()) {
            ensure!(usage.is_object(), "invalid Z.ai usage");
            self.usage = usage.clone();
        }
        let choices = event["choices"]
            .as_array()
            .context("missing Z.ai stream choices")?;
        if choices.is_empty() {
            ensure!(
                event["usage"].is_object(),
                "empty Z.ai stream event has no usage"
            );
            return Ok(());
        }
        ensure!(
            choices.len() == 1 && choices[0]["index"].as_u64() == Some(0),
            "invalid Z.ai streaming choice"
        );
        ensure!(
            self.finish.is_none(),
            "Z.ai received choice after terminal reason"
        );
        let delta = &choices[0]["delta"];
        ensure!(delta.is_object(), "invalid Z.ai stream delta");
        if let Some(role) = delta.get("role").filter(|v| !v.is_null()) {
            ensure!(role == "assistant", "invalid Z.ai stream role");
        }
        for (field, output) in [
            ("content", &mut self.text),
            ("reasoning_content", &mut self.reasoning),
        ] {
            if let Some(value) = delta.get(field).filter(|v| !v.is_null()) {
                output.push_str(value.as_str().context("invalid Z.ai text delta")?);
                if field == "reasoning_content" {
                    self.has_reasoning = true;
                }
            }
        }
        ensure!(
            delta.get("refusal").is_none_or(|v| v.is_null()),
            "Z.ai refused response; no tools from this response executed"
        );
        if let Some(calls) = delta.get("tool_calls").filter(|v| !v.is_null()) {
            for fragment in calls.as_array().context("invalid Z.ai tool delta")? {
                let index = fragment["index"]
                    .as_u64()
                    .filter(|i| *i < 128)
                    .context("invalid Z.ai tool index")?;
                let call = self.calls.entry(index).or_insert_with(
                    || json!({"type":"function","function":{"name":"","arguments":""}}),
                );
                if let Some(kind) = fragment.get("type").filter(|v| !v.is_null()) {
                    ensure!(kind == "function", "unsupported Z.ai tool delta type");
                }
                if let Some(id) = fragment.get("id").filter(|v| !v.is_null()) {
                    identifier(id)?;
                    ensure!(
                        call.get("id").is_none_or(|old| old == id),
                        "Z.ai tool id changed during stream"
                    );
                    call["id"] = id.clone();
                }
                if let Some(function) = fragment.get("function").filter(|v| !v.is_null()) {
                    ensure!(function.is_object(), "invalid Z.ai function delta");
                    for field in ["name", "arguments"] {
                        if let Some(value) = function.get(field).filter(|v| !v.is_null()) {
                            let mut text = call["function"][field].as_str().unwrap().to_owned();
                            text.push_str(
                                value.as_str().context("invalid Z.ai function fragment")?,
                            );
                            call["function"][field] = json!(text);
                        }
                    }
                }
            }
        }
        if let Some(finish) = choices[0].get("finish_reason").filter(|v| !v.is_null()) {
            self.finish = Some(
                finish
                    .as_str()
                    .context("invalid Z.ai terminal reason")?
                    .into(),
            );
        }
        Ok(())
    }
    fn complete(&mut self) -> Result<Value> {
        let finish = self.finish.as_ref().context(
            "Z.ai stream ended without terminal reason; no tools from this response executed",
        )?;
        ensure!(
            self.calls.keys().copied().eq(0..self.calls.len() as u64),
            "Z.ai stream has missing tool indices; no tools from this response executed"
        );
        let mut message = json!({"role":"assistant","content":self.text});
        if self.has_reasoning {
            message["reasoning_content"] = json!(self.reasoning);
        }
        if !self.calls.is_empty() {
            message["tool_calls"] = json!(self.calls.values().collect::<Vec<_>>());
        }
        let value = json!({"choices":[{"index":0,"finish_reason":finish,"message":message}],"usage":self.usage});
        // Full parse before returning; no partially decoded call is ever dispatched.
        parse(value.clone())?;
        Ok(value)
    }
}
pub(super) async fn read_response(
    response: reqwest::Response,
    diagnostic: &mut crate::transport::Exchange,
) -> Result<Value> {
    use futures_util::StreamExt;
    let sse = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("text/event-stream"));
    diagnostic.streaming(sse);
    let mut stream = response.bytes_stream();
    let mut decoder = Decoder {
        capture_errors: diagnostic.capture_enabled(),
        ..Default::default()
    };
    let mut bytes = Vec::new();
    let mut received = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(reqwest::Error::without_url)
            .context("Z.ai response stream interrupted; no tools from this response executed")?;
        diagnostic.chunk(chunk.len());
        received = received.saturating_add(chunk.len());
        diagnostic.stage = "size_limit";
        ensure!(
            received <= 32 * 1024 * 1024,
            "Z.ai response exceeded size limit; no tools from this response executed"
        );
        if sse {
            let result = decoder.feed(&chunk);
            diagnostic.decoded(&result);
            diagnostic.events(
                decoder.events,
                decoder.last_error_event.or(Some("chat.completion.chunk")),
                0,
            );
            if let Some(value) = result? {
                return Ok(value);
            }
        } else {
            bytes.extend_from_slice(&chunk);
        }
        diagnostic.stage = "body";
    }
    if sse {
        diagnostic.stage = "premature_eof";
        return Err(retry::StreamFailure::PrematureEof.into());
    }
    diagnostic.stage = "json_decode";
    let value: Value = serde_json::from_slice(&bytes).context("invalid Z.ai provider JSON")?;
    parse(value.clone())?;
    Ok(value)
}

#[cfg(test)]
#[path = "provider_zai_tests.rs"]
mod tests;

/// MCP's current harness path serializes media as JSON, not native perception.
/// Keep original data and top-level isError, without assuming every result is an object.
pub(crate) fn annotate_mcp(value: &mut Value) {
    let notice = json!(
        "MCP results are textual JSON, not native media input. Encoded image/audio/resource data or previews are NOT visual/audio evidence. Originals remain in the MCP response/result store; use an explicit analysis tool or vision-capable read input. No hidden vision-provider call was made."
    );
    if let Some(map) = value.as_object_mut() {
        map.insert("_pantheon_media_notice".into(), notice);
    } else {
        *value = json!({"result":std::mem::take(value),"_pantheon_media_notice":notice});
    }
}
