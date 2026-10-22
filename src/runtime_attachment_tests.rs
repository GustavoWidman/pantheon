#[tokio::test]
async fn root_send_file_snapshots_once_and_worker_cannot_publish() {
    let (dir, h, mut run, _mock, server) = fixture("openai", vec![]).await;
    let path = dir.path().join("report.txt");
    std::fs::write(&path, "original artifact").unwrap();
    let call = ToolCall {
        id: "send-artifact".into(),
        name: "send_file".into(),
        arguments: json!({"path":"report.txt","caption":"Report"}).into(),
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

#[tokio::test]
async fn native_owned_file_delivery_uses_raw_id_not_display_and_reopens_exact_receipts() {
    use crate::provider::{CallId,entry_json::Json as JsJson};
    let(dir,h,mut run,_mock,server)=fixture("openai",vec![]).await;
    let ids=[r#""\ud800""#,r#""\udfff""#,r#""�""#].map(|raw|CallId::from_json(Some(&JsJson::parse(raw).unwrap())).unwrap());
    assert_eq!(ids[0].as_str(),ids[1].as_str());assert_eq!(ids[1].as_str(),ids[2].as_str());
    let mut deliveries=Vec::new();let files=[("high.txt","HIGH"),("low.txt","LOW"),("replacement.txt","REPLACEMENT")];
    for(i,(path,contents))in files.iter().enumerate(){std::fs::write(dir.path().join(path),contents).unwrap();let call=ToolCall{id:ids[i].clone(),name:"send_file".into(),arguments:json!({"path":path}).into()};let result:Value=serde_json::from_str(&h.execute_tool(&mut run,&call).await.unwrap()).unwrap();assert_eq!(result["state"],"queued");deliveries.push(result["delivery_id"].as_str().unwrap().to_owned());}
    assert_eq!(deliveries.iter().collect::<HashSet<_>>().len(),3,"distinct raw UTF16 must never conflate snapshots/outbox rows");
    let scope=run.invocation_scope.clone();let expected_scalar=format!("file-{}",hex::encode(sha2::Sha256::digest(format!("{scope}:�").as_bytes())));assert_eq!(deliveries[2],expected_scalar,"existing scalar delivery identity remains byte-identical");
    std::fs::write(dir.path().join("high.txt"),"CHANGED AFTER SNAPSHOT").unwrap();let repeated=ToolCall{id:ids[0].clone(),name:"send_file".into(),arguments:json!({"path":"high.txt"}).into()};let retry:Value=serde_json::from_str(&h.execute_tool(&mut run,&repeated).await.unwrap()).unwrap();assert_eq!(retry["delivery_id"],deliveries[0]);assert_eq!(retry["bytes"],4);
    let rows=|store:&Store|{let db=store.db.lock().unwrap();let mut q=db.prepare("SELECT id,attachment FROM outbox ORDER BY rowid").unwrap();q.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()};
    let expected=rows(&h.store);assert_eq!(expected.len(),3);assert_eq!(h.attachments.files(1,None).unwrap().len(),3);
    for(i,(id,attachment))in expected.iter().enumerate(){assert_eq!(id,&deliveries[i]);assert_eq!(attachment,id);let file=h.attachments.get(1,attachment).unwrap();assert_eq!(tokio::fs::read(h.attachments.outgoing_path(&file)).await.unwrap(),files[i].1.as_bytes());}
    let attachment_config=h.config.attachments.clone();let state=h.config.state_dir.clone();let workspace=h.config.workspace.clone();drop(run);drop(h);server.abort();
    let store=Store::open(&state.join("runtime.sqlite")).unwrap();let attachments=crate::attachments::Attachments::open(&state,&workspace,attachment_config).unwrap();assert_eq!(rows(&store),expected,"queued-only receipts reopen, no send/replay");assert_eq!(attachments.files(1,None).unwrap().len(),3);
    for(i,id)in deliveries.iter().enumerate(){let file=attachments.get(1,id).unwrap();assert_eq!(tokio::fs::read(attachments.outgoing_path(&file)).await.unwrap(),files[i].1.as_bytes());assert_eq!(format!("file-{}",hex::encode(sha2::Sha256::digest(ids[i].scoped_preimage(&scope)))),*id,"inspect-only recomputation matches persisted receipt identity");}
    eprintln!("MEASURED send_file raw high/low/replacement IDs: three immutable snapshots/outbox rows, repeated same ID one receipt, scalar hash unchanged, exact reopen; no Discord submission");
}
