use super::*;
#[test]
fn config_and_models_are_explicit() {
    assert!(!Config::default().enabled);
    assert!(
        Config {
            max_in_flight: Some(0),
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        Config {
            max_output_tokens: 0,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    assert!(validate_model("glm-4.7").is_err());
    assert!(validate_model("glm-5.3-flashx").is_err());
    assert!(validate_effort("none").is_err());
    assert!(validate_effort("medium").is_err());
    assert!(!supports_images("zai/glm-5.3"));
    assert!(supports_images("zai/glm-5.3-flash"));
}
#[test]
fn request_and_multi_tool_transcript_keep_native_reasoning_private() {
    let value = json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","content":"Checking","reasoning_content":"private\nunchanged","tool_calls":[{"id":"a","type":"function","function":{"name":"read","arguments":"{\"path\":\"one\"}"}},{"id":"b","type":"function","function":{"name":"read","arguments":"{\"path\":\"two\"}"}}]}}],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":30}}});
    let response = parse(value).unwrap();
    assert_eq!(response.texts, [("Checking".into(), false)]);
    assert_eq!(crate::cache::Usage::parse(&response.usage).cached, Some(30));
    let mut history = super::super::Provider::start("zai", "view", "prompt");
    super::super::Provider::append_response("zai", &mut history, &response);
    assert!(request_body("glm-5.3", "low", "system", &history, &[]).is_err());
    for call in &response.calls {
        super::super::Provider::append_result("zai", &mut history, call, "ok", false);
    }
    let body = request_body(
        "glm-5.3",
        "high",
        "system",
        &history,
        &[json!({"name":"read","description":"Read","input_schema":{"type":"object"}})],
    )
    .unwrap();
    assert_eq!(
        body["messages"][2]["reasoning_content"],
        "private\nunchanged"
    );
    assert_eq!(body["messages"][3]["tool_call_id"], "a");
    assert_eq!(body["messages"][4]["tool_call_id"], "b");
    assert_eq!(body["tools"][0]["function"]["name"], "read");
    assert!(body.get("parallel_tool_calls").is_none());
    assert_eq!(body["thinking"]["clear_thinking"], false);
}
fn event(delta: Value, finish: Value) -> String {
    format!(
        "data: {}\r\n\r\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
}
#[test]
fn fragmented_sse_and_parallel_calls_reassemble_at_done_only() {
    let mut wire = event(
        json!({"role":"assistant","reasoning_content":"think 🧠","content":"hello","tool_calls":[{"index":1,"id":"b","type":"function","function":{"name":"read","arguments":"{\"path\":"}},{"index":0,"id":"a","function":{"name":"read","arguments":"{"}}]}),
        Value::Null,
    );
    wire += &event(
        json!({"reasoning_content":"\nmore","tool_calls":[{"index":0,"function":{"arguments":"\"path\":\"one\"}"}},{"index":1,"function":{"arguments":"\"two\"}"}}]}),
        json!("tool_calls"),
    );
    wire += "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":7}}\r\n\r\ndata: [DONE]\r\n\r\n";
    let mut decoder = Decoder::default();
    let mut result = None;
    for byte in wire.as_bytes() {
        if let Some(v) = decoder.feed(&[*byte]).unwrap() {
            result = Some(v);
        }
    }
    let response = parse(result.unwrap()).unwrap();
    assert_eq!(
        response.calls.iter().map(|c| &c.id).collect::<Vec<_>>(),
        [&"a".to_owned(), &"b".to_owned()]
    );
    assert_eq!(response.calls[1].arguments, json!({"path":"two"}));
    assert_eq!(response.native[0]["reasoning_content"], "think 🧠\nmore");
    assert_eq!(response.texts, [("hello".into(), false)]);
    assert_eq!(response.usage["input_tokens"], 5);
}
#[test]
fn incomplete_and_malformed_streams_fail_closed() {
    for (delta, finish) in [
        (json!({"content":"x"}), json!("length")),
        (json!({"content":"x"}), Value::Null),
        (
            json!({"tool_calls":[{"index":0,"function":{"name":"read","arguments":"{}"}}]}),
            json!("tool_calls"),
        ),
        (
            json!({"tool_calls":[{"index":1,"id":"a","function":{"name":"read","arguments":"{}"}}]}),
            json!("tool_calls"),
        ),
        (
            json!({"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":"{"}}]}),
            json!("tool_calls"),
        ),
        (
            json!({"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":"[]"}}]}),
            json!("tool_calls"),
        ),
        (
            json!({"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":"{}"}}]}),
            json!("stop"),
        ),
        (json!({"content":"x"}), json!("tool_calls")),
        (json!({"content":"x","refusal":"no"}), json!("stop")),
    ] {
        let mut decoder = Decoder::default();
        let mut wire = event(delta, finish);
        wire += "data: [DONE]\n\n";
        assert!(decoder.feed(wire.as_bytes()).is_err());
    }
    assert!(Decoder::default().feed(b"data: garbage\n\n").is_err());
    let mut decoder = Decoder::default();
    assert!(
        decoder
            .feed(event(json!({"content":"partial"}), json!("stop")).as_bytes())
            .unwrap()
            .is_none()
    );
    // A terminal reason without DONE never releases tools.
    assert_eq!(decoder.finish.as_deref(), Some("stop"));
}
#[test]
fn errors_and_conflicting_ids_are_private() {
    let mut decoder = Decoder::default();
    let err=decoder.feed(b"data: {\"error\":{\"code\":\"invalid_request_error\",\"message\":\"secret-key-and-prompt\"}}\n\n").unwrap_err();
    assert!(!format!("{err:#}").contains("secret-key"));
    let mut decoder = Decoder::default();
    decoder
        .feed(event(json!({"tool_calls":[{"index":0,"id":"a"}]}), Value::Null).as_bytes())
        .unwrap();
    assert!(
        decoder
            .feed(event(json!({"tool_calls":[{"index":0,"id":"b"}]}), Value::Null).as_bytes())
            .is_err()
    );
}
#[test]
fn provider_switch_and_visual_inputs_require_compatible_fresh_history() {
    let legacy = super::super::Provider::start("openai", "view", "prompt");
    assert!(request_body("glm-5.3", "low", "sys", &legacy, &[]).is_err());
    assert!(
        request_body(
            "glm-5.3",
            "low",
            "sys",
            &[json!({"type":"reasoning","encrypted_content":"opaque"})],
            &[]
        )
        .is_err()
    );
    let mut user = super::super::Provider::user("zai", "image");
    user["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"image_url","image_url":{"url":"data:image/png;base64,eA=="}}));
    assert!(request_body("glm-5.3", "low", "sys", &[user.clone()], &[]).is_err());
    assert!(request_body("glm-5.3-flash", "low", "sys", &[user], &[]).is_ok());
    let bad = json!({"role":"tool","tool_call_id":"ghost","content":"ok"});
    assert!(request_body("glm-5.3", "low", "sys", &[bad], &[]).is_err());
}
#[test]
fn invalid_nonstream_call_batches_and_mcp_media_are_safe() {
    for calls in [
        json!([{"id":"a","type":"function","function":{"name":"read","arguments":"{}"}},{"id":"a","type":"function","function":{"name":"read","arguments":"{}"}}]),
        json!([{"id":"","type":"function","function":{"name":"read","arguments":"{}"}}]),
        json!([{"id":"a","type":"function","function":{"name":"","arguments":"{}"}}]),
    ] {
        assert!(parse(json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":calls}}]})).is_err());
    }
    let mut media = json!({"isError":false,"content":[{"type":"image","data":"base64-original","mimeType":"image/png"}]});
    annotate_mcp(&mut media);
    assert_eq!(media["content"][0]["data"], "base64-original");
    assert_eq!(media["isError"], false);
    assert!(
        media["_pantheon_media_notice"]
            .as_str()
            .unwrap()
            .contains("NOT visual")
    );
    let mut array = json!(["data"]);
    annotate_mcp(&mut array);
    assert_eq!(array["result"], json!(["data"]));
}
async fn server(response: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    use axum::{Router, http::header, routing::post};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/",
        post(move || async move { ([(header::CONTENT_TYPE, "text/event-stream")], response) }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}
#[tokio::test]
async fn real_transport_sse_is_complete_and_incomplete_fails_before_calls() {
    let (endpoint,task)=server("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\",\"reasoning_content\":\"private\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").await;
    let provider = super::super::Provider::mock(endpoint);
    let response = provider
        .step(
            "zai/glm-5.3",
            "low",
            "sys",
            &super::super::Provider::start("zai", "view", "prompt"),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(response.texts, [("ok".into(), false)]);
    task.abort();
    for wire in [
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"write\",\"arguments\":\"{\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
    ] {
        let (endpoint, task) = server(wire).await;
        assert!(
            super::super::Provider::mock(endpoint)
                .step(
                    "zai/glm-5.3",
                    "low",
                    "sys",
                    &super::super::Provider::start("zai", "view", "prompt"),
                    &[]
                )
                .await
                .is_err()
        );
        task.abort();
    }
}
#[tokio::test]
async fn shared_admission_and_cancel_release_permit() {
    use axum::{Router, http::header, routing::post};
    use std::sync::Arc;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (sender, mut received) = tokio::sync::mpsc::channel(10);
    let release = Arc::new(tokio::sync::Notify::new());
    let handler_release = release.clone();
    let app=Router::new().route("/",post(move || {
        let sender=sender.clone();let release=handler_release.clone();
        async move {
            sender.send(()).await.unwrap();
            release.notified().await;
            ([(header::CONTENT_TYPE,"text/event-stream")],"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
        }
    }));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = super::super::Provider::mock(url)
        .with_zai(Config {
            enabled: true,
            max_in_flight: Some(1),
            max_output_tokens: 100,
        })
        .unwrap();
    let first = provider.clone();
    let second = provider.clone();
    assert!(Arc::ptr_eq(
        first.zai_slots.as_ref().unwrap(),
        second.zai_slots.as_ref().unwrap()
    ));
    let call = |provider: super::super::Provider| {
        tokio::spawn(async move {
            provider
                .step(
                    "zai/glm-5.3",
                    "low",
                    "sys",
                    &super::super::Provider::start("zai", "v", "p"),
                    &[],
                )
                .await
        })
    };
    let active = call(first);
    received.recv().await.unwrap();
    let waiting = call(second);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), received.recv())
            .await
            .is_err()
    );
    waiting.abort();
    let _ = waiting.await;
    active.abort();
    let _ = active.await;
    assert_eq!(provider.zai_slots.as_ref().unwrap().available_permits(), 1);
    let resumed = call(provider.clone());
    received.recv().await.unwrap();
    release.notify_waiters();
    assert!(resumed.await.unwrap().is_ok());
    assert_eq!(provider.zai_slots.as_ref().unwrap().available_permits(), 1);
    task.abort();
}
#[tokio::test]
async fn disabled_provider_and_unsupported_search_do_not_contact_transport() {
    let provider = super::super::Provider::mock("http://127.0.0.1:1".into())
        .with_zai(Config::default())
        .unwrap();
    let error = provider
        .step("zai/glm-5.3", "low", "s", &[], &[])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("disabled"));
    let error = provider
        .search("zai/glm-5.3", "test", 1, &[])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("web.search_model"));
    assert!(
        crate::web::WebConfig {
            search_model: Some("zai/glm-5.3".into()),
            ..Default::default()
        }
        .validate()
        .is_err()
    );
}
#[tokio::test]
async fn http_errors_are_redacted_and_retry_is_bounded() {
    use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let count = Arc::new(AtomicUsize::new(0));
        let handler_count = count.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/",
            post(move || {
                let count = handler_count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    (status, "private-credential-prompt-response").into_response()
                }
            }),
        );
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = super::super::Provider::mock(url)
            .with_zai(Config {
                enabled: true,
                max_in_flight: Some(2),
                ..Default::default()
            })
            .unwrap()
            .with_request_retries(2, 1)
            .unwrap();
        let error = provider
            .step("zai/glm-5.3", "low", "private-request", &[], &[])
            .await
            .unwrap_err();
        assert!(!format!("{error:#}").contains("private"));
        assert!(error.to_string().contains("zai returned HTTP"));
        assert_eq!(
            count.load(Ordering::SeqCst),
            if status == StatusCode::UNAUTHORIZED {
                1
            } else {
                2
            }
        );
        assert_eq!(provider.zai_slots.as_ref().unwrap().available_permits(), 2);
        task.abort();
    }
}

#[test]
fn concurrency_config_normalizes_unlimited_and_roundtrips() {
    for (setting, expected) in [
        ("", None),
        ("max_in_flight = \"none\"", None),
        ("max_in_flight = 0", None),
        ("max_in_flight = 1", Some(1)),
        ("max_in_flight = 32", Some(32)),
    ] {
        let config: Config = toml::from_str(&format!("enabled = true\n{setting}")).unwrap();
        config.validate().unwrap();
        assert!(config.enabled);
        assert_eq!(config.max_in_flight, expected);
        assert_eq!(config.max_output_tokens, 131072);
        let serialized = toml::to_string(&config).unwrap();
        let roundtrip: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(roundtrip.max_in_flight, expected);
        assert_eq!(roundtrip.max_output_tokens, 131072);
        assert!(roundtrip.enabled);
        assert_eq!(serialized.contains("max_in_flight"), expected.is_some());
    }
    // Serde callers can also express an actual None/null, without a string sentinel.
    assert!(
        serde_json::from_value::<Config>(json!({"max_in_flight":null}))
            .unwrap()
            .max_in_flight
            .is_none()
    );
    for value in [
        "-1",
        "33",
        "true",
        "1.5",
        "[]",
        "{}",
        "\"unlimited\"",
        "\"2\"",
        "\"None\"",
        "none",
    ] {
        let parsed = toml::from_str::<Config>(&format!("max_in_flight = {value}"));
        assert!(
            parsed.is_err() || parsed.unwrap().validate().is_err(),
            "{value}"
        );
    }
    for value in [0, 131073] {
        assert!(
            Config {
                max_output_tokens: value,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
    for value in [1, 131072] {
        Config {
            max_output_tokens: value,
            ..Default::default()
        }
        .validate()
        .unwrap();
    }
}

#[tokio::test]
async fn unlimited_default_has_no_semaphore_and_admits_more_than_old_cap() {
    use axum::{Router, http::header, routing::post};
    use std::sync::Arc;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (sender, mut received) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let handler_release = release.clone();
    let app = Router::new().route("/", post(move || {
        let sender = sender.clone();
        let release = handler_release.clone();
        async move {
            sender.send(()).unwrap();
            release.acquire().await.unwrap().forget();
            ([(header::CONTENT_TYPE, "text/event-stream")], "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    assert!(super::super::Provider::new(1).unwrap().zai_slots.is_none());
    for setting in ["", "max_in_flight = \"none\"", "max_in_flight = 0"] {
        let config: Config = toml::from_str(&format!("enabled = true\n{setting}")).unwrap();
        let provider = super::super::Provider::mock(url.clone())
            .with_zai(config)
            .unwrap();
        assert!(provider.zai_slots.is_none());
        let mut calls = Vec::new();
        for _ in 0..40 {
            let provider = provider.clone();
            calls.push(tokio::spawn(async move {
                provider.step("zai/glm-5.3", "low", "sys", &[], &[]).await
            }));
        }
        // Every request must reach the fixture while no response has been released.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            for _ in 0..40 {
                received.recv().await.unwrap();
            }
        })
        .await
        .expect("unlimited calls were incorrectly queued by the harness");
        release.add_permits(40);
        for call in calls {
            assert!(call.await.unwrap().is_ok());
        }
    }
    server.abort();
}

#[tokio::test]
async fn minimal_opt_in_lists_models_and_sends_default_or_custom_output_limit() {
    use axum::{Json, Router, http::header, routing::post};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("pantheon.toml");
    let main_before = crate::config::Config::default().agent.model;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (sender, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route("/", post(move |Json(body): Json<Value>| {
        let sender = sender.clone();
        async move {
            sender.send(body).unwrap();
            ([(header::CONTENT_TYPE, "text/event-stream")], "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for (setting, expected) in [("", 131072), ("max_output_tokens = 777", 777)] {
        // Existing Discord authorization remains required; the only new opt-in is enabled.
        std::fs::write(
            &path,
            format!(
                "[discord]\napplication_id=1\nallowed_users=[2]\n[zai]\nenabled=true\n{setting}\n"
            ),
        )
        .unwrap();
        let config = crate::config::Config::load(&path).unwrap();
        assert_eq!(config.agent.model, main_before);
        assert!(config.zai.max_in_flight.is_none());
        let provider = super::super::Provider::mock(url.clone())
            .with_zai(config.zai.clone())
            .unwrap();
        let models = provider.list_models("zai").await.unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            MODELS
        );
        for model in ["zai/glm-5.3", "zai/glm-5.3-flash"] {
            assert!(provider.step(model, "low", "sys", &[], &[]).await.is_ok());
            let wire = received.recv().await.unwrap();
            assert_eq!(wire["max_tokens"], expected);
            assert_eq!(wire["model"], model.strip_prefix("zai/").unwrap());
        }
        let serialized = toml::to_string(&config.zai).unwrap();
        assert_eq!(
            toml::from_str::<Config>(&serialized)
                .unwrap()
                .max_output_tokens,
            expected
        );
    }
    assert_eq!(
        request_body("glm-5.3", "low", "sys", &[], &[]).unwrap()["max_tokens"],
        131072
    );
    server.abort();
}
