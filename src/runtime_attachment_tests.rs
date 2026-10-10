#[tokio::test]
async fn root_send_file_snapshots_once_and_worker_cannot_publish() {
    let (dir, h, mut run, _mock, server) = fixture("openai", vec![]).await;
    let path = dir.path().join("report.txt");
    std::fs::write(&path, "original artifact").unwrap();
    let call = ToolCall {
        id: "send-artifact".into(),
        name: "send_file".into(),
        arguments: json!({"path":"report.txt","caption":"Report"}),
    };
    let result: Value =
        serde_json::from_str(&h.execute_tool(&mut run, &call).await.unwrap()).unwrap();
    assert_eq!(result["state"], "queued");
    let out = h.store.next_outbound().unwrap().unwrap();
    assert_eq!(out.reply_to, None);
    assert_eq!(out.attachment.as_deref(), result["delivery_id"].as_str());
    std::fs::write(&path, "changed artifact").unwrap();
    let file = h
        .attachments
        .get(1, out.attachment.as_deref().unwrap())
        .unwrap();
    assert_eq!(
        tokio::fs::read(h.attachments.outgoing_path(&file))
            .await
            .unwrap(),
        b"original artifact"
    );
    h.execute_tool(&mut run, &call).await.unwrap();
    assert_eq!(drain(&h).len(), 1);
    run.child = true;
    assert!(
        h.execute_tool(&mut run, &call)
            .await
            .unwrap_err()
            .to_string()
            .contains("unavailable")
    );
    run.child = false;
    run.cancel.cancel();
    let cancelled = ToolCall {
        id: "cancelled-artifact".into(),
        ..call
    };
    assert!(
        h.execute_tool(&mut run, &cancelled)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert_eq!(h.attachments.files(1, None).unwrap().len(), 1);
    h.shutdown.cancel();
    server.abort();
}
#[tokio::test]
async fn receiving_steer_waits_without_inference_and_appends_native_media_without_rewriting_prefix()
{
    let (_dir, h, mut run, mock, server) = fixture("openai", vec![]).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/picture", axum::routing::get(|| async { "image bytes" }));
    let cdn = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let input = Input {
        id: "100".into(),
        channel: 1,
        user: 2,
        text: "inspect this".into(),
    };
    let descriptor = crate::attachments::IncomingFile {
        id: "200".into(),
        filename: "picture.png".into(),
        content_type: Some("image/png".into()),
        size: 11,
        url: format!("http://{address}/picture"),
    };
    h.attachments.queue(&input, &[descriptor]).unwrap();
    let pending = h.attachments.next(&HashSet::new()).unwrap().unwrap();
    let prefix = run.history.clone();
    let owner = h.clone();
    let memory = run.memory.clone();
    let steering = tokio::spawn(async move {
        assert!(owner.steer(&mut run, "openai").await.unwrap());
        run
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!steering.is_finished());
    assert!(mock.requests.lock().await.is_empty());
    let ready = h
        .attachments
        .receive(&pending, &h.discord, &CancellationToken::new())
        .await
        .unwrap();
    h.store.admit(&ready).unwrap();
    memory.incoming.notify_one();
    let run = tokio::time::timeout(Duration::from_secs(2), steering)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.history[..prefix.len()], prefix);
    let last = run.history.last().unwrap();
    assert!(
        last["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["type"] == "input_image")
    );
    assert!(run.inputs.contains(&"100".into()));
    let history = memory.memory.lock().await.export_html();
    assert!(!history.contains("base64"));
    assert!(!history.contains("http://127.0.0.1"));
    assert!(history.contains("picture.png"));
    h.shutdown.cancel();
    server.abort();
    cdn.abort();
}
#[tokio::test]
async fn pending_input_blocks_completion_and_stop_wins_before_admission() {
    let (_dir, h, run, _mock, server) = fixture("openai", vec![]).await;
    let next = Input {
        id: "100".into(),
        channel: 1,
        user: 2,
        text: "next prompt".into(),
    };
    h.attachments.queue(&next, &[]).unwrap();
    assert!(
        !h.store
            .complete_turn(&run.inputs, "premature", 1, 2, &["done".into()])
            .unwrap()
    );
    assert!(h.store.has_active_work().unwrap());
    h.attachments.cancel_channel(1).unwrap();
    assert!(!h.store.admit(&next).unwrap());
    assert!(h.store.queued(1).unwrap().is_empty());
    assert!(
        h.store
            .complete_turn(&run.inputs, "settled", 1, 2, &["done".into()])
            .unwrap()
    );
    h.shutdown.cancel();
    server.abort();
}
