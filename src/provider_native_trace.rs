//! Ephemeral native ordinary stream observations. Never a persistent provider
//! session, compatibility format, action queue or a source-summary fallback.
use super::{Observation, ObservationKind, Response};
use serde_json::json;
use std::sync::{Arc, Mutex};
#[derive(Default, Debug)]
pub(super) struct Trace {
    #[cfg(test)]
    pub probe: Option<Arc<tokio::sync::Notify>>,
    entries: Vec<(&'static str, Vec<u16>)>,
}
impl Trace {
    pub fn clear(&mut self) {
        self.entries.clear();
    }
    pub fn read(&mut self, event: &super::NativeValue) {
        let units = |item: &super::NativeValue, field: &str| item.units(field).to_vec();
        let (kind, text, fresh) = match event["type"].as_str() {
            Some("response.output_text.delta") => ("reply", units(event, "delta"), false),
            Some("response.reasoning_summary_text.delta") => {
                ("thought", units(event, "delta"), false)
            }
            Some("response.reasoning_summary_part.added")
                if event["summary_index"].as_u64().unwrap_or(0) > 0 =>
            {
                ("thought", "\n\n".encode_utf16().collect(), false)
            }
            Some("response.function_call_arguments.delta") => {
                ("tool", units(event, "delta"), false)
            }
            Some("response.output_item.added")
                if matches!(
                    event["item"]["type"].as_str(),
                    Some("function_call" | "web_search_call")
                ) =>
            {
                let item = event.field("item").unwrap();
                let mut name = if item.source.get("name").is_some() {
                    units(&item, "name")
                } else {
                    "web_search".encode_utf16().collect()
                };
                name.push(32);
                ("tool", name, true)
            }
            Some("content_block_start")
                if event["content_block"]["type"]
                    .as_str()
                    .is_some_and(|t| t.ends_with("tool_use")) =>
            {
                let mut name = units(&event.field("content_block").unwrap(), "name");
                name.push(32);
                ("tool", name, true)
            }
            Some("content_block_delta") => {
                let delta = event.field("delta").unwrap();
                match delta["type"].as_str() {
                    Some("text_delta") => ("reply", units(&delta, "text"), false),
                    Some("thinking_delta") => ("thought", units(&delta, "thinking"), false),
                    Some("input_json_delta") => ("tool", units(&delta, "partial_json"), false),
                    _ => return,
                }
            }
            _ => return,
        };
        #[cfg(test)]
        if kind == "reply"
            && let Some(probe) = &self.probe
        {
            probe.notify_one();
        }
        if let Some((last_kind, last_text)) = self.entries.last_mut()
            && *last_kind == kind
            && !fresh
        {
            last_text.extend_from_slice(&text);
        } else {
            self.entries.push((kind, text));
        }
    }
    pub fn cut_entries(&self, why: &str) -> Vec<(ObservationKind, String)> {
        // Named safety difference: sanitize provider/account reasons.
        let why = crate::activity::safe(why, 300);
        self.entries
            .iter()
            .filter_map(|(kind, text)| {
                let well_formed = String::from_utf16_lossy(text);
                let text = crate::memory::summary_trim(&well_formed);
                let kind = match *kind {
                    "tool" => ObservationKind::Tool,
                    "thought" => ObservationKind::Thought,
                    _ => ObservationKind::Reply,
                };
                (!text.is_empty()).then(|| (kind, format!("{text}\n[cut off: {why}]")))
            })
            .collect()
    }
    pub fn cut(&self, why: &str) -> Vec<(String, bool)> {
        self.cut_entries(why)
            .into_iter()
            .filter_map(|(kind, text)| match kind {
                ObservationKind::Tool | ObservationKind::Echo => None,
                ObservationKind::Thought => Some((text, true)),
                ObservationKind::Reply => Some((text, false)),
            })
            .collect()
    }
}
/// Dropping an active ordinary call records the streamed partial exactly once;
/// completed steps and discarded retry attempts clear the shared trace first.
pub(super) struct Guard {
    pub trace: Arc<Mutex<Trace>>,
    pub observations: Option<tokio::sync::mpsc::UnboundedSender<Observation>>,
    pub finished: bool,
}
impl Guard {
    pub fn emit(&self, why: &str) {
        let entries = self.trace.lock().unwrap().cut_entries(why);
        let texts = self.trace.lock().unwrap().cut(why);
        if !entries.is_empty()
            && let Some(sender) = &self.observations
        {
            let _ = sender.send(Observation {
                response: Response {
                    native: vec![],
                    texts,
                    calls: vec![],
                    usage: json!({}),
                },
                intermediate: true,
                completed: None,
                entries: Some(entries),
            });
        }
        self.trace.lock().unwrap().clear();
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if !self.finished {
            self.emit("stopped");
        }
    }
}
pub(super) fn failure(model: &str, value: &super::NativeValue) -> Option<String> {
    if model.starts_with("anthropic/") {
        let stop = value["stop_reason"].as_str().unwrap_or("");
        if matches!(stop, "end_turn" | "tool_use" | "pause_turn") {
            return None;
        }
        let mut why = format!("stopped: {stop}");
        if let Some(explanation) = value
            .source
            .get("stop_details")
            .and_then(|v| v.get("explanation"))
            .filter(|v| v.truthy())
        {
            let explanation = super::completed_entries::display_native(Some(explanation));
            why.push_str(&format!(" ({explanation})"));
        }
        Some(why)
    } else {
        match value["status"].as_str() {
            Some("failed") => Some(
                value
                    .source
                    .get("error")
                    .and_then(|v| v.get("message"))
                    .filter(|v| !matches!(v, super::entry_json::Json::Null))
                    .map(|v| super::completed_entries::display_native(Some(v)))
                    .unwrap_or_else(|| "failed".into()),
            ),
            Some("incomplete") => Some(format!(
                "incomplete: {}",
                super::completed_entries::display_native(
                    value
                        .source
                        .get("incomplete_details")
                        .and_then(|v| v.get("reason"))
                )
            )),
            _ => {
                let refusals: Vec<_> = value
                    .field("output")
                    .map(|v| v.elements())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|i| i["type"] == "message")
                    .flat_map(|i| i.field("content").map(|v| v.elements()).unwrap_or_default())
                    .filter(|b| b["type"] == "refusal")
                    .map(|b| super::completed_entries::display_native(b.source.get("refusal")))
                    .collect();
                (!refusals.is_empty()).then(|| format!("refused: {}", refusals.join(" ")))
            }
        }
    }
}
