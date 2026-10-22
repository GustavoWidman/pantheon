use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
};
use rmcp::{
    ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation, RawToolArguments,
    },
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[tokio::test]
async fn owned_mcp_integrated_reconnect_cancel_json_sse_and_awaited_session_delete() {
    use rmcp::transport::common::client_side_sse::FixedInterval;
    use rmcp::transport::streamable_http_client::StreamableHttpClient;
    let records = Arc::new(Mutex::new(Vec::<(Method, HeaderMap, Vec<u8>)>::new()));
    let get_count = Arc::new(AtomicUsize::new(0));
    let reconnected = Arc::new(Notify::new());
    let slow = Arc::new(Notify::new());
    let deleted = Arc::new(AtomicUsize::new(0));
    let (record, count, resumed, started, cleaned) = (
        records.clone(),
        get_count.clone(),
        reconnected.clone(),
        slow.clone(),
        deleted.clone(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener,Router::new().route("/",axum::routing::any(move|method:Method,headers:HeaderMap,bytes:Bytes| {
        let(record,count,resumed,started,cleaned)=(record.clone(),count.clone(),resumed.clone(),started.clone(),cleaned.clone());
        async move {
            record.lock().unwrap().push((method.clone(),headers.clone(),bytes.to_vec()));
            assert_eq!(headers["authorization"],"Bearer synthetic-owned");assert_eq!(headers["x-fixture"],"kept");
            if method==Method::DELETE {assert_eq!(headers["mcp-session-id"],"owned-session");cleaned.fetch_add(1,Ordering::SeqCst);return StatusCode::NO_CONTENT.into_response();}
            if method==Method::GET {
                assert_eq!(headers["mcp-session-id"],"owned-session");
                if count.fetch_add(1,Ordering::SeqCst)==0 {
                    return ([("content-type","text/event-stream")],"id: resume-one\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"level\":\"info\",\"data\":\"fixture\"}}\n\n").into_response();
                }
                assert_eq!(headers["last-event-id"],"resume-one");resumed.notify_one();
                return ([("content-type","text/event-stream")],Body::from_stream(futures_util::stream::pending::<Result<Bytes,std::io::Error>>())).into_response();
            }
            let raw=NativeValue::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
            if raw.get("id").is_none(){return StatusCode::ACCEPTED.into_response();}
            if raw["method"]=="initialize" {
                return ([("mcp-session-id","owned-session")],axum::Json(json!({"jsonrpc":"2.0","id":raw["id"],"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}))).into_response();
            }
            assert_eq!(headers["mcp-session-id"],"owned-session");assert_eq!(headers["mcp-protocol-version"],"2025-11-25");
            match raw["params"]["name"].as_str().unwrap() {
                "slow"=>{started.notify_one(); ([("content-type","text/event-stream")],Body::from_stream(futures_util::stream::pending::<Result<Bytes,std::io::Error>>())).into_response()},
                "auth-error"=>(StatusCode::UNAUTHORIZED,[("www-authenticate","Bearer realm=\"fixture\"")],"not a payload diagnostic").into_response(),
                name=>{
                    let response=json!({"jsonrpc":"2.0","id":raw["id"],"result":{"content":[{"type":"text","text":name}]}});
                    if name=="sse" { ([("content-type","text/event-stream")],format!("event: message\ndata: {response}\n\n")).into_response() } else {axum::Json(response).into_response()}
                }
            }
        }
    }))).await.unwrap();
    });
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest_mcp::Client::new();
    let mut config =
        StreamableHttpClientTransportConfig::with_uri(uri.clone()).auth_header("synthetic-owned");
    config
        .custom_headers
        .insert("x-fixture".parse().unwrap(), "kept".parse().unwrap());
    config.max_sse_event_size = 1024;
    let mut retry = FixedInterval::default();
    retry.max_times = Some(1);
    retry.duration = Duration::from_millis(1);
    config.retry_config = Arc::new(retry);
    config.reinit_on_expired_session = false;
    let mut client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("fixture", "1"),
    )
    .serve(StreamableHttpClientTransport::with_client(
        http.clone(),
        config,
    ))
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), reconnected.notified())
        .await
        .unwrap();
    assert_eq!(
        get_count.load(Ordering::SeqCst),
        2,
        "actual worker reconnect, not a second direct GET call"
    );
    for name in ["json", "sse"] {
        let params = CallToolRequestParams::new(name).with_raw_arguments(
            RawToolArguments::new(r#"{"z":"\ud800","a":null}"#.into()).unwrap(),
        );
        let result = client.call_tool(params).await.unwrap();
        assert_eq!(
            serde_json::to_value(result).unwrap()["content"][0]["text"],
            name
        );
    }
    let error = client
        .call_tool(CallToolRequestParams::new("auth-error"))
        .await
        .unwrap_err();
    assert!(
        format!("{error:?}").contains("AuthRequired"),
        "actual typed HTTP auth classification: {error:?}"
    );
    let before = records.lock().unwrap().len();
    let mut reserved = HashMap::new();
    reserved.insert(
        "accept".parse().unwrap(),
        "must-not-override".parse().unwrap(),
    );
    let msg = rmcp::model::ClientJsonRpcMessage::request(
        rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
            CallToolRequestParams::new("reserved"),
        )),
        rmcp::model::RequestId::Number(123),
    );
    let denied = http
        .post_message_with_max_sse_event_size(
            uri.into(),
            msg,
            Some("owned-session".into()),
            Some("synthetic-owned".into()),
            reserved,
            1024,
        )
        .await;
    assert!(denied.is_err());
    assert_eq!(
        records.lock().unwrap().len(),
        before,
        "reserved header conflicts fail before HTTP"
    );
    let stop = client.cancellation_token();
    let mut waiting = Box::pin(client.call_tool(CallToolRequestParams::new("slow")));
    tokio::select! {result=&mut waiting=>panic!("slow request ended before stop {result:?}"),_=slow.notified()=>{}}
    stop.cancel();
    assert!(waiting.await.is_err());
    tokio::time::timeout(Duration::from_secs(5), client.close())
        .await
        .unwrap()
        .unwrap();
    assert!(client.is_closed());
    assert_eq!(
        deleted.load(Ordering::SeqCst),
        1,
        "awaited transport close cleans its real session exactly once"
    );
    let records = records.lock().unwrap();
    let calls = records
        .iter()
        .filter(|(method, _, bytes)| {
            *method == Method::POST && std::str::from_utf8(bytes).unwrap().contains("tools/call")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        calls.len(),
        4,
        "no hidden redispatch of cancelled/auth-failed requests"
    );
    for (_, _, bytes) in calls.iter().take(2).copied() {
        assert!(
            std::str::from_utf8(bytes)
                .unwrap()
                .contains(r#""arguments":{"z":"\ud800","a":null}"#)
        );
    }
    eprintln!(
        "MEASURED integrated MCP init/auth/session/custom, worker reconnect Last-Event-ID, JSON/SSE, typed401, before-POST reserved-header conflict, cancel and awaited single DELETE cleanup"
    );
    task.abort();
}
