//! Ordered completed Entries, separate from executable Uses and native wire.
use super::{CompletedEntry, ObservationKind as Kind, Response, entry_json::Json};
use crate::memory::summary_trim;
use serde_json::Value;
fn entry(kind: Kind, text: String, of: Option<super::CallId>) -> CompletedEntry {
    CompletedEntry { kind, text, of }
}
pub(super) fn display_units(value: Option<&Json>) -> Vec<u16> {
    match value {
        None => "undefined".encode_utf16().collect(),
        Some(Json::Null) => "null".encode_utf16().collect(),
        Some(Json::String(s)) => s.clone(),
        Some(Json::Object(_)) => "[object Object]".encode_utf16().collect(),
        Some(Json::Array(items)) => {
            let mut units = Vec::new();
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    units.push(44);
                }
                if !matches!(v, Json::Null) {
                    units.extend(display_units(Some(v)));
                }
            }
            units
        }
        Some(Json::Number(v)) if !v.is_finite() => (if v.is_nan() {
            "NaN"
        } else if *v > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        })
        .encode_utf16()
        .collect(),
        Some(v) => v.stringify().encode_utf16().collect(),
    }
}
pub(super) fn display_native(value: Option<&Json>) -> String {
    String::from_utf16_lossy(&display_units(value))
}
/// Anthropic.result, keeping JSON object order from the raw content block.
/// JS-only typed input/continuation remains F2; this is observation text only.
pub(super) fn server_result(content: Option<&Json>) -> String {
    let get = |key| content.and_then(|c| c.get(key));
    let text = if let Some(Json::Array(items)) = content {
        let mut units = Vec::new();
        for (i, v) in items.iter().enumerate() {
            if i > 0 {
                units.push(10);
            }
            units.extend(display_units(v.get("title")));
            units.push(32);
            units.extend(display_units(v.get("url")));
        }
        String::from_utf16_lossy(&units)
    } else if display_native(get("type")).starts_with("encrypted") {
        let mut units = "(output encrypted by the API)\n"
            .encode_utf16()
            .collect::<Vec<_>>();
        if let Some(stderr) = get("stderr").filter(|v| !matches!(v, Json::Null)) {
            units.extend(display_units(Some(stderr)));
        }
        summary_trim(&String::from_utf16_lossy(&units)).into()
    } else if let Some(Json::String(stdout)) = get("stdout") {
        let mut units = stdout.clone();
        if let Some(stderr) = get("stderr").filter(|v| !matches!(v, Json::Null)) {
            units.extend(display_units(Some(stderr)));
        }
        summary_trim(&String::from_utf16_lossy(&units)).into()
    } else {
        content.map(Json::stringify).unwrap_or_default()
    };
    if text.is_empty() {
        "(no output)".into()
    } else {
        text
    }
}
pub(super) fn read(
    vendor: &str,
    _value: &super::NativeValue,
    response: &Response,
) -> Vec<CompletedEntry> {
    let mut entries = Vec::new();
    let text = |units: &[u16]| summary_trim(&String::from_utf16_lossy(units)).to_owned();
    let mut reply = Vec::<u16>::new();
    let flush = |reply: &mut Vec<u16>, entries: &mut Vec<CompletedEntry>| {
        let value = text(reply);
        if !value.is_empty() {
            entries.push(entry(Kind::Reply, value, None));
        }
        reply.clear();
    };
    for item in &response.native {
        let kind = item["type"].as_str().unwrap_or("");
        if vendor == "anthropic" && kind == "text" {
            reply.extend_from_slice(item.units("text"));
            continue;
        }
        if vendor == "anthropic"
            && !(matches!(kind, "thinking" | "tool_use" | "server_tool_use")
                || kind.ends_with("_tool_result"))
        {
            continue;
        }
        flush(&mut reply, &mut entries);
        match kind {
            "message" if vendor == "openai" => {
                let content = item
                    .field("content")
                    .map(|v| v.elements())
                    .unwrap_or_default();
                let units = content
                    .iter()
                    .flat_map(|v| v.units("text").iter().copied())
                    .collect::<Vec<_>>();
                let value = text(&units);
                if !value.is_empty() {
                    entries.push(entry(Kind::Reply, value, None));
                }
                let mut seen = std::collections::HashSet::new();
                let sources = content
                    .iter()
                    .flat_map(|b| {
                        b.field("annotations")
                            .map(|v| v.elements())
                            .unwrap_or_default()
                    })
                    .filter(|a| a["type"] == "url_citation")
                    .map(|a| {
                        let mut units = display_units(a.source.get("title"));
                        units.push(32);
                        units.extend(display_units(a.source.get("url")));
                        units
                    })
                    .filter(|units| seen.insert(units.clone()))
                    .collect::<Vec<_>>();
                if !sources.is_empty() {
                    let mut units = "sources:\n".encode_utf16().collect::<Vec<_>>();
                    for (i, source) in sources.into_iter().enumerate() {
                        if i > 0 {
                            units.push(10);
                        }
                        units.extend(source);
                    }
                    entries.push(entry(Kind::Echo, String::from_utf16_lossy(&units), None));
                }
            }
            "reasoning" if vendor == "openai" => {
                let mut units = Vec::new();
                for (i, b) in item
                    .field("summary")
                    .map(|v| v.elements())
                    .unwrap_or_default()
                    .iter()
                    .enumerate()
                {
                    if i > 0 {
                        units.extend("\n\n".encode_utf16());
                    }
                    units.extend_from_slice(b.units("text"));
                }
                let value = text(&units);
                if !value.is_empty() {
                    entries.push(entry(Kind::Thought, value, None));
                }
            }
            "thinking" => {
                let value = text(item.units("thinking"));
                if !value.is_empty() {
                    entries.push(entry(Kind::Thought, value, None));
                }
            }
            "function_call" if vendor == "openai" => entries.push(entry(
                Kind::Tool,
                format!(
                    "{} {}",
                    display_native(item.source.get("name")),
                    String::from_utf16_lossy(item.units("arguments"))
                ),
                super::CallId::from_json(item.source.get("call_id")),
            )),
            "web_search_call" if vendor == "openai" => {
                if let Some(value) = openai_web(&item.source) {
                    entries.push(entry(Kind::Tool, value, None));
                }
            }
            "tool_use" | "server_tool_use" if vendor == "anthropic" => entries.push(entry(
                Kind::Tool,
                format!(
                    "{} {}",
                    display_native(item.source.get("name")),
                    item.source
                        .get("input")
                        .map(Json::stringify)
                        .unwrap_or_else(|| "undefined".into())
                ),
                super::CallId::from_json(item.source.get("id")),
            )),
            k if vendor == "anthropic" && k.ends_with("_tool_result") => entries.push(entry(
                Kind::Echo,
                server_result(item.source.get("content")),
                super::CallId::from_json(item.source.get("tool_use_id")),
            )),
            _ => {}
        }
    }
    flush(&mut reply, &mut entries);
    // Pinned Anthropic.entries trims every emitted Entry after run merging.
    // OpenAI.entries trims replies/thoughts when built, but its Tool arguments
    // (including empty/whitespace raw JSON strings) remain completely verbatim.
    if vendor == "anthropic" {
        for entry in &mut entries {
            entry.text = summary_trim(&entry.text).into();
        }
        entries.retain(|e| !e.text.is_empty());
    }
    entries
}

/// Raw web-search action rendering, observation only.
pub(super) fn openai_web(item: &Json) -> Option<String> {
    if item.get("type").and_then(Json::typed) != Some(Value::String("web_search_call".into())) {
        return None;
    }
    let action = item.get("action").filter(|v| !matches!(v, Json::Null));
    Some(format!(
        "web_search {}",
        action.map(Json::stringify).unwrap_or_else(|| "{}".into())
    ))
}

#[cfg(test)]
mod whitespace_regression_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn openai_raw_tool_arguments_are_not_entry_trimmed() {
        for raw in ["", " ", "\t\r\n", " \n{\"z\":1}\t "] {
            let value = super::super::NativeValue::from(
                json!({"status":"completed","output":[{"type":"function_call","call_id":"id","name":"tool","arguments":raw}]}),
            );
            let response =
                super::super::native_transport::ordinary_response("openai", &value).unwrap();
            let entries = read("openai", &value, &response);
            assert_eq!(entries.len(), 1);
            assert_eq!(
                entries[0].text,
                format!("tool {raw}"),
                "raw OpenAI arguments retain leading/trailing whitespace, even empty"
            );
        }
    }
    #[test]
    fn openai_source_whitespace_is_verbatim_while_anthropic_entries_trim() {
        let value = super::super::NativeValue::from(
            json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":" reply ","annotations":[{"type":"url_citation","title":" title ","url":" https://example.test/a \t"}]}]}]}),
        );
        let response = super::super::native_transport::ordinary_response("openai", &value).unwrap();
        let entries = read("openai", &value, &response);
        assert_eq!(entries[0].text, "reply");
        assert_eq!(
            entries[1].text,
            "sources:\n title   https://example.test/a \t"
        );
        let value = super::super::NativeValue::from(
            json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"id","name":"\u{feff}tool ","input":{}}]}),
        );
        let response =
            super::super::native_transport::ordinary_response("anthropic", &value).unwrap();
        let entries = read("anthropic", &value, &response);
        assert_eq!(entries[0].text, "tool  {}");
    }
}
