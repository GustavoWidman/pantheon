#[tokio::test]
async fn global_channels_share_ids_zoom_date_and_keep_routing_private() {
    let (_dir, h, mut root, mock, server) = fixture("openai", vec![]).await;
    let other = h.channel(2).await.unwrap();
    assert!(Arc::ptr_eq(&root.memory.shared, &other.shared));
    assert!(!std::ptr::eq(&root.memory.incoming, &other.incoming));
    assert!(!std::ptr::eq(&root.memory.cancel, &other.cancel));
    assert!(!std::ptr::eq(
        &root.memory.settings_lock,
        &other.settings_lock
    ));
    let input = Input {
        id: "second-channel".into(),
        channel: 2,
        user: 9,
        text: "shared fact".into(),
    };
    Harness::append_input(&other, &input).await.unwrap();
    let (id, zoom, date) = {
        let memory = other.memory.lock().await;
        let id = memory.lookup_source(&input.id).unwrap();
        (id, memory.zoom(id, 1).unwrap(), memory.date(id).unwrap())
    };
    assert_eq!(id, 1);
    assert!(zoom.contains("[channel:2 user:9]"));
    assert!(zoom.contains("shared fact"));
    assert_eq!(root.memory.memory.lock().await.zoom(id, 1).unwrap(), zoom);
    assert_eq!(root.memory.memory.lock().await.date(id).unwrap(), date);
    // Native worker recall intentionally uses the shared root, not trace IDs.
    root.child = true;
    root.trace = Some(Memory::open(h.config.state_dir.join("subagents/test"), 128000).unwrap());
    assert!(root.trace.as_ref().unwrap().zoom(id, 1).is_err());
    assert_eq!(root.memory.memory.lock().await.zoom(id, 1).unwrap(), zoom);
    let call = ToolCall {
        id: "global-zoom".into(),
        name: "zoom".into(),
        arguments: json!({"id":id,"n":1}).into(),
    };
    assert_eq!(h.execute_tool(&mut root, &call).await.unwrap(), zoom);
    let call = ToolCall {
        id: "global-date".into(),
        name: "date".into(),
        arguments: json!({"id":id}).into(),
    };
    assert_eq!(h.execute_tool(&mut root, &call).await.unwrap(), date);
    assert_eq!(root.channel, 1);
    assert_eq!(
        h.store.reply_context(&root.inputs[0]).unwrap().activity,
        root.context.activity
    );
    assert!(mock.requests.lock().await.is_empty());
    h.shutdown.cancel();
    server.abort();
}

#[tokio::test]
async fn global_recovery_is_ordered_and_append_before_ack_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        state_dir: dir.path().join("state"),
        workspace: dir.path().join("workspace"),
        ..Default::default()
    };
    crate::state_namespace::initialize(&config.state_dir).unwrap();
    let mut memory = Memory::open(config.state_dir.join("memory"), 128000).unwrap();
    let store = Store::open(&config.state_dir.join("runtime.sqlite")).unwrap();
    let inputs = [
        Input {
            id: "a".into(),
            channel: 99,
            user: 2,
            text: "first".into(),
        },
        Input {
            id: "b".into(),
            channel: 1,
            user: 2,
            text: "second".into(),
        },
    ];
    for input in &inputs {
        store.admit(input).unwrap();
        store.input_state(&input.id, "cancelled").unwrap();
    }
    // Equal durable timestamps deliberately disagree with channel order.
    store
        .db
        .lock()
        .unwrap()
        .execute("UPDATE inbox SET created=10", [])
        .unwrap();
    memory
        .append_with_id(Kind::User, &attributed_input(&inputs[0]), "a")
        .unwrap();
    // Simulate death after journal append, before mark_logged.
    drop(memory);
    drop(store);
    let discord = Arc::new(Discord::new("mock".into(), 1, vec![2]).unwrap());
    let h = Harness::new(config.clone(), discord.clone(), CancellationToken::new()).unwrap();
    let before = {
        let m = h.shared.memory.lock().await;
        assert_eq!(m.stats()["messages"], 2);
        assert_eq!(m.lookup_source("a"), Some(0));
        assert_eq!(m.lookup_source("b"), Some(1));
        assert!(m.zoom(0, 1).unwrap().contains("[channel:99 user:2]"));
        assert!(m.zoom(1, 1).unwrap().contains("[channel:1 user:2]"));
        m.export_html()
    };
    assert!(h.store.unlogged_global().unwrap().is_empty());
    h.store
        .set_compactor_model(GLOBAL_COMPACTOR, Some("openai/persisted-global"))
        .unwrap();
    h.store
        .set_reasoning_override(GLOBAL_COMPACTOR, "compact", Some("high"))
        .unwrap();
    let identity = h.store.cache_affinity("channel:99").unwrap();
    h.shutdown.cancel();
    drop(h);
    let reopened = Harness::new(config, discord, CancellationToken::new()).unwrap();
    assert_eq!(reopened.shared.memory.lock().await.export_html(), before);
    assert_eq!(
        reopened.store.cache_affinity("channel:99").unwrap(),
        identity
    );
    assert!(!reopened.config.state_dir.join("chats").exists());
    assert_eq!(
        reopened.discord.models.kind_model(99, "compact").as_deref(),
        Some("openai/persisted-global")
    );
    assert_eq!(
        reopened.discord.models.kind_model(1, "compact").as_deref(),
        Some("openai/persisted-global")
    );
    assert_eq!(
        reopened
            .store
            .reasoning_override(GLOBAL_COMPACTOR, "compact")
            .unwrap()
            .as_deref(),
        Some("high")
    );
}

#[tokio::test]
async fn global_compactor_settings_commands_and_context_are_instance_wide() {
    let (_dir, h, _root, mock, server) = fixture("openai", vec![]).await;
    h.discord
        .models
        .replace("openai", vec![crate::models::Model::plain("shared")]);
    h.command(
        1,
        2,
        "model",
        &json!([{"name":"kind","value":"compact"},{"name":"model","value":"openai/shared"}]),
    )
    .await
    .unwrap();
    assert_eq!(
        h.store
            .compactor_model(GLOBAL_COMPACTOR, "fallback")
            .unwrap(),
        "openai/shared"
    );
    assert_eq!(h.store.compactor_model(1, "fallback").unwrap(), "fallback");
    assert_eq!(
        h.discord.models.kind_model(2, "compact").as_deref(),
        Some("openai/shared")
    );
    let shown = h
        .command(2, 2, "model", &json!([{"name":"kind","value":"compact"}]))
        .await
        .unwrap()
        .to_string();
    assert!(shown.contains("openai/shared"));
    assert!(shown.contains("instance-wide"));
    let card = h
        .command(2, 2, "context", &json!([]))
        .await
        .unwrap()
        .to_string();
    assert!(card.contains("128,000 bytes"));
    assert!(card.contains("32,000 bytes"));
    assert!(card.contains("125.00 KiB"));
    assert!(card.contains("31.25 KiB"));
    assert!(card.contains("instance-global"));
    assert!(mock.requests.lock().await.is_empty());
    h.shutdown.cancel();
    server.abort();
}

#[tokio::test]
async fn global_waiting_coordinator_stop_preserves_inputs_without_inference() {
    let (_dir, h, _root, mock, server) = fixture("openai", vec![]).await;
    let permit = h.coordinator_turn.lock().await;
    let input = Input {
        id: "blocked".into(),
        channel: 2,
        user: 2,
        text: "preserve even when stopped".into(),
    };
    h.store.admit(&input).unwrap();
    let c = h.channel(2).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if c.cancel.lock().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    h.command(2, 2, "stop", &json!([])).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if h.store.unlogged_global().unwrap().is_empty()
                && c.memory.lock().await.lookup_source("blocked").is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(h.store.queued(2).unwrap().is_empty());
    assert!(
        c.memory
            .lock()
            .await
            .zoom(1, 1)
            .unwrap()
            .contains("preserve even when stopped")
    );
    assert!(mock.requests.lock().await.is_empty());
    drop(permit);
    h.shutdown.cancel();
    server.abort();
}

#[tokio::test]
async fn global_compactor_drains_idle_reopen_without_any_channel_actor() {
    let (_dir, h, _root, mock, server) =
        fixture("openai", vec![final_response("openai", "settled")]).await;
    h.store
        .set_compactor_model(GLOBAL_COMPACTOR, Some("openai/test"))
        .unwrap();
    // Tiny sources settle locally without an HTTP call; use one long source
    // so this test observes the fake provider instead of waiting on no request.
    h.shared
        .memory
        .lock()
        .await
        .append(Kind::User, &"long fake compaction source ".repeat(80))
        .unwrap();
    // Scheduler must not need a channel actor.
    let worker = tokio::spawn(h.clone().compactor(GLOBAL_COMPACTOR, h.shared.clone()));
    tokio::time::timeout(Duration::from_secs(5), mock.started.notified())
        .await
        .unwrap();
    mock.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if h.shared.memory.lock().await.is_settled() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(mock.requests.lock().await.len(), 1);
    h.shutdown.cancel();
    worker.await.unwrap().unwrap();
    server.abort();
}

#[tokio::test]
async fn global_coordinator_turns_are_mutually_exclusive_and_reply_to_source_channels() {
    let (_dir, h, root, mock, server) = fixture(
        "openai",
        vec![
            final_response("openai", "first-channel-result"),
            final_response("openai", "second-channel-result"),
        ],
    )
    .await;
    h.store.input_state("first", "done").unwrap();
    let worker = tokio::spawn(h.clone().channel_worker(1, root.memory.clone()));
    let compactor = tokio::spawn(h.clone().compactor(GLOBAL_COMPACTOR, h.shared.clone()));
    h.store
        .admit(&Input {
            id: "channel-one".into(),
            channel: 1,
            user: 2,
            text: "first channel question".into(),
        })
        .unwrap();
    root.memory.incoming.notify_one();
    tokio::time::timeout(Duration::from_secs(5), mock.started.notified())
        .await
        .unwrap();
    h.store
        .admit(&Input {
            id: "channel-two".into(),
            channel: 2,
            user: 2,
            text: "second channel question".into(),
        })
        .unwrap();
    let second = h.channel(2).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if second.cancel.lock().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // The real second channel actor is now waiting for the first turn, not
    // capturing a stale view or concurrently issuing a model request.
    assert_eq!(mock.requests.lock().await.len(), 1);
    assert_eq!(
        h.shared.memory.lock().await.lookup_source("channel-two"),
        None
    );
    mock.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let done:i64=h.store.db.lock().unwrap().query_row("SELECT count(*) FROM inbox WHERE id IN ('channel-one','channel-two') AND state='done'",[],|r|r.get(0)).unwrap();
            if done==2 {break;}
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let requests = mock.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]["input"]
            .to_string()
            .contains("first-channel-result")
    );
    assert!(
        requests[1]["input"]
            .to_string()
            .contains("[channel:2 user:2]")
    );
    drop(requests);
    let delivered = drain(&h);
    assert!(
        delivered
            .iter()
            .any(|m| m.channel == 1 && m.text.contains("first-channel-result"))
    );
    assert!(
        delivered
            .iter()
            .any(|m| m.channel == 2 && m.text.contains("second-channel-result"))
    );
    assert!(
        !delivered
            .iter()
            .any(|m| m.channel == 2 && m.text.contains("first-channel-result"))
    );
    assert!(
        !delivered
            .iter()
            .any(|m| m.channel == 1 && m.text.contains("second-channel-result"))
    );
    h.shutdown.cancel();
    worker.await.unwrap().unwrap();
    compactor.await.unwrap().unwrap();
    server.abort();
}
