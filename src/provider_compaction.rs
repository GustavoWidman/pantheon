//! UniiChat0500bf6 compactor outcomes and shared native wire profiles.
//! Ordinary observations remain separate from summary/source-fallback policy.
use super::{Provider, codex, model_parts, retry::StreamFailure};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CompactionReply {
    Answer(String),
    ModelFailure,
}
/// Only an explicitly typed model terminal can supply fail semantics. Never use
/// free-text matching or convert setup/HTTP/EOF/cancel/unknown errors to refusal.
pub(super) fn model_failure(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<StreamFailure>(), Some(StreamFailure::Terminal(d))
        if d.event == "response.incomplete" || (d.event == "response.failed" && !d.retryable))
}
/// AskAI.patient ownership: an unavailable request is not an answered attempt.
/// Retry the same call (including its frozen correction) until answered or stop.
/// Retained outside transport retries so no native reservation is released.
pub(crate) async fn patient<T, F, Fut>(
    stop: &tokio_util::sync::CancellationToken,
    mut request: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    loop {
        let attempt = tokio::select! {
            biased;
            _ = stop.cancelled() => bail!("compaction stopped"),
            result = request() => result,
        };
        match attempt {
            Ok(answer) => return Ok(answer),
            Err(error) => {
                tracing::warn!(error=%error, "compaction request failed; patient retry in 60s");
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => bail!("compaction stopped"),
                    _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
                }
            }
        }
    }
}

/// Structural Unii request profile, isolated from ordinary executable turns.
/// Capability-dependent thinking is selected before this profile; do not guess
/// adaptive support for unknown/legacy models or change subscription projection.
pub(super) fn request_profile(model: &str, body: &mut Value) -> Result<()> {
    let (vendor, _) = model_parts(model)?;
    if model.starts_with("openai/") {
        let first = body["input"]
            .as_array_mut()
            .context("missing compactor input")?
            .first_mut()
            .context("missing compactor system")?;
        let system = first["content"]
            .as_str()
            .context("missing compactor system text")?
            .to_owned();
        *first = json!({"role":"developer","content":[{"type":"input_text","text":system}]});
        body["max_output_tokens"] = json!(128000);
        body["service_tier"] = json!("default");
        body["prompt_cache_options"] = json!({"mode":"explicit"});
        // Public API compactor follows the reference streaming transport.
        body["stream"] = json!(true);
    } else if vendor == "anthropic" {
        body["max_tokens"] = json!(128000);
        // A compactor task/correction is unmarked: upstream does NOT cache the
        // request end automatically. Only system and the memory prefix are marked.
        body.as_object_mut().unwrap().remove("cache_control");
        if body["tools"].as_array().is_some_and(Vec::is_empty) {
            body.as_object_mut().unwrap().remove("tools");
        }
        if body["thinking"]["type"] == "adaptive" {
            body["thinking"]["display"] = json!("summarized");
        }
        // Anthropic's wire merges adjacent items of the same role. This also
        // prevents multiple pause_turns producing consecutive assistant items.
        let messages = body["messages"]
            .as_array_mut()
            .context("missing compactor messages")?;
        let mut merged: Vec<Value> = Vec::new();
        for message in std::mem::take(messages) {
            if let Some(last) = merged.last_mut()
                && last["role"] == message["role"]
            {
                last["content"]
                    .as_array_mut()
                    .context("missing compactor content")?
                    .extend(
                        message["content"]
                            .as_array()
                            .context("missing compactor content")?
                            .iter()
                            .cloned(),
                    );
            } else {
                merged.push(message);
            }
        }
        *messages = merged;
        body["stream"] = json!(true);
    }
    Ok(())
}

/// Ordinary turns share the public native wire, but keep the final ordinary
/// task marked. Cache end policy is derived from the original User text block,
/// not the last tool/assistant/steering item in a continuing transcript.
pub(super) fn ordinary_profile(model: &str, body: &mut Value) -> Result<()> {
    let field = if model.starts_with("openai/") {
        "input"
    } else {
        "messages"
    };
    let mark = if model.starts_with("openai/") {
        "prompt_cache_breakpoint"
    } else {
        "cache_control"
    };
    let marked = body[field]
        .as_array()
        .into_iter()
        .flatten()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_array())
        .and_then(|blocks| {
            blocks
                .iter()
                .rfind(|b| matches!(b["type"].as_str(), Some("text" | "input_text")))
        })
        .is_some_and(|block| block.get(mark).is_some());
    request_profile(model, body)?;
    // Native public wire shares the pinned Back projection across every route,
    // including private Session callers that keep generic String results in
    // their ephemeral transcript. Never flatten existing image/mixed parts.
    // Subscription/Z.ai projections do not pass through this native profile.
    if model.starts_with("openai/") {
        for item in body["input"].as_array_mut().into_iter().flatten() {
            if item["type"] == "function_call_output"
                && let Some(text) = item["output"].as_str()
            {
                item["output"] = json!([{"type":"input_text","text":text}]);
            }
            if item["type"] == "function_call_output" {
                for part in item["output"].as_array_mut().into_iter().flatten() {
                    if part["type"] == "input_image"
                        && let Some(fields) = part.as_object_mut()
                    {
                        fields.remove("detail"); // pinned OpenAI.image has no detail field
                    }
                }
            }
        }
        if marked {
            body.as_object_mut().unwrap().remove("prompt_cache_options");
        }
        if body["reasoning"].is_null() {
            body["reasoning"] = json!({"effort":"none","summary":"auto","context":"all_turns"});
        }
        for tool in body["tools"].as_array_mut().into_iter().flatten() {
            tool.as_object_mut().unwrap().remove("strict");
        }
    } else {
        for message in body["messages"].as_array_mut().into_iter().flatten() {
            for item in message["content"].as_array_mut().into_iter().flatten() {
                if item["type"] == "tool_result"
                    && let Some(text) = item["content"].as_str()
                {
                    item["content"] = if text.is_empty() {
                        json!([])
                    } else {
                        json!([{"type":"text","text":text}])
                    };
                }
                if item["type"] == "tool_result"
                    && let Some(parts) = item["content"].as_array_mut()
                {
                    parts.retain(|part| !(part["type"] == "text" && part["text"] == ""));
                }
            }
        }
        if marked {
            body["cache_control"] = json!({"type":"ephemeral"});
        }
        for tool in body["tools"].as_array_mut().into_iter().flatten() {
            tool["eager_input_streaming"] = json!(true);
        }
    }
    Ok(())
}

pub(super) fn continue_anthropic(
    history: &mut Vec<crate::provider::NativeValue>,
    value: &super::NativeValue,
) -> Result<()> {
    let blocks = value["content"]
        .as_array()
        .context("missing pause_turn content")?;
    ensure!(
        !blocks.iter().any(|b| b["type"] == "tool_use"),
        "compactor returned tools during pause; not executed"
    );
    let blocks = super::anthropic_stream::normalize_native(
        value
            .field("content")
            .context("missing pause content")?
            .elements(),
    );
    let mut item = super::NativeValue::from(json!({"role":"assistant"}));
    item.set("content", super::NativeValue::array(blocks));
    history.push(item);
    Ok(())
}

pub(super) fn parse(model: &str, value: impl Into<super::NativeValue>) -> Result<CompactionReply> {
    let value = value.into();
    let (vendor, _) = model_parts(model)?;
    if vendor == "zai" {
        // No UniiChat Z.ai reference exists. Preserve its existing strict parser;
        // do not infer semantic refusal from generic finish/error wording.
        let r = Provider::parse(vendor, value)?;
        ensure!(r.calls.is_empty(), "compactor returned tools; not executed");
        return Ok(CompactionReply::Answer(
            r.texts
                .into_iter()
                .filter(|(_, thought)| !thought)
                .map(|(t, _)| t)
                .collect::<Vec<_>>()
                .join("\n"),
        ));
    }
    if vendor == "openai" {
        match value["status"].as_str() {
            Some("incomplete") => return Ok(CompactionReply::ModelFailure),
            Some("failed") => {
                ensure!(
                    value["error"]["code"] != "server_error",
                    "compactor provider busy; not a semantic refusal"
                );
                return Ok(CompactionReply::ModelFailure);
            }
            Some("completed") => {}
            _ => bail!("unknown compactor response status"),
        }
        if value["output"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|item| {
                item["type"] == "message"
                    && item["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|block| block["type"] == "refusal")
            })
        {
            return Ok(CompactionReply::ModelFailure);
        }
    } else {
        match value["stop_reason"].as_str() {
            Some("end_turn" | "tool_use") => {}
            Some("pause_turn") => {
                bail!("compactor pause_turn continuation unsupported; not refusal")
            }
            Some(reason) if !reason.is_empty() => return Ok(CompactionReply::ModelFailure),
            _ => bail!("missing compactor stop reason"),
        }
    }
    let response = if model.starts_with("codex/") {
        codex::parse(value.projected())?
    } else {
        super::native_transport::ordinary_response(vendor, &value)?
    };
    ensure!(
        response.calls.is_empty(),
        "compactor returned tools; not executed"
    );
    // Subscription validation/namespace policy above stays strict, while its
    // compactor answer uses the same OpenAI Entry joining/ECMAScript trimming
    // as the public adapter. Generic ordinary Codex turns remain per-block.
    let texts = if model.starts_with("codex/") {
        super::native_transport::ordinary_response(vendor, &value)?.texts
    } else {
        response.texts
    };
    Ok(CompactionReply::Answer(
        texts
            .into_iter()
            .filter(|(_, thought)| !thought)
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n\n"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test(start_paused = true)]
    async fn patient_retries_same_owner_after_exact_minute_without_consuming_answers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        let stop = tokio_util::sync::CancellationToken::new();
        let sent = AtomicUsize::new(0);
        let begin = tokio::time::Instant::now();
        let got = patient(&stop, || async {
            if sent.fetch_add(1, Ordering::SeqCst) < 2 {
                bail!("synthetic down/setup")
            }
            Ok(CompactionReply::Answer("answered".into()))
        })
        .await
        .unwrap();
        assert_eq!(got, CompactionReply::Answer("answered".into()));
        assert_eq!(sent.load(Ordering::SeqCst), 3);
        assert_eq!(
            tokio::time::Instant::now() - begin,
            Duration::from_secs(120)
        );
        let sent = AtomicUsize::new(0);
        assert_eq!(
            patient(&stop, || async {
                sent.fetch_add(1, Ordering::SeqCst);
                Ok(CompactionReply::ModelFailure)
            })
            .await
            .unwrap(),
            CompactionReply::ModelFailure
        );
        assert_eq!(sent.load(Ordering::SeqCst), 1);
    }
    #[tokio::test(start_paused = true)]
    async fn patient_cancels_wait_and_active_request_without_retry_or_fallback() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        let stop = tokio_util::sync::CancellationToken::new();
        let sent = AtomicUsize::new(0);
        let cancel = stop.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            cancel.cancel();
        });
        let result: Result<CompactionReply> = patient(&stop, || async {
            sent.fetch_add(1, Ordering::SeqCst);
            bail!("synthetic failure")
        })
        .await;
        assert!(result.is_err());
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        timer.await.unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        let cancel = stop.clone();
        let timer = tokio::spawn(async move {
            tokio::task::yield_now().await;
            cancel.cancel();
        });
        let result: Result<CompactionReply> = patient(&stop, std::future::pending).await;
        assert!(result.is_err());
        timer.await.unwrap();
        let sent = AtomicUsize::new(0);
        let result: Result<CompactionReply> = patient(&stop, || async {
            sent.fetch_add(1, Ordering::SeqCst);
            Ok(CompactionReply::Answer("must not submit".into()))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(sent.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn pause_json_native_output_matches_actual_upstream_step() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/compactor-contract-0500bf6.json"
        ))
        .unwrap();
        let mut history = Provider::start_compaction("anthropic", "<chat>\n</chat>\n", "task");
        continue_anthropic(
            &mut history,
            &json!({"content":fixture["pauseJson"]}).into(),
        )
        .unwrap();
        assert_eq!(history[1]["content"], fixture["pauseNative"]);
    }
    #[test]
    fn request_profile_matches_real_upstream_supported_wire_projection() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/compactor-contract-0500bf6.json"
        ))
        .unwrap();
        let context = fixture["context"].as_str().unwrap();
        let task = crate::memory::compaction_task(
            crate::memory::NodeKey { level: 0, index: 1 },
            fixture["source"].as_str().unwrap(),
        );
        for (vendor, model) in [
            ("openai", "openai/gpt-6-fixture"),
            ("anthropic", "anthropic/fixture"),
        ] {
            let history = Provider::start_compaction(vendor, context, &task);
            let mut body =
                Provider::request_body(model, "medium", "synthetic system", &history, &[]).unwrap();
            if vendor == "anthropic" {
                body["thinking"] = json!({"type":"adaptive"});
                body["output_config"] = json!({"effort":"medium"});
            }
            request_profile(model, &mut body).unwrap();
            let expected = fixture["bodies"][vendor].clone();
            // Host preserves encrypted-reasoning replay support; not an input,
            // summary, cache-mark or model-failure semantic difference.
            body.as_object_mut().unwrap().remove("include");
            assert_eq!(body, expected, "{vendor}");
        }
        // Ordinary agent/tool requests retain existing host behavior.
        let ordinary =
            Provider::request_body("openai/gpt-6-fixture", "medium", "system", &[], &[]).unwrap();
        assert_eq!(ordinary["input"][0]["role"], "system");
        assert_eq!(ordinary["max_output_tokens"], 16384);
    }
    #[test]
    fn explicit_model_outcomes_not_generic_failures() {
        for value in [
            json!({"status":"incomplete"}),
            json!({"status":"failed","error":{"code":"policy_error"}}),
            json!({"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"no"}]}]}),
        ] {
            assert_eq!(
                parse("openai/test", value).unwrap(),
                CompactionReply::ModelFailure
            );
        }
        assert!(
            parse(
                "openai/test",
                json!({"status":"failed","error":{"code":"server_error"}})
            )
            .is_err()
        );
        assert!(parse("openai/test", json!({})).is_err());
        for stop in ["max_tokens", "refusal", "model_context_window_exceeded"] {
            assert_eq!(
                parse("anthropic/test", json!({"stop_reason":stop})).unwrap(),
                CompactionReply::ModelFailure
            );
        }
        assert!(
            parse(
                "anthropic/test",
                json!({"stop_reason":"pause_turn","content":[]})
            )
            .is_err()
        );
        assert!(!model_failure(&anyhow::anyhow!(
            "response.incomplete refusal"
        )));
        assert!(!model_failure(&StreamFailure::PrematureEof.into()));
        for (event, expected) in [
            (json!({"type":"response.incomplete"}), true),
            (
                json!({"type":"response.failed","response":{"error":{"code":"policy_error"}}}),
                true,
            ),
            (
                json!({"type":"error","error":{"code":"invalid_request_error"}}),
                false,
            ),
        ] {
            assert_eq!(
                model_failure(
                    &StreamFailure::Terminal(super::super::retry::event_details(&event)).into()
                ),
                expected
            );
        }
    }
    #[test]
    fn openai_entry_joining_empty_messages_unicode_and_codex_same_wire_shape() {
        let value = json!({"status":"completed","output":[
            {"type":"message","content":[{"type":"output_text","text":"\u{feff}a"},{"type":"output_text","text":"b "}]},
            {"type":"message","content":[{"type":"output_text","text":" \u{feff}"}]},
            {"type":"message","content":[{"type":"output_text","text":"\u{0085}c"}]}]});
        for model in ["openai/test", "codex/test"] {
            assert_eq!(
                parse(model, value.clone()).unwrap(),
                CompactionReply::Answer("ab\n\n\u{0085}c".into())
            );
        }
        // General turn parser remains untouched: still per-block native texts.
        assert_eq!(Provider::parse("openai", value).unwrap().texts.len(), 4);
    }
    #[test]
    fn anthropic_adjacent_runs_trim_after_join_and_thinking_separates() {
        let value = json!({"stop_reason":"end_turn","content":[{"type":"text","text":" a "},{"type":"text","text":"b "},{"type":"thinking","thinking":"private"},{"type":"text","text":" c"},{"type":"text","text":"d "}]});
        assert_eq!(
            parse("anthropic/test", value).unwrap(),
            CompactionReply::Answer("a b\n\ncd".into())
        );
    }
    #[test]
    fn native_back_projection_matches_16_pinned_actual_wire_shapes() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/native-back-shapes-0500bf6.json"
        ))
        .unwrap();
        let mut checked = 0;
        for case in fixtures["records"]
            .as_array()
            .unwrap()
            .iter()
            .chain(std::iter::once(&fixtures["multi"]))
        {
            for vendor in ["openai", "anthropic"] {
                let mut history = vec![];
                for back in case["input"]["backs"].as_array().unwrap() {
                    let call = super::super::ToolCall {
                        id: back["id"].as_str().unwrap().into(),
                        name: "synthetic".into(),
                        arguments: json!({}).into(),
                    };
                    let output = &back["out"];
                    // imgs is optional in the pinned Out shape. The multi-Back
                    // fixture intentionally omits it, unlike named image cases.
                    let image = output["imgs"]
                        .as_array()
                        .and_then(|images| images.first())
                        .map(|v| v["data"].as_str().unwrap());
                    super::super::Provider::append_result_with_image(
                        vendor,
                        &mut history,
                        &call,
                        output["text"].as_str().unwrap(),
                        output["fail"].as_bool().unwrap(),
                        image,
                    );
                }
                let mut body = if vendor == "openai" {
                    let mut input = vec![json!({"role":"system","content":"synthetic"})];
                    input.extend(super::super::projected_history(&history));
                    json!({"input":input,"tools":[]})
                } else {
                    json!({"messages":history,"tools":[]})
                };
                ordinary_profile(
                    if vendor == "openai" {
                        "openai/gpt-6-fixture"
                    } else {
                        "anthropic/test"
                    },
                    &mut body,
                )
                .unwrap();
                let actual = if vendor == "openai" {
                    Value::Array(body["input"].as_array().unwrap()[1..].to_vec())
                } else {
                    body["messages"][0].clone()
                };
                assert_eq!(actual, case[vendor], "{} {vendor}", case["name"]);
                checked += 1;
            }
        }
        assert_eq!(checked, 16);
    }
}
