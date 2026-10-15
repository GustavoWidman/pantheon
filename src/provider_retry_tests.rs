mod request_retries {
    use super::*;
    use axum::{
        Json, Router, extract::State, http::HeaderMap, response::IntoResponse, routing::post,
    };
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Recorded {
        requests: Arc<Mutex<Vec<(HeaderMap, Value, tokio::time::Instant)>>>,
        replies: Arc<Mutex<std::collections::VecDeque<Reply>>>,
    }
    enum Reply {
        Status(u16, Option<&'static str>),
        Json(Value),
        Sse(String),
        Refresh(std::path::PathBuf),
    }
    async fn respond(
        State(state): State<Recorded>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        state
            .requests
            .lock()
            .unwrap()
            .push((headers, body, tokio::time::Instant::now()));
        match state
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected retry")
        {
            Reply::Status(status, after) => {
                let mut response = (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    "private-provider-error",
                )
                    .into_response();
                if let Some(value) = after {
                    response
                        .headers_mut()
                        .insert("retry-after", value.parse().unwrap());
                }
                response
            }
            Reply::Json(value) => Json(value).into_response(),
            Reply::Sse(text) => ([("content-type", "text/event-stream")], text).into_response(),
            Reply::Refresh(home) => {
                std::fs::write(
                    home.join("auth.json"),
                    json!({"tokens":{"access_token":"new-test-token","account_id":"test-account"}})
                        .to_string(),
                )
                .unwrap();
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    "private-provider-error",
                )
                    .into_response()
            }
        }
    }
    async fn server(
        replies: Vec<Reply>,
        attempts: usize,
    ) -> (Provider, Recorded, tokio::task::JoinHandle<()>) {
        let state = Recorded {
            requests: Arc::new(Mutex::new(vec![])),
            replies: Arc::new(Mutex::new(replies.into())),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider = Provider::mock(format!("http://{}/", listener.local_addr().unwrap()));
        let server_state = state.clone();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/", post(respond))
                    .with_state(server_state),
            )
            .await
            .unwrap()
        });
        let mut provider = provider.with_request_retries(attempts, 1).unwrap();
        // Exercise actual sleeps without making offline tests spend seconds per attempt.
        provider.retries.backoff = Duration::from_millis(30);
        (provider, state, task)
    }
    fn completed() -> Value {
        json!({"status":"completed","output":[],"usage":{}})
    }
    fn completed_sse() -> String {
        format!(
            "data: {}\n\n",
            json!({"type":"response.completed","response":completed()})
        )
    }
    fn auth(home: &std::path::Path) -> AuthConfig {
        std::fs::write(
            home.join("auth.json"),
            json!({"tokens":{"access_token":"test-token","account_id":"test-account"}}).to_string(),
        )
        .unwrap();
        AuthConfig {
            codex_home: Some(home.into()),
            codex_cli: None,
        }
    }

    #[tokio::test]
    async fn transient_http_retries_are_linear_bounded_and_preserve_the_native_request() {
        use tracing::instrument::WithSubscriber;
        let (provider, state, task) = server(
            vec![
                Reply::Status(503, None),
                Reply::Status(502, None),
                Reply::Sse(completed_sse()),
            ],
            4,
        )
        .await;
        let home = tempfile::tempdir().unwrap();
        let identity = uuid::Uuid::new_v4();
        let provider = provider
            .with_auth(auth(home.path()))
            .with_cache_affinity(identity);
        let history = vec![
            json!({"type":"reasoning","id":"prior-reasoning","encrypted_content":"opaque-prior-reasoning"}),
            json!({"type":"function_call","name":"write","call_id":"already-completed","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"already-completed","output":"done"}),
        ];
        let submitted = std::sync::atomic::AtomicUsize::new(0);
        let on_submitted = || {
            submitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };
        let log = DiagnosticLog::default();
        provider
            .step_observed(
                "codex/test",
                "none",
                "stable system",
                &history,
                &[],
                Some(&on_submitted),
            )
            .with_subscriber(capture(&log))
            .await
            .unwrap();
        let requests = state.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(submitted.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(requests[0].1, requests[1].1);
        assert_eq!(requests[1].1, requests[2].1);
        for (headers, body, _) in requests.iter() {
            assert_eq!(headers["session-id"], identity.to_string());
            assert_eq!(body["prompt_cache_key"], identity.to_string());
            assert_eq!(body["input"][0]["role"], "developer");
            assert_eq!(body["input"][0]["content"][0]["text"], "stable system");
            assert_eq!(body["input"].as_array().unwrap()[1..], history);
            assert!(!headers.contains_key(crate::provider::codex::LITE_HEADER));
        }
        assert!(requests[1].2.duration_since(requests[0].2) >= Duration::from_millis(30));
        assert!(requests[2].2.duration_since(requests[1].2) >= Duration::from_millis(60));
        let (text, records) = log.records();
        assert_eq!(
            records
                .iter()
                .map(|r| r["attempt"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(text.contains("retry_delay_ms=30"));
        assert!(text.contains("retry_delay_ms=60"));
        assert!(!text.contains("private-provider-error"));
        task.abort();
    }

    #[tokio::test]
    async fn exhausted_attempts_return_the_final_diagnostic_and_one_attempt_disables_retries() {
        use tracing::instrument::WithSubscriber;
        for attempts in [1, 3] {
            let (provider, state, task) = server(
                (0..attempts).map(|_| Reply::Status(503, None)).collect(),
                attempts,
            )
            .await;
            let log = DiagnosticLog::default();
            let error = provider
                .step("openai/test", "none", "", &[], &[])
                .with_subscriber(capture(&log))
                .await
                .unwrap_err();
            assert_eq!(state.requests.lock().unwrap().len(), attempts);
            let (text, records) = log.records();
            assert_eq!(records.len(), attempts);
            assert!(
                error
                    .to_string()
                    .contains(records.last().unwrap()["id"].as_str().unwrap())
            );
            assert!(!text.contains("private-provider-error"));
            task.abort();
        }
    }

    #[tokio::test]
    async fn permanent_errors_and_long_retry_after_do_not_retry() {
        for reply in [
            Reply::Status(400, None),
            Reply::Status(401, None),
            Reply::Status(403, None),
            Reply::Status(429, Some("61")),
            Reply::Status(503, Some("9999999999999999999999999")),
            Reply::Json(
                json!({"status":"completed","output":[{"type":"function_call","name":"write","call_id":"bad","arguments":"invalid-json"}]}),
            ),
            Reply::Sse("data: not-json\n\n".into()),
            Reply::Sse(format!(
                "data: {}\n\n",
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}})
            )),
        ] {
            let (provider, state, task) = server(vec![reply], 4).await;
            provider
                .step("openai/test", "none", "", &[], &[])
                .await
                .unwrap_err();
            assert_eq!(state.requests.lock().unwrap().len(), 1);
            task.abort();
        }
    }

    #[tokio::test]
    async fn partial_tool_items_are_discarded_on_stream_retry() {
        let partial = format!(
            "data: {}\n\n",
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"shell","call_id":"never-dispatch","arguments":"{}"}})
        );
        for ending in [
            "".into(),
            format!(
                "data: {}\n\n",
                json!({"type":"error","error":{"code":"server_error","message":"private-provider-error"}})
            ),
        ] {
            let (provider, state, task) = server(
                vec![
                    Reply::Sse(format!("{partial}{ending}")),
                    Reply::Sse(completed_sse()),
                ],
                3,
            )
            .await;
            let response = provider
                .step("openai/test", "none", "", &[], &[])
                .await
                .unwrap();
            assert!(response.calls.is_empty());
            assert!(response.native.is_empty());
            let requests = state.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].1, requests[1].1);
            task.abort();
        }
    }

    #[tokio::test]
    async fn cancellation_drops_backoff_without_a_second_request() {
        let (mut provider, state, task) = server(vec![Reply::Status(503, None)], 4).await;
        provider.retries.backoff = Duration::from_secs(30);
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            provider.step("openai/test", "none", "", &[], &[]),
        )
        .await;
        assert!(result.is_err());
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(state.requests.lock().unwrap().len(), 1);
        task.abort();
    }

    #[tokio::test]
    async fn codex_auth_refresh_after_a_transient_error_shares_the_total_attempt_limit() {
        for attempts in [2, 3] {
            let home = tempfile::tempdir().unwrap();
            let mut replies = vec![Reply::Status(503, None), Reply::Refresh(home.path().into())];
            if attempts == 3 {
                replies.push(Reply::Sse(completed_sse()));
            }
            let (provider, state, task) = server(replies, attempts).await;
            let provider = provider.with_auth(auth(home.path()));
            let result = provider.step("codex/test", "none", "", &[], &[]).await;
            assert_eq!(result.is_ok(), attempts == 3);
            let requests = state.requests.lock().unwrap();
            assert_eq!(requests.len(), attempts);
            assert_eq!(requests[0].0["authorization"], "Bearer test-token");
            if attempts == 3 {
                assert_eq!(requests[2].0["authorization"], "Bearer new-test-token");
            }
            task.abort();
        }
    }

    #[tokio::test]
    async fn retry_after_delays_the_next_http_attempt() {
        let (provider, state, task) = server(
            vec![Reply::Status(429, Some("1")), Reply::Json(completed())],
            3,
        )
        .await;
        provider
            .step("openai/test", "none", "", &[], &[])
            .await
            .unwrap();
        let requests = state.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].2.duration_since(requests[0].2) >= Duration::from_secs(1));
        task.abort();
    }

    #[tokio::test]
    async fn a_broken_http_body_retries_without_appending_partial_json() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            for attempt in 1..=2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 8192];
                let received = socket.read(&mut request).await.unwrap();
                assert!(
                    received > 0,
                    "expected an HTTP request before the broken response"
                );
                let body = if attempt == 1 {
                    "{".into()
                } else {
                    completed().to_string()
                };
                let length = if attempt == 1 { 1000 } else { body.len() };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
            }
        });
        let mut provider = Provider::mock(endpoint).with_request_retries(3, 1).unwrap();
        provider.retries.backoff = Duration::from_millis(30);
        let response = tokio::time::timeout(
            Duration::from_secs(3),
            provider.step("openai/test", "none", "", &[], &[]),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(response.native.is_empty());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn hosted_search_uses_the_same_request_retry_policy() {
        let (provider,state,task)=server(vec![Reply::Status(529,None),Reply::Json(json!({"stop_reason":"end_turn","content":[{"type":"server_tool_use","name":"web_search","id":"search","input":{"query":"find docs"}},{"type":"text","text":"result"}],"usage":{"input_tokens":10,"output_tokens":2}}))],3).await;
        let observer = std::sync::atomic::AtomicUsize::new(0);
        let result = provider
            .search_limited("anthropic/test", "find docs", 1, &[], 1, |_| {
                observer.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(observer.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(result["answer"], "result");
        let requests = state.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].1, requests[1].1);
        task.abort();
    }

    #[tokio::test]
    async fn terminal_sse_errors_are_classified_without_exposing_provider_text() {
        use tracing::instrument::WithSubscriber;
        let cases = [
            (json!({"type":"error","error":{"code":"invalid_request_error","type":"invalid_request_error","status":400,"message":"private-api-token"}}), "validation", false, Some("invalid_request_error")),
            (json!({"type":"response.failed","response":{"error":{"code":"context_length_exceeded","message":"private-api-token"}}}), "context", false, Some("context_length_exceeded")),
            (json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens","message":"private-api-token"},"error":{"code":"server_error"}}}), "incomplete", false, Some("server_error")),
            (json!({"type":"error","error":{"code":"private-api-token","type":"private-api-token","status":"503 private-api-token","message":"private-api-token","request_id":"private-api-token"}}), "unknown", false, None),
            (json!({"type":"error","error":{"code":{"value":"private-api-token"},"type":["private-api-token"],"status":9999,"message":"private-api-token"}}), "unknown", false, None),
        ];
        for (event, kind, retryable, code) in cases {
            let (provider, state, task) = server(vec![Reply::Sse(format!("data: {event}\n\n"))], 3).await;
            let log = DiagnosticLog::default();
            let error = provider.step("openai/test", "none", "private-system-text", &[json!({"role":"user","content":"private-api-token"})], &[])
                .with_subscriber(capture(&log)).await.unwrap_err();
            let (text, records) = log.records();
            assert_eq!(state.requests.lock().unwrap().len(), 1);
            assert_eq!(records.len(), 1, "{text}");
            let record = &records[0];
            assert_eq!(record["status"], 200);
            assert_eq!(record["last_sse_event"], event["type"]);
            assert_eq!(record["stream_error"]["event"], event["type"]);
            assert_eq!(record["stream_error"]["kind"], kind);
            assert_eq!(record["stream_error"]["retryable"], retryable);
            assert_eq!(record["stream_error"]["code"].as_str(), code);
            assert_eq!(record["request_shape"]["input_items"], 2);
            assert_eq!(record["request_shape"]["tool_definitions"], 0);
            assert!(record["request_shape"]["serialized_bytes"].as_u64().unwrap() > 0);
            assert!(error.to_string().contains("stream failed or was incomplete"));
            assert!(!text.contains("private-api-token"));
            assert!(!text.contains("private-system-text"));
            assert!(!error.to_string().contains("private-api-token"));
            task.abort();
        }
    }

    #[tokio::test]
    async fn known_transient_sse_error_preserves_retry_policy_and_discards_partial_tool() {
        use tracing::instrument::WithSubscriber;
        let partial = json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"shell","call_id":"never-dispatch","arguments":"private-arguments"}});
        let event = json!({"type":"response.failed","response":{"error":{"code":"server_error","type":"server_error","status":503,"message":"private-api-token"}}});
        let sse = format!("data: {partial}\n\ndata: {event}\n\n");
        let (provider, state, task) = server(vec![Reply::Sse(sse), Reply::Sse(completed_sse())], 2).await;
        let log = DiagnosticLog::default();
        let response = provider.step("openai/test", "none", "secret-system", &[], &[])
            .with_subscriber(capture(&log)).await.unwrap();
        assert!(response.calls.is_empty());
        assert_eq!(state.requests.lock().unwrap().len(), 2);
        let (text, records) = log.records();
        assert_eq!(records.len(), 1, "{text}");
        assert_eq!(records[0]["completed_output_items"], 1);
        assert_eq!(records[0]["stream_error"]["kind"], "upstream");
        assert_eq!(records[0]["stream_error"]["code"], "server_error");
        assert_eq!(records[0]["stream_error"]["type"], "server_error");
        assert_eq!(records[0]["stream_error"]["status"], 503);
        assert_eq!(records[0]["stream_error"]["retryable"], true);
        for secret in ["private-arguments", "private-api-token", "secret-system", "never-dispatch"] {
            assert!(!text.contains(secret));
        }
        task.abort();
    }

    #[test]
    fn event_details_reject_unknown_names_values_and_free_text() {
        let unknown = retry::event_details(&json!({"type":"private-api-token","error":{"code":"server_error"}}));
        assert_eq!(unknown.event, "unknown");
        assert!(!unknown.retryable);
        let event = json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"private-api-token"}},"error":{"status":-5}});
        let details = retry::event_details(&event);
        assert_eq!(details.incomplete_reason, None);
        assert_eq!(details.status, None);
        assert_eq!(details.kind, "incomplete");
        let event = json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}});
        assert_eq!(retry::event_details(&event).incomplete_reason, Some("content_filter"));
    }

    #[tokio::test]
    async fn error_placeholders_do_not_hide_transient_failures_or_release_partial_tools() {
        use tracing::instrument::WithSubscriber;
        let partial = json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"shell","call_id":"never-dispatch","arguments":"private-arguments"}});
        for (event, source) in [
            (json!({"type":"error","code":"server_error","error":null,"message":"private-provider-text"}), "event"),
            (json!({"type":"response.failed","error":null,"response":{"error":{"code":"server_error"}}}), "response.error"),
            (json!({"type":"error","code":"server_error","error":"private-provider-text"}), "event"),
        ] {
            let (provider, state, task) = server(vec![
                Reply::Sse(format!("data: {partial}\n\ndata: {event}\n\n")),
                Reply::Sse(completed_sse()),
            ], 2).await;
            let log = DiagnosticLog::default();
            let response = provider.step("openai/test", "none", "", &[], &[])
                .with_subscriber(capture(&log)).await.unwrap();
            assert!(response.calls.is_empty());
            assert!(response.native.is_empty());
            let requests = state.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].1, requests[1].1);
            let (text, records) = log.records();
            assert_eq!(records.len(), 1, "{text}");
            assert_eq!(records[0]["stream_error"]["code"], "server_error");
            assert_eq!(records[0]["stream_error"]["code_state"], "recognized");
            assert_eq!(records[0]["stream_error"]["error_source"], source);
            assert_eq!(records[0]["stream_error"]["retryable"], true);
            assert!(!text.contains("private-provider-text"));
            assert!(!text.contains("private-arguments"));
            task.abort();
        }
    }

    #[tokio::test]
    async fn terminal_error_field_states_remain_distinct_private_and_non_retryable() {
        use tracing::instrument::WithSubscriber;
        for (event, source, code_state, type_state) in [
            (json!({"type":"error","message":"private-provider-text"}), "event", "missing", "not_applicable"),
            (json!({"type":"error","code":null}), "event", "null", "not_applicable"),
            (json!({"type":"error","error":{"code":"private-provider-text","type":"private-provider-text"}}), "error", "unrecognized", "unrecognized"),
            (json!({"type":"error","error":{"code":{},"type":17}}), "error", "invalid_type", "invalid_type"),
            // Do not fall through or combine a real primary error with a
            // conflicting transient code/status in another location.
            (json!({"type":"error","code":"server_error","status":503,"error":{"code":"invalid_request_error","type":null},"response":{"error":{"code":"server_error"}}}), "error", "recognized", "null"),
            (json!({"type":"error","code":"server_error","error":{}}), "error", "missing", "missing"),
            (json!({"type":"response.failed","error":null,"response":{"error":null}}), "none", "missing", "missing"),
        ] {
            let (provider, state, task) = server(vec![Reply::Sse(format!("data: {event}\n\n"))], 3).await;
            let log = DiagnosticLog::default();
            let error = provider.step("openai/test", "none", "", &[], &[])
                .with_subscriber(capture(&log)).await.unwrap_err();
            assert_eq!(state.requests.lock().unwrap().len(), 1);
            let (text, records) = log.records();
            assert_eq!(records.len(), 1, "{text}");
            let details = &records[0]["stream_error"];
            assert_eq!(details["error_source"], source);
            assert_eq!(details["code_state"], code_state);
            assert_eq!(details["type_state"], type_state);
            assert_eq!(details["retryable"], false);
            assert!(error.to_string().contains("category="));
            assert!(error.to_string().contains(if code_state == "recognized" {
                "code=invalid_request_error"
            } else {
                code_state
            }));
            assert!(!text.contains("private-provider-text"));
            assert!(!error.to_string().contains("private-provider-text"));
            task.abort();
        }
    }

    #[test]
    fn retry_after_and_transient_classification_preserve_bounded_policy() {
        let policy = retry::RequestRetries {
            max_attempts: 4,
            backoff: Duration::from_secs(2),
        };
        for status in [408, 429, 500, 502, 503, 504, 529] {
            let error = HttpFailure {
                status: reqwest::StatusCode::from_u16(status).unwrap(),
                provider: "openai",
                retry_after: None,
            }
            .into();
            assert_eq!(policy.delay(&error, 1), Some(Duration::from_secs(2)));
            assert_eq!(policy.delay(&error, 2), Some(Duration::from_secs(4)));
            assert_eq!(policy.delay(&error, 3), Some(Duration::from_secs(6)));
            assert_eq!(policy.delay(&error, 4), None);
        }
        let error = HttpFailure {
            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            provider: "openai",
            retry_after: Some(Duration::from_secs(5)),
        }
        .into();
        assert_eq!(policy.delay(&error, 1), Some(Duration::from_secs(5)));
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "5".parse().unwrap());
        assert_eq!(retry::retry_after(&headers), Some(Duration::from_secs(5)));
        headers.insert(
            "retry-after",
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry::retry_after(&headers), Some(Duration::ZERO));
        headers.insert("retry-after", "invalid".parse().unwrap());
        assert_eq!(retry::retry_after(&headers), None);
        let future = (chrono::Utc::now() + chrono::Duration::seconds(30))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        headers.insert("retry-after", future.parse().unwrap());
        let delay = retry::retry_after(&headers).unwrap();
        assert!((28..=30).contains(&delay.as_secs()));
        assert!(retry::transient_event(
            &json!({"type":"error","code":"server_error","message":"private-provider-error"})
        ));
        assert!(retry::transient_event(
            &json!({"type":"response.failed","response":{"error":{"code":"server_error"}}})
        ));
        assert!(!retry::transient_event(
            &json!({"type":"error","code":"invalid_request_error","message":"HTTP 503 forged message"})
        ));
        assert!(
            policy
                .delay(&anyhow::anyhow!("HTTP 503 private forged error"), 1)
                .is_none()
        );
    }
}

#[test]
fn private_capture_selection_matches_classification_without_changing_retries() {
    use super::retry::*;
    for (event, source, code) in [
        (json!({"type":"error","error":{"code":"novel","message":"top reason"},"response":{"error":{"code":"ignored"}}}),"error","novel"),
        (json!({"type":"response.failed","error":null,"response":{"error":{"code":"novel","message":"nested reason"}}}),"response.error","novel"),
        (json!({"type":"error","error":false,"code":"novel","message":"flat reason"}),"event","novel"),
        (json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}),"none","")
    ] {
        let plain=event_details(&event); let captured=event_details_captured(&event,true);
        assert_eq!(plain.error_source,source);
        assert_eq!(captured.retryable,plain.retryable);
        assert_eq!(captured.code,plain.code);
        assert_eq!(captured.kind,plain.kind);
        assert!(plain.private_fields.is_none());
        assert!(captured.private_fields.is_some());
        if !code.is_empty() { assert_eq!(plain.code_state,"unrecognized"); }
        assert!(!format!("{captured:?}").contains("top reason"));
        assert!(!format!("{captured:?}").contains("nested reason"));
        assert!(!format!("{captured:?}").contains("flat reason"));
        let error:anyhow::Error=StreamFailure::Terminal(captured).into();
        assert!(RequestRetries::default().delay(&error,1).is_none());
    }
}
