#[tokio::test]
async fn native_compactor_retries_are_bounded_original_input_only_for_each_transport() {
    for vendor in ["openai", "anthropic", "zai"] {
        let answers: Vec<_> = [900, 800, 700, 650, 600]
            .iter()
            .enumerate()
            .map(|(i, n)| format!("ANSWER_{i}_{}", "x".repeat(*n)))
            .collect();
        let responses = answers.iter().map(|s| final_response(vendor, s)).collect();
        let (_dir, h, _run, mock, server) = fixture(vendor, responses).await;
        mock.release.notify_one();
        let model = if vendor == "zai" {
            "zai/glm-5.3".to_owned()
        } else {
            format!("{vendor}/test")
        };
        let effort = if vendor == "zai" { "low" } else { "medium" };
        let source = "echo: INERT shell touch /must-not-execute; exact original source";
        let output = h
            .compress_with_effort(
                1,
                &model,
                effort,
                NodeKey { level: 0, index: 7 },
                "<chat>\n0+1|known context\n</chat>\n",
                source,
            )
            .await
            .unwrap();
        assert_eq!(output, answers[4]);
        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 5);
        for (i, r) in requests.iter().enumerate() {
            let wire = r.to_string();
            assert!(wire.contains("Compaction: compress message 7"));
            assert!(wire.contains(&"-".repeat(512)));
            assert!(wire.contains(source));
            assert!(r["tools"].as_array().is_none_or(|t| t.is_empty()));
            let messages = if vendor == "openai" {
                r["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|m| m["role"] != "system" && m["role"] != "developer")
                    .collect::<Vec<_>>()
            } else {
                r["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|m| m["role"] != "system" && m["role"] != "developer")
                    .collect::<Vec<_>>()
            };
            assert_eq!(messages.len(), 1);
            assert!(messages.iter().all(|m| m["role"] == "user"));
            for j in 0..answers.len() {
                assert_eq!(
                    wire.contains(&format!("ANSWER_{j}_")),
                    i > 0 && j == i - 1,
                    "{vendor} retry{i} answer{j}"
                );
            }
            assert_eq!(
                wire.matches("Too long: your last line").count(),
                usize::from(i > 0)
            );
            let sys = if vendor == "openai" {
                r["input"][0]["content"][0]["text"].as_str().unwrap().to_owned()
            } else if vendor == "anthropic" {
                r["system"][0]["text"].as_str().unwrap().to_owned()
            } else {
                r["messages"][0]["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned()
            };
            if vendor != "zai" {
                assert!(sys.contains("# Compactions"));
                assert!(sys.contains("Current harness capabilities"));
            }
        }
        server.abort();
    }
}
#[tokio::test]
async fn native_compactor_answered_empty_clips_source_without_retry_or_tools() {
    let (_dir, h, _run, mock, server) = fixture("openai", vec![final_response("openai", "")]).await;
    mock.release.notify_one();
    let source = format!("echo: {}", "😀".repeat(500));
    let output = h
        .compress(
            1,
            "openai/test",
            NodeKey { level: 0, index: 0 },
            "<chat>\n</chat>\n",
            &source,
        )
        .await
        .unwrap();
    assert_eq!(output, crate::memory::compaction_cut(&source));
    assert!(output.len() <= 512);
    assert_eq!(mock.requests.lock().await.len(), 1);
    server.abort();
}
#[tokio::test]
async fn native_compactor_retains_shortest_answer_when_later_answer_is_empty() {
    let (_dir, h, _run, mock, server) = fixture(
        "openai",
        vec![
            final_response("openai", &"x".repeat(600)),
            final_response("openai", ""),
        ],
    )
    .await;
    mock.release.notify_one();
    let output = h
        .compress(
            1,
            "openai/test",
            NodeKey { level: 0, index: 0 },
            "<chat>\n</chat>\n",
            "source",
        )
        .await
        .unwrap();
    assert_eq!(output, "x".repeat(600));
    assert_eq!(mock.requests.lock().await.len(), 2);
    server.abort();
}
#[tokio::test]
async fn native_zoom_utf16_cursor_reaches_actual_root_tool_executor() {
    let (_dir, h, mut run, _mock, server) = fixture("openai", vec![]).await;
    let id = run
        .memory
        .memory
        .lock()
        .await
        .append(Kind::User, "😀Z")
        .unwrap();
    let call = ToolCall {
        id: "native-zoom".into(),
        name: "zoom".into(),
        arguments: json!({"id":id,"n":1,"at":2}).into(),
    };
    assert_eq!(
        h.execute_tool(&mut run, &call).await.unwrap(),
        format!("{id}+0|user: Z")
    );
    let invalid = ToolCall {
        arguments: json!({"id":id,"n":1,"at":1}).into(),
        ..call
    };
    let (_,units)=h.execute_tool_output(&mut run,&invalid,None).await.unwrap();
    assert_eq!(units.unwrap(),format!("{id}+0|user: ").encode_utf16().chain([0xde00,90]).collect::<Vec<_>>());
    server.abort();
}

#[tokio::test]
async fn native_full_zoom_page_survives_root_and_child_tool_delivery() {
    for vendor in ["openai", "anthropic", "zai"] {
        for child in [false, true] {
            let arguments = json!({"id":1,"n":1});
            let calls = match vendor {
                "openai" => {
                    json!({"status":"completed","output":[{"type":"function_call","call_id":"full-page","name":"zoom","arguments":arguments.to_string()}]})
                }
                "anthropic" => {
                    json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"full-page","name":"zoom","input":arguments}]})
                }
                _ => {
                    json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[{"id":"full-page","type":"function","function":{"name":"zoom","arguments":arguments.to_string()}}]}}]})
                }
            };
            let (dir, h, mut run, mock, server) =
                fixture(vendor, vec![calls, final_response(vendor, "done")]).await;
            let mut page = format!(
                "{}CENTRAL_EXACT_HISTORY_SENTINEL{}",
                "a".repeat(14985),
                "b".repeat(14985)
            );
            assert_eq!(page.chars().count(), 30000);
            if child {
                page.replace_range(..5, "\n\"\\😀");
            }
            assert_eq!(page.encode_utf16().count(), 30000);
            let expected = {
                let mut mem = run.memory.memory.lock().await;
                assert_eq!(mem.append(Kind::User, &page).unwrap(), 1);
                mem.zoom(1, 1).unwrap()
            };
            assert!(expected.chars().count() > crate::memory::TOOL_CHARS);
            if child {
                run.owner = "worker-full-page".into();
                run.child = true;
                run.trace = Some(Memory::open(dir.path().join("trace"), 128000).unwrap());
            }
            mock.release.notify_one();
            h.run_steps(&mut run, vendor, &[], "test", "full-page-run")
                .await
                .unwrap();
            let requests = mock.requests.lock().await;
            assert_eq!(requests.len(), 2);
            fn contains_text(value: &Value, expected: &str) -> bool {
                match value {
                    Value::String(s) => s.contains(expected),
                    Value::Array(a) => a.iter().any(|v| contains_text(v, expected)),
                    Value::Object(m) => m.values().any(|v| contains_text(v, expected)),
                    _ => false,
                }
            }
            assert!(
                contains_text(&requests[1], &expected),
                "{vendor} child={child}: page lost on wire"
            );
            assert!(!requests[1].to_string().contains("characters omitted"));
            drop(requests);
            let echoed = if let Some(trace) = &run.trace {
                trace
                    .say(0, trace.stats()["messages"].as_u64().unwrap())
                    .unwrap()
            } else {
                let mem = run.memory.memory.lock().await;
                mem.say(2, mem.stats()["messages"].as_u64().unwrap())
                    .unwrap()
            };
            assert!(
                echoed.contains(&expected),
                "{vendor} child={child}: logged result lost content"
            );
            server.abort();
        }
    }
}
