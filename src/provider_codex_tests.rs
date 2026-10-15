use super::*;
use crate::auth::AuthConfig;
#[path = "provider_codex_fixtures.rs"]
mod fixtures;
const SYSTEM: &str = "Synthetic fixture: answer briefly; do not execute tools.";
fn affinity() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
}
fn function() -> Value {
    json!({"name":"fixture_echo","description":"Synthetic inert schema only.","input_schema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}})
}
fn wire(body: &Value, lite: Option<bool>) -> Request {
    Request::prepare(
        body,
        Capabilities {
            use_responses_lite: lite,
            ..Default::default()
        },
        affinity(),
    )
    .unwrap()
}
#[test]
fn pinned_standard_and_lite_fixtures_execute_real_serializer() {
    for (expected, lite) in [
        (fixtures::EMPTY_TOOLS_NATIVE_STANDARD, false),
        (fixtures::EMPTY_TOOLS_NATIVE_LITE, true),
        (fixtures::ONE_FUNCTION_NATIVE_STANDARD, false),
        (fixtures::ONE_FUNCTION_NATIVE_LITE, true),
        (fixtures::FUNCTION_ROUNDTRIP_NATIVE_STANDARD, false),
        (fixtures::FUNCTION_ROUNDTRIP_NATIVE_LITE, true),
    ] {
        let mut expected: Value = serde_json::from_str(expected).unwrap();
        // Native fixtures include optional low verbosity and empty telemetry, which Pantheon
        // does not request. Compare the complete supported request projection (not a wire probe).
        expected.as_object_mut().unwrap().remove("text");
        expected.as_object_mut().unwrap().remove("client_metadata");
        let prefix = if lite && expected["input"][0]["type"] == "additional_tools" {
            2
        } else {
            1
        };
        let history = expected["input"].as_array().unwrap()[prefix..].to_vec();
        let tools = if (lite && prefix == 2)
            || (!lite && expected["tools"].as_array().unwrap().len() == 1)
        {
            vec![function()]
        } else {
            vec![]
        };
        let body =
            Provider::request_body("codex/gpt-6.1-sol", "high", SYSTEM, &history, &tools).unwrap();
        let request = wire(&body, Some(lite));
        let encoded = serde_json::to_vec(&request).unwrap();
        assert!(encoded.starts_with(b"{\"model\":\"gpt-6.1-sol\",\"stream\":true,\"input\":"));
        assert_eq!(serde_json::from_slice::<Value>(&encoded).unwrap(), expected);
        let http = request
            .apply_mode_header(reqwest::Client::new().post("http://localhost.invalid/"))
            .json(&request)
            .build()
            .unwrap();
        assert_eq!(http.body().unwrap().as_bytes().unwrap(), encoded);
        assert_eq!(
            http.headers().get(LITE_HEADER).map(|h| h.to_str().unwrap()),
            lite.then_some("true")
        );
    }
}
#[test]
fn empty_instructions_tools_multiple_tools_and_stable_scoped_prefixes() {
    let empty = Provider::request_body("codex/unknown", "none", "", &[], &[]).unwrap();
    let request = wire(&empty, Some(true));
    assert!(request.input.is_empty());
    assert!(request.tools.is_none());
    assert!(request.reasoning.is_none());
    let mut second = function();
    second["name"] = json!("other");
    let body =
        Provider::request_body("codex/unknown", "low", SYSTEM, &[], &[function(), second]).unwrap();
    let request = wire(&body, Some(true));
    assert_eq!(
        request.input[0]["tools"][0]["tools"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(request.input[1]["role"], "developer");
    assert_eq!(
        wire(&body, Some(true)).value().unwrap(),
        request.value().unwrap()
    );
    let independent = Request::prepare(
        &body,
        Capabilities {
            use_responses_lite: Some(true),
            ..Default::default()
        },
        Uuid::from_u128(2),
    )
    .unwrap();
    assert_ne!(independent.input[0]["id"], request.input[0]["id"]);
    assert_ne!(independent.input[1]["id"], request.input[1]["id"]);
    let changed = Provider::request_body(
        "codex/unknown",
        "low",
        "new instructions",
        &[],
        &[function()],
    )
    .unwrap();
    assert_ne!(
        wire(&changed, Some(true)).input[1]["id"],
        request.input[1]["id"]
    );
}
#[test]
fn missing_false_and_malformed_metadata_are_standard_without_summary_inference() {
    let body = Provider::request_body("codex/gpt-6.1-sol", "low", "sys", &[], &[]).unwrap();
    for metadata in [
        json!({}),
        json!({"use_responses_lite":false}),
        json!({"use_responses_lite":"true"}),
        json!({"use_responses_lite":null}),
    ] {
        let request =
            Request::prepare(&body, Capabilities::from_metadata(&metadata), affinity()).unwrap();
        assert!(!request.lite);
        assert!(request.tools.is_some());
        assert!(
            request.value().unwrap()["reasoning"]
                .get("context")
                .is_none()
        );
        assert_eq!(request.value().unwrap()["reasoning"]["summary"], "auto");
    }
    let request = Request::prepare(
        &body,
        Capabilities::from_metadata(
            &json!({"use_responses_lite":true,"supports_reasoning_summary_parameter":false}),
        ),
        affinity(),
    )
    .unwrap();
    assert!(
        request.value().unwrap()["reasoning"]
            .get("summary")
            .is_none()
    );
    assert_eq!(
        request.value().unwrap()["reasoning"]["context"],
        "all_turns"
    );
}
#[test]
fn mixed_hosted_tools_keep_native_coalescing_and_search_contract() {
    let base = Provider::request_body("codex/unknown", "low", "sys", &[], &[function()]).unwrap();
    let mut body = base.clone();
    let mut f2 = base["tools"][0].clone();
    f2["name"] = json!("other");
    body["tools"] = json!([{"type":"web_search","filters":{"allowed_domains":["example.org"]}}, base["tools"][0], {"type":"namespace","name":"functions","description":"explicit","tools":[f2]}, {"type":"other_builtin"}]);
    body["tool_choice"] = json!({"type":"web_search"});
    let request = wire(&body, Some(true));
    let tools = request.input[0]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0], body["tools"][0]);
    assert_eq!(tools[1]["name"], "functions");
    assert_eq!(tools[1]["description"], "explicit");
    assert_eq!(tools[1]["tools"].as_array().unwrap().len(), 2);
    assert_eq!(tools[2], body["tools"][3]);
    assert_eq!(request.tool_choice, body["tool_choice"]);
    assert!(request.tools.is_none());
}
#[test]
fn namespace_dispatch_and_continuation_preserve_native_call_ids_images_and_reasoning() {
    for namespace in [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!("functions")),
    ] {
        let mut call = json!({"type":"function_call","id":"fc_server","call_id":"call_server","name":"pantheon_web_search","arguments":"{}","encrypted_content":"opaque-call"});
        if let Some(namespace) = namespace {
            call["namespace"] = namespace;
        }
        let value = json!({"status":"completed","output":[{"type":"reasoning","id":"rs_server","encrypted_content":"opaque-reasoning","summary":[]}, call],"usage":{}});
        let response = parse(value.clone()).unwrap();
        assert_eq!(response.calls[0].name, "web_search");
        assert_eq!(response.calls[0].id, "call_server");
        assert_eq!(response.native, value["output"].as_array().unwrap().clone());
        let mut history = vec![];
        Provider::append_response("openai", &mut history, &response);
        Provider::append_result_with_image(
            "openai",
            &mut history,
            &response.calls[0],
            "done",
            false,
            Some("iVBORw0KGgo="),
        );
        let user = json!({"role":"user","content":[{"type":"input_text","text":"continue","prompt_cache_breakpoint":{"mode":"explicit"}},{"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo="} ]});
        history.push(user);
        let original = history.clone();
        let body = Provider::request_body("codex/unknown", "high", "sys", &history, &[function()])
            .unwrap();
        let request = wire(&body, Some(true));
        assert_eq!(
            history, original,
            "wire projection must not modify saved history"
        );
        assert_eq!(request.input[2], original[0]);
        assert_eq!(request.input[3], original[1]);
        assert_eq!(request.input[4], original[2]);
        assert_eq!(request.input[4]["call_id"], "call_server");
        assert_eq!(request.input[5]["type"], "message");
        assert!(
            request.input[5]["content"][0]
                .get("prompt_cache_breakpoint")
                .is_none()
        );
        assert_eq!(request.input[5]["content"][1], original[3]["content"][1]);
    }
    for namespace in [json!("other"), json!({"name":"functions"}), json!(true)] {
        let value = json!({"status":"completed","output":[{"type":"function_call","call_id":"c","name":"date","namespace":namespace,"arguments":"{}"}]});
        assert!(
            parse(value)
                .unwrap_err()
                .to_string()
                .contains("unsupported function namespace")
        );
    }
}
#[test]
fn public_openai_and_glm_do_not_inherit_subscription_projection() {
    let tools = [function()];
    let h = [Provider::user("openai", "fixture")];
    let public = Provider::request_body("openai/gpt-6.1-sol", "high", "sys", &h, &tools).unwrap();
    assert_eq!(public["input"][0], json!({"role":"system","content":"sys"}));
    assert_eq!(public["tools"][0]["name"], "fixture_echo");
    assert_eq!(public["max_output_tokens"], 16384);
    assert_eq!(public["reasoning"]["context"], "all_turns");
    assert!(public.get("tool_choice").is_none());
    assert!(public.get("stream").is_none());
    let glm_h = [Provider::user("zai", "fixture")];
    let glm = Provider::request_body("zai/glm-5.3", "high", "sys", &glm_h, &tools).unwrap();
    assert_eq!(
        glm,
        crate::provider::zai::request_body("glm-5.3", "high", "sys", &glm_h, &tools).unwrap()
    );
    assert!(glm.get("input").is_none());
    assert!(glm.get("additional_tools").is_none());
}
#[test]
fn metadata_is_configured_exact_model_not_name_heuristics_and_catalog_survives_serialization() {
    let home = tempfile::tempdir().unwrap();
    let auth = AuthConfig {
        codex_home: Some(home.path().into()),
        codex_cli: None,
    };
    std::fs::write(home.path().join("models_cache.json"), json!({"models":[{"slug":"arbitrary-new-model","use_responses_lite":true},{"slug":"gpt-6.1-sol","use_responses_lite":false}]}).to_string()).unwrap();
    let provider = Provider::mock("http://localhost.invalid/".into()).with_auth(auth);
    assert_eq!(
        provider
            .codex_capabilities("codex/arbitrary-new-model")
            .use_responses_lite,
        Some(true)
    );
    assert_eq!(
        provider
            .codex_capabilities("codex/gpt-6.1-sol")
            .use_responses_lite,
        Some(false)
    );
    assert_eq!(
        provider
            .codex_capabilities("codex/arbitrary-new")
            .use_responses_lite,
        None
    );
    let mut model = crate::models::Model::plain("arbitrary-new-model");
    model.codex = Some(Capabilities {
        use_responses_lite: Some(false),
        ..Default::default()
    });
    let model: crate::models::Model =
        serde_json::from_value(serde_json::to_value(model).unwrap()).unwrap();
    let catalog = std::sync::Arc::new(crate::models::Catalog::default());
    catalog.replace("codex", vec![model]);
    let provider = provider.with_catalog(catalog);
    assert_eq!(
        provider
            .codex_capabilities("codex/arbitrary-new-model")
            .use_responses_lite,
        Some(false),
        "discovered catalog is authoritative over CLI fallback"
    );
    std::fs::write(home.path().join("models_cache.json"), "malformed").unwrap();
    assert_eq!(
        provider
            .codex_capabilities("codex/unknown")
            .use_responses_lite,
        None
    );
}
#[tokio::test]
async fn provider_http_path_pairs_metadata_mode_header_body_and_namespace_roundtrip() {
    use axum::{Router, body::Bytes, extract::State, http::HeaderMap, routing::post};
    type Requests = std::sync::Arc<std::sync::Mutex<Vec<(HeaderMap, Vec<u8>)>>>;
    let requests: Requests = Default::default();
    let app = Router::new().route("/", post(|State(requests): State<Requests>, headers: HeaderMap, bytes: Bytes| async move {
        requests.lock().unwrap().push((headers, bytes.to_vec()));
        // Output-item.done fallback is intentionally exercised, as is the separate namespace field.
        let call = json!({"type":"function_call","id":"fc_synthetic","call_id":"call_synthetic","namespace":"functions","name":"date","arguments":"{\"id\":1}"});
        format!("data: {}\n\ndata: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_synthetic","name":"date","namespace":"functions","arguments":""}}),
            json!({"type":"response.output_item.done","output_index":0,"item":call}),
            json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":{}}}))
    })).with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("auth.json"), json!({"tokens":{"access_token":"synthetic-test-token","account_id":"synthetic-test-account"}}).to_string()).unwrap();
    std::fs::write(
        home.path().join("models_cache.json"),
        json!({"models":[{"slug":"new-lite-model","use_responses_lite":true}]}).to_string(),
    )
    .unwrap();
    let provider = Provider::mock(endpoint)
        .with_auth(AuthConfig {
            codex_home: Some(home.path().into()),
            codex_cli: None,
        })
        .with_cache_affinity(affinity());
    let tools = [
        json!({"name":"date","description":"timestamp lookup","input_schema":{"type":"object","properties":{"id":{"type":"integer"}}}}),
    ];
    let history = Provider::start("openai", "<chat>synthetic history</chat>", "go");
    let response = provider
        .step("codex/new-lite-model", "low", "sys", &history, &tools)
        .await
        .unwrap();
    assert_eq!(response.calls[0].name, "date");
    assert_eq!(response.calls[0].id, "call_synthetic");
    let mut continuation = history.clone();
    Provider::append_response("openai", &mut continuation, &response);
    Provider::append_result(
        "openai",
        &mut continuation,
        &response.calls[0],
        "done",
        false,
    );
    provider
        .step("codex/new-lite-model", "low", "sys", &continuation, &tools)
        .await
        .unwrap();
    // Unknown capability remains standard despite the same cached model and provider.
    provider
        .step("codex/not-advertised", "low", "sys", &history, &tools)
        .await
        .unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for (headers, bytes) in &requests[..2] {
        assert_eq!(headers[LITE_HEADER], "true");
        assert!(bytes.starts_with(b"{\"model\":\"new-lite-model\",\"stream\":true,\"input\":"));
        let body: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(
            headers["session-id"],
            body["prompt_cache_key"].as_str().unwrap()
        );
        assert!(body.get("tools").is_none());
        assert!(body.get("instructions").is_none());
        assert_eq!(body["input"][0]["tools"][0]["name"], "functions");
        assert_eq!(body["reasoning"]["context"], "all_turns");
        assert_eq!(body["parallel_tool_calls"], false);
    }
    let second: Value = serde_json::from_slice(&requests[1].1).unwrap();
    assert_eq!(second["input"][3], response.native[0]);
    assert_eq!(second["input"][4]["call_id"], "call_synthetic");
    assert!(!requests[2].0.contains_key(LITE_HEADER));
    let standard: Value = serde_json::from_slice(&requests[2].1).unwrap();
    assert!(standard["tools"].is_array());
    assert!(standard["reasoning"].get("context").is_none());
    assert_eq!(standard["parallel_tool_calls"], true);
    server.abort();
}
#[test]
fn unknown_namespaces_are_rejected_after_done_item_fallback_and_final_output() {
    for final_output in [false, true] {
        let call = json!({"type":"function_call","id":"fc_synthetic","call_id":"call_synthetic","namespace":"foreign","name":"date","arguments":"{}"});
        let completed_output = if final_output {
            json!([call.clone()])
        } else {
            json!([])
        };
        let events = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_item.added","output_index":0,"item":call}),
            json!({"type":"response.output_item.done","output_index":0,"item":call}),
            json!({"type":"response.completed","response":{"status":"completed","output":completed_output,"usage":{}}})
        );
        let value = crate::provider::SseDecoder::default()
            .feed(events.as_bytes())
            .unwrap()
            .unwrap();
        assert!(parse(value).is_err());
    }
}

#[test]
fn anthropic_callable_alias_cannot_bypass_codex_namespace_checks() {
    for namespace in [None, Some(json!("functions")), Some(json!("foreign"))] {
        let mut call = json!({"type":"tool_use","id":"call_alias","call_id":"call_alias","name":"date","arguments":"{}","input":{}});
        if let Some(namespace) = namespace {
            call["namespace"] = namespace;
        }
        let value = json!({"status":"completed","output":[call.clone()]});
        assert_eq!(
            Provider::parse("openai", value.clone())
                .unwrap()
                .calls
                .len(),
            1,
            "prove shared-parser bypass fixture"
        );
        assert!(
            parse(value)
                .unwrap_err()
                .to_string()
                .contains("unsupported callable item type")
        );
        for final_output in [false, true] {
            let output = if final_output {
                json!([call.clone()])
            } else {
                json!([])
            };
            let events = format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"type":"response.output_item.done","output_index":0,"item":call}),
                json!({"type":"response.completed","response":{"status":"completed","output":output}})
            );
            let decoded = crate::provider::SseDecoder::default()
                .feed(events.as_bytes())
                .unwrap()
                .unwrap();
            assert!(parse(decoded).is_err());
        }
    }
}
