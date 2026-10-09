#[tokio::test]
async fn provider_request_retries_keep_completed_shell_effects_and_do_not_count_as_model_steps() {
    use axum::response::IntoResponse;
    let (directory, mut h, run, _mock, old_server) = fixture("openai", vec![]).await;
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let received = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let app=Router::new().route("/",post(move |Json(body):Json<Value>| {
        let received=received.clone();
        async move {
            let index={let mut requests=received.lock().await;let i=requests.len();requests.push(body);i};
            match index {
                0=>Json(json!({"status":"completed","output":[{"type":"reasoning","id":"prior","encrypted_content":"unchanged-opaque-reasoning"},{"type":"function_call","call_id":"append-once","name":"shell","arguments":"{\"command\":\"printf x >> effects.txt\"}"}],"usage":{}})).into_response(),
                1=>(axum::http::StatusCode::SERVICE_UNAVAILABLE,"untrusted-provider-body").into_response(),
                2=>Json(final_response("openai","done")).into_response(),
                _=>panic!("unexpected repeated tool/model request"),
            }
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Arc::get_mut(&mut h).unwrap().provider =
        Provider::mock(endpoint).with_request_retries(3, 1).unwrap();
    tokio::time::timeout(Duration::from_secs(5), h.clone().run_agent(run))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(directory.path().join("effects.txt")).unwrap(),
        "x"
    );
    let requests = requests.lock().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1], requests[2]);
    assert_eq!(
        requests[2]["input"][2]["encrypted_content"],
        "unchanged-opaque-reasoning"
    );
    let outputs = requests[2]["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["type"] == "function_call_output")
        .count();
    assert_eq!(outputs, 1);
    // Two completed model iterations qualify as two, despite three HTTP attempts.
    assert_eq!(h.skills.channel_status("1").unwrap()["queued_count"], 0);
    h.shutdown.cancel();
    server.abort();
    old_server.abort();
}
