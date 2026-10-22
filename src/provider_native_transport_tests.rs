mod native_transport_tests {
    use super::*;
    use axum::{Json, Router, body::Body, extract::State, response::IntoResponse, routing::post};
    use std::sync::{Arc, Mutex};
    type WireReply = (u16, Vec<(&'static str, &'static str)>, String);
    #[derive(Clone)]
    struct StateData {
        requests: Arc<Mutex<Vec<(Value, tokio::time::Instant)>>>,
        replies: Arc<Mutex<std::collections::VecDeque<WireReply>>>,
    }
    async fn reply(
        State(data): State<StateData>,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        data.requests
            .lock()
            .unwrap()
            .push((body, tokio::time::Instant::now()));
        let (status, headers, text) = data
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("no ninth dispatch");
        let mut response = (reqwest::StatusCode::from_u16(status).unwrap(), text).into_response();
        for (key, value) in headers {
            response.headers_mut().insert(key, value.parse().unwrap());
        }
        response
    }
    fn done() -> String {
        format!(
            "data: {}\n",
            json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":{}}})
        )
    }
    async fn server(replies: Vec<WireReply>) -> (Provider, StateData, tokio::task::JoinHandle<()>) {
        let data = StateData {
            requests: Arc::default(),
            replies: Arc::new(Mutex::new(replies.into())),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut provider = Provider::mock(format!("http://{}/", listener.local_addr().unwrap()));
        provider.native_transport = true;
        let copy = data.clone();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/", post(reply)).with_state(copy),
            )
            .await
            .unwrap();
        });
        (provider, data, task)
    }
    async fn request(provider: &Provider) -> Result<Response> {
        provider
            .step(
                "openai/gpt-6-fixture",
                "medium",
                "system",
                &Provider::start("openai", "<chat>\n</chat>\n", "task"),
                &[],
            )
            .await
    }
    #[tokio::test]
    async fn native_actual_eight_request_budget_with_numeric_retry_after() {
        for success in [false, true] {
            let mut replies =
                vec![(503, vec![("retry-after", "0.001")], "private error".into()); 8];
            if success {
                replies[7] = (200, vec![("content-type", "text/event-stream")], done());
            }
            let (provider, data, task) = server(replies).await;
            assert_eq!(request(&provider).await.is_ok(), success);
            let requests = data.requests.lock().unwrap();
            assert_eq!(requests.len(), 8);
            for pair in requests.windows(2) {
                assert!(pair[1].1 - pair[0].1 >= Duration::from_millis(1));
                assert_eq!(pair[0].0, pair[1].0, "same body through busy attempts");
            }
            task.abort();
        }
    }
    #[tokio::test]
    async fn native_actual_headers_and_stream_failure_retry_after() {
        for (status, headers, n, expected) in [
            (400, vec![("x-should-retry", "true")], 2, 2000),
            (503, vec![("x-should-retry", "false")], 1, 0),
            (409, vec![], 2, 2000),
            (501, vec![("retry-after", "0.25")], 2, 250),
            (429, vec![("retry-after", "Infinity")], 2, 1),
            (
                200,
                vec![
                    ("retry-after", "0x10"),
                    ("content-type", "text/event-stream"),
                ],
                2,
                16000,
            ),
        ] {
            let (provider, data, task) = server(vec![
                (status, headers, "broken stream".into()),
                (200, vec![("content-type", "text/event-stream")], done()),
            ])
            .await;
            assert_eq!(request(&provider).await.is_ok(), n == 2);
            let requests = data.requests.lock().unwrap();
            assert_eq!(requests.len(), n);
            if n == 2 {
                let actual = requests[1].1 - requests[0].1;
                assert!(actual >= Duration::from_millis(expected));
                assert!(actual < Duration::from_millis(expected + 1000));
            }
            task.abort();
        }
    }
    #[tokio::test]
    async fn native_actual_cancel_busy_sleep_never_redispatches() {
        let (provider, data, task) = server(vec![(503, vec![], "busy".into())]).await;
        let mut future = Box::pin(request(&provider));
        tokio::select! { result=&mut future=>panic!("unexpected {result:?}"),_ = async {
            loop { if data.requests.lock().unwrap().len()==1 {break;} tokio::task::yield_now().await; }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }=>{} }
        drop(future);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(data.requests.lock().unwrap().len(), 1);
        task.abort();
    }
    #[test]
    fn native_actual_event_lines_and_terminal_vendor_classification() {
        for (anthropic, event, retryable) in [
            (false, json!({"type":"error","code":"server_error"}), true),
            (
                false,
                json!({"type":"error","code":"rate_limit_exceeded"}),
                false,
            ),
            (
                false,
                json!({"type":"error","error":{"type":"overloaded_error"}}),
                false,
            ),
            (
                true,
                json!({"type":"error","error":{"type":"authentication_error"}}),
                true,
            ),
            (
                false,
                json!({"type":"response.failed","response":{"error":{"code":"server_error"}}}),
                true,
            ),
            (
                false,
                json!({"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded"}}}),
                false,
            ),
            (
                false,
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}),
                false,
            ),
        ] {
            let mut decoder = SseDecoder {
                native: true,
                anthropic_wire: anthropic,
                ..Default::default()
            };
            let result = decoder.feed(format!(" data: {event} \n").as_bytes());
            if let Err(error) = result {
                assert_eq!(
                    matches!(
                        error.downcast_ref::<StreamFailure>(),
                        Some(StreamFailure::Transient(_))
                    ),
                    retryable,
                    "{event}"
                );
            } else {
                assert!(!retryable);
                assert!(result.unwrap().is_some());
            }
        }
        let mut decoder = SseDecoder {
            native: true,
            ..Default::default()
        };
        assert!(
            decoder
                .feed(b": comment\ndata: {\"type\":\"response.created\"}\ndata: ")
                .unwrap()
                .is_none()
        );
        assert_eq!(decoder.events, 1);
        assert!(
            decoder
                .feed(b"{\"type\":\"response.in_progress\"}\n")
                .unwrap()
                .is_none()
        );
        assert_eq!(decoder.events, 2);
    }
    async fn stream_chunks(chunks: Vec<(u64, &'static str)>, period: u64) -> Result<NativeValue> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let router = Router::new().route(
                "/",
                post(move || {
                    let chunks = chunks.clone();
                    async move {
                        let body = futures_util::stream::unfold(
                            chunks.into_iter(),
                            |mut chunks| async move {
                                let (seconds, text) = chunks.next()?;
                                tokio::time::sleep(Duration::from_millis(seconds)).await;
                                Some((Ok::<_, std::io::Error>(text), chunks))
                            },
                        );
                        (
                            [("content-type", "text/event-stream")],
                            Body::from_stream(body),
                        )
                    }
                }),
            );
            axum::serve(listener, router).await.unwrap();
        });
        let response = reqwest::Client::new().post(url).send().await.unwrap();
        let period = Duration::from_millis(period);
        let result = read_response_lifecycle(
            response,
            true,
            None,
            None,
            Some((period, tokio::time::Instant::now() + period, false)),
            None,
        )
        .await;
        task.abort();
        result
    }
    #[tokio::test]
    async fn native_actual_idle_refreshes_only_complete_json_lines() {
        let end = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n";
        let start = tokio::time::Instant::now();
        assert!(
            stream_chunks(
                vec![
                    (70, "data: {\"type\":\"response.created\"}\n"),
                    (70, "data: {\"type\":\"response.in_progress\"}\n"),
                    (70, end)
                ],
                100
            )
            .await
            .is_ok()
        );
        assert!(start.elapsed() > Duration::from_millis(200));
        let error = stream_chunks(vec![(70, ": keepalive\n"), (70, end)], 100)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("idle timeout"));
        let error = stream_chunks(
            vec![
                (70, "data: {\"type\":"),
                (70, "\"response.created\"}\n"),
                (1, end),
            ],
            100,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("idle timeout"));
    }
    #[test]
    fn ordinary_task_mark_and_original_end_cache_survive_steering() {
        for (vendor, model, field, mark) in [
            (
                "openai",
                "openai/gpt-6-fixture",
                "input",
                "prompt_cache_breakpoint",
            ),
            (
                "anthropic",
                "anthropic/fixture",
                "messages",
                "cache_control",
            ),
        ] {
            let mut ordinary = Provider::start(vendor, "<chat>\n</chat>\n", "task");
            assert!(
                ordinary[0]["content"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()
                    .get(mark)
                    .is_some()
            );
            ordinary.push(Provider::user(vendor, "steering"));
            let mut body =
                Provider::request_body(model, "medium", "system", &ordinary, &[]).unwrap();
            compaction::ordinary_profile(model, &mut body).unwrap();
            if vendor == "openai" {
                assert!(body.get("prompt_cache_options").is_none());
            } else {
                assert_eq!(body["cache_control"], json!({"type":"ephemeral"}));
            }
            let original = &body[field]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "user")
                .unwrap()["content"];
            assert!(
                original
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|b| b["text"] == "task" && b.get(mark).is_some())
            );
            let compact = Provider::start_compaction(vendor, "<chat>\n</chat>\n", "task");
            assert!(
                compact[0]["content"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()
                    .get(mark)
                    .is_none()
            );
        }
    }
    fn anthropic_done() -> String {
        [
            json!({"type":"message_start","message":{"content":[],"usage":{}}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            json!({"type":"message_stop"}),
        ]
        .into_iter()
        .map(|ev| format!("data: {ev}\n"))
        .collect()
    }
    #[tokio::test]
    async fn native_actual_upstream_twenty_builder_cases_all_four_request_shapes() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/ordinary-request-0500bf6.json"
        ))
        .unwrap();
        assert_eq!(
            fixture["upstream"],
            "0500bf6a8ea4bf0ce047488562022b1b6532cc30"
        );
        let mut compared = 0;
        for case in fixture["cases"].as_array().unwrap().iter().filter(|c| {
            c["name"]
                .as_str()
                .unwrap()
                .starts_with("actual-root-worker-compactor-build-")
        }) {
            for (ask_key, body_key, compactor) in [
                ("root", "root", false),
                ("worker", "worker", false),
                ("compactor", "compactor", true),
                ("correction", "compactor-correction", true),
            ] {
                let ask = &case[ask_key];
                let anthropic = ask["vendor"] == "claude-api";
                let vendor = if anthropic { "anthropic" } else { "openai" };
                let model = format!("{vendor}/{}", ask["model"].as_str().unwrap());
                let blocks: Vec<Value> = ask["blocks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|block| {
                        let mut out =
                            Provider::user(vendor, block["text"].as_str().unwrap())["content"][0]
                                .clone();
                        if block["mark"] == true {
                            out[if anthropic {
                                "cache_control"
                            } else {
                                "prompt_cache_breakpoint"
                            }] = if anthropic {
                                json!({"type":"ephemeral"})
                            } else {
                                json!({"mode":"explicit"})
                            };
                        }
                        out
                    })
                    .collect();
                let history = [json!({"role":"user","content":blocks})];
                let mut expected = case["bodies"][body_key].clone();
                // Named product difference: Pantheon's hosted search is an
                // explicit external web tool, not an implicit provider search.
                // All other expected fields remain literal upstream values.
                let tools:Vec<Value>=expected["tools"].as_array().into_iter().flatten().filter(|t|t["type"].is_null()||t["type"]=="function").map(|tool|json!({"name":tool["name"],"description":tool["description"],"input_schema":if anthropic {tool["input_schema"].clone()} else {tool["parameters"].clone()}})).collect();
                if let Some(defs) = expected["tools"].as_array_mut() {
                    defs.retain(|t| t["type"].is_null() || t["type"] == "function");
                }
                expected.as_object_mut().unwrap().remove("max_tool_calls");
                if anthropic && tools.is_empty() {
                    expected.as_object_mut().unwrap().remove("tools");
                }
                let (mut provider, data, task) = server(vec![(
                    200,
                    vec![("content-type", "text/event-stream")],
                    if anthropic { anthropic_done() } else { done() },
                )])
                .await;
                let catalog = Arc::new(crate::models::Catalog::default());
                let mut metadata = crate::models::Model::plain(ask["model"].as_str().unwrap());
                metadata.efforts = vec!["medium".into()];
                metadata.adaptive_thinking = anthropic;
                catalog.replace(vendor, vec![metadata]);
                provider = provider.with_catalog(catalog);
                if compactor {
                    provider
                        .step_compaction(
                            &model,
                            "medium",
                            ask["system"].as_str().unwrap(),
                            &history.iter().cloned().map(Into::into).collect::<Vec<crate::provider::NativeValue>>(),
                        )
                        .await
                        .unwrap();
                } else {
                    provider
                        .step(
                            &model,
                            "medium",
                            ask["system"].as_str().unwrap(),
                            &history.iter().cloned().map(Into::into).collect::<Vec<crate::provider::NativeValue>>(),
                            &tools,
                        )
                        .await
                        .unwrap();
                }
                let mut actual = data.requests.lock().unwrap()[0].0.clone();
                if !anthropic {
                    assert_eq!(actual["include"], json!(["reasoning.encrypted_content"]));
                    // Named host requirement: retain encrypted reasoning across
                    // tool/steering turns. No other field is normalized.
                    actual.as_object_mut().unwrap().remove("include");
                }
                assert_eq!(actual, expected, "{} {body_key}", case["name"]);
                compared += 1;
                task.abort();
            }
        }
        assert_eq!(compared, 80);
    }

    #[tokio::test]
    async fn native_busy_retry_discards_partial_entries_and_preserves_final_once() {
        let busy = format!(
            "data: {}\ndata: {}\n",
            json!({"type":"response.output_text.delta","delta":"discarded"}),
            json!({"type":"response.failed","response":{"status":"failed","error":{"code":"server_error"},"output":[]}})
        );
        let final_value = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"success"}]}]}});
        let (provider, data, task) = server(vec![
            (
                200,
                vec![
                    ("content-type", "text/event-stream"),
                    ("retry-after", "0.001"),
                ],
                busy,
            ),
            (
                200,
                vec![("content-type", "text/event-stream")],
                format!("data: {final_value}\n"),
            ),
        ])
        .await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let response = provider
            .step_observed_events(
                "openai/gpt-6-fixture",
                "medium",
                "synthetic",
                &Provider::start("openai", "<chat>\n</chat>\n", "task"),
                &[],
                None,
                Some(tx),
            )
            .await
            .unwrap();
        assert_eq!(response.texts, vec![("success".into(), false)]);
        let observations: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(observations.len(), 1);
        assert!(observations[0].entries.is_none());
        assert_eq!(observations[0].response.texts, response.texts);
        assert_eq!(data.requests.lock().unwrap().len(), 2);
        task.abort();
    }
    #[test]
    fn native_json_null_use_is_rejected_but_native_output_normalizes_and_raw_metadata_is_not_wire()
    {
        let value = json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"call","name":"never_execute","input":null}]});
        let response = native_transport::ordinary_response("anthropic", &value.into()).unwrap();
        assert!(response.calls[0].arguments.is_null());
        assert_eq!(response.native[0]["input"], json!({}));
        let mut history = Provider::start("anthropic", "<chat>\n</chat>\n", "task");
        Provider::append_response("anthropic", &mut history, &response);
        let body =
            Provider::request_body("anthropic/fixture", "medium", "synthetic", &history, &[])
                .unwrap();
        assert!(!body.to_string().contains("_pantheon_tool_inputs"));
        let err: anyhow::Error = native_transport::LocalBoundary.into();
        assert!(native_transport::delay(&err, 1).is_none());
    }
    #[tokio::test]
    async fn native_openai_top_level_error_is_answered_failure_for_compactor_and_ordinary() {
        for code in [Some("invalid_prompt"), None] {
            for message in [Some("synthetic invalid request"), None] {
                let mut event = json!({"type":"error"});
                if let Some(code) = code {
                    event["code"] = json!(code);
                }
                if let Some(message) = message {
                    event["message"] = json!(message);
                }
                let wire = format!(
                    "data: {}\ndata: {event}\n",
                    json!({"type":"response.output_text.delta","delta":" partial "})
                );
                let (provider, data, task) = server(vec![(
                    200,
                    vec![("content-type", "text/event-stream")],
                    wire.clone(),
                )])
                .await;
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let result = provider
                    .step_observed_events(
                        "openai/gpt-6-fixture",
                        "medium",
                        "synthetic",
                        &Provider::start("openai", "<chat>\n</chat>\n", "task"),
                        &[],
                        None,
                        Some(tx),
                    )
                    .await;
                assert!(result.is_err());
                let observed = rx.try_recv().unwrap();
                let entries = observed.entries.unwrap();
                assert_eq!(entries.len(), 1);
                assert!(matches!(entries[0].0, ObservationKind::Reply));
                assert_eq!(
                    entries[0].1,
                    format!("partial\n[cut off: {}]", message.unwrap_or("failed"))
                );
                assert!(rx.try_recv().is_err(), "terminal observation emitted once");
                assert_eq!(data.requests.lock().unwrap().len(), 1);
                task.abort();
                let (provider, data, task) = server(vec![(
                    200,
                    vec![("content-type", "text/event-stream")],
                    wire,
                )])
                .await;
                let result = provider
                    .step_compaction(
                        "openai/gpt-6-fixture",
                        "medium",
                        "synthetic",
                        &Provider::start("openai", "<chat>\n</chat>\n", "task"),
                    )
                    .await
                    .unwrap();
                assert_eq!(result, CompactionReply::ModelFailure);
                assert_eq!(data.requests.lock().unwrap().len(), 1);
                task.abort();
            }
        }
    }

    #[tokio::test]
    async fn native_openai_top_level_server_error_stays_busy_and_discards_prior_trace() {
        let busy = format!(
            "data: {}\ndata: {}\n",
            json!({"type":"response.output_text.delta","delta":"discarded"}),
            json!({"type":"error","code":"server_error","message":"synthetic busy"})
        );
        let final_value = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"success"}]}]}});
        let (provider, data, task) = server(vec![
            (
                200,
                vec![
                    ("content-type", "text/event-stream"),
                    ("retry-after", "0.001"),
                ],
                busy,
            ),
            (
                200,
                vec![("content-type", "text/event-stream")],
                format!("data: {final_value}\n"),
            ),
        ])
        .await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = provider
            .step_observed_events(
                "openai/gpt-6-fixture",
                "medium",
                "synthetic",
                &Provider::start("openai", "<chat>\n</chat>\n", "task"),
                &[],
                None,
                Some(tx),
            )
            .await
            .unwrap();
        assert_eq!(result.texts, vec![("success".into(), false)]);
        let observed = rx.try_recv().unwrap();
        assert!(observed.entries.is_none());
        assert_eq!(observed.response.texts, result.texts);
        assert!(rx.try_recv().is_err());
        assert_eq!(data.requests.lock().unwrap().len(), 2);
        task.abort();
    }
}
