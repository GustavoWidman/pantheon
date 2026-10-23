//! Invocation-owned JSON. `source` is authoritative; `view` is a disposable,
//! read-only Rust host projection. No ambient registries or parallel AST arrays.
//! This type is ephemeral and must never be placed in journal/cache records.
use super::entry_json::Json;
use serde::{Serialize, Serializer};
use serde_json::Value;

#[derive(Clone)]
pub struct NativeValue {
    pub(crate) source: Json,
    view: Value,
}
impl std::fmt::Debug for NativeValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeValue")
            .field("shape", &self.source)
            .finish_non_exhaustive()
    }
}
impl NativeValue {
    pub(crate) fn text_units(units: Vec<u16>) -> Self {
        Self::new(Json::String(units))
    }
    pub(crate) fn new(source: Json) -> Self {
        let view = project(&source);
        Self { source, view }
    }
    pub(crate) fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(Self::new(Json::parse(text)?))
    }
    pub(crate) fn array(values: impl IntoIterator<Item = Self>) -> Self {
        Self::new(Json::Array(values.into_iter().map(|v| v.source).collect()))
    }
    pub(crate) fn field(&self, key: &str) -> Option<Self> {
        self.source.get(key).cloned().map(Self::new)
    }
    pub(crate) fn elements(&self) -> Vec<Self> {
        match &self.source {
            Json::Array(v) => v.iter().cloned().map(Self::new).collect(),
            _ => vec![],
        }
    }
    pub(crate) fn set(&mut self, key: &str, value: impl Into<Self>) {
        self.source.set(key, value.into().source);
        self.view = project(&self.source);
    }
    pub(crate) fn remove(&mut self, key: &str) {
        self.source.remove(key);
        self.view = project(&self.source);
    }
    /// Mutate only host-created values (e.g. a User media block). Replacing an
    /// opaque model block with its host projection is intentionally impossible.
    pub(crate) fn edit_host(&mut self, edit: impl FnOnce(&mut Value)) {
        assert!(
            self.source.typed().is_some(),
            "cannot edit opaque native value through Rust projection"
        );
        edit(&mut self.view);
        self.source = Json::parse(&self.view.to_string()).expect("host JSON");
    }
    pub(crate) fn host_compatible(&self) -> bool {
        self.source.scalar_compatible()
    }
    pub(crate) fn units(&self, key: &str) -> &[u16] {
        match self.source.get(key) {
            Some(Json::String(s)) => s,
            _ => &[],
        }
    }
    pub(crate) fn canonical(&self) -> String {
        self.source.stringify()
    }
    pub(crate) fn projected(&self) -> Value {
        self.view.clone()
    }
}
pub(crate) fn project(value: &Json) -> Value {
    match value {
        Json::Array(values) => Value::Array(values.iter().map(project).collect()),
        Json::Object(fields) => Value::Object(
            fields
                .iter()
                .filter_map(|(k, v)| Some((String::from_utf16(k).ok()?, project(v))))
                .collect(),
        ),
        _ => serde_json::from_str(&value.stringify()).unwrap_or(Value::Null),
    }
}
impl<'de> serde::Deserialize<'de> for NativeValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::from)
    }
}
impl From<Value> for NativeValue {
    fn from(value: Value) -> Self {
        Self::new(Json::parse(&value.to_string()).expect("typed JSON"))
    }
}
impl From<Json> for NativeValue {
    fn from(value: Json) -> Self {
        Self::new(value)
    }
}
impl std::ops::Deref for NativeValue {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.view
    }
}
impl Serialize for NativeValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // RawValue is a structural serde seam, not placeholder replacement.
        serde_json::value::RawValue::from_string(self.canonical())
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
impl std::fmt::Display for NativeValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.canonical())
    }
}
impl PartialEq<Value> for NativeValue {
    fn eq(&self, other: &Value) -> bool {
        self.view == *other
    }
}
impl PartialEq for NativeValue {
    fn eq(&self, other: &Self) -> bool {
        self.canonical() == other.canonical()
    }
}
/// Only for size/vision budgeting; never transport, continuation or cache keys.
pub(crate) fn projected(items: &[NativeValue]) -> Vec<Value> {
    items.iter().map(NativeValue::projected).collect()
}

/// Profiles operate on host-created controls; replace history structurally from
/// its owned source, preserving every native field, order and UTF16 code unit.
pub(super) fn prepare_request(
    model: &str,
    body: &Value,
    history: &[NativeValue],
    compactor: bool,
) -> anyhow::Result<NativeValue> {
    use serde_json::json;
    let mut out = NativeValue::from(body.clone());
    if model.starts_with("codex/") {
        let mut input = Vec::new();
        if let Some(first) = body["input"]
            .as_array()
            .and_then(|v| v.first())
            .filter(|v| v["role"] == "developer")
        {
            input.push(first.clone().into());
        }
        for source in history {
            let mut item = source.clone();
            if item.get("type").is_none() && item.get("role").is_some() {
                item.set("type", json!("message"));
            }
            if let Some(content) = item.field("content").filter(|c| c.is_array()) {
                item.set(
                    "content",
                    NativeValue::array(content.elements().into_iter().map(|mut b| {
                        b.remove("prompt_cache_breakpoint");
                        b
                    })),
                );
            }
            input.push(item);
        }
        out.set("input", NativeValue::array(input));
        return Ok(out);
    }
    if !(model.starts_with("anthropic/") || model.starts_with("openai/")) {
        return Ok(out);
    }
    let openai = model.starts_with("openai/");
    let native_profile = body["stream"] == true
        && body[if openai {
            "max_output_tokens"
        } else {
            "max_tokens"
        }] == 128000;
    let mut messages = Vec::<NativeValue>::new();
    if openai && let Some(system) = body["input"].as_array().and_then(|v| v.first()) {
        messages.push(system.clone().into());
    }
    for source in history {
        let mut item = source.clone();
        if openai {
            let id = model.strip_prefix("openai/").unwrap_or("");
            if !(id.starts_with("gpt-5.6") || id.starts_with("gpt-6"))
                && let Some(content) = item.field("content").filter(|v| v.is_array())
            {
                let blocks = content.elements().into_iter().map(|mut b| {
                    b.remove("prompt_cache_breakpoint");
                    b
                });
                item.set("content", NativeValue::array(blocks));
            }
            if native_profile && !compactor && item["type"] == "function_call_output" {
                if let Some(text) = item["output"].as_str() {
                    item.set("output", json!([{"type":"input_text","text":text}]));
                }
                if let Some(parts) = item.field("output").filter(|v| v.is_array()) {
                    item.set(
                        "output",
                        NativeValue::array(parts.elements().into_iter().map(|mut p| {
                            if p["type"] == "input_image" {
                                p.remove("detail");
                            }
                            p
                        })),
                    );
                }
            }
        } else if native_profile {
            if let Some(content) = item.field("content").filter(|v| v.is_array()) {
                let blocks = content.elements().into_iter().map(|mut b| {
                    if !compactor && b["type"] == "tool_result" {
                        if let Some(text) = b["content"].as_str() {
                            b.set(
                                "content",
                                if text.is_empty() {
                                    json!([])
                                } else {
                                    json!([{"type":"text","text":text}])
                                },
                            );
                        }
                        if let Some(parts) = b.field("content").filter(|v| v.is_array()) {
                            b.set(
                                "content",
                                NativeValue::array(
                                    parts
                                        .elements()
                                        .into_iter()
                                        .filter(|p| !(p["type"] == "text" && p["text"] == "")),
                                ),
                            );
                        }
                    }
                    b
                });
                item.set("content", NativeValue::array(blocks));
            }
            if let Some(last) = messages.last_mut()
                && last["role"] == item["role"]
            {
                let mut blocks = last
                    .field("content")
                    .map(|v| v.elements())
                    .unwrap_or_default();
                blocks.extend(
                    item.field("content")
                        .map(|v| v.elements())
                        .unwrap_or_default(),
                );
                last.set("content", NativeValue::array(blocks));
                continue;
            }
        }
        messages.push(item);
    }
    out.set(
        if openai { "input" } else { "messages" },
        NativeValue::array(messages),
    );
    Ok(out)
}

impl PartialEq<NativeValue> for Value {
    fn eq(&self, other: &NativeValue) -> bool {
        *self == other.view
    }
}
