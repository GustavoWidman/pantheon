use super::*;
use crate::mcp::{Mcp, McpConfig, ServerConfig};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::HeaderMap,
    response::IntoResponse,
    routing::post,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Wire {
    replies: Mutex<VecDeque<String>>,
    requests: Mutex<Vec<(Vec<u8>, HeaderMap)>>,
}
async fn endpoint(wire: Arc<Wire>, mcp: bool) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        async fn model(
            State(w): State<Arc<Wire>>,
            headers: HeaderMap,
            bytes: Bytes,
        ) -> impl IntoResponse {
            w.requests.lock().unwrap().push((bytes.to_vec(), headers));
            let body = w
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected native request");
            (
                [
                    ("content-type", "text/event-stream"),
                    ("retry-after", "0.001"),
                ],
                Body::from(body),
            )
        }
        async fn rpc(
            State(w): State<Arc<Wire>>,
            headers: HeaderMap,
            bytes: Bytes,
        ) -> impl IntoResponse {
            let raw = NativeValue::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
            w.requests.lock().unwrap().push((bytes.to_vec(), headers));
            if raw.get("id").is_none() {
                return (axum::http::StatusCode::ACCEPTED, Json(json!({}))).into_response();
            }
            let result = match raw["method"].as_str().unwrap() {
                "initialize" => {
                    json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
                }
                "tools/list" => json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}),
                "tools/call" => json!({"content":[{"type":"text","text":"ok"}]}),
                other => panic!("unexpected fixture method {other}"),
            };
            Json(json!({"jsonrpc":"2.0","id":raw["id"],"result":result})).into_response()
        }
        let router = if mcp {
            Router::new().route("/", post(rpc))
        } else {
            Router::new().route("/", post(model))
        };
        axum::serve(listener, router.with_state(wire))
            .await
            .unwrap();
    });
    (uri, server)
}
fn tool_wire(raw: &str, delta: bool) -> String {
    let input = if delta { "{}" } else { raw };
    let mut wire = format!(
        "data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{}}}}}}\n\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"same-id\",\"name\":\"mcp\",\"input\":{input}}}}}\n\n"
    );
    if delta {
        wire.push_str(&format!("data: {}\n\n",json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":raw}})));
    }
    wire.push_str("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n");
    wire
}
const DONE: &str = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n";

// B captures actual bytes from existing authorized rmcp HTTP AND child writers;
// C captures actual next provider HTTP bytes (not serde Value round trips).
#[tokio::test]
async fn owned_d1_d2_36_corpus_uses_rpc_http_stdio_and_next_http_bytes() {
    let corpus: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/anthropic-entry-bytes-0500bf6.json"
    ))
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let rpc = Arc::new(Wire::default());
    let (rpc_uri, rpc_task) = endpoint(rpc.clone(), true).await;
    let peer = r#"import sys,json
for line in sys.stdin:
 with open('rpc-capture','ab') as f:f.write(line.encode())
 r=json.loads(line)
 if 'id' not in r:continue
 if r['method']=='initialize':v={'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
 else:v={'content':[{'type':'text','text':'ok'}]}
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':v}),flush=True)
"#;
    let manager = |config| {
        Mcp::new(
            &McpConfig {
                servers: BTreeMap::from([("demo".into(), config)]),
            },
            dir.path(),
            dir.path(),
        )
        .unwrap()
    };
    let http = manager(ServerConfig {
        url: Some(rpc_uri),
        ..Default::default()
    });
    let stdio = manager(ServerConfig {
        command: Some("python3".into()),
        args: vec!["-u".into(), "-c".into(), peer.into()],
        ..Default::default()
    });
    let cancel = CancellationToken::new();
    for case in corpus["cases"].as_array().unwrap() {
        let expected = case["expected_tool_entry"]
            .as_str()
            .unwrap()
            .strip_prefix("synthetic_tool ")
            .unwrap();
        let raw = case["raw_input"].as_str().unwrap();
        let outer =
            format!(r#"{{"action":"call","server":"demo","tool":"echo","arguments":{raw}}}"#);
        let canonical_outer =
            format!(r#"{{"action":"call","server":"demo","tool":"echo","arguments":{expected}}}"#);
        let wire = Arc::new(Wire::default());
        wire.replies
            .lock()
            .unwrap()
            .extend([tool_wire(&outer, case["mode"] == "delta"), DONE.into()]);
        let (uri, server) = endpoint(wire.clone(), false).await;
        let provider = Provider::mock(uri).with_native_transport_for_test();
        let mut history = Provider::start("anthropic", "<chat></chat>", "task");
        let response = provider
            .step("anthropic/test", "none", "system", &history, &[])
            .await
            .unwrap();
        assert_eq!(response.calls.len(), 1);
        let call = &response.calls[0];
        // A: completed owned Use is pre-native-normalization, JS object retained.
        assert!(call.arguments.is_object());
        assert_eq!(
            call.arguments.member_canonical("arguments").unwrap(),
            expected,
            "A {} {}",
            case["name"],
            case["mode"]
        );
        for (transport, mcp) in [("http", &http), ("stdio", &stdio)] {
            assert_eq!(
                mcp.execute_input(1, false, false, &call.arguments, &cancel)
                    .await
                    .unwrap()["content"][0]["text"],
                "ok"
            );
            let request = if transport == "http" {
                rpc.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|(b, _)| std::str::from_utf8(b).unwrap().contains("tools/call"))
                    .unwrap()
                    .0
                    .clone()
            } else {
                std::fs::read_to_string(dir.path().join("rpc-capture"))
                    .unwrap()
                    .lines()
                    .rev()
                    .find(|l| l.contains("tools/call"))
                    .unwrap()
                    .as_bytes()
                    .to_vec()
            };
            let expected_field = format!(r#""arguments":{expected}"#);
            assert!(
                std::str::from_utf8(&request)
                    .unwrap()
                    .contains(&expected_field),
                "B {transport} {} {} actual={}",
                case["name"],
                case["mode"],
                String::from_utf8_lossy(&request)
            );
        }
        Provider::append_response("anthropic", &mut history, &response);
        Provider::append_result("anthropic", &mut history, call, "ok", false);
        provider
            .step("anthropic/test", "none", "system", &history, &[])
            .await
            .unwrap();
        let sent = wire.requests.lock().unwrap();
        assert_eq!(sent.len(), 2);
        let next = std::str::from_utf8(&sent[1].0).unwrap();
        assert!(
            next.contains(&format!(r#""input":{canonical_outer}"#)),
            "C {} {} {next}",
            case["name"],
            case["mode"]
        );
        assert!(!next.contains("_pantheon_"));
        eprintln!(
            "MEASURED D1/D2 {} {} A=object B=http+stdio C=next-http",
            case["name"], case["mode"]
        );
        drop(sent);
        server.abort();
    }
    rpc_task.abort();
}

#[tokio::test]
async fn owned_raw_utf16_input_value_key_split_pair_and_invalid_syntax() {
    // Real native SSE decoder; inner partial_json contains raw UTF16 units,
    // not a backslash-u escape in JSON text after the outer event is decoded.
    for (name, raw, expected) in [
        ("high-value", r#"{"x":"\ud800"}"#, r#"{"x":"\ud800"}"#),
        ("low-key", r#"{"\udfff":1}"#, r#"{"\udfff":1}"#),
        ("outside", r#"{\ud800}"#, "null"),
    ] {
        let units_json =
            entry_json::Json::parse(&format!("\"{}\"", raw.replace('"', "\\\""))).unwrap();
        let entry_json::Json::String(units) = units_json else {
            panic!()
        };
        let partial = entry_json::Json::String(units).stringify();
        let mut wire = tool_wire("{}", false);
        let at = wire.find("data: {\"type\":\"message_delta\"").unwrap();
        wire.insert_str(at,&format!(r#"data: {{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":{partial}}}}}

"#));
        let record = Arc::new(Wire::default());
        record.replies.lock().unwrap().push_back(wire);
        let (uri, task) = endpoint(record, false).await;
        let provider = Provider::mock(uri).with_native_transport_for_test();
        let response = provider
            .step("anthropic/test", "none", "sys", &[], &[])
            .await
            .unwrap();
        assert_eq!(response.calls[0].arguments.canonical(), expected, "{name}");
        task.abort();
    }
    let high = [
        b'{' as u16,
        b'"' as u16,
        b'x' as u16,
        b'"' as u16,
        b':' as u16,
        b'"' as u16,
        0xd83d,
    ];
    let low = [0xde00, b'"' as u16, b'}' as u16];
    let mut raw = high.to_vec();
    raw.extend(low);
    assert_eq!(
        ToolInput::agent_parse_units(&raw).canonical(),
        "{\"x\":\"😀\"}"
    );
}

#[tokio::test]
async fn owned_utf16_raw_corpus_both_vendors_use_and_next_request() {
    let corpus: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/raw-partial-json-utf16-0500bf6.json"
    ))
    .unwrap();
    for vendor in ["anthropic", "openai"] {
        for case in corpus["records"].as_array().unwrap() {
            let units = case["partial_json_units"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u16)
                .collect::<Vec<_>>();
            let arguments = NativeValue::new(entry_json::Json::String(units));
            let expected = case["use_input"].as_str().unwrap();
            let mut first = if vendor == "anthropic" {
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"same\",\"name\":\"unknown\",\"input\":{\"superseded\":true}}}\n\n".into()
            } else {
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"same\",\"name\":\"unknown\",\"arguments\":\"\"}}\n\n".to_owned()
            };
            for delta in case["outer_delta_json"].as_array().unwrap() {
                if vendor == "anthropic" {
                    first.push_str(&format!(
                        "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{}}}\n\n",
                        delta.as_str().unwrap()
                    ));
                } else {
                    let partial = NativeValue::parse(delta.as_str().unwrap())
                        .unwrap()
                        .field("partial_json")
                        .unwrap();
                    first.push_str(&format!("data: {{\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":{partial}}}\n\n"));
                }
            }
            if vendor == "anthropic" {
                first.push_str("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n");
            } else {
                first.push_str(&format!(r#"data: {{"type":"response.completed","response":{{"status":"completed","output":[{{"type":"function_call","call_id":"same","name":"unknown","arguments":{arguments}}}],"usage":{{}}}}}}

"#));
            }
            let last = if vendor == "anthropic" {
                DONE.to_owned()
            } else {
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{}}}\n\n".into()
            };
            let wire = Arc::new(Wire::default());
            wire.replies.lock().unwrap().extend([first, last]);
            let (uri, task) = endpoint(wire.clone(), false).await;
            let p = Provider::mock(uri).with_native_transport_for_test();
            let model = format!("{vendor}/test");
            let mut history = Provider::start(vendor, "<chat></chat>", "task");
            let response = p
                .step(&model, "none", "system", &history, &[])
                .await
                .unwrap();
            assert_eq!(
                response.calls[0].arguments.canonical(),
                expected,
                "A {vendor} {}",
                case["name"]
            );
            Provider::append_response(vendor, &mut history, &response);
            Provider::append_result(
                vendor,
                &mut history,
                &response.calls[0],
                "inert unknown",
                true,
            );
            p.step(&model, "none", "system", &history, &[])
                .await
                .unwrap();
            let sent = wire.requests.lock().unwrap();
            let next = std::str::from_utf8(&sent[1].0).unwrap();
            let field = if vendor == "anthropic" {
                format!(
                    "\"input\":{}",
                    if expected == "null" { "{}" } else { expected }
                )
            } else {
                format!("\"arguments\":{arguments}")
            };
            assert!(
                next.contains(&field),
                "C {vendor} {} expected={field} actual={next}",
                case["name"]
            );
            eprintln!(
                "MEASURED raw-UTF16 {} vendor={vendor} A/C preserved",
                case["name"]
            );
            drop(sent);
            task.abort();
        }
    }
}

#[test]
fn owned_utf16_presentation_and_cut_trace_join_units_before_utf8() {
    for vendor in ["anthropic", "openai"] {
        let raw = if vendor == "anthropic" {
            r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"a\ud83d"},{"type":"text","text":"\ude00b"},{"type":"thinking","thinking":"t\ud800","signature":"s"},{"type":"text","text":"z\udfff"}]}"#
        } else {
            r#"{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"a\ud83d"},{"type":"output_text","text":"\ude00b","annotations":[{"type":"url_citation","title":"\ud800","url":"fixture"}]}]},{"type":"function_call","call_id":"same","name":"unknown","arguments":"{\"x\":\"\ud800\"}"}]}"#
        };
        let value = NativeValue::parse(raw).unwrap();
        let response = native_transport::ordinary_response(vendor, &value).unwrap();
        let entries = completed_entries::read(vendor, &value, &response);
        assert_eq!(entries[0].text, "a😀b");
        if vendor == "openai" {
            assert_eq!(entries[1].text, "sources:\n� fixture");
            assert_eq!(entries[2].text, "unknown {\"x\":\"�\"}");
        } else {
            assert_eq!(entries[1].text, "t�");
            assert_eq!(entries[2].text, "z�");
        }
        let mut trace = native_trace::Trace::default();
        for unit in [r#""a\ud83d""#, r#""\ude00b""#] {
            let wire = if vendor == "anthropic" {
                format!(
                    r#"{{"type":"content_block_delta","delta":{{"type":"text_delta","text":{unit}}}}}"#
                )
            } else {
                format!(r#"{{"type":"response.output_text.delta","delta":{unit}}}"#)
            };
            trace.read(&NativeValue::parse(&wire).unwrap());
        }
        assert_eq!(
            trace.cut("stopped"),
            vec![("a😀b\n[cut off: stopped]".into(), false)]
        );
    }
}

#[test]
fn owned_terminal_reason_uses_source_before_retained_sanitization() {
    for (vendor, wire, expected) in [
        (
            "openai",
            r#"{"status":"failed","error":{"message":null}}"#,
            "failed",
        ),
        (
            "openai",
            r#"{"status":"incomplete","incomplete_details":{"reason":"\ud800"}}"#,
            "incomplete: �",
        ),
        (
            "anthropic",
            r#"{"stop_reason":"max_tokens","stop_details":{"explanation":"\ud800"}}"#,
            "stopped: max_tokens (�)",
        ),
        (
            "anthropic",
            r#"{"stop_reason":"max_tokens","stop_details":{"explanation":""}}"#,
            "stopped: max_tokens",
        ),
        (
            "anthropic",
            r#"{"stop_reason":"max_tokens","stop_details":{"explanation":null}}"#,
            "stopped: max_tokens",
        ),
    ] {
        assert_eq!(
            native_trace::failure(
                &format!("{vendor}/test"),
                &NativeValue::parse(wire).unwrap()
            )
            .as_deref(),
            Some(expected)
        );
    }
}

#[tokio::test]
async fn owned_modern_mcp_actual_body_headers_versions_annotations_and_utf16_limit() {
    use base64::Engine;
    use rmcp::{
        ServiceExt,
        model::{
            CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation,
            ProtocolVersion, RawToolArguments, RequestMetaObject,
        },
        transport::{
            StreamableHttpClientTransport,
            streamable_http_client::StreamableHttpClientTransportConfig,
        },
    };
    let captures = Arc::new(Wire::default());
    let seen = captures.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener,Router::new().route("/",post(move |headers:HeaderMap,bytes:Bytes| {let seen=seen.clone();async move {
        let raw=NativeValue::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();seen.requests.lock().unwrap().push((bytes.to_vec(),headers));
        if raw.get("id").is_none(){return axum::http::StatusCode::ACCEPTED.into_response();}
        let result=match raw["method"].as_str().unwrap(){
            "initialize"=>json!({"protocolVersion":"2026-07-28","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
            "tools/list"=>json!({"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{"s":{"type":"string","x-mcp-header":"S"},"flag":{"type":"boolean","x-mcp-header":"Flag"},"n":{"type":"integer","x-mcp-header":"N"}}}},{"name":"plain","inputSchema":{"type":"object"}}]}),
            "tools/call"=>json!({"content":[{"type":"text","text":"ok"}]}),
            method=>panic!("unexpected method {method}"),
        };
        if raw["method"]=="initialize" {([("mcp-session-id","fixture-session")],Json(json!({"jsonrpc":"2.0","id":raw["id"],"result":result}))).into_response()}
        else{Json(json!({"jsonrpc":"2.0","id":raw["id"],"result":result})).into_response()}
    }}))).await.unwrap();
    });
    let mut config =
        StreamableHttpClientTransportConfig::with_uri(uri).auth_header("synthetic-token");
    config
        .custom_headers
        .insert("x-fixture".parse().unwrap(), "kept".parse().unwrap());
    config.max_sse_event_size = 2048;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("fixture", "1"),
    )
    .serve(StreamableHttpClientTransport::with_client(
        reqwest_mcp::Client::new(),
        config,
    ))
    .await
    .unwrap();
    fn request(name: &str, raw: &str) -> CallToolRequestParams {
        let input = ToolInput::agent_parse(&format!(r#"{{"arguments":{raw}}}"#));
        CallToolRequestParams::new(name.to_owned()).with_raw_arguments(
            RawToolArguments::new(input.member_canonical("arguments").unwrap()).unwrap(),
        )
    }
    // No cached schema: the method/name owners still work; no spurious params.
    client
        .call_tool(request("echo", r#"{"s":"before-cache"}"#))
        .await
        .unwrap();
    {
        let c = captures.requests.lock().unwrap();
        let (_, h) = c.last().unwrap();
        assert_eq!(h["mcp-method"], "tools/call");
        assert_eq!(h["mcp-name"], "echo");
        assert!(!h.contains_key("mcp-param-s"));
    }
    client
        .call_tool(
            CallToolRequestParams::new("echo")
                .with_raw_arguments(RawToolArguments::new("null".into()).unwrap()),
        )
        .await
        .unwrap();
    {
        let c = captures.requests.lock().unwrap();
        let (bytes, h) = c.last().unwrap();
        assert!(
            std::str::from_utf8(bytes)
                .unwrap()
                .contains("\"arguments\":null")
        );
        assert!(!h.contains_key("mcp-param-s"));
    }
    client.list_tools(None).await.unwrap();
    for scalar in [
        "\"\"",
        r#""ascii""#,
        r#"" padded ""#,
        r#""a\nb""#,
        r#""á""#,
        r#""=?base64?abc?=""#,
    ] {
        let raw = format!(
            r#"{{"s":{scalar},"flag":true,"n":9007199254740993,"unused":{{"x":"\ud800","n":1e400}}}}"#
        );
        client.call_tool(request("echo", &raw)).await.unwrap();
        let c = captures.requests.lock().unwrap();
        let (bytes, h) = c.last().unwrap();
        let body = std::str::from_utf8(bytes).unwrap();
        assert!(body.contains(r#""n":9007199254740992,"unused":{"x":"\ud800","n":null}"#));
        let sent = NativeValue::parse(body).unwrap();
        let expected = sent["params"]["arguments"]["s"].as_str().unwrap();
        let header = h["mcp-param-s"].to_str().unwrap();
        let decoded = if let Some(encoded) = header
            .strip_prefix("=?base64?")
            .and_then(|v| v.strip_suffix("?="))
        {
            String::from_utf8(base64::prelude::BASE64_STANDARD.decode(encoded).unwrap()).unwrap()
        } else {
            header.into()
        };
        assert_eq!(decoded, expected);
        assert_eq!(h["mcp-param-flag"], "true");
        assert_eq!(h["mcp-param-n"], "9007199254740992");
        assert_eq!(h["authorization"], "Bearer synthetic-token");
        assert_eq!(h["mcp-session-id"], "fixture-session");
        assert_eq!(h["mcp-protocol-version"], "2026-07-28");
        assert_eq!(h["x-fixture"], "kept");
        assert!(h["accept"].to_str().unwrap().contains("text/event-stream"));
        eprintln!(
            "MEASURED modern-header scalar={scalar} body/header/auth/session/version/custom preserved"
        );
    }
    for raw in [r#"{"s":null,"n":1e400}"#, r#"{"unused":{"x":"\ud800"}}"#] {
        client.call_tool(request("echo", raw)).await.unwrap();
        let c = captures.requests.lock().unwrap();
        assert!(!c.last().unwrap().1.contains_key("mcp-param-s"));
        assert!(!c.last().unwrap().1.contains_key("mcp-param-n"));
    }
    client
        .call_tool(CallToolRequestParams::new("echo"))
        .await
        .unwrap();
    client
        .call_tool(request(
            "plain",
            r#"{"s":"\ud800","unused":{"x":"\udfff"}}"#,
        ))
        .await
        .unwrap();
    {
        let c = captures.requests.lock().unwrap();
        let (bytes, h) = c.last().unwrap();
        assert!(
            std::str::from_utf8(bytes)
                .unwrap()
                .contains(r#""s":"\ud800""#)
        );
        assert!(!h.contains_key("mcp-param-s"));
    }
    let mut drift = request("echo", r#"{"s":"authoritative","n":9007199254740993}"#);
    drift.arguments = Some(
        json!({"s":"NOT-authoritative","n":1})
            .as_object()
            .unwrap()
            .clone(),
    );
    client.call_tool(drift).await.unwrap();
    {
        let c = captures.requests.lock().unwrap();
        let (bytes, h) = c.last().unwrap();
        assert!(
            std::str::from_utf8(bytes)
                .unwrap()
                .contains(r#""arguments":{"s":"authoritative","n":9007199254740992}"#)
        );
        assert_eq!(h["mcp-param-s"], "authoritative");
        assert_eq!(h["mcp-param-n"], "9007199254740992");
    }
    let replaced = request("echo", r#"{"s":"old-raw"}"#)
        .with_arguments(json!({"s":"replacement"}).as_object().unwrap().clone());
    client.call_tool(replaced).await.unwrap();
    {
        let c = captures.requests.lock().unwrap();
        let (bytes, h) = c.last().unwrap();
        assert!(
            std::str::from_utf8(bytes)
                .unwrap()
                .contains(r#""arguments":{"s":"replacement"}"#)
        );
        assert!(!std::str::from_utf8(bytes).unwrap().contains("old-raw"));
        assert_eq!(h["mcp-param-s"], "replacement");
    }
    client
        .call_tool(request(
            "echo",
            r#"{"s":"still-valid","unused":{"\ud800":"value"},"\udfff":"ignored"}"#,
        ))
        .await
        .unwrap();
    {
        let c = captures.requests.lock().unwrap();
        let (bytes, h) = c.last().unwrap();
        assert!(
            std::str::from_utf8(bytes)
                .unwrap()
                .contains(r#""\udfff":"ignored""#)
        );
        assert_eq!(h["mcp-param-s"], "still-valid");
    }
    let before = captures.requests.lock().unwrap().len();
    assert!(
        client
            .call_tool(request("echo", r#"{"s":"\ud800"}"#))
            .await
            .is_err(),
        "named modern annotated-scalar limit must fail before POST, not corrupt bytes"
    );
    assert_eq!(captures.requests.lock().unwrap().len(), before);
    // An explicit per-request version override has its own scope, never an
    // implicit downgrade. The existing rmcp version/header owner stays intact.
    let old = rmcp::model::CallToolRequest::new(request("echo", r#"{"s":"\ud800"}"#));
    let mut meta = RequestMetaObject::new();
    meta.set_protocol_version(ProtocolVersion::V_2025_11_25);
    client
        .peer()
        .send_request_with_option(
            rmcp::model::ClientRequest::CallToolRequest(old),
            rmcp::service::PeerRequestOptions::no_options().with_meta(meta),
        )
        .await
        .unwrap()
        .await_response()
        .await
        .unwrap();
    {
        let c = captures.requests.lock().unwrap();
        let (bytes, h) = c.last().unwrap();
        assert!(
            std::str::from_utf8(bytes)
                .unwrap()
                .contains(r#""s":"\ud800""#)
        );
        assert_eq!(h["mcp-protocol-version"], "2025-11-25");
        assert!(!h.contains_key("mcp-param-s"));
        assert!(!h.contains_key("mcp-method"));
    }
    client.cancellation_token().cancel();
    task.abort();
}

#[tokio::test]
async fn owned_cache_prefix_keys_follow_actual_included_ast_not_attempts_or_tail() {
    let wire = Arc::new(Wire::default());
    for _ in 0..5 {
        wire.replies.lock().unwrap().push_back(DONE.into());
    }
    let (uri, task) = endpoint(wire.clone(), false).await;
    let id = uuid::Uuid::new_v4();
    let probe = Arc::new(Mutex::new(Vec::<NativeCacheObservation>::new()));
    let provider = Provider::mock(uri)
        .with_native_transport_for_test()
        .with_cache_affinity(id)
        .with_native_cache_probe(probe.clone());
    let history = |included: &str, task: &str, tail: &str| {
        let mut user = NativeValue::from(
            json!({"role":"user","content":[{"type":"text","text":"prefix","cache_control":{"type":"ephemeral"},"native":null},{"type":"text","text":"more","cache_control":{"type":"ephemeral"}},{"type":"text","text":"task","cache_control":{"type":"ephemeral"},"native":null}]}),
        );
        let mut blocks = user.field("content").unwrap().elements();
        blocks[0].set("native", NativeValue::parse(included).unwrap());
        blocks[2].set("native", NativeValue::parse(task).unwrap());
        user.set("content", NativeValue::array(blocks));
        let mut after = NativeValue::from(json!({"type":"text","text":"after","native":null}));
        after.set("native", NativeValue::parse(tail).unwrap());
        let mut turn = NativeValue::from(json!({"role":"assistant"}));
        turn.set("content", NativeValue::array([after]));
        vec![user, turn]
    };
    let variants = [
        ("{}", "{}", "{}"),
        (r#"{"x":"\ud800"}"#, "{}", "{}"),
        (r#"{"x":"\udfff"}"#, "{}", "{}"),
        (
            r#"{"x":"\ud800"}"#,
            r#"{"x":"\udfff"}"#,
            r#"{"x":"\udfff"}"#,
        ),
        ("{}", r#"{"x":"\ud800"}"#, r#"{"x":"\udfff"}"#),
    ];
    for (a, b, c) in variants {
        provider
            .step("anthropic/test", "none", "system", &history(a, b, c), &[])
            .await
            .unwrap();
    }
    let records = probe.lock().unwrap();
    assert_eq!(records.len(), 5);
    assert_eq!(records[0].keys.len(), 2);
    assert_eq!(
        records[0].keys,
        cache_coordination::keys("anthropic/test", &records[0].body, id)
            .unwrap()
            .0,
        "unchanged prefixes retain exact old identities"
    );
    assert_eq!(
        records[1].body, records[2].body,
        "host projection collision is real"
    );
    assert_ne!(
        records[1].keys, records[2].keys,
        "included AST differences split"
    );
    assert_eq!(
        records[1].keys, records[3].keys,
        "excluded task/tail AST must not split"
    );
    assert_eq!(
        records[0].keys, records[4].keys,
        "no generic hashing of new ASTs outside included prefix"
    );
    let sent = wire.requests.lock().unwrap();
    assert!(
        std::str::from_utf8(&sent[1].0)
            .unwrap()
            .contains(r#""x":"\ud800""#)
    );
    assert!(
        std::str::from_utf8(&sent[2].0)
            .unwrap()
            .contains(r#""x":"\udfff""#)
    );
    task.abort();
}

#[tokio::test]
async fn owned_mcp_reqwest_nondefault_sse_bound_on_post_get_and_resume() {
    use futures_util::StreamExt;
    use rmcp::{
        model::{
            CallToolRequest, CallToolRequestParams, ClientJsonRpcMessage, ClientRequest, RequestId,
        },
        transport::streamable_http_client::{StreamableHttpClient, StreamableHttpPostResponse},
    };
    let seen = Arc::new(Wire::default());
    let record = seen.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/{n}",
                axum::routing::any(
                    move |axum::extract::Path(n): axum::extract::Path<usize>,
                          headers: HeaderMap,
                          bytes: Bytes| {
                        let record = record.clone();
                        async move {
                            record
                                .requests
                                .lock()
                                .unwrap()
                                .push((bytes.to_vec(), headers));
                            (
                                [("content-type", "text/event-stream")],
                                format!("data: {}\n\n", "a".repeat(n)),
                            )
                        }
                    },
                ),
            ),
        )
        .await
        .unwrap();
    });
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest_mcp::Client::new();
    for route in ["post", "get", "resume"] {
        for n in [1, 2, 3] {
            let uri: Arc<str> = format!("{base}/{n}").into();
            let mut stream = if route == "post" {
                let request = ClientJsonRpcMessage::request(
                    ClientRequest::CallToolRequest(CallToolRequest::new(
                        CallToolRequestParams::new("echo"),
                    )),
                    RequestId::Number(1),
                );
                match http
                    .post_message_with_max_sse_event_size(
                        uri,
                        request,
                        Some("s".into()),
                        Some("synthetic".into()),
                        Default::default(),
                        9,
                    )
                    .await
                    .unwrap()
                {
                    StreamableHttpPostResponse::Sse(stream, _) => stream,
                    other => panic!("expected SSE {other:?}"),
                }
            } else {
                http.get_stream_with_max_sse_event_size(
                    uri,
                    Some("s".into()),
                    (route == "resume").then(|| "event1".into()),
                    Some("synthetic".into()),
                    Default::default(),
                    9,
                )
                .await
                .unwrap()
            };
            let first = stream.next().await.unwrap();
            assert_eq!(
                first.is_err(),
                n == 3,
                "{route} raw event payload length={n}, exact bound=9; {first:?}"
            );
            if let Ok(event) = first {
                assert_eq!(event.data.as_deref(), Some("a".repeat(n).as_str()));
            }
            let records = seen.requests.lock().unwrap();
            let (_, h) = records.last().unwrap();
            assert_eq!(h["mcp-session-id"], "s");
            assert_eq!(h["authorization"], "Bearer synthetic");
            if route == "resume" {
                assert_eq!(h["last-event-id"], "event1");
            }
            eprintln!(
                "MEASURED MCP {route} nondefault raw-event bound below/at/above length={n} bounded=true"
            );
        }
    }
    task.abort();
}

#[tokio::test]
async fn owned_input_absence_null_malformed_array_primitive_and_normalization() {
    for (mode, input, delta, kind, output) in [
        ("absent", None, None, "absent", None),
        ("null", Some("null"), None, "null", Some("{}")),
        ("malformed", Some("{}"), Some("{"), "null", Some("{}")),
        ("array", Some("[]"), None, "array", Some("[]")),
        ("primitive", Some("42"), None, "number", Some("42")),
        ("overflow", Some("1e400"), None, "number", Some("null")),
    ] {
        let mut wire =
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n".to_owned();
        let input = input.map(|v| format!(",\"input\":{v}")).unwrap_or_default();
        wire.push_str(&format!("data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"same\",\"name\":\"unknown\"{input}}}}}\n\n"));
        if let Some(delta) = delta {
            wire.push_str(&format!("data: {}\n\n",json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":delta}})));
        }
        wire.push_str("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n");
        let record = Arc::new(Wire::default());
        record.replies.lock().unwrap().push_back(wire);
        let (uri, task) = endpoint(record, false).await;
        let response = Provider::mock(uri)
            .with_native_transport_for_test()
            .step("anthropic/test", "none", "sys", &[], &[])
            .await
            .unwrap();
        assert_eq!(response.calls[0].arguments.kind(), kind, "A {mode}");
        assert_eq!(
            response.native[0]
                .field("input")
                .map(|v| v.canonical())
                .as_deref(),
            output,
            "native-out {mode}"
        );
        task.abort();
    }
    for (raw, kind) in [
        ("", "object"),
        ("{", "null"),
        ("null", "null"),
        ("[]", "array"),
        ("42", "number"),
    ] {
        assert_eq!(
            ToolInput::agent_parse_units(&raw.encode_utf16().collect::<Vec<_>>()).kind(),
            kind
        );
    }
}

#[tokio::test]
async fn owned_compactor_pause_busy_pause_done_preserves_raw_native_c_and_final_only_answer() {
    let wire = Arc::new(Wire::default());
    let pause = |unit: &str, reason: &str| {
        format!(
            r#"data: {{"type":"message_start","message":{{"usage":{{}}}}}}

data: {{"type":"content_block_start","index":0,"content_block":{{"type":"thinking","thinking":"private thought","signature":"{unit}"}}}}

data: {{"type":"content_block_start","index":1,"content_block":{{"type":"text","text":"not final {reason}"}}}}

data: {{"type":"content_block_start","index":2,"content_block":{{"type":"server_tool_use","id":"same","name":"web_search","input":{{"q":"{unit}","n":1e400}}}}}}

data: {{"type":"content_block_start","index":3,"content_block":{{"type":"code_execution_tool_result","tool_use_id":"same","content":{{"stdout":"\ud83d","stderr":"\ude00"}}}}}}

data: {{"type":"message_delta","delta":{{"stop_reason":"pause_turn"}}}}

data: {{"type":"message_stop"}}

"#
        )
    };
    let busy="data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"name\":\"shell\",\"id\":\"MUST_NOT_EXECUTE\",\"input\":{}}}\n\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n".to_owned();
    let final_wire=r#"data: {"type":"message_start","message":{"usage":{}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":"a\ud83d"}}

data: {"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}

data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":"\ude00b"}}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}

data: {"type":"message_stop"}

"#.to_owned();
    wire.replies.lock().unwrap().extend([
        pause("\\ud800", "first"),
        busy,
        pause("\\udfff", "second"),
        final_wire,
    ]);
    let (uri, task) = endpoint(wire.clone(), false).await;
    let provider = Provider::mock(uri).with_native_transport_for_test();
    let history = Provider::start_compaction("anthropic", "<chat></chat>", "summarize");
    let reply = provider
        .step_compaction("anthropic/test", "none", "system", &history)
        .await
        .unwrap();
    assert_eq!(reply, CompactionReply::Answer("a😀b".into()));
    let requests = wire.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests[1].0, requests[2].0,
        "busy retries exactly the same owned paused request"
    );
    for (index, unit) in [(1, "\\ud800"), (3, "\\udfff")] {
        let body = std::str::from_utf8(&requests[index].0).unwrap();
        assert!(body.contains(&format!(r#""signature":"{unit}""#)));
        assert!(body.contains(&format!(r#""input":{{"q":"{unit}","n":null}}"#)));
        assert!(!body.contains("MUST_NOT_EXECUTE"));
    }
    let last = std::str::from_utf8(&requests[3].0).unwrap();
    assert!(last.contains(r#""signature":"\ud800""#));
    assert!(last.contains(r#""stdout":"\ud83d","stderr":"\ude00""#));
    eprintln!(
        "MEASURED compactor pause/busy/pause/final actual C UTF16/signatures/order preserved, retry bytes identical, final answer only"
    );
    task.abort();
}

#[test]
fn owned_native_budget_does_not_count_unpaired_string_as_four_byte_null() {
    let normal = NativeValue::from(
        json!({"role":"assistant","content":[{"type":"text","text":"ordinary"}]}),
    );
    let baseline = crate::attachments::remaining_budget("anthropic/test", &[normal.projected()]);
    let native = crate::attachments::remaining_budget_native(
        "anthropic/test",
        std::slice::from_ref(&normal),
    );
    assert_eq!(native.payload, baseline.payload);
    assert_eq!(
        crate::attachments::text_chars_native(&normal),
        crate::attachments::text_chars(&normal)
    );
    let raw = format!(
        r#"{{"role":"assistant","content":[{{"type":"text","text":"{}\ud800"}}]}}"#,
        "x".repeat(10000)
    );
    let item = NativeValue::parse(&raw).unwrap();
    let lossy = crate::attachments::remaining_budget("anthropic/test", &[item.projected()]);
    let actual =
        crate::attachments::remaining_budget_native("anthropic/test", std::slice::from_ref(&item));
    assert!(actual.payload + 7000 < lossy.payload);
    assert!(crate::attachments::text_chars_native(&item) > 10000);
    let key = format!(r#"{{"{}\udfff":"opaque"}}"#, "k".repeat(10000));
    let keyed = NativeValue::parse(&key).unwrap();
    let lossy_key = crate::attachments::remaining_budget("anthropic/test", &[keyed.projected()]);
    let actual_key =
        crate::attachments::remaining_budget_native("anthropic/test", std::slice::from_ref(&keyed));
    assert!(
        actual_key.payload + 7000 < lossy_key.payload,
        "large non-scalar key must not disappear from resource accounting"
    );
    assert!(crate::attachments::text_chars_native(&keyed) > 10000);
}

#[test]
fn owned_call_identity_utf16_hash_and_display_are_separate() {
    let ids = [
        r#""\ud800""#,
        r#""\udfff""#,
        r#""�""#,
        r#""\ud800""#,
        r#""\ud83d\ude00""#,
        r#""😀""#,
    ]
    .map(|raw| CallId::from_json(Some(&entry_json::Json::parse(raw).unwrap())).unwrap());
    assert_eq!(ids[0], ids[3]);
    assert_eq!(ids[4], ids[5]);
    assert_ne!(ids[0], ids[1]);
    assert_ne!(ids[0], ids[2]);
    assert_ne!(ids[1], ids[2]);
    assert_eq!(ids[0].as_str(), ids[1].as_str());
    assert_eq!(ids[0].as_str(), ids[2].as_str());
    assert_eq!(
        ids.into_iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4
    );
}

#[test]
fn owned_default_debug_is_shape_only_not_hidden_native_payload() {
    let value =
        NativeValue::parse(r#"{"secret":"SYNTHETIC_NATIVE_VALUE_SENTINEL\ud800"}"#).unwrap();
    let input = ToolInput::agent_parse(r#"{"secret":"SYNTHETIC_NATIVE_VALUE_SENTINEL\ud800"}"#);
    let id = CallId::from("SYNTHETIC_NATIVE_VALUE_SENTINEL");
    for debug in [
        format!("{value:?}"),
        format!("{input:?}"),
        format!("{id:?}"),
    ] {
        assert!(!debug.contains("SYNTHETIC_NATIVE_VALUE_SENTINEL"));
        assert!(!debug.contains("55296"));
    }
    let raw = rmcp::model::RawToolArguments::new(
        r#"{"secret":"SYNTHETIC_NATIVE_VALUE_SENTINEL\ud800"}"#.into(),
    )
    .unwrap();
    let params = rmcp::model::CallToolRequestParams::new("echo").with_raw_arguments(raw);
    assert!(!format!("{params:?}").contains("SYNTHETIC_NATIVE_VALUE_SENTINEL"));
    assert!(
        serde_json::to_string(&params)
            .unwrap()
            .contains("SYNTHETIC_NATIVE_VALUE_SENTINEL"),
        "actual RPC still carries its authorized source, diagnostics do not"
    );
}
