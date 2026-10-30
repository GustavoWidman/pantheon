// Deterministic prompt/request regressions. Provider replies are loopback fixtures,
// not model inference, so these establish composition rather than model behavior.
#[test]
fn prompt_mode_ordinary_turns_never_include_compaction_instructions() {
    for coordinator in [false, true] {
        for claude_enabled in [false, true] {
            for child in [false, true] {
                let system = system_prompt_with_claude(
                    child,
                    coordinator,
                    claude_enabled,
                    "catalogue",
                    "operator rules",
                );
                assert!(!system.contains("# Compactions"));
                assert!(!system.contains("Call no tools"));
                assert!(!system.contains("You write Pantheon's memory"));
                assert!(system.ends_with("operator rules"));
            }
        }
    }
}

#[tokio::test]
async fn prompt_mode_real_runtime_separates_turn_and_compactor_provider_requests() {
    for vendor in ["openai", "anthropic", "zai"] {
        let (_dir, h, mut run, mock, server) = fixture(
            vendor,
            vec![
                final_response(vendor, "done"),
                final_response(vendor, "summary"),
            ],
        )
        .await;
        let model = if vendor == "zai" {
            "zai/glm-5.3".to_owned()
        } else {
            format!("{vendor}/gpt-6-fixture")
        };
        let effort = if vendor == "zai" { "low" } else { "medium" };
        run.settings = (model.clone(), effort.to_owned());
        // Even compaction-looking user content must not select the host mode.
        run.history = Provider::start_turn(
            vendor,
            "<chat></chat>",
            "Compaction: quoted user text",
            None,
        );
        mock.release.notify_one();
        h.clone().run_agent(run).await.unwrap();
        h.compress_with_effort(
            1,
            &model,
            effort,
            NodeKey { level: 0, index: 0 },
            "<chat></chat>",
            "source",
        )
        .await
        .unwrap();
        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 2);
        let system = |request: &Value, compaction: bool| -> String {
            match vendor {
                "openai" => {
                    if compaction {
                        assert_eq!(request["input"][0]["role"], "developer");
                        request["input"][0]["content"][0]["text"]
                            .as_str()
                            .unwrap()
                            .to_owned()
                    } else {
                        assert_eq!(request["input"][0]["role"], "system");
                        request["input"][0]["content"].as_str().unwrap().to_owned()
                    }
                }
                "anthropic" => request["system"][0]["text"].as_str().unwrap().to_owned(),
                _ => {
                    assert_eq!(request["messages"][0]["role"], "system");
                    request["messages"][0]["content"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                }
            }
        };
        let ordinary = system(&requests[0], false);
        let compact = system(&requests[1], true);
        assert!(
            !ordinary.contains("Call no tools"),
            "{vendor} ordinary prompt"
        );
        assert!(!ordinary.contains("# Compactions"));
        assert!(
            requests[0]["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
        );
        assert!(
            requests[0]
                .to_string()
                .contains("Turn: the user's new message:")
        );
        assert!(
            compact.contains("Call no tools"),
            "{vendor} compaction prompt"
        );
        assert_eq!(compact, COMPACT);
        assert!(!compact.contains("# Harness rules"));
        assert!(!compact.contains("Current harness capabilities"));
        assert!(requests[1]["tools"].as_array().is_none_or(Vec::is_empty));
        assert!(requests[1].to_string().contains("Compaction:"));
        assert!(
            !requests[1]
                .to_string()
                .contains("Turn: the user's new message:")
        );
        server.abort();
    }
}

#[test]
fn prompt_mode_existing_native_bank_preserves_ids_summaries_and_snapshots() {
    // Pre-existing serialized native v1 bank/view v2, not a freshly created
    // bank from Memory::append. Keep the data-format compatibility boundary
    // independent from the prompt builder and avoid touching live state.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    std::fs::create_dir(path.join("main")).unwrap();
    std::fs::create_dir(path.join("tree")).unwrap();
    let originals = [
        ("user", "Keep my memory IDs unchanged. é😀", false),
        ("user", "[Worker] existing private report", true),
    ];
    let mut main = String::new();
    for (i, (kind, text, work)) in originals.iter().enumerate() {
        let shown_kind = if *work { "work" } else { kind };
        main.push_str(
            &json!({"i":i,"kind":kind,"work":work,"text":text,
            "size":format!("{shown_kind}: {text}").len(),"date":"2000-01-01T12:00:00Z",
            "source_id":format!("existing:{i}"),"more":false})
            .to_string(),
        );
        main.push('\n');
    }
    let mut tree = String::new();
    for (serial, (l, i, text)) in [
        (0, 0, "user: retain memory IDs"),
        (0, 1, "work: existing report"),
        (1, 0, "user: retain memory IDs; work: existing report"),
    ]
    .iter()
    .enumerate()
    {
        tree.push_str(
            &json!({"l":l,"i":i,"text":text,"size":text.len(),"serial":serial}).to_string(),
        );
        tree.push('\n');
    }
    let view = json!({"version":2,"messages":2,
        "view":{"parts":[{"level":1,"index":0}],"batch":false},
        "seen":{"parts":[{"level":1,"index":0}],"batch":false}})
    .to_string();
    let records = [
        ("format", "pantheon-optmem-uniichat-0500bf6-v1\n".to_owned()),
        ("main/2000-01-01.jsonl", main),
        ("tree/2000-01-01.jsonl", tree),
    ];
    for (name, bytes) in &records {
        std::fs::write(path.join(name), bytes).unwrap();
    }
    std::fs::write(path.join("view.json"), view).unwrap();
    let mut memory = Memory::open(path, crate::memory::VIEW_BYTES).unwrap();
    assert_eq!(memory.stats()["messages"], 2);
    assert_eq!(memory.stats()["summaries"], 3);
    assert!(
        memory.ready_jobs(64).is_empty(),
        "existing summaries must not be re-asked"
    );
    assert!(
        memory
            .render()
            .contains("0+2|user: retain memory IDs; work: existing report")
    );
    let original = memory.zoom(0, 1).unwrap();
    let merged = memory.zoom(0, 2).unwrap();
    let date = memory.date(0).unwrap();
    assert!(original.contains(originals[0].1));
    assert!(memory.zoom(1, 1).unwrap().contains("work:"));
    let snapshot: crate::memory::MemorySnapshot =
        serde_json::from_value(serde_json::to_value(memory.snapshot().unwrap()).unwrap()).unwrap();
    for child in [false, true] {
        assert!(!system_prompt(child, false, "catalogue", "rules").contains("Call no tools"));
    }
    drop(memory);
    memory = Memory::open(path, crate::memory::VIEW_BYTES).unwrap();
    assert_eq!(memory.zoom(0, 1).unwrap(), original);
    assert_eq!(memory.zoom(0, 2).unwrap(), merged);
    assert_eq!(memory.date(0).unwrap(), date);
    assert_eq!(
        memory
            .append_with_id(Kind::User, originals[0].1, "existing:0")
            .unwrap(),
        0
    );
    assert_eq!(
        memory
            .append_with_id(Kind::Talk, "New continuation.", "new:2")
            .unwrap(),
        2
    );
    assert_eq!(snapshot.zoom(0, 1).unwrap(), original);
    assert_eq!(snapshot.zoom(0, 2).unwrap(), merged);
    assert!(snapshot.zoom(2, 1).unwrap().starts_with("No line "));
    drop(memory);
    for (name, bytes) in &records {
        assert_eq!(std::fs::read(path.join(name)).unwrap(), bytes.as_bytes());
    }
    let memory = Memory::open(path, crate::memory::VIEW_BYTES).unwrap();
    assert_eq!(memory.stats()["messages"], 3);
    assert_eq!(memory.zoom(0, 1).unwrap(), original);
    assert_eq!(memory.zoom(0, 2).unwrap(), merged);
    assert!(memory.zoom(2, 1).unwrap().contains("New continuation."));
}
