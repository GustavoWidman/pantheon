//! Subscription Responses contract, pinned to native Codex 4ca84a1.
//! Public OpenAI/GLM never use this module. Source parity is not server acceptance.
use super::{Provider, Response};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) const LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Capabilities {
    /// None means unknown, not inferred from a model slug. Native omission defaults false.
    pub use_responses_lite: Option<bool>,
    pub supports_reasoning_summary_parameter: Option<bool>,
}
impl Capabilities {
    pub fn from_metadata(model: &Value) -> Self {
        Self {
            use_responses_lite: model["use_responses_lite"].as_bool(),
            supports_reasoning_summary_parameter: model["supports_reasoning_summary_parameter"]
                .as_bool(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Request {
    // Struct declaration order survives reqwest.json: incremental gateways see routing first.
    model: String,
    stream: bool,
    input: Vec<super::NativeValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    tool_choice: Value,
    parallel_tool_calls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Value>,
    store: bool,
    include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip)]
    lite: bool,
}
impl Request {
    /// Conservative standard projection for the public request_body helper.
    /// Actual subscription mode is resolved once at send-time from model metadata.
    pub fn standard(
        model: &str,
        effort: &str,
        system: &str,
        history: &[Value],
        tools: &[Value],
    ) -> Result<Self> {
        let mut input = Vec::new();
        if !system.is_empty() {
            input.push(json!({"type":"message","role":"developer",
                "id":prefix_id(Uuid::nil(), "msg", system.as_bytes()),
                "content":[{"type":"input_text","text":system}]}));
        }
        input.extend_from_slice(history);
        for item in &mut input {
            // Normalize only locally assembled shorthand messages on the wire.
            if item.get("type").is_none() && item.get("role").is_some() {
                item["type"] = json!("message");
            }
            if let Some(blocks) = item.get_mut("content").and_then(Value::as_array_mut) {
                for block in blocks {
                    if let Some(map) = block.as_object_mut() {
                        map.remove("prompt_cache_breakpoint");
                    }
                }
            }
        }
        let tools = tools.iter().map(|t| json!({"type":"function",
            "name":if t["name"] == "web_search" { json!("pantheon_web_search") } else { t["name"].clone() },
            "description":t["description"],"parameters":t["input_schema"],"strict":false})).collect();
        Ok(Self {
            model: model.into(),
            stream: true,
            input: input.into_iter().map(Into::into).collect(),
            tools: Some(tools),
            tool_choice: json!("auto"),
            parallel_tool_calls: true,
            reasoning: (effort != "none").then(|| json!({"effort":effort,"summary":"auto"})),
            store: false,
            include: vec!["reasoning.encrypted_content".into()],
            prompt_cache_key: None,
            lite: false,
        })
    }
    #[cfg(test)]
    pub fn prepare(body: &Value, capabilities: Capabilities, affinity: Uuid) -> Result<Self> {
        Self::prepare_native(&body.clone().into(), capabilities, affinity)
    }
    pub fn prepare_native(
        body: &super::NativeValue,
        capabilities: Capabilities,
        affinity: Uuid,
    ) -> Result<Self> {
        let mut request: Self = serde_json::from_value(body.projected())?;
        request.input = body
            .field("input")
            .map(|v| v.elements())
            .unwrap_or_default();
        // Hosted search is server-executed. Responses Lite supports only client
        // tools: moving web_search into additional_tools hides it from the
        // gateway, while leaving it top-level is explicitly rejected. Keep
        // this isolated request on standard Responses, with no Lite header.
        // Never retry a rejected request under a different mode.
        let hosted_search = request.tools.as_ref().is_some_and(|tools| {
            tools.iter().any(|tool| {
                matches!(
                    tool["type"].as_str(),
                    Some("web_search" | "web_search_preview")
                )
            })
        });
        request.lite = capabilities.use_responses_lite == Some(true) && !hosted_search;
        request.prompt_cache_key = Some(affinity.to_string());
        // Rebind only our generated base-instruction prefix, never saved native history IDs.
        if let Some(first) = request.input.first_mut()
            && first["type"] == "message"
            && first["role"] == "developer"
            && let Some(text) = first["content"][0]["text"].as_str()
            && first["id"] == prefix_id(Uuid::nil(), "msg", text.as_bytes())
        {
            first.set("id", json!(prefix_id(affinity, "msg", text.as_bytes())));
        }
        if let Some(reasoning) = request.reasoning.as_mut().and_then(Value::as_object_mut) {
            reasoning.remove("context");
            if request.lite {
                reasoning.insert("context".into(), json!("all_turns"));
            }
            // Native ModelInfo defaults summary support true when the field is omitted.
            if capabilities.supports_reasoning_summary_parameter == Some(false) {
                reasoning.remove("summary");
            }
        }
        if request.lite {
            request.parallel_tool_calls = false;
            let tools = lite_tools(request.tools.take().unwrap_or_default());
            if !tools.is_empty() {
                let id = prefix_id(affinity, "at", &serde_json::to_vec(&tools)?);
                request.input.insert(
                    0,
                    json!({"type":"additional_tools","id":id,"role":"developer","tools":tools})
                        .into(),
                );
            }
        }
        Ok(request)
    }
    pub fn apply_mode_header(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.lite {
            request.header(LITE_HEADER, "true")
        } else {
            request
        }
    }
    pub fn native(&self) -> Result<super::NativeValue> {
        super::NativeValue::parse(&serde_json::to_string(self)?)
    }
    pub fn value(&self) -> Result<Value> {
        Ok(self.native()?.projected())
    }
}
fn prefix_id(affinity: Uuid, prefix: &str, bytes: &[u8]) -> String {
    // Native uses UUIDv5(thread OID namespace, payload), with explicit msg_/at_ prefixes.
    let namespace = Uuid::new_v5(&Uuid::NAMESPACE_OID, affinity.to_string().as_bytes());
    format!("{prefix}_{}", Uuid::new_v5(&namespace, bytes))
}
fn lite_tools(tools: Vec<Value>) -> Vec<Value> {
    // Mirror native coalescing: built-ins stay outside functions and keep relative order.
    let mut functions = Vec::new();
    let mut index = None;
    let mut description = String::new();
    let mut converted = Vec::new();
    for tool in tools {
        if tool["type"] == "function" || tool["type"] == "custom" {
            index.get_or_insert(converted.len());
            functions.push(tool);
        } else if tool["type"] == "namespace" && tool["name"] == "functions" {
            index.get_or_insert(converted.len());
            if let Some(value) = tool["description"]
                .as_str()
                .filter(|d| !d.trim().is_empty())
            {
                description = value.into();
            }
            functions.extend(tool["tools"].as_array().into_iter().flatten().cloned());
        } else {
            converted.push(tool);
        }
    }
    if let Some(index) = index.filter(|_| !functions.is_empty()) {
        converted.insert(index, json!({"type":"namespace","name":"functions","description":description,"tools":functions}));
    }
    converted
}
pub(crate) fn parse(value: impl Into<super::NativeValue>) -> Result<Response> {
    let value = value.into().projected();
    // A separate namespace is part of tool identity, not a dotted name to strip blindly.
    for item in value["output"].as_array().into_iter().flatten() {
        // The shared parser accepts Anthropic tool_use too; Codex never advertises it.
        ensure!(
            item["type"] != "tool_use",
            "Codex returned an unsupported callable item type; no tools from this response executed"
        );
        if item["type"] == "function_call" {
            ensure!(
                item.get("namespace")
                    .is_none_or(|n| n.is_null() || n == "" || n == "functions"),
                "Codex returned an unsupported function namespace; no tools from this response executed"
            );
        }
    }
    let mut response = Provider::parse("openai", value)?;
    for call in &mut response.calls {
        if call.name == "pantheon_web_search" {
            call.name = "web_search".into();
        }
    }
    Ok(response)
}

#[cfg(test)]
#[path = "provider_codex_tests.rs"]
mod tests;
