use pantheon::mcp::{Mcp, McpConfig, ServerConfig};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio_util::sync::CancellationToken;

const SERVER: &str = r#"
import sys,json,time,os
for line in sys.stdin:
 r=json.loads(line)
 if 'id' not in r: continue
 method=r['method'];p=r.get('params',{});result={}
 if method=='initialize':
  result={'protocolVersion':'2025-11-25','capabilities':{'tools':{},'resources':{},'prompts':{}},'serverInfo':{'name':'fixture','version':'1'}}
 elif method=='tools/list':
  tool='second' if p.get('cursor')=='page2' else 'echo'
  result={'tools':[{'name':tool,'description':'Return input','inputSchema':{'type':'object'}}]}
  if tool=='echo':result['nextCursor']='page2'
 elif method=='tools/call':
  name=p['name'];args=p.get('arguments',{})
  if name=='slow':
   with open('effects','a') as f:f.write('issued\n')
   time.sleep(10)
  result={'content':[{'type':'text','text':json.dumps(args)}],'isError':name=='fail'}
  if name=='large':result={'content':[{'type':'text','text':'z'*50000}]}
 elif method=='resources/list':result={'resources':[{'uri':'course://lesson','name':'Lesson'}]}
 elif method=='resources/read':result={'contents':[{'uri':p['uri'],'text':'Practice lesson'}]}
 elif method=='prompts/list':result={'prompts':[{'name':'plan','description':'A workflow'}]}
 elif method=='prompts/get':result={'messages':[{'role':'user','content':{'type':'text','text':'Use the supplied criteria'}}]}
 elif method=='resources/templates/list':result={'resourceTemplates':[]}
 else:
  print(json.dumps({'jsonrpc':'2.0','id':r['id'],'error':{'code':-32601,'message':'Unknown method'}}),flush=True);continue
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':result}),flush=True)
"#;
fn manager(dir: &tempfile::TempDir, server: ServerConfig) -> Mcp {
    let cfg = McpConfig {
        servers: BTreeMap::from([("demo".into(), server)]),
    };
    Mcp::new(&cfg, dir.path(), dir.path()).unwrap()
}
fn stdio() -> ServerConfig {
    ServerConfig {
        command: Some("python3".into()),
        args: vec!["-u".into(), "-c".into(), SERVER.into()],
        ..Default::default()
    }
}

#[tokio::test]
async fn stdio_discovery_calls_resources_prompts_and_error_results() {
    let dir = tempfile::tempdir().unwrap();
    let m = manager(&dir, stdio());
    let cancel = CancellationToken::new();
    let client = &m;
    let token = &cancel;
    let run = |a: Value| async move { client.execute(1, false, false, &a, token).await.unwrap() };
    assert_eq!(
        run(json!({"action":"servers"})).await["servers"][0]["transport"],
        "stdio"
    );
    let first = run(json!({"action":"list_tools","server":"demo"})).await;
    assert_eq!(first["tools"][0]["name"], "echo");
    assert_eq!(first["nextCursor"], "page2");
    assert_eq!(
        run(json!({"action":"list_tools","server":"demo","cursor":"page2"})).await["tools"][0]["name"],
        "second"
    );
    let result =
        run(json!({"action":"call","server":"demo","tool":"echo","arguments":{"message":"hello"}}))
            .await;
    assert_eq!(
        serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap()["message"],
        "hello"
    );
    assert_eq!(
        run(json!({"action":"call","server":"demo","tool":"fail"})).await["isError"],
        true
    );
    assert_eq!(
        run(json!({"action":"list_resources","server":"demo"})).await["resources"][0]["uri"],
        "course://lesson"
    );
    assert_eq!(
        run(json!({"action":"read_resource","server":"demo","uri":"course://lesson"})).await["contents"]
            [0]["text"],
        "Practice lesson"
    );
    assert_eq!(
        run(json!({"action":"list_prompts","server":"demo"})).await["prompts"][0]["name"],
        "plan"
    );
    assert_eq!(
        run(json!({"action":"get_prompt","server":"demo","prompt":"plan"})).await["messages"][0]["content"]
            ["text"],
        "Use the supplied criteria"
    );
}

#[tokio::test]
async fn access_rules_and_large_results_survive_reopening_without_cross_channel_access() {
    let dir = tempfile::tempdir().unwrap();
    let server = ServerConfig {
        allowed_channels: vec![1],
        workers: false,
        allowed_tools: vec!["large".into()],
        ..stdio()
    };
    let m = manager(&dir, server.clone());
    let cancel = CancellationToken::new();
    assert!(
        m.servers(2, false)["servers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for (channel, child) in [(2, false), (1, true)] {
        assert!(
            m.execute(
                channel,
                child,
                false,
                &json!({"action":"list_tools","server":"demo"}),
                &cancel
            )
            .await
            .is_err()
        );
    }
    assert!(
        m.execute(
            1,
            false,
            false,
            &json!({"action":"call","server":"demo","tool":"echo"}),
            &cancel
        )
        .await
        .is_err()
    );
    assert!(
        m.execute(
            1,
            false,
            true,
            &json!({"action":"call","server":"demo","tool":"large"}),
            &cancel
        )
        .await
        .is_err()
    );
    let result = m
        .execute(
            1,
            false,
            false,
            &json!({"action":"call","server":"demo","tool":"large"}),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(result["truncated"], true);
    let id = result["result_id"].as_str().unwrap().to_owned();
    drop(m);
    let reopened = manager(&dir, server);
    let page = reopened
        .execute(
            1,
            false,
            false,
            &json!({"action":"read_result","server":"demo","result_id":id,"offset":8000}),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(page["text"], "z".repeat(8000));
    let other = manager(&dir, stdio());
    assert!(
        other
            .execute(
                2,
                false,
                false,
                &json!({"action":"read_result","server":"demo","result_id":id}),
                &cancel
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn cancelled_mutation_is_not_replayed_and_next_request_reconnects() {
    let dir = tempfile::tempdir().unwrap();
    let m = manager(&dir, stdio());
    let cancel = CancellationToken::new();
    let call = json!({"action":"call","server":"demo","tool":"slow"});
    let task = m.execute(1, false, false, &call, &cancel);
    let stop = async {
        for _ in 0..100 {
            if std::fs::read_to_string(dir.path().join("effects"))
                .is_ok_and(|text| text == "issued\n")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel.cancel();
    };
    let (result, _) = tokio::join!(task, stop);
    assert!(result.unwrap_err().to_string().contains("inspect effects"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("effects")).unwrap(),
        "issued\n"
    );
    let result=m.execute(1,false,false,&json!({"action":"call","server":"demo","tool":"echo","arguments":{"message":"after reconnect"}}),&CancellationToken::new()).await.unwrap();
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("after reconnect")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("effects")).unwrap(),
        "issued\n"
    );
}

#[tokio::test]
async fn streamable_http_negotiates_session_headers_and_handles_sse_results() {
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::post,
    };
    async fn endpoint(
        State(expected): State<String>,
        headers: HeaderMap,
        Json(r): Json<Value>,
    ) -> Response {
        let method = r["method"].as_str().unwrap();
        if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(expected.as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if method == "notifications/initialized" {
            return StatusCode::ACCEPTED.into_response();
        }
        let result = if method == "initialize" {
            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
        } else {
            assert_eq!(headers.get("mcp-session-id").unwrap(), "fixture-session");
            assert_eq!(headers.get("mcp-protocol-version").unwrap(), "2025-11-25");
            json!({"content":[{"type":"text","text":"HTTP confirmed"}]})
        };
        let response = json!({"jsonrpc":"2.0","id":r["id"],"result":result});
        if method == "initialize" {
            ([("mcp-session-id", "fixture-session")], Json(response)).into_response()
        } else {
            (
                [("content-type", "text/event-stream")],
                format!("event: message\ndata: {response}\n\n"),
            )
                .into_response()
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let expected = format!("Bearer {}", std::env::var("PATH").unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/mcp", post(endpoint))
                .with_state(expected),
        )
        .await
        .unwrap()
    });
    let dir = tempfile::tempdir().unwrap();
    let m = manager(
        &dir,
        ServerConfig {
            url: Some(url),
            bearer_env: Some("PATH".into()),
            ..Default::default()
        },
    );
    let result = m
        .execute(
            1,
            false,
            false,
            &json!({"action":"call","server":"demo","tool":"echo"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["content"][0]["text"], "HTTP confirmed");
    server.abort();
}
