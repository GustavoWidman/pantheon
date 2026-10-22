//! One owned executable input. JS semantics are authoritative; the read-only
//! projection exists only for existing Rust host adapters, not RPC serialization.
use super::entry_json::Json;
use anyhow::{Result, bail};
use serde::{Serialize, Serializer};
use serde_json::Value;

#[derive(Clone)]
pub struct ToolInput {
    source: Json,
    projection: Value,
    absent: bool,
}
impl std::fmt::Debug for ToolInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolInput")
            .field("absent", &self.absent)
            .field("shape", &self.source)
            .finish_non_exhaustive()
    }
}
impl ToolInput {
    pub(crate) fn native(source: Json) -> Self {
        let projection = project(&source);
        Self {
            source,
            projection,
            absent: false,
        }
    }
    pub fn is_object(&self) -> bool {
        !self.absent && matches!(self.source, Json::Object(_))
    }
    pub fn is_null(&self) -> bool {
        !self.absent && matches!(self.source, Json::Null)
    }
    #[cfg(test)]
    pub(crate) fn kind(&self) -> &'static str {
        if self.absent {
            return "absent";
        }
        match self.source {
            Json::Null => "null",
            Json::Bool(_) => "boolean",
            Json::Number(_) => "number",
            Json::String(_) => "string",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }
    pub(crate) fn absent() -> Self {
        let mut value = Self::native(Json::Null);
        value.absent = true;
        value
    }
    #[cfg(test)]
    pub(crate) fn agent_parse(raw: &str) -> Self {
        Self::native(Json::parse(if raw.is_empty() { "{}" } else { raw }).unwrap_or(Json::Null))
    }
    pub(crate) fn agent_parse_units(raw: &[u16]) -> Self {
        Self::native(if raw.is_empty() {
            Json::parse("{}").unwrap()
        } else {
            Json::parse_utf16(raw).unwrap_or(Json::Null)
        })
    }
    pub(crate) fn canonical(&self) -> String {
        if self.absent {
            "undefined".into()
        } else {
            self.source.stringify()
        }
    }
    pub(crate) fn member_canonical(&self, key: &str) -> Option<String> {
        self.source.get(key).map(Json::stringify)
    }
    pub(crate) fn validate_prompt_arguments(&self) -> Result<()> {
        if self
            .source
            .get("arguments")
            .is_some_and(|v| !scalar_strings(v))
        {
            bail!(
                "MCP prompt argument scalar cannot be represented by the typed get_prompt adapter; tools/call uses the lossless RPC route"
            );
        }
        Ok(())
    }
    /// Unknown fields are inert. Only fields actually consumed by a builtin
    /// schema need Rust scalar strings; do not reject a whole valid object for
    /// an unrelated JS-only member. Remote MCP arguments bypass this adapter.
    pub(crate) fn validate_builtin(&self, schema: &Value, remote: bool) -> Result<()> {
        if let Some(properties) = schema["properties"].as_object() {
            for key in properties.keys() {
                if remote && key == "arguments" {
                    if self.projection["action"] == "get_prompt" {
                        self.validate_prompt_arguments()?;
                    }
                    continue;
                }
                if let Some(value) = self.source.get(key)
                    && !host_field(value, &properties[key])
                {
                    bail!("builtin argument {key} cannot be represented by this Rust host adapter");
                }
            }
        }
        Ok(())
    }
}
fn scalar_strings(value: &Json) -> bool {
    match value {
        Json::String(units) => String::from_utf16(units).is_ok(),
        Json::Array(values) => values.iter().all(scalar_strings),
        Json::Object(fields) => fields
            .iter()
            .all(|(key, value)| String::from_utf16(key).is_ok() && scalar_strings(value)),
        _ => true,
    }
}
fn host_field(value: &Json, schema: &Value) -> bool {
    match (value, schema["type"].as_str()) {
        (Json::String(units), Some("string")) => String::from_utf16(units).is_ok(),
        (Json::Array(values), Some("array")) => {
            values.iter().all(|v| host_field(v, &schema["items"]))
        }
        (Json::Object(fields), Some("object")) => fields.iter().all(|(key, value)| {
            let Ok(key) = String::from_utf16(key) else {
                return schema["additionalProperties"] == false;
            };
            if let Some(field) = schema["properties"].get(&key) {
                host_field(value, field)
            } else if schema["additionalProperties"].is_object() {
                host_field(value, &schema["additionalProperties"])
            } else {
                true
            }
        }),
        // Rust numeric validation still rejects nonfinite/null at consumed field;
        // unknown members are not a reason to reject an otherwise valid object.
        _ => true,
    }
}
/// A structural projection is not an encoding: nonfinite JS numbers become null
/// exactly as on the JSON wire; unrepresentable strings become null only in this
/// non-authoritative host view. validate_builtin rejects consumed such fields.
fn project(value: &Json) -> Value {
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
impl From<Value> for ToolInput {
    fn from(value: Value) -> Self {
        Self::native(Json::parse(&value.to_string()).expect("typed JSON input"))
    }
}
impl std::ops::Deref for ToolInput {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.projection
    }
}
impl std::fmt::Display for ToolInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.canonical())
    }
}
// Existing diagnostics/durable records never gain an opaque AST payload.
impl Serialize for ToolInput {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.projection.serialize(serializer)
    }
}
impl PartialEq<Value> for ToolInput {
    fn eq(&self, other: &Value) -> bool {
        self.projection == *other
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_input_preserves_js_wire_without_rejecting_unknown_members() {
        let input = ToolInput::agent_parse(
            r#"{"path":"ok","unused":"\ud800","large":1e400,"n":9007199254740993}"#,
        );
        assert!(input.is_object());
        assert_eq!(
            input.canonical(),
            r#"{"path":"ok","unused":"\ud800","large":null,"n":9007199254740992}"#
        );
        let schema = serde_json::json!({"properties":{"path":{"type":"string"}}});
        input.validate_builtin(&schema, false).unwrap();
        let bad = ToolInput::agent_parse(r#"{"path":"\ud800"}"#);
        assert!(bad.validate_builtin(&schema, false).is_err());
    }
}
