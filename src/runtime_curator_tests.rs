// Included inside runtime::tests to exercise the production actors and mock wire bodies.

fn curator_test_proposal(id: &str, revision: i64) -> crate::skill_library::Proposal {
    serde_json::from_value(json!({
        "changes":[{"id":id,"expected_revision":revision,"files":{
            "SKILL.md":format!("---\nname: {id}\ndescription: Verify observed deployment results.\n---\nCheck the running executable and service health before reporting success.")
        },"summary":"Added executable and service verification.","purpose":"Use when verifying a deployment."}],
        "task_family":"Deployment verification","triggers":"Deploying a service",
        "procedure":"Inspect executable and service health","variables":"Service and executable",
        "verification":"Check observable process state","limits":"Requires access to the service",
        "reason":"Observed deployment needs concrete verification","evidence":[]
    })).unwrap()
}

fn curator_test_publish(h: &Harness, channel: u64, id: &str, revision: i64) {
    let generation = uuid::Uuid::new_v4().to_string();
    h.skills
        .enqueue_fork(
            &channel.to_string(),
            &generation,
            &json!({"task":"Verify a deployment"}),
        )
        .unwrap();
    let fork = h.skills.take_fork(&channel.to_string()).unwrap().unwrap();
    h.skills
        .publish_fork(
            &fork.id,
            &curator_test_proposal(id, revision),
            &json!({"approved":true}),
            &[],
        )
        .unwrap();
}

async fn curator_test_input_done(h: &Harness, id: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let done: bool = h
                .store
                .db
                .lock()
                .unwrap()
                .query_row("SELECT state='done' AND NOT EXISTS(SELECT 1 FROM ui_agents WHERE owner='channel:'||inbox.channel AND active=1) FROM inbox WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .unwrap();
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("mock root input must settle within bounded deadline");
}

#[tokio::test]
async fn curator_idle_publication_waits_for_real_input_and_preserves_memory_prefix() {
    let (_dir, h, run, mock, server) =
        fixture("openai", vec![final_response("openai", "Ready")]).await;
    h.store.input_state("first", "done").unwrap();
    let c = run.memory.clone();
    let before = c.memory.lock().await.render();
    curator_test_publish(&h, 1, "deployment-checks", 0);
    assert_eq!(c.memory.lock().await.render(), before);
    assert!(h.store.queued(1).unwrap().is_empty());
    assert!(mock.requests.lock().await.is_empty());
    assert_eq!(h.skills.notifications("1").unwrap().len(), 1);
    let actor = tokio::spawn(h.clone().channel_worker(1, c.clone()));
    let input = Input {
        id: "real-trigger".into(),
        channel: 1,
        user: 2,
        text: "Continue the conversation".into(),
    };
    h.admit_prompt(&input).unwrap();
    c.incoming.notify_one();
    mock.started.notified().await;
    {
        let requests = mock.requests.lock().await;
        let blocks = requests[0]["input"][1]["content"].as_array().unwrap();
        assert_eq!(
            blocks[..blocks.len() - 1]
                .iter()
                .map(|b| b["text"].as_str().unwrap())
                .collect::<String>(),
            before
        );
        let incoming = blocks.last().unwrap()["text"].as_str().unwrap();
        assert!(incoming.starts_with("<system-notification>"));
        assert!(incoming.contains("<curator-skill-add name=\"deployment-checks\" revision=\"1\">"));
        assert!(incoming.ends_with("Continue the conversation"));
        assert_eq!(
            h.skills
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"deployment-checks"}))
                .unwrap()["revision"],
            1
        );
    }
    mock.release.notify_one();
    curator_test_input_done(&h, "real-trigger").await;
    assert!(h.skills.notifications("1").unwrap().is_empty());
    h.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), actor)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn curator_catalogue_freezes_all_entries_and_counters_until_next_idle_fresh_turn() {
    let (_dir, h, run, mock, server) =
        fixture("openai", vec![final_response("openai", "Done"); 4]).await;
    h.store.input_state("first", "done").unwrap();
    for index in 0..18 {
        h.skills
            .publish(&curator_test_proposal(&format!("extra-{index:02}"), 0))
            .unwrap();
    }
    let c = run.memory.clone();
    let actor = tokio::spawn(h.clone().channel_worker(1, c.clone()));
    h.admit_prompt(&Input {
        id: "catalogue-a".into(),
        channel: 1,
        user: 2,
        text: "hello".into(),
    })
    .unwrap();
    c.incoming.notify_one();
    mock.started.notified().await;
    mock.release.notify_one();
    curator_test_input_done(&h, "catalogue-a").await;
    h.skills
        .record_invocation("research", "observed-guide-use")
        .unwrap();
    curator_test_publish(&h, 1, "research", 1);
    h.admit_prompt(&Input {
        id: "catalogue-b".into(),
        channel: 1,
        user: 2,
        text: "another hello".into(),
    })
    .unwrap();
    c.incoming.notify_one();
    curator_test_input_done(&h, "catalogue-b").await;
    // Age only this temporary fixture's saved idle timestamp so the production
    // channel_worker refresh path runs without sleeping five minutes.
    rusqlite::Connection::open(h.config.state_dir.join("skills.sqlite"))
        .unwrap()
        .execute(
            "UPDATE skill_channel_settled SET settled=?1 WHERE channel='1'",
            [crate::store::now() - h.config.curator.idle_seconds as i64 - 1],
        )
        .unwrap();
    for id in ["catalogue-c", "catalogue-d"] {
        h.admit_prompt(&Input {
            id: id.into(),
            channel: 1,
            user: 2,
            text: "hello again".into(),
        })
        .unwrap();
        c.incoming.notify_one();
        curator_test_input_done(&h, id).await;
    }
    {
        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 4);
        let systems = requests.iter().map(|r| &r["input"][0]).collect::<Vec<_>>();
        assert!(
            systems[0].to_string().contains("extra-17"),
            "complete catalogue must exceed the old 16-entry cutoff"
        );
        assert_eq!(
            systems[0], systems[1],
            "short-gap publication and invocation counters must not mutate root system prefix"
        );
        assert_ne!(systems[1], systems[2]);
        assert_eq!(systems[2], systems[3]);
        let refreshed = systems[2]["content"].as_str().unwrap();
        assert!(refreshed.contains("\"invocations\":1"));
        assert!(refreshed.contains("\"refinements\":1"));
        for request in requests.iter().skip(1) {
            assert_eq!(request["tools"], requests[0]["tools"]);
        }
    }
    h.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), actor)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.abort();
}

async fn curator_test_active_native_continuity(vendor: &str) {
    let first = if vendor == "openai" {
        json!({"status":"completed","output":[
            {"type":"reasoning","encrypted_content":"native-private-reasoning"},
            {"type":"function_call","call_id":"read-evidence","name":"read","arguments":"{\"path\":\"evidence.txt\"}"},
            {"type":"function_call","call_id":"old-guide","name":"skill","arguments":"{\"action\":\"load\",\"id\":\"research\"}"}
        ]})
    } else {
        json!({"stop_reason":"tool_use","content":[
            {"type":"thinking","thinking":"private thought","signature":"native-private-signature"},
            {"type":"tool_use","id":"read-evidence","name":"read","input":{"path":"evidence.txt"}},
            {"type":"tool_use","id":"old-guide","name":"skill","input":{"action":"load","id":"research"}}
        ]})
    };
    let next = if vendor == "openai" {
        json!({"status":"completed","output":[{"type":"function_call","call_id":"new-guide","name":"skill","arguments":"{\"action\":\"load\",\"id\":\"research\"}"}]})
    } else {
        json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"new-guide","name":"skill","input":{"action":"load","id":"research"}}]})
    };
    let (dir, h, run, mock, server) = fixture(
        vendor,
        vec![
            first.clone(),
            next,
            final_response(vendor, "Reviewed new guide"),
        ],
    )
    .await;
    std::fs::write(
        dir.path().join("evidence.txt"),
        "observable source evidence",
    )
    .unwrap();
    let task = tokio::spawn(h.clone().run_agent(run));
    mock.started.notified().await;
    curator_test_publish(&h, 1, "research", 1);
    mock.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    {
        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 3);
        for request in requests.iter().skip(1) {
            assert_eq!(request["tools"], requests[0]["tools"]);
        }
        if vendor == "openai" {
            for request in requests.iter().skip(1) {
                assert_eq!(request["input"][0], requests[0]["input"][0]);
            }
            let middle = requests[1]["input"].as_array().unwrap();
            assert_eq!(middle[1], requests[0]["input"][1]);
            assert_eq!(&middle[2..5], first["output"].as_array().unwrap());
            assert_eq!(middle[5]["call_id"], "read-evidence");
            assert_eq!(middle[6]["call_id"], "old-guide");
            let old: Value = serde_json::from_str(middle[6]["output"].as_str().unwrap()).unwrap();
            assert_eq!(old["revision"], 1);
            assert!(
                middle[7]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("curator-skill-modify")
            );
            let last = requests[2]["input"].as_array().unwrap();
            assert_eq!(&last[..middle.len()], middle);
            let new: Value =
                serde_json::from_str(last.last().unwrap()["output"].as_str().unwrap()).unwrap();
            assert_eq!(new["revision"], 2);
            assert!(new["text"].as_str().unwrap().contains("running executable"));
        } else {
            for request in requests.iter().skip(1) {
                assert_eq!(request["system"], requests[0]["system"]);
            }
            let middle = requests[1]["messages"].as_array().unwrap();
            assert_eq!(middle[0], requests[0]["messages"][0]);
            assert_eq!(middle[1]["content"], first["content"]);
            let results = middle[2]["content"].as_array().unwrap();
            assert_eq!(results.len(), 2);
            assert_eq!(results[0]["tool_use_id"], "read-evidence");
            assert_eq!(results[1]["tool_use_id"], "old-guide");
            let old: Value = serde_json::from_str(results[1]["content"].as_str().unwrap()).unwrap();
            assert_eq!(old["revision"], 1);
            assert!(
                middle[3]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("curator-skill-modify")
            );
            let last = requests[2]["messages"].as_array().unwrap();
            assert_eq!(&last[..middle.len()], middle);
            let new: Value = serde_json::from_str(
                last.last().unwrap()["content"][0]["content"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(new["revision"], 2);
        }
    }
    server.abort();
}

#[tokio::test]
async fn curator_active_publication_preserves_openai_native_prefix_and_loads_announced_revision() {
    curator_test_active_native_continuity("openai").await;
}

#[tokio::test]
async fn curator_active_publication_preserves_anthropic_signature_and_complete_tool_batch() {
    curator_test_active_native_continuity("anthropic").await;
}

#[tokio::test]
async fn curator_new_prompts_never_cancel_private_channel_jobs() {
    let (_dir, h, _run, _mock, server) = fixture("openai", vec![]).await;
    let first = CancellationToken::new();
    let other = CancellationToken::new();
    h.curators.lock().await.insert(1, first.clone());
    h.curators.lock().await.insert(2, other.clone());
    assert!(
        h.admit_prompt(&Input {
            id: "new-main-work".into(),
            channel: 1,
            user: 2,
            text: "continue".into()
        })
        .unwrap()
    );
    assert!(!first.is_cancelled());
    assert!(!other.is_cancelled());
    server.abort();
}

#[tokio::test]
async fn curator_eligibility_counts_model_iterations_not_simple_chat_messages() {
    let (_dir, h, run, mock, server) =
        fixture("openai", vec![final_response("openai", "Hello")]).await;
    mock.release.notify_one();
    h.clone().run_agent(run).await.unwrap();
    assert!(h.skills.queued_channels().unwrap().is_empty());
    server.abort();
    let call = |id: &str| json!({"status":"completed","output":[{"type":"function_call","call_id":id,"name":"read","arguments":"{\"path\":\"evidence.txt\"}"}]});
    let (dir, h, run, mock, server) = fixture(
        "openai",
        vec![
            call("one"),
            call("two"),
            final_response("openai", "Verified"),
        ],
    )
    .await;
    std::fs::write(dir.path().join("evidence.txt"), "verified evidence").unwrap();
    mock.release.notify_one();
    h.clone().run_agent(run).await.unwrap();
    assert_eq!(h.skills.queued_channels().unwrap(), vec!["1"]);
    let fork = h.skills.take_fork("1").unwrap().unwrap();
    assert_eq!(fork.payload["model_iterations"], 3);
    assert!(h.skills.take_fork("1").unwrap().is_none());
    server.abort();
}

#[tokio::test]
async fn curator_other_channel_runs_while_main_channel_provider_is_busy() {
    let (_dir, h, run, mock, server) =
        fixture("openai", vec![final_response("openai", "Done"); 2]).await;
    let main_memory = run.memory.clone();
    let main = tokio::spawn(h.clone().run_agent(run));
    mock.started.notified().await;
    let before = main_memory.memory.lock().await.export_html();
    assert!(h.store.has_channel_work(1).unwrap());
    let _other = h.channel(2).await.unwrap();
    h.skills
        .enqueue_fork(
            "2",
            "eligible-other-work",
            &json!({"task":"Settled independent channel work","model_iterations":3}),
        )
        .unwrap();
    h.skills.note_channel_settled("2", 0).unwrap();
    let maintenance = tokio::spawn(h.clone().curator_worker());
    h.curator_changed.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if h.skills.channel_status("2").unwrap()["latest"]["status"] == "no_change" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("independent channel must curate without waiting for main provider");
    assert_eq!(main_memory.memory.lock().await.export_html(), before);
    assert!(h.store.has_channel_work(1).unwrap());
    assert!(
        h.skills.queued_channels().unwrap().is_empty(),
        "private model iteration must not enqueue another curator"
    );
    {
        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]["input"]
                .to_string()
                .contains("Settled independent channel work")
        );
        assert!(
            !requests[1]["tools"]
                .to_string()
                .contains("\"name\":\"spawn\"")
        );
    }
    mock.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), main)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    h.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), maintenance)
        .await
        .unwrap()
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn curator_context_bridge_recovers_a_journal_commit_before_ack_without_repeating_note() {
    let (_directory,h,run,_mock,server)=fixture("openai",vec![]).await;
    curator_test_publish(&h,1,"research",1);
    let notes=h.skills.notifications("1").unwrap();assert_eq!(notes.len(),1);
    let text=curator_note(&notes[0]).unwrap();
    let source=format!("curator-note:{}",notes[0]["id"].as_str().unwrap());
    run.memory.memory.lock().await.append_with_id(Kind::User,&text,&source).unwrap();
    let committed=run.memory.memory.lock().await.export_html();
    assert!(h.skill_notifications(1,&run.memory).await.unwrap().is_none());
    assert_eq!(run.memory.memory.lock().await.export_html(),committed);
    assert!(h.skills.notifications("1").unwrap().is_empty());
    assert!(h.store.queued(1).unwrap().is_empty());
    server.abort();
}
