//! One attempt owns every block. No execution or call-ID-keyed sidecar is kept.
//! Initial input is replaced by terminal delta parse before Use capture; native
//! output normalization happens later and never changes executable inputs.
use super::{NativeValue, entry_json::Json};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) fn normalize_native(blocks: impl IntoIterator<Item = NativeValue>) -> Vec<NativeValue> {
    blocks
        .into_iter()
        .filter_map(|mut block| {
            if block["type"] == "text" && block.units("text").is_empty() {
                return None;
            }
            if matches!(block.source.get("input"), Some(Json::Null)) {
                block.set("input", json!({}));
            }
            Some(block)
        })
        .collect()
}
#[derive(Default)]
pub(super) struct Decoder {
    message: Option<NativeValue>,
    blocks: BTreeMap<u64, NativeValue>,
    inputs: BTreeMap<u64, Vec<u16>>,
}
impl Decoder {
    pub(super) fn read_raw(
        &mut self,
        projected: &Value,
        raw: Option<&str>,
    ) -> Result<Option<NativeValue>> {
        let event = raw
            .map(NativeValue::parse)
            .transpose()?
            .unwrap_or_else(|| projected.clone().into());
        match event["type"].as_str() {
            Some("message_start") => {
                ensure!(self.message.is_none(), "duplicate Messages stream start");
                let mut message = event
                    .field("message")
                    .context("missing Messages stream start")?;
                ensure!(message.is_object(), "missing Messages stream start");
                message.set("content", json!([]));
                self.message = Some(message);
            }
            Some("content_block_start") => {
                ensure!(self.message.is_some(), "Messages block before start");
                let index = event["index"]
                    .as_u64()
                    .context("missing Messages block index")?;
                let block = event
                    .field("content_block")
                    .context("missing Messages content block")?;
                ensure!(block.is_object(), "missing Messages content block");
                ensure!(
                    self.blocks.insert(index, block).is_none(),
                    "duplicate Messages block index"
                );
            }
            Some("content_block_delta") => {
                let index = event["index"]
                    .as_u64()
                    .context("missing Messages delta index")?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .context("Messages delta before block")?;
                let delta = event.field("delta").context("missing Messages delta")?;
                match delta["type"].as_str() {
                    Some("text_delta" | "thinking_delta" | "signature_delta") => {
                        let field = match delta["type"].as_str().unwrap() {
                            "text_delta" => "text",
                            "thinking_delta" => "thinking",
                            _ => "signature",
                        };
                        let Some(Json::String(suffix)) = delta.source.get(field) else {
                            anyhow::bail!("missing Messages delta text")
                        };
                        let mut units = if field == "signature" {
                            vec![]
                        } else {
                            match block.source.get(field) {
                                Some(Json::String(s)) => s.clone(),
                                _ => vec![],
                            }
                        };
                        units.extend_from_slice(suffix);
                        block.set(field, NativeValue::new(Json::String(units)));
                    }
                    Some("input_json_delta") => {
                        let Some(Json::String(suffix)) = delta.source.get("partial_json") else {
                            anyhow::bail!("missing Messages input delta")
                        };
                        self.inputs
                            .entry(index)
                            .or_default()
                            .extend_from_slice(suffix);
                    }
                    Some("citations_delta") => {
                        let mut citations = block
                            .field("citations")
                            .map(|v| v.elements())
                            .unwrap_or_default();
                        citations.push(
                            delta
                                .field("citation")
                                .unwrap_or_else(|| Value::Null.into()),
                        );
                        block.set("citations", NativeValue::array(citations));
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = event["index"]
                    .as_u64()
                    .context("missing Messages stop index")?;
                ensure!(
                    self.blocks.contains_key(&index),
                    "Messages stop before block"
                );
            }
            Some("message_delta") => {
                let message = self
                    .message
                    .as_mut()
                    .context("Messages delta before start")?;
                if let Some(Json::Object(fields)) = event.source.get("delta") {
                    for (key, value) in fields {
                        message.set(
                            &String::from_utf16(key).context("Messages delta key")?,
                            NativeValue::new(value.clone()),
                        );
                    }
                }
                if let Some(Json::Object(fields)) = event.source.get("usage") {
                    let mut usage = message
                        .field("usage")
                        .filter(|v| v.is_object())
                        .unwrap_or_else(|| json!({}).into());
                    for (key, value) in fields {
                        usage.set(
                            &String::from_utf16(key).context("Messages usage key")?,
                            NativeValue::new(value.clone()),
                        );
                    }
                    message.set("usage", usage);
                }
            }
            Some("message_stop") => {
                let mut message = self.message.take().context("Messages stop before start")?;
                for (index, units) in std::mem::take(&mut self.inputs) {
                    let block = self
                        .blocks
                        .get_mut(&index)
                        .context("Messages input without block")?;
                    // JSON.parse consumes UTF16, including raw unpaired units
                    // inside quoted values/keys. Do not reject the whole object.
                    let parsed = if units.is_empty() {
                        Json::parse("{}").unwrap()
                    } else {
                        Json::parse_utf16(&units).unwrap_or(Json::Null)
                    };
                    block.set("input", NativeValue::new(parsed));
                }
                ensure!(
                    self.blocks.keys().copied().eq(0..self.blocks.len() as u64),
                    "Messages content index gap"
                );
                ensure!(
                    message["stop_reason"].is_string(),
                    "missing Messages terminal reason"
                );
                message.set(
                    "content",
                    NativeValue::array(std::mem::take(&mut self.blocks).into_values()),
                );
                return Ok(Some(message));
            }
            _ => {}
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn messages_stream_matches_actual_pinned_native_blocks_across_byte_splits() {
        let f: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/compactor-contract-0500bf6.json"
        ))
        .unwrap();
        let wire = f["stream"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| format!("data: {e}\n\n"))
            .collect::<String>();
        for chunk in [1, 2, 7, wire.len()] {
            let mut decoder = super::super::SseDecoder::default();
            let mut value = None;
            for bytes in wire.as_bytes().chunks(chunk) {
                if let Some(result) = decoder.feed(bytes).unwrap() {
                    assert!(value.replace(result).is_none());
                }
            }
            let value = value.unwrap();
            let native =
                super::super::native_transport::ordinary_response("anthropic", &value).unwrap();
            assert_eq!(
                super::super::projected_history(&native.native),
                f["stream"]["out"].as_array().unwrap().clone()
            );
            assert_eq!(value["stop_reason"], "pause_turn");
            assert_eq!(value["usage"]["output_tokens"], 17);
            assert!(decoder.cache_begun);
            assert_eq!(value["content"][0]["signature"], "opaque signature");
        }
    }
    #[test]
    fn messages_missing_frontier_or_unterminated_output_never_completes() {
        for events in [
            vec![json!({"type":"message_stop"})],
            vec![
                json!({"type":"message_start","message":{}}),
                json!({"type":"message_stop"}),
            ],
            vec![
                json!({"type":"message_start","message":{}}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"gap"}}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
                json!({"type":"message_stop"}),
            ],
        ] {
            let mut decoder = super::super::SseDecoder::default();
            let wire = events
                .iter()
                .map(|e| format!("data: {e}\n\n"))
                .collect::<String>();
            assert!(decoder.feed(wire.as_bytes()).is_err());
        }
        let mut decoder = super::super::SseDecoder::default();
        assert!(
            decoder
                .feed(b"data: {\"type\":\"message_start\",\"message\":{}}\n\n")
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
mod entry_byte_tests {
    use super::*;
    #[test]
    fn actual_wire_completed_entries_match_36_pinned_js_oracles() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/anthropic-entry-bytes-0500bf6.json"
        ))
        .unwrap();
        for case in fixtures["cases"].as_array().unwrap() {
            let raw = case["raw_input"].as_str().unwrap();
            let start = if case["mode"] == "start" { raw } else { "{}" };
            let mut wire = format!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{}}}}}}\n\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"synthetic-call\",\"name\":\"synthetic_tool\",\"input\":{start}}}}}\n\n"
            );
            if case["mode"] == "delta" {
                wire.push_str(&format!("data: {}\n\n",json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":raw}})));
            }
            wire.push_str("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n");
            for chunk in [1, 7, wire.len()] {
                let mut decoder = super::super::SseDecoder {
                    native: true,
                    anthropic_wire: true,
                    ..Default::default()
                };
                let mut result = None;
                for bytes in wire.as_bytes().chunks(chunk) {
                    if let Some(value) = decoder.feed(bytes).unwrap() {
                        result = Some(value);
                    }
                }
                let result = result.unwrap();
                let response =
                    super::super::native_transport::ordinary_response("anthropic", &result)
                        .unwrap();
                assert_eq!(
                    super::super::completed_entries::read("anthropic", &result, &response)
                        .into_iter()
                        .filter(|e| matches!(e.kind, super::super::ObservationKind::Tool))
                        .map(|e| e.text)
                        .collect::<Vec<_>>(),
                    vec![case["expected_tool_entry"].as_str().unwrap().to_owned()],
                    "{} {} chunk{chunk}",
                    case["name"],
                    case["mode"]
                );
            }
        }
    }
}
