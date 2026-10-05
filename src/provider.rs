//! Native provider transcripts are ephemeral and replayed without rewriting output items.
use crate::memory::cache_chunks;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone)]
pub struct Provider {
    http: reqwest::Client,
    #[cfg(test)]
    endpoint: Option<String>,
}
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Debug)]
pub struct Response {
    pub native: Vec<Value>,
    pub texts: Vec<(String, bool)>,
    pub calls: Vec<ToolCall>,
    pub usage: Value,
}
pub fn model_parts(model: &str) -> Result<(&str, &str)> {
    let (vendor, id) = model
        .split_once('/')
        .context("model must be provider/model-id")?;
    ensure!(
        ["openai", "anthropic"].contains(&vendor) && !id.is_empty(),
        "supported providers: openai, anthropic"
    );
    Ok((vendor, id))
}
impl Provider {
    pub fn new(timeout_seconds: u64) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_seconds))
                .build()?,
            #[cfg(test)]
            endpoint: None,
        })
    }
    #[cfg(test)]
    pub fn mock(endpoint: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: Some(endpoint),
        }
    }
    pub fn start(vendor: &str, view: &str, text: &str) -> Vec<Value> {
        let pieces = cache_chunks(view);
        let mut blocks: Vec<Value> = pieces
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if vendor == "openai" {
                    let mut v = json!({"type":"input_text","text":p});
                    if i + 1 < pieces.len() {
                        v["prompt_cache_breakpoint"] = json!({"mode":"explicit"});
                    }
                    v
                } else {
                    let mut v = json!({"type":"text","text":p});
                    if i + 1 < pieces.len() {
                        v["cache_control"] = json!({"type":"ephemeral"});
                    }
                    v
                }
            })
            .collect();
        blocks.push(if vendor == "openai" {
            json!({"type":"input_text","text":text})
        } else {
            json!({"type":"text","text":text})
        });
        vec![json!({"role":"user","content":blocks})]
    }
    pub fn user(vendor: &str, text: &str) -> Value {
        json!({"role":"user","content":[{"type":if vendor=="openai" {"input_text"} else {"text"},"text":text}]})
    }
    pub fn result(vendor: &str, call: &ToolCall, result: &str, error: bool) -> Value {
        if vendor == "openai" {
            json!({"type":"function_call_output","call_id":call.id,"output":result})
        } else {
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":call.id,"content":result,"is_error":error}]})
        }
    }
    pub fn append_result(
        vendor: &str,
        history: &mut Vec<Value>,
        call: &ToolCall,
        result: &str,
        error: bool,
    ) {
        Self::append_result_with_image(vendor, history, call, result, error, None);
    }
    pub fn append_result_with_image(
        vendor: &str,
        history: &mut Vec<Value>,
        call: &ToolCall,
        result: &str,
        error: bool,
        image: Option<&str>,
    ) {
        let mut item = Self::result(vendor, call, result, error);
        if let Some(data) = image {
            if vendor == "openai" {
                item["output"] = json!([{"type":"input_text","text":result},{"type":"input_image","image_url":format!("data:image/png;base64,{data}"),"detail":"auto"}]);
            } else {
                item["content"][0]["content"] = json!([{"type":"text","text":result},{"type":"image","source":{"type":"base64","media_type":"image/png","data":data}}]);
            }
        }
        if vendor == "anthropic"
            && let Some(last) = history.last_mut()
            && last["role"] == "user"
            && last["content"]
                .as_array()
                .is_some_and(|a| a.iter().all(|b| b["type"] == "tool_result"))
        {
            last["content"]
                .as_array_mut()
                .unwrap()
                .push(item["content"][0].clone());
            return;
        }
        history.push(item);
    }
    pub fn append_response(vendor: &str, history: &mut Vec<Value>, response: &Response) {
        if vendor == "openai" {
            history.extend(response.native.clone());
        } else {
            history.push(json!({"role":"assistant","content":response.native}));
        }
    }
    pub fn request_body(
        model: &str,
        reasoning: &str,
        system: &str,
        history: &[Value],
        tools: &[Value],
    ) -> Result<Value> {
        let (vendor, id) = model_parts(model)?;
        if vendor == "openai" {
            let mut input = vec![json!({"role":"system","content":system})];
            input.extend_from_slice(history);
            // Explicit breakpoints are supported only by GPT-5.6 and later.
            if !(id.starts_with("gpt-5.6") || id.starts_with("gpt-6")) {
                for item in &mut input {
                    if let Some(blocks) = item["content"].as_array_mut() {
                        for block in blocks {
                            if let Some(map) = block.as_object_mut() {
                                map.remove("prompt_cache_breakpoint");
                            }
                        }
                    }
                }
            }
            let converted:Vec<Value>=tools.iter().map(|t|json!({"type":"function","name":t["name"],"description":t["description"],"parameters":t["input_schema"],"strict":false})).collect();
            let mut body = json!({"model":id,"store":false,"include":["reasoning.encrypted_content"],"input":input,"tools":converted,"max_output_tokens":16384});
            // None is usable for models without reasoning; all_turns preserves the prefix after steering.
            if reasoning != "none" {
                body["reasoning"] =
                    json!({"effort":reasoning,"context":"all_turns","summary":"auto"});
            }
            Ok(body)
        } else {
            let mut body = json!({"model":id,"system":system,"messages":history,"tools":tools,"max_tokens":16384,"cache_control":{"type":"ephemeral"}});
            if reasoning != "none" {
                let budget = match reasoning {
                    "minimal" => 1024,
                    "low" => 2048,
                    "high" => 8192,
                    "xhigh" => 12000,
                    _ => 4096,
                };
                body["thinking"] = json!({"type":"enabled","budget_tokens":budget});
            }
            Ok(body)
        }
    }
    pub async fn step(
        &self,
        model: &str,
        reasoning: &str,
        system: &str,
        history: &[Value],
        tools: &[Value],
    ) -> Result<Response> {
        let (vendor, _) = model_parts(model)?;
        let key_name = if vendor == "openai" {
            "OPENAI_API_KEY"
        } else {
            "ANTHROPIC_API_KEY"
        };
        #[cfg(test)]
        let key = if self.endpoint.is_some() {
            "mock-key".into()
        } else {
            std::env::var(key_name).with_context(|| format!("missing {key_name}"))?
        };
        #[cfg(not(test))]
        let key = std::env::var(key_name).with_context(|| format!("missing {key_name}"))?;
        let body = Self::request_body(model, reasoning, system, history, tools)?;
        #[cfg(test)]
        let endpoint = self.endpoint.as_deref().unwrap_or(if vendor == "openai" {
            "https://api.openai.com/v1/responses"
        } else {
            "https://api.anthropic.com/v1/messages"
        });
        #[cfg(not(test))]
        let endpoint = if vendor == "openai" {
            "https://api.openai.com/v1/responses"
        } else {
            "https://api.anthropic.com/v1/messages"
        };
        let req = if vendor == "openai" {
            self.http.post(endpoint).bearer_auth(key)
        } else {
            self.http
                .post(endpoint)
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
        };
        let response = req
            .json(&body)
            .send()
            .await
            .context("provider transport failure")?;
        let status = response.status();
        // Do not expose provider error bodies: they can echo prompts or credentials.
        if !status.is_success() {
            bail!("{vendor} returned HTTP {status}");
        }
        let value: Value = response.json().await.context("invalid provider JSON")?;
        Self::parse(vendor, value)
    }
    pub fn parse(vendor: &str, value: Value) -> Result<Response> {
        if vendor == "openai" {
            ensure!(
                value["status"] == "completed",
                "provider response incomplete; no tools executed"
            );
        } else {
            ensure!(
                value["stop_reason"] != "max_tokens" && value["stop_reason"] != "refusal",
                "provider response incomplete or refused; no tools executed"
            );
        }
        let native = value[if vendor == "openai" {
            "output"
        } else {
            "content"
        }]
        .as_array()
        .context("missing provider output")?
        .clone();
        let mut texts = Vec::new();
        let mut calls = Vec::new();
        for item in &native {
            match item["type"].as_str().unwrap_or("") {
                "message" => {
                    for block in item["content"].as_array().into_iter().flatten() {
                        if let Some(t) =
                            block["text"].as_str().or_else(|| block["refusal"].as_str())
                        {
                            texts.push((t.into(), false));
                        }
                    }
                }
                "text" => texts.push((item["text"].as_str().unwrap_or("").into(), false)),
                "thinking" => texts.push((item["thinking"].as_str().unwrap_or("").into(), true)),
                "reasoning" => {
                    for b in item["summary"].as_array().into_iter().flatten() {
                        if let Some(t) = b["text"].as_str() {
                            texts.push((t.into(), true));
                        }
                    }
                }
                "function_call" | "tool_use" => {
                    let id = if vendor == "openai" { "call_id" } else { "id" };
                    let arguments = if vendor == "openai" {
                        serde_json::from_str(
                            item["arguments"]
                                .as_str()
                                .context("missing tool arguments")?,
                        )
                        .context("tool JSON incomplete")?
                    } else {
                        item["input"].clone()
                    };
                    calls.push(ToolCall {
                        id: item[id].as_str().context("missing call id")?.into(),
                        name: item["name"].as_str().context("missing tool name")?.into(),
                        arguments,
                    });
                }
                _ => {}
            }
        }
        Ok(Response {
            native,
            texts,
            calls,
            usage: value["usage"].clone(),
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_reasoning_and_output_verbatim() {
        let raw = json!({"status":"completed","output":[{"type":"reasoning","id":"r","encrypted_content":"opaque","summary":[]},{"type":"function_call","id":"i","call_id":"c","name":"date","arguments":"{\"id\":0}"}],"usage":{}});
        let r = Provider::parse("openai", raw.clone()).unwrap();
        let mut h = vec![];
        Provider::append_response("openai", &mut h, &r);
        assert_eq!(json!(h), raw["output"]);
        h.push(Provider::user("openai", "steer"));
        let b = Provider::request_body("openai/test", "medium", "fixed", &h, &[]).unwrap();
        assert_eq!(b["reasoning"]["context"], "all_turns");
        assert_eq!(b["store"], false);
        assert_eq!(b["input"][1]["encrypted_content"], "opaque");
    }
    #[test]
    fn incomplete_call_never_executes() {
        assert!(Provider::parse("openai", json!({"status":"incomplete","output":[]})).is_err());
    }
    #[test]
    fn malformed_arguments_rejected() {
        assert!(Provider::parse("openai",json!({"status":"completed","output":[{"type":"function_call","call_id":"a","name":"shell","arguments":"{"}]})).is_err());
    }
    #[test]
    fn screenshot_content_uses_native_provider_image_shapes() {
        let call = ToolCall {
            id: "c".into(),
            name: "browser".into(),
            arguments: json!({}),
        };
        let mut h = vec![];
        Provider::append_result_with_image(
            "openai",
            &mut h,
            &call,
            "metadata",
            false,
            Some("cG5n"),
        );
        assert_eq!(h[0]["output"][1]["type"], "input_image");
        assert_eq!(h[0]["output"][1]["image_url"], "data:image/png;base64,cG5n");
        let mut h = vec![];
        Provider::append_result_with_image(
            "anthropic",
            &mut h,
            &call,
            "metadata",
            false,
            Some("cG5n"),
        );
        assert_eq!(
            h[0]["content"][0]["content"][1]["source"]["media_type"],
            "image/png"
        );
    }
    #[test]
    fn explicit_view_marks_are_stable_and_only_sent_to_supported_models() {
        let view = (0..300)
            .map(|i| format!("{i}+1|{}\n", "é".repeat(300)))
            .collect::<String>();
        let history = Provider::start("openai", &view, "go");
        let supported =
            Provider::request_body("openai/gpt-5.6", "medium", "fixed", &history, &[]).unwrap();
        let unsupported =
            Provider::request_body("openai/gpt-5-mini", "medium", "fixed", &history, &[]).unwrap();
        let blocks = supported["input"][1]["content"].as_array().unwrap();
        assert_eq!(
            blocks
                .iter()
                .filter(|b| !b["prompt_cache_breakpoint"].is_null())
                .count(),
            2
        );
        assert!(
            unsupported["input"][1]["content"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["prompt_cache_breakpoint"].is_null())
        );
        assert_eq!(
            Provider::request_body("openai/gpt-5.6", "medium", "fixed", &history, &[]).unwrap(),
            supported
        );
    }
}
