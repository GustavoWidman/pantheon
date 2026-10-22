// Actual Harness -> native Provider -> loopback SSE, isolated native journals.
#[tokio::test]
async fn native_ordinary_pause_entries_survive_success_refusal_and_native_reopen() {
    for failed in [false, true] {
        let pause = json!({"stop_reason":"pause_turn","content":[{"type":"thinking","thinking":"transient thought","signature":"opaque"},{"type":"text","text":" interim "}],"usage":{"input_tokens":3,"output_tokens":1}});
        let last = if failed {
            json!({"stop_reason":"refusal","content":[{"type":"text","text":"partial failed"}],"usage":{"input_tokens":4,"output_tokens":2}})
        } else {
            json!({"stop_reason":"end_turn","content":[{"type":"text","text":" a"},{"type":"text","text":"b "}],"usage":{"input_tokens":4,"output_tokens":2}})
        };
        let (dir, mut h, mut run, mock, server) = fixture("anthropic", vec![pause, last]).await;
        let provider = h.provider.clone().with_native_transport_for_test();
        Arc::get_mut(&mut h).unwrap().provider = provider;
        mock.release.notify_one();
        let result = h
            .run_steps(
                &mut run,
                "anthropic",
                &[],
                "synthetic system",
                "native-pause",
            )
            .await;
        assert_eq!(result.is_err(), failed);
        if !failed {
            assert_eq!(result.unwrap(), "interim\nab\n");
        }
        let memory = run.memory.memory.lock().await;
        let count = memory.stats()["messages"].as_u64().unwrap();
        let originals = memory.say(0, count).unwrap();
        assert_eq!(originals.matches("interim").count(), 1);
        assert!(!originals.contains("transient thought"));
        assert_eq!(
            originals
                .matches(if failed { "partial failed" } else { "ab" })
                .count(),
            if failed { 0 } else { 1 }
        );
        drop(memory);
        assert_eq!(mock.requests.lock().await.len(), 2);
        let second = mock.requests.lock().await[1].clone();
        assert_eq!(second["messages"][1]["content"][0]["signature"], "opaque");
        drop(run);
        drop(h);
        server.abort();
        let reopened = Memory::open(dir.path().join("state/memory"), 128000).unwrap();
        assert_eq!(reopened.say(0, count).unwrap(), originals);
        assert!(reopened.stats()["journal_healthy"].as_bool().unwrap());
    }
}
#[tokio::test]
async fn native_ordinary_parser_multiblock_and_invalid_tool_inputs_use_error_results() {
    for (vendor, args) in [
        ("openai", json!("")),
        ("openai", json!("{")),
        ("openai", json!("42")),
        ("anthropic", Value::Null),
    ] {
        let first = if vendor == "openai" {
            json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":" a"},{"type":"output_text","text":"b "}]},{"type":"function_call","name":"unknown_test_tool","call_id":"synthetic","arguments":args}],"usage":{}})
        } else {
            json!({"stop_reason":"tool_use","content":[{"type":"text","text":" a"},{"type":"text","text":"b "},{"type":"tool_use","id":"synthetic","name":"unknown_test_tool","input":args}],"usage":{}})
        };
        let (_dir, mut h, mut run, mock, server) =
            fixture(vendor, vec![first, final_response(vendor, "finished")]).await;
        let provider = h.provider.clone().with_native_transport_for_test();
        Arc::get_mut(&mut h).unwrap().provider = provider;
        mock.release.notify_one();
        let result = h
            .run_steps(&mut run, vendor, &[], "synthetic", "native-invalid")
            .await
            .unwrap();
        assert_eq!(result, "ab\nfinished\n");
        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 2);
        let second = requests[1].to_string();
        assert!(!second.contains("_pantheon_tool_inputs"));
        assert!(second.contains("unknown_test_tool is unavailable to this agent"), "known tool lookup must precede input classification: {second}");
        server.abort();
    }
}
#[tokio::test]
async fn native_actual_stream_pause_fail_down_and_partial_cancel_preserve_history() {
    use axum::{body::Body, response::IntoResponse};
    use futures_util::StreamExt;
    for mode in ["refusal", "down", "cancel", "tool-refusal", "tool-cancel"] {
        let (dir, mut h, mut run, _mock, old_server) = fixture("anthropic", vec![]).await;
        old_server.abort();
        let events = |stop: &str, text: &str| {
            vec![
                json!({"type":"message_start","message":{"usage":{"input_tokens":4}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":3}}),
                json!({"type":"message_stop"}),
            ]
        };
        let first = events("pause_turn", " interim ");
        let mut second = events("refusal", " partial ");
        if mode.starts_with("tool-") {
            second.insert(2,json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"partial-inert","name":"shell","input":{}}}));
            second.insert(3,json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}));
        }
        if mode.ends_with("cancel") {
            second.truncate(if mode.starts_with("tool-") { 5 } else { 3 });
        }
        let replies = Arc::new(Mutex::new(std::collections::VecDeque::from([
            first, second,
        ])));
        let seen_first = Arc::new(Notify::new());
        let server_release = seen_first.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let router = Router::new().route(
                "/",
                post(move || {
                    let replies = replies.clone();
                    let release = server_release.clone();
                    async move {
                        let (batch, is_second) = {
                            let mut replies = replies.lock().await;
                            let second = replies.len() == 1;
                            (replies.pop_front().expect("no replay"), second)
                        };
                        if is_second && mode == "down" {
                            return (axum::http::StatusCode::UNAUTHORIZED, "synthetic denied")
                                .into_response();
                        }
                        if is_second && mode.ends_with("cancel") {
                            release.notified().await;
                        }
                        let text = batch
                            .into_iter()
                            .map(|ev| format!("data: {ev}\n\n"))
                            .collect::<String>();
                        let body = if is_second && mode.ends_with("cancel") {
                            Body::from_stream(
                                futures_util::stream::once(async { Ok::<_, std::io::Error>(text) })
                                    .chain(futures_util::stream::pending()),
                            )
                        } else {
                            Body::from(text)
                        };
                        ([("content-type", "text/event-stream")], body).into_response()
                    }
                }),
            );
            axum::serve(listener, router).await.unwrap();
        });
        let probe = Arc::new(Notify::new());
        Arc::get_mut(&mut h).unwrap().provider = Provider::mock(endpoint)
            .with_native_transport_for_test()
            .with_native_trace_probe(probe.clone());
        let cancel = run.cancel.clone();
        let mut future =
            Box::pin(h.run_steps(&mut run, "anthropic", &[], "synthetic", "wire-history"));
        if mode.ends_with("cancel") {
            tokio::select! {
                result=&mut future=>panic!("finished before cancellation {result:?}"),
                _=async{probe.notified().await;seen_first.notify_one();probe.notified().await;}=>{}
            }
            cancel.cancel();
        }
        assert!(future.await.is_err());
        let memory = run.memory.memory.lock().await;
        let count = memory.stats()["messages"].as_u64().unwrap();
        let originals = memory.say(0, count).unwrap();
        drop(memory);
        assert_eq!(originals.matches("interim").count(), 1);
        if mode == "down" {
            assert!(!originals.contains("partial"));
        } else {
            let expected = if mode.ends_with("cancel") {
                "partial\n[cut off: stopped]"
            } else {
                "partial\n[cut off: stopped: refusal]"
            };
            assert_eq!(
                originals.matches(expected).count(),
                1,
                "{mode}: {originals}"
            );
        }
        if mode.starts_with("tool-") {
            let why = if mode.ends_with("cancel") {
                "stopped"
            } else {
                "stopped: refusal"
            };
            assert!(
                originals.contains(&format!("shell {{\"command\":\n[cut off: {why}]")),
                "{mode}: {originals}"
            );
            let count: i64 = h
                .store
                .db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM tool_runs", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                count, 0,
                "inert partial tool observations must never execute"
            );
        }
        drop(run);
        drop(h);
        server.abort();
        let reopened = Memory::open(dir.path().join("state/memory"), 128000).unwrap();
        assert_eq!(reopened.say(0, count).unwrap(), originals);
    }
}

// Pinned OpenAI.read(error) -> step.fail -> Agent.fail(seen), not HTTP busy.
#[tokio::test]
async fn native_openai_error_event_preserves_terminal_reason_and_inert_history_on_reopen() {
    use axum::{body::Body, response::IntoResponse};
    for partial_tool in [false, true] {
        let (dir, mut h, mut run, _mock, old_server) = fixture("openai", vec![]).await;
        old_server.abort();
        let mut events = vec![json!({"type":"response.output_text.delta","delta":" partial "})];
        if partial_tool {
            events.push(json!({"type":"response.output_item.added","item":{"type":"function_call","name":"shell","call_id":"inert"}}));
            events.push(
                json!({"type":"response.function_call_arguments.delta","delta":"{\"command\":"}),
            );
        }
        events.push(
            json!({"type":"error","code":"invalid_prompt","message":"synthetic invalid request"}),
        );
        let text = events
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        let server = tokio::spawn(async move {
            let router = Router::new().route(
                "/",
                post(move || {
                    let text = text.clone();
                    let seen = seen.clone();
                    async move {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        ([("content-type", "text/event-stream")], Body::from(text)).into_response()
                    }
                }),
            );
            axum::serve(listener, router).await.unwrap();
        });
        Arc::get_mut(&mut h).unwrap().provider =
            Provider::mock(endpoint).with_native_transport_for_test();
        assert!(
            h.run_steps(&mut run, "openai", &[], "synthetic", "terminal-event")
                .await
                .is_err()
        );
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "semantic failures must not retry"
        );
        let memory = run.memory.memory.lock().await;
        let n = memory.stats()["messages"].as_u64().unwrap();
        let originals = memory.say(0, n).unwrap();
        drop(memory);
        assert_eq!(
            originals
                .matches("partial\n[cut off: synthetic invalid request]")
                .count(),
            1,
            "{originals}"
        );
        assert!(!originals.contains("busy or unavailable"));
        if partial_tool {
            assert_eq!(
                originals
                    .matches("shell {\"command\":\n[cut off: synthetic invalid request]")
                    .count(),
                1,
                "{originals}"
            );
        }
        let tool_runs: i64 = h
            .store
            .db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM tool_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(tool_runs, 0, "partial tools must stay inert");
        drop(run);
        drop(h);
        server.abort();
        let reopened = Memory::open(dir.path().join("state/memory"), 128000).unwrap();
        assert_eq!(reopened.say(0, n).unwrap(), originals);
        assert!(reopened.stats()["journal_healthy"].as_bool().unwrap());
    }
}

#[tokio::test]
async fn native_completed_openai_tool_original_retains_raw_arguments_not_parsed_json() {
    for name in ["unknown_test_tool", "read"] {
        for raw in [r#"{"z":1, "a":2}"#, "", "{", "null", " ", " \n{\"z\":1}\t "] {
            let call = json!({"status":"completed","output":[{"type":"function_call","name":name,"call_id":"raw","arguments":raw}],"usage":{}});
            let (dir, mut h, mut run, mock, server) = fixture("openai", vec![call, final_response("openai", "finished")]).await;
            let provider = h.provider.clone().with_native_transport_for_test();
            Arc::get_mut(&mut h).unwrap().provider = provider;
            mock.release.notify_one();
            let defs = tools::definitions_with_claude(false, h.config.agent.coordinator_root, h.config.claude.enabled);
            let result = h.run_steps(&mut run, "openai", &defs, "synthetic", "raw-tool").await.unwrap();
            assert_eq!(result, "finished\n");
            let memory = run.memory.memory.lock().await;
            let n = memory.stats()["messages"].as_u64().unwrap();
            // Individual immutable originals, not a renderer that flattens text.
            let bodies = (0..n).map(|i| memory.say(i, i + 1).unwrap()).collect::<Vec<_>>();
            assert_eq!(bodies.iter().filter(|body| body.as_str() == format!("{name} {raw}")).count(), 1, "{bodies:?}");
            let originals = memory.say(0, n).unwrap();
            drop(memory);
            let requests = mock.requests.lock().await;
            assert_eq!(requests.len(), 2);
            let expected = if name == "unknown_test_tool" {
                "unknown_test_tool is unavailable to this agent"
            } else if matches!(raw, "{" | "null" | " ") {
                "The input is not a JSON object."
            } else {
                "missing string path"
            };
            assert!(requests[1].to_string().contains(expected), "known name lookup precedes input validation; {name} raw={raw:?}: {}", requests[1]);
            drop(requests); drop(run); drop(h); server.abort();
            let reopened = Memory::open(dir.path().join("state/memory"), 128000).unwrap();
            assert_eq!(reopened.say(0, n).unwrap(), originals);
            eprintln!("MEASURED raw tool name={name} raw={raw:?} exact original/error-class/C/reopen");
        }
    }
}

// A pre-sorted synthetic fixture remains sorted: the wire, not typed key
// construction or durable serde configuration, owns native Entry enumeration.
#[tokio::test]
async fn native_anthropic_completed_tool_observes_presorted_wire_fixture() {
    let call = json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"typed","name":"unknown_test_tool","input":{"z":1,"a":2}}],"usage":{}});
    let (_dir, mut h, mut run, mock, server) = fixture(
        "anthropic",
        vec![call, final_response("anthropic", "finished")],
    )
    .await;
    let provider = h.provider.clone().with_native_transport_for_test();
    Arc::get_mut(&mut h).unwrap().provider = provider;
    mock.release.notify_one();
    h.run_steps(&mut run, "anthropic", &[], "synthetic", "typed-tool")
        .await
        .unwrap();
    let memory = run.memory.memory.lock().await;
    let n = memory.stats()["messages"].as_u64().unwrap();
    let originals = memory.say(0, n).unwrap();
    assert_eq!(
        originals
            .matches(r#"unknown_test_tool {"a":2,"z":1}"#)
            .count(),
        1
    );
    assert_eq!(mock.requests.lock().await.len(), 2);
    server.abort();
}

// Pinned actual-wire JS oracle through Harness logging and native journal reopen.
// Observation equality is not a claim that typed Uses/continuation have JS types.
#[tokio::test]
async fn native_completed_anthropic_tool_entries_match_js_bytes_and_reopen_36_cases() {
    use axum::{body::Body, response::IntoResponse};
    let fixtures: Value = serde_json::from_str(include_str!("../tests/fixtures/anthropic-entry-bytes-0500bf6.json")).unwrap();
    for case in fixtures["cases"].as_array().unwrap() {
        let (dir, mut h, mut run, _mock, old_server) = fixture("anthropic", vec![]).await;
        old_server.abort();
        let raw = case["raw_input"].as_str().unwrap();
        let start = if case["mode"] == "start" { raw } else { "{}" };
        let mut first = format!("data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{}}}}}}\n\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"synthetic-call\",\"name\":\"synthetic_tool\",\"input\":{start}}}}}\n\n");
        if case["mode"] == "delta" {
            first.push_str(&format!("data: {}\n\n",json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":raw}})));
        }
        first.push_str("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n");
        let last = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n".to_owned();
        let replies = Arc::new(Mutex::new(VecDeque::from([first, last])));
        let requests = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let record = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/",listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let router = Router::new().route("/", post(move |body: axum::body::Bytes| {
                let replies = replies.clone(); let record = record.clone();
                async move {
                    record.lock().await.push(body.to_vec());
                    let text = replies.lock().await.pop_front().expect("no extra dispatch");
                    ([("content-type","text/event-stream")],Body::from(text)).into_response()
                }
            }));
            axum::serve(listener,router).await.unwrap();
        });
        Arc::get_mut(&mut h).unwrap().provider = Provider::mock(endpoint).with_native_transport_for_test();
        h.run_steps(&mut run,"anthropic",&[],"synthetic","js-entry").await.unwrap();
        let memory = run.memory.memory.lock().await;
        let n = memory.stats()["messages"].as_u64().unwrap();
        let bodies = (0..n).map(|i| memory.say(i,i+1).unwrap()).collect::<Vec<_>>();
        let expected = case["expected_tool_entry"].as_str().unwrap();
        assert_eq!(bodies.iter().filter(|body|body.as_str()==expected).count(),1,"{} {} {bodies:?}",case["name"],case["mode"]);
        assert_eq!(expected.len() as u64,case["entry_utf8_bytes"].as_u64().unwrap());
        let originals = memory.say(0,n).unwrap();
        drop(memory);
        let requests = requests.lock().await;
        assert_eq!(requests.len(),2);
        assert!(!std::str::from_utf8(&requests[1]).unwrap().contains("_pantheon_tool_entries"),"local Entry metadata must not enter wire/cache context");
        drop(requests); drop(run); drop(h); server.abort();
        let reopened = Memory::open(dir.path().join("state/memory"),128000).unwrap();
        assert_eq!(reopened.say(0,n).unwrap(),originals);
        assert!(reopened.stats()["journal_healthy"].as_bool().unwrap());
    }
}

// U1: actual provider completion -> owned cancellation -> durable native reopen.
// Side-effect-free unavailable calls exercise dispatch without external actions.
#[tokio::test]
async fn native_completed_call_batch_survives_owned_cancel_before_or_between_uses() {
    for vendor in ["openai", "anthropic"] {
        for before in [0, 1] {
            for child in [false, true] {
                let call = |id: &str, name: &str| if vendor == "openai" {
                    json!({"type":"function_call","call_id":id,"name":name,"arguments":"{}"})
                } else {
                    json!({"type":"tool_use","id":id,"name":name,"input":{}})
                };
                let first = if vendor == "openai" {
                    json!({"status":"completed","output":[call("10","unavailable_first"),call("2","unavailable_second")],"usage":{}})
                } else {
                    json!({"stop_reason":"tool_use","content":[call("10","unavailable_first"),call("2","unavailable_second")],"usage":{}})
                };
                let (dir, mut h, mut run, mock, server) = fixture(vendor, vec![first]).await;
                let arrived = Arc::new(Notify::new());
                let release = Arc::new(Notify::new());
                let owned = Arc::get_mut(&mut h).unwrap();
                owned.provider = owned.provider.clone().with_native_transport_for_test();
                owned.native_before_use = Some((before, arrived.clone(), release.clone()));
                let path = if child {
                    run.child = true;
                    run.owner = "test-child".into();
                    let path = dir.path().join("test-private-trace");
                    run.trace = Some(Memory::open(&path,128000).unwrap());
                    path
                } else { dir.path().join("state/memory") };
                mock.release.notify_one();
                let cancel = run.cancel.clone();
                let owner = h.clone();
                let job = tokio::spawn(async move {
                    let result = owner.run_steps(&mut run,vendor,&[],"synthetic","cancel-completed").await;
                    (run,result)
                });
                tokio::time::timeout(Duration::from_secs(10),arrived.notified()).await.unwrap();
                cancel.cancel(); release.notify_one();
                let (mut run,result) = job.await.unwrap();
                assert!(result.unwrap_err().to_string().contains("cancelled"));
                assert_eq!(mock.requests.lock().await.len(),1,"no redispatch on owned cancel");
                let bodies = if let Some(trace) = &mut run.trace {
                    let n=trace.stats()["messages"].as_u64().unwrap();
                    (0..n).map(|i|trace.say(i,i+1).unwrap()).collect::<Vec<_>>()
                } else {
                    let m=run.memory.memory.lock().await;
                    let n=m.stats()["messages"].as_u64().unwrap();
                    (1..n).map(|i|m.say(i,i+1).unwrap()).collect::<Vec<_>>()
                };
                let typed=if let Some(m)=&run.trace {native_trace_entries(m,0)}else{native_trace_entries(&*run.memory.memory.lock().await,1)};
                let kinds=typed.iter().map(|e|e["kind"].as_str().unwrap()).collect::<Vec<_>>();
                assert_eq!(kinds,if before==0{vec!["tool","tool"]}else{vec!["tool","echo","tool"]});
                if before==0 {
                    assert_eq!(bodies,vec!["unavailable_second {}","unavailable_first {}"],"numeric Object.values order");
                } else {
                    assert_eq!(bodies.len(),3);
                    assert_eq!(bodies[0],"unavailable_first {}");
                    assert!(bodies[1].contains("unavailable_first is unavailable"));
                    assert_eq!(bodies[2],"unavailable_second {}");
                }
                drop(run);drop(h);server.abort();
                let reopened=Memory::open(&path,128000).unwrap();
                let n=reopened.stats()["messages"].as_u64().unwrap();
                let stored=(if child{0}else{1}..n).map(|i|reopened.say(i,i+1).unwrap()).collect::<Vec<_>>();
                assert_eq!(stored,bodies);
                assert_eq!(native_trace_entries(&reopened,if child{0}else{1}),typed,"U1 kinds after reopen");
                assert!(reopened.stats()["journal_healthy"].as_bool().unwrap());
            }
        }
    }
}

#[tokio::test]
async fn native_unanswered_pause_call_flushes_once_on_success_and_failure() {
    for ending in ["success", "failure"] {
        let pause=json!({"stop_reason":"pause_turn","content":[{"type":"tool_use","id":"unanswered","name":"never_executed","input":{}},{"type":"text","text":"interim"}],"usage":{}});
        let last=if ending=="failure" {json!({"stop_reason":"refusal","content":[],"usage":{}})} else {final_response("anthropic","final")};
        let (dir,mut h,mut run,mock,server)=fixture("anthropic",vec![pause,last]).await;
        Arc::get_mut(&mut h).unwrap().provider=h.provider.clone().with_native_transport_for_test();
        mock.release.notify_one();
        let result=h.run_steps(&mut run,"anthropic",&[],"synthetic","pause-unanswered").await;
        assert_eq!(result.is_err(),ending=="failure");
        let memory=run.memory.memory.lock().await;let n=memory.stats()["messages"].as_u64().unwrap();
        let bodies=(1..n).map(|i|memory.say(i,i+1).unwrap()).collect::<Vec<_>>();
        assert_eq!(bodies.iter().filter(|b|b.as_str()=="never_executed {}").count(),1);
        assert!(!bodies.iter().any(|b|b.contains("is unavailable")),"pause Uses must not execute");
        assert_eq!(bodies.last().unwrap(),"never_executed {}");
        let originals=memory.say(0,n).unwrap();drop(memory);drop(run);drop(h);server.abort();
        let reopened=Memory::open(dir.path().join("state/memory"),128000).unwrap();assert_eq!(reopened.say(0,n).unwrap(),originals);
    }
}

#[tokio::test]
async fn independent_completed_server_tool_pair_survives_native_reopen() {
    let first=json!({"stop_reason":"end_turn","content":[
        {"type":"text","text":"before"},
        {"type":"server_tool_use","id":"server-pair","name":"web_search","input":{"query":"synthetic"}},
        {"type":"web_search_tool_result","tool_use_id":"server-pair","content":[]},
        {"type":"text","text":"after"}],"usage":{}});
    let (dir,mut h,mut run,mock,server)=fixture("anthropic",vec![first]).await;
    let provider=h.provider.clone().with_native_transport_for_test();
    Arc::get_mut(&mut h).unwrap().provider=provider;mock.release.notify_one();
    h.run_steps(&mut run,"anthropic",&[],"synthetic","independent-server-pair").await.unwrap();
    let m=run.memory.memory.lock().await;let n=m.stats()["messages"].as_u64().unwrap();
    let originals=m.say(0,n).unwrap();drop(m);drop(run);drop(h);server.abort();
    let reopened=Memory::open(dir.path().join("state/memory"),128000).unwrap();assert_eq!(reopened.say(0,n).unwrap(),originals);
    assert!(originals.contains("before")&&originals.contains("after"));
    assert!(originals.contains(r#"web_search {"query":"synthetic"}"#),"U2: completed server Tool Entry missing after native reopen: {originals}");
    assert!(originals.contains("(no output)"),"U2: server result Echo missing after native reopen: {originals}");
}

#[tokio::test]
async fn native_completed_observer_matches_pinned_chat_buffering_and_reopen() {
    use crate::provider::{CompletedEntry,Observation,ObservationKind};
    let fixture_cases:Value=serde_json::from_str(include_str!("../tests/fixtures/completed-chat-buffering-0500bf6.json")).unwrap();
    for case in fixture_cases["cases"].as_array().unwrap().iter().filter(|c|c["events"].is_array()) {
        for child in [false,true] {
            let (dir,mut h,mut run,_mock,server)=fixture("anthropic",vec![]).await;
            Arc::get_mut(&mut h).unwrap().config.agent.show_reasoning=false;
            let path=if child { run.child=true;run.owner="private-observer".into();let path=dir.path().join("private-observer");run.trace=Some(Memory::open(&path,128000).unwrap());path } else {dir.path().join("state/memory")};
            let completed=case["events"].as_array().unwrap().iter().map(|e|CompletedEntry {
                kind:match e["kind"].as_str().unwrap(){"tool"=>ObservationKind::Tool,"echo"=>ObservationKind::Echo,"mind"=>ObservationKind::Thought,_=>ObservationKind::Reply},
                text:e["text"].as_str().unwrap().into(),of:e["of"].as_str().map(Into::into),
            }).collect();
            let observation=Observation{response:Provider::parse("anthropic",final_response("anthropic","")).unwrap(),intermediate:false,completed:Some(completed),entries:None};
            let mut pending=Vec::new();let mut transcript=String::new();
            h.observe_native_step(&mut run,&observation,&mut transcript,"oracle-buffer",0,0,"fresh_turn",&mut pending).await.unwrap();
            h.flush_native_calls(&mut run,&mut pending).await.unwrap();
            h.flush_native_calls(&mut run,&mut pending).await.unwrap();
            let bodies=if let Some(m)=&run.trace {let n=m.stats()["messages"].as_u64().unwrap();(0..n).map(|i|m.say(i,i+1).unwrap()).collect::<Vec<_>>()}else{let m=run.memory.memory.lock().await;let n=m.stats()["messages"].as_u64().unwrap();(1..n).map(|i|m.say(i,i+1).unwrap()).collect::<Vec<_>>()};
            let expected=if case["name"]=="prototype-setter-call-is-not-own-entry" {
                // Explicit safety difference: do not copy JS prototype-setter
                // data loss. Completed native tool IDs are ordinary keys.
                vec!["prototype".into()]
            } else {case["stored"].as_array().unwrap().iter().map(|e|e["text"].as_str().unwrap().to_owned()).collect::<Vec<_>>()};
            assert_eq!(bodies,expected,"{} child={child}",case["name"]);
            let expected_typed=if case["name"]=="prototype-setter-call-is-not-own-entry" { vec![json!({"kind":"tool","text":"prototype"})] } else { case["stored"].as_array().unwrap().clone() };
            let typed=if let Some(m)=&run.trace {native_trace_entries(m,0)}else{native_trace_entries(&*run.memory.memory.lock().await,1)};
            assert_eq!(typed,expected_typed,"kinds {} child={child}",case["name"]);
            drop(run);drop(h);server.abort();
            let reopened=Memory::open(&path,128000).unwrap();let n=reopened.stats()["messages"].as_u64().unwrap();
            assert_eq!((if child{0}else{1}..n).map(|i|reopened.say(i,i+1).unwrap()).collect::<Vec<_>>(),bodies);
            assert_eq!(native_trace_entries(&reopened,if child{0}else{1}),typed,"reopen kinds");
        }
    }
}

#[tokio::test]
async fn native_interleaved_completed_server_and_client_entries_keep_order_on_cancel() {
    for before in [0,1] {
        let first=json!({"stop_reason":"tool_use","content":[
            {"type":"text","text":" before "},
            {"type":"tool_use","id":"10","name":"unavailable_first","input":{}},
            {"type":"text","text":" between "},
            {"type":"tool_use","id":"2","name":"unavailable_second","input":{}},
            {"type":"server_tool_use","id":"server","name":"web_search","input":{"query":"fake"}},
            {"type":"web_search_tool_result","tool_use_id":"server","content":[]},
            {"type":"text","text":" after "}],"usage":{}});
        let (dir,mut h,mut run,mock,server)=fixture("anthropic",vec![first]).await;
        let arrived=Arc::new(Notify::new());let release=Arc::new(Notify::new());
        let owned=Arc::get_mut(&mut h).unwrap();owned.provider=owned.provider.clone().with_native_transport_for_test();owned.native_before_use=Some((before,arrived.clone(),release.clone()));
        let cancel=run.cancel.clone();mock.release.notify_one();let owner=h.clone();
        let job=tokio::spawn(async move {let result=owner.run_steps(&mut run,"anthropic",&[],"synthetic","ordered-cancel").await;(run,result)});
        tokio::time::timeout(Duration::from_secs(10),arrived.notified()).await.unwrap();cancel.cancel();release.notify_one();
        let (run,result)=job.await.unwrap();assert!(result.is_err());assert_eq!(mock.requests.lock().await.len(),1);
        let m=run.memory.memory.lock().await;let n=m.stats()["messages"].as_u64().unwrap();let bodies=(1..n).map(|i|m.say(i,i+1).unwrap()).collect::<Vec<_>>();
        assert_eq!(&bodies[..5],&["before","between","web_search {\"query\":\"fake\"}","(no output)","after"]);
        if before==0 {assert_eq!(&bodies[5..],&["unavailable_second {}","unavailable_first {}"]);}else{assert_eq!(bodies.len(),8);assert_eq!(bodies[5],"unavailable_first {}");assert!(bodies[6].contains("unavailable_first is unavailable"));assert_eq!(bodies[7],"unavailable_second {}");}
        let typed=native_trace_entries(&m,1);
        let kinds=typed.iter().map(|e|e["kind"].as_str().unwrap()).collect::<Vec<_>>();
        assert_eq!(kinds,if before==0{vec!["unii","unii","tool","echo","unii","tool","tool"]}else{vec!["unii","unii","tool","echo","unii","tool","echo","tool"]});
        let all=m.say(0,n).unwrap();drop(m);drop(run);drop(h);server.abort();let reopened=Memory::open(dir.path().join("state/memory"),128000).unwrap();assert_eq!(reopened.say(0,n).unwrap(),all);
        assert_eq!(native_trace_entries(&reopened,1),typed,"interleaving kinds after reopen");
    }
}

// Actual raw fake wire from the independent pinned Agent.loop + Chat oracle.
// All representable cases must retain the exact stored trace through reopen.
// JS-only cases explicitly expose F2; an error is NOT equivalent observation.
#[tokio::test]
async fn native_completed_server_entries_match_pinned_wire_or_expose_f2() {
    use axum::{body::Body,response::IntoResponse};
    let fixtures:Value=serde_json::from_str(include_str!("../tests/fixtures/completed-server-entries-0500bf6.json")).unwrap();
    let mut matched=0;
    for case in fixtures["cases"].as_array().unwrap() {
        let name=case["name"].as_str().unwrap();
        let vendor=if name.starts_with("openai"){"openai"}else{"anthropic"};
        let (dir,mut h,mut run,_mock,old_server)=fixture(vendor,vec![]).await;old_server.abort();
        let wire=case["wire"].as_str().unwrap().to_owned();
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint=format!("http://{}/",listener.local_addr().unwrap());
        let count=Arc::new(std::sync::atomic::AtomicUsize::new(0));let seen=count.clone();
        let server=tokio::spawn(async move {axum::serve(listener,Router::new().route("/",post(move || {let wire=wire.clone();let seen=seen.clone();async move{seen.fetch_add(1,std::sync::atomic::Ordering::SeqCst);([("content-type","text/event-stream"),("retry-after","0.001")],Body::from(wire)).into_response()}}))).await.unwrap();});
        Arc::get_mut(&mut h).unwrap().provider=Provider::mock(endpoint).with_native_transport_for_test();
        let result=h.run_steps(&mut run,vendor,&[],"synthetic","server-wire").await;
        eprintln!("MEASURED server case={name} result={result:?} attempts={}",count.load(std::sync::atomic::Ordering::SeqCst));
        assert!(result.is_ok(),"{name}: {result:?}");matched+=1;
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst),1,"no redispatch {name}");
        let memory=run.memory.memory.lock().await;let n=memory.stats()["messages"].as_u64().unwrap();
        let stored=(1..n).map(|i|memory.say(i,i+1).unwrap()).collect::<Vec<_>>();
        let expected=case["stored"].as_array().unwrap().iter().map(|e|e["text"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        let typed=native_trace_entries(&memory,1);
        assert_eq!(stored,expected,"{name}");assert_eq!(typed,case["stored"].as_array().unwrap().clone(),"kind/text {name}");
        drop(memory);drop(run);drop(h);server.abort();
        let reopened=Memory::open(dir.path().join("state/memory"),128000).unwrap();
        assert_eq!((1..n).map(|i|reopened.say(i,i+1).unwrap()).collect::<Vec<_>>(),stored,"reopen {name}");
        assert_eq!(native_trace_entries(&reopened,1),typed,"reopen kind/text {name}");
    }
    eprintln!("MEASURED server totals matched={matched}");
    assert_eq!(matched,fixtures["cases"].as_array().unwrap().len(),"every case measured with native kind/body reopen");
}

// Public native reader, including original kinds (not only body equality).
fn native_trace_entries(memory: &Memory, first:u64) -> Vec<Value> {
    let n=memory.stats()["messages"].as_u64().unwrap();
    (first..n).map(|i|{
        let original=memory.zoom(i,1).unwrap();
        let typed=original.strip_prefix(&format!("{i}+0|")).unwrap();
        let (kind,text)=typed.split_once(": ").unwrap();
        json!({"kind":if kind=="talk" {"unii"} else {kind},"text":text})
    }).collect()
}

// Real Harness callback/execution/cleanup batches with queued incoming and
// compactor readers. The test seam yields after the first fsynced append while
// retaining the same mutex; neither contender can observe the half pair.
#[tokio::test]
async fn native_completed_batches_exclude_arrivals_and_compactor_until_all_entries() {
    use std::task::Poll;
    for mode in ["server", "execute", "unanswered"] {
        let first=json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"client","name":"unavailable_pair","input":{}}],"usage":{}});
        let responses=if mode=="execute" {vec![first,final_response("anthropic","done")]}else{vec![]};
        let (dir,mut h,mut run,mock,server)=fixture("anthropic",responses).await;
        let arrived=Arc::new(Notify::new());let release=Arc::new(Notify::new());
        let owner=Arc::get_mut(&mut h).unwrap();owner.native_batch_paused=Some((arrived.clone(),release.clone()));owner.provider=owner.provider.clone().with_native_transport_for_test();
        let channel=run.memory.clone();let owner=h.clone();mock.release.notify_one();
        let writer=tokio::spawn(async move {
            let result=match mode {
                "execute"=>owner.run_steps(&mut run,"anthropic",&[],"synthetic","batch-execute").await.map(|_|()),
                "unanswered"=>owner.flush_native_calls(&mut run,&mut vec![("10".into(),"ten".into()),("2".into(),"two".into())]).await,
                _=>{
                    use crate::provider::{Observation,CompletedEntry,ObservationKind};
                    let observation=Observation{response:crate::provider::Response{native:vec![],texts:vec![],calls:vec![],usage:json!({})},intermediate:false,entries:None,completed:Some(vec![CompletedEntry{kind:ObservationKind::Tool,text:"server {}".into(),of:Some("server".into())},CompletedEntry{kind:ObservationKind::Echo,text:"result".into(),of:Some("server".into())}])};
                    owner.observe_native_step(&mut run,&observation,&mut String::new(),"batch-server",0,0,"main",&mut vec![]).await
                }
            };(run,result)
        });
        tokio::time::timeout(Duration::from_secs(10),arrived.notified()).await.unwrap();
        let mut arrival=Box::pin(channel.memory.lock());
        let mut compactor=Box::pin(channel.memory.lock());
        std::future::poll_fn(|cx| {assert!(arrival.as_mut().poll(cx).is_pending(),"arrival sees half batch {mode}");assert!(compactor.as_mut().poll(cx).is_pending(),"compactor sees half batch {mode}");Poll::Ready(())}).await;
        release.notify_one();
        let mut incoming=arrival.await;incoming.append(Kind::User,"arrival").unwrap();drop(incoming);
        let snapshot=compactor.await;let after=native_trace_entries(&snapshot,1);drop(snapshot);
        let expected=match mode {"server"=>vec![json!({"kind":"tool","text":"server {}"}),json!({"kind":"echo","text":"result"})],"unanswered"=>vec![json!({"kind":"tool","text":"two"}),json!({"kind":"tool","text":"ten"})],_=>{assert_eq!(after[0],json!({"kind":"tool","text":"unavailable_pair {}"}));assert_eq!(after[1]["kind"],"echo");assert!(after[1]["text"].as_str().unwrap().contains("unavailable_pair is unavailable"));after[..2].to_vec()}};
        assert_eq!(&after[..2],expected.as_slice(),"no interposed input {mode}");assert_eq!(after[2],json!({"kind":"user","text":"arrival"}));
        let (run,result)=writer.await.unwrap();result.unwrap();let all=native_trace_entries(&*run.memory.memory.lock().await,1);
        drop(run);drop(h);drop(channel);server.abort();let reopened=Memory::open(dir.path().join("state/memory"),128000).unwrap();assert_eq!(native_trace_entries(&reopened,1),all,"kind/body reopen {mode}");assert!(reopened.stats()["journal_healthy"].as_bool().unwrap());
    }
}

#[tokio::test]
async fn native_completed_batch_storage_failure_stops_prefix_without_alternate_writer() {
    for private in [false,true] {
        let (dir,h,mut run,_mock,server)=fixture("anthropic",vec![]).await;
        let path=if private {let path=dir.path().join("private-batch");run.trace=Some(Memory::open(&path,128000).unwrap());run.child=true;path}else{dir.path().join("state/memory")};
        std::fs::create_dir(path.join("view.json.new")).unwrap();
        let error=h.log_run_batch(&mut run,&[(Kind::Tool,"durable call".into()),(Kind::Echo,"not admitted".into())]).await.unwrap_err();
        assert!(error.to_string().contains("checkpoint"),"{error:#}");
        assert!(h.log_run(&mut run,Kind::Echo,"no alternate writer").await.is_err());
        let healthy=if let Some(trace)=&run.trace{trace.stats()["journal_healthy"].as_bool().unwrap()}else{run.memory.memory.lock().await.stats()["journal_healthy"].as_bool().unwrap()};assert!(!healthy);
        drop(run);drop(h);server.abort();std::fs::remove_dir(path.join("view.json.new")).unwrap();
        let reopened=Memory::open(path,128000).unwrap();assert_eq!(native_trace_entries(&reopened,if private{0}else{1}),vec![json!({"kind":"tool","text":"durable call"})]);
    }
}

// Independent pinned ordinary Agent.loop/Chat expectations, not the server
// helper. Exercise the actual Harness callback and unchanged native reader.
#[tokio::test]
async fn native_ordinary_ecmascript_whitespace_matches_pinned_wire_and_reopen() {
    use axum::{body::Body, response::IntoResponse};
    let fixtures: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/ordinary-whitespace-0500bf6.json"
    )).unwrap();
    for case in fixtures["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let vendor = if name.starts_with("openai") { "openai" } else { "anthropic" };
        for private in [false, true] {
            let (dir, mut h, mut run, _mock, old_server) = fixture(vendor, vec![]).await;
            old_server.abort();
            let path = if private {
                let path = dir.path().join("ordinary-private");
                run.trace = Some(Memory::open(&path, 128000).unwrap());
                run.child = true;
                path
            } else { dir.path().join("state/memory") };
            let wire = case["wire"].as_str().unwrap().to_owned();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/", listener.local_addr().unwrap());
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let seen = count.clone();
            let server = tokio::spawn(async move {
                axum::serve(listener, Router::new().route("/", post(move || {
                    let wire = wire.clone(); let seen = seen.clone();
                    async move {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        ([("content-type", "text/event-stream")], Body::from(wire)).into_response()
                    }
                }))).await.unwrap();
            });
            Arc::get_mut(&mut h).unwrap().provider = Provider::mock(endpoint).with_native_transport_for_test();
            let result = h.run_steps(&mut run, vendor, &[], "synthetic", "ordinary-space").await.unwrap();
            assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1, "{name} private={private}");
            let first = if private { 0 } else { 1 };
            let stored = if let Some(trace) = &run.trace {
                native_trace_entries(trace, first)
            } else {
                native_trace_entries(&*run.memory.memory.lock().await, first)
            };
            assert_eq!(stored, *case["stored"].as_array().unwrap(), "{name} private={private}");
            assert_eq!(result, if name.ends_with("nel") {
                if private { "\u{0085}" } else { "\u{0085}\n" }
            } else { "" }, "final response {name} private={private}");
            if !private {
                let output: Vec<String> = {
                    let db = h.store.db.lock().unwrap();
                    let mut query = db.prepare("SELECT text FROM outbox ORDER BY rowid").unwrap();
                    query.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
                };
                assert_eq!(output, if name.ends_with("nel") { vec!["<@2> \u{0085}".to_owned()] } else { vec![] }, "output bytes {name}");
            }
            drop(run); drop(h); server.abort();
            let reopened = Memory::open(path, 128000).unwrap();
            assert_eq!(native_trace_entries(&reopened, first), stored, "reopen {name} private={private}");
        }
    }
}

#[tokio::test]
async fn native_owned_utf16_presentation_failure_cancel_kind_body_reopen() {
    use axum::{body::Body,response::IntoResponse};
    use futures_util::StreamExt;
    use crate::provider::NativeValue;
    let corpus=NativeValue::parse(include_str!("../tests/fixtures/presentation-utf16-0500bf6.json")).unwrap();
    let mut scenarios=Vec::<(String,String,String,Vec<Value>)>::new();
    for case in corpus.field("records").unwrap().elements() {
        let name=case["name"].as_str().unwrap();
        if name.starts_with("citation") {
            let raw=case["raw"].as_str().unwrap();
            let wire=format!(r#"data: {{"type":"response.completed","response":{{"status":"completed","output":[{raw}],"usage":{{}}}}}}

"#);
            scenarios.push((name.into(),"openai".into(),wire,case["wellformed"].as_array().unwrap().clone()));
        }
    }
    for (name,vendor,terminal,why) in [
        ("refusal-high","openai",r#"{"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"\ud800"}]}]}"#,"refused: �"),
        ("incomplete-low","openai",r#"{"status":"incomplete","incomplete_details":{"reason":"\udfff"},"output":[]}"#,"incomplete: �"),
        ("explanation-high","anthropic",r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_details":{"explanation":"\ud800"}}}"#,"stopped: max_tokens [redacted])"),
        ("explanation-empty","anthropic",r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_details":{"explanation":""}}}"#,"stopped: max_tokens"),
        ("cancel-openai","openai","","stopped"),
        ("cancel-anthropic","anthropic","","stopped"),
    ] {
        let wire=if vendor=="openai" {
            let mut wire="data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\\ud83d\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"\\ude00b\"}\n\n".to_owned();
            if !terminal.is_empty(){wire.push_str(&format!("data: {{\"type\":\"response.completed\",\"response\":{terminal}}}\n\n"));}wire
        }else{
            let mut wire="data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a\\ud83d\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"\\ude00b\"}}\n\n".to_owned();
            if !terminal.is_empty(){wire.push_str(&format!("data: {terminal}\n\ndata: {{\"type\":\"message_stop\"}}\n\n"));}wire
        };
        scenarios.push((name.into(),vendor.into(),wire,vec![json!({"kind":"unii","text":format!("a😀b\n[cut off: {why}]")})]));
    }
    for (name,blocks,expected) in [
        ("joined-ignored-block",r#"[{"type":"text","text":"a\ud83d"},{"type":"redacted_thinking","data":"opaque"},{"type":"text","text":"\ude00b"}]"#,vec![json!({"kind":"unii","text":"a😀b"})]),
        ("server-stdout-pair",r#"[{"type":"code_execution_tool_result","tool_use_id":"s","content":{"stdout":"\ud83d","stderr":"\ude00"}}]"#,vec![json!({"kind":"echo","text":"😀"})]),
        ("server-stderr-overflow",r#"[{"type":"code_execution_tool_result","tool_use_id":"s","content":{"stdout":"x","stderr":1e400}}]"#,vec![json!({"kind":"echo","text":"xInfinity"})]),
    ] {
        let blocks: Vec<Box<serde_json::value::RawValue>>=serde_json::from_str(blocks).unwrap();
        let mut wire="data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n".to_owned();
        for(i,block) in blocks.iter().enumerate(){wire.push_str(&format!("data: {{\"type\":\"content_block_start\",\"index\":{i},\"content_block\":{block}}}\n\n"));}
        wire.push_str("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n");
        scenarios.push((name.into(),"anthropic".into(),wire,expected));
    }
    for (name,vendor,wire,expected) in scenarios {
        let(dir,mut h,mut run,_mock,old_server)=fixture(&vendor,vec![]).await;old_server.abort();
        let cancelled=name.starts_with("cancel");let failed=!(name.starts_with("citation") || name.starts_with("joined") || name.starts_with("server"));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let endpoint=format!("http://{}/",listener.local_addr().unwrap());
        let server=tokio::spawn(async move {axum::serve(listener,Router::new().route("/",post(move || {let wire=wire.clone();async move{
            let body=if cancelled {Body::from_stream(futures_util::stream::once(async{Ok::<_,std::io::Error>(wire)}).chain(futures_util::stream::pending()))}else{Body::from(wire)};
            ([("content-type","text/event-stream")],body).into_response()
        }}))).await.unwrap();});
        let probe=Arc::new(Notify::new());Arc::get_mut(&mut h).unwrap().provider=Provider::mock(endpoint).with_native_transport_for_test().with_native_trace_probe(probe.clone());
        let stop=run.cancel.clone();let mut future=Box::pin(h.run_steps(&mut run,&vendor,&[],"fixture","utf16-owned"));
        if cancelled {tokio::select!{r=&mut future=>panic!("finished before stop {name} {r:?}"),_=probe.notified()=>{}}stop.cancel();}
        let result=future.await;assert_eq!(result.is_err(),failed,"{name}: {result:?}");
        let memory=run.memory.memory.lock().await;let stored=native_trace_entries(&memory,1);assert_eq!(stored,expected,"{name}");drop(memory);
        let n:i64=h.store.db.lock().unwrap().query_row("SELECT count(*) FROM tool_runs",[],|row|row.get(0)).unwrap();assert_eq!(n,0,"inert observations never execute");
        drop(run);drop(h);server.abort();let reopened=Memory::open(dir.path().join("state/memory"),128000).unwrap();assert_eq!(native_trace_entries(&reopened,1),stored,"kind/body reopen {name}");
        eprintln!("MEASURED UTF16 {name} actual-harness kind/body/reopen inert=true");
    }
}

#[tokio::test]
async fn native_owned_root_worker_duplicate_ids_retry_and_real_stdio_rpc() {
    use axum::{body::{Body,Bytes},response::IntoResponse};
    use crate::provider::NativeValue;
    for vendor in ["anthropic","openai"] {
    let(dir,mut h,mut root,_mock,old_server)=fixture(vendor,vec![]).await;old_server.abort();
    let peer=r#"import sys,json
for line in sys.stdin:
 with open('owned-root-worker-rpc','ab') as f:f.write(line.encode())
 r=json.loads(line)
 if 'id' not in r:continue
 if r['method']=='initialize':v={'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
 else:
  a=r['params']['arguments'];v={'content':[{'type':'text','text':'ack-'+a['who']+'-'+str(a['slot'])}]}
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':v}),flush=True)
"#;
    let rpc_config=crate::mcp::McpConfig{servers:std::collections::BTreeMap::from([("demo".into(),crate::mcp::ServerConfig{command:Some("python3".into()),args:vec!["-u".into(),"-c".into(),peer.into()],..Default::default()})])};
    Arc::get_mut(&mut h).unwrap().mcp=crate::mcp::Mcp::new(&rpc_config,dir.path(),dir.path()).unwrap();
    let captures=Arc::new(Mutex::new(Vec::<(String,Vec<u8>)>::new()));let record=captures.clone();let counts=Arc::new(Mutex::new(std::collections::HashMap::<String,usize>::new()));
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let endpoint=format!("http://{}/",listener.local_addr().unwrap());
    let server=tokio::spawn(async move{axum::serve(listener,Router::new().route("/",post(move|body:Bytes|{let record=record.clone();let counts=counts.clone();async move{
        let raw=NativeValue::parse(std::str::from_utf8(&body).unwrap()).unwrap();let route=if raw.canonical().contains("ROOT_ROUTE"){"ROOT"}else{"WORKER"};record.lock().await.push((route.into(),body.to_vec()));
        let step={let mut counts=counts.lock().await;let n=counts.entry(route.into()).or_default();let at=*n;*n+=1;at};
        let retry=route=="ROOT"&&step==0;let stage=step.saturating_sub(usize::from(route=="ROOT"));
        let args=|slot:usize,unit:&str|format!(r#"{{"action":"call","server":"demo","tool":"echo","arguments":{{"who":"{route}","slot":{slot},"x":"{unit}","n":1e400}}}}"#);
        let wire=if vendor=="openai" {
            if retry { "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"same\",\"name\":\"mcp\",\"arguments\":\"MUST_NOT_EXECUTE\"}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"server_error\"},\"output\":[]}}\n\n".into() }
            else if stage<2 {
                let slots=if stage==0 {vec![(1,"\\ud800"),(2,"\\udfff")]} else {vec![(3,"\\ud800")]};
                let output=slots.iter().map(|(slot,unit)|json!({"type":"function_call","call_id":"same","name":"mcp","arguments":args(*slot,unit)})).collect::<Vec<_>>();
                format!("data: {}\n\n",json!({"type":"response.completed","response":{"status":"completed","output":output,"usage":{}}}))
            } else {format!("data: {}\n\n",json!({"type":"response.completed","response":final_response("openai","done")}))}
        } else {
        let mut wire="data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n".to_owned();
        if retry {
            wire.push_str(r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"same","name":"mcp","input":{"action":"call","server":"demo","tool":"echo","arguments":{"who":"MUST_NOT_EXECUTE"}}}}

data: {"type":"error","error":{"type":"overloaded_error"}}

"#);
        } else if stage<2 {
            let slots=if stage==0{vec![(1,"\\ud800"),(2,"\\udfff")]}else{vec![(3,"\\ud800")]};
            for(index,(slot,unit))in slots.iter().enumerate(){
                let input=if *slot==2{r#"{"WRONG_INITIAL_INPUT":true}"#.into()}else{args(*slot,unit)};
                wire.push_str(&format!("data: {{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"same\",\"name\":\"mcp\",\"input\":{input}}}}}\n\n"));
                if *slot==2{wire.push_str(&format!("data: {}\n\n",json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":args(*slot,unit)}})));}
            }
            wire.push_str("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n");
        }else{wire.push_str("data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"done\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n");}
            wire
        };
        ([("content-type","text/event-stream"),("retry-after","0.001")],Body::from(wire)).into_response()
    }}))).await.unwrap();});
    Arc::get_mut(&mut h).unwrap().provider=Provider::mock(endpoint).with_native_transport_for_test();
    let path=dir.path().join("owned-worker-trace");
    let mut worker=Run{channel:root.channel,user:root.user,owner:"owned-worker".into(),child:true,memory:root.memory.clone(),cancel:h.shutdown.child_token(),settings:root.settings.clone(),history:Provider::start(vendor,"<chat></chat>","worker task"),inputs:vec![],trace:Some(Memory::open(&path,128000).unwrap()),steering:vec![],media:vec![],context:root.context.clone(),skills:root.skills.clone(),task:"worker task".into(),system:None,steps:0,invocation_scope:uuid::Uuid::new_v4().to_string(),counted_skills:HashSet::new()};
    let root_defs=tools::definitions_with_claude(false,false,false);let worker_defs=tools::definitions_with_claude(true,false,false);
    let(a,b)=tokio::join!(h.run_steps(&mut root,vendor,&root_defs,"ROOT_ROUTE","owned-root"),h.run_steps(&mut worker,vendor,&worker_defs,"WORKER_ROUTE","owned-worker"));assert_eq!(a.unwrap(),"done\n");assert_eq!(b.unwrap(),"done");
    let records=captures.lock().await;let root_requests=records.iter().filter(|(who,_)|who=="ROOT").collect::<Vec<_>>();assert_eq!(root_requests.len(),4);assert_eq!(root_requests[0].1,root_requests[1].1,"passing retry sends identical prepared bytes");assert_eq!(records.iter().filter(|(who,_)|who=="WORKER").count(),3);
    for who in ["ROOT","WORKER"] {
        let selected=records.iter().filter(|(route,_)|route==who).collect::<Vec<_>>();
        let last=std::str::from_utf8(&selected.last().unwrap().1).unwrap();
        for(slot,unit)in[(1,"\\ud800"),(2,"\\udfff"),(3,"\\ud800")] {
            let args=format!(r#"{{"action":"call","server":"demo","tool":"echo","arguments":{{"who":"{who}","slot":{slot},"x":"{unit}","n":1e400}}}}"#);
            let expected=if vendor=="openai"{format!("\"arguments\":{}",serde_json::to_string(&args).unwrap())}else{format!("\"input\":{}",args.replace("1e400","null"))};
            assert!(last.contains(&expected),"C {vendor} {who} slot={slot} expected={expected} actual={last}");
        }
        let(one,two,three)=(format!("ack-{who}-1"),format!("ack-{who}-2"),format!("ack-{who}-3"));
        assert!(last.find(&one).unwrap()<last.find(&two).unwrap());assert!(last.find(&two).unwrap()<last.find(&three).unwrap(),"ordered duplicate-ID result association");
    }
    drop(records);
    let rpc=std::fs::read_to_string(dir.path().join("owned-root-worker-rpc")).unwrap();let calls=rpc.lines().filter(|line|line.contains("tools/call")).collect::<Vec<_>>();assert_eq!(calls.len(),6);assert!(!rpc.contains("MUST_NOT_EXECUTE"));assert!(!rpc.contains("WRONG_INITIAL_INPUT"));
    for who in ["ROOT","WORKER"]{for(slot,unit)in[(1,"\\ud800"),(2,"\\udfff"),(3,"\\ud800")]{let expected=format!(r#""arguments":{{"who":"{who}","slot":{slot},"x":"{unit}","n":null}}"#);assert_eq!(calls.iter().filter(|line|line.contains(&expected)).count(),1,"B actual child writer {expected} {rpc}");}}
    let root_trace=native_trace_entries(&*root.memory.memory.lock().await,0);let private_trace=native_trace_entries(worker.trace.as_ref().unwrap(),0);assert!(!serde_json::to_string(&root_trace).unwrap().contains("WORKER"));assert!(!serde_json::to_string(&private_trace).unwrap().contains("ROOT"));
    let expected_trace=|who:&str| {
        let tool=|slot,unit| {let args=format!(r#"{{"action":"call","server":"demo","tool":"echo","arguments":{{"who":"{who}","slot":{slot},"x":"{unit}","n":1e400}}}}"#);format!("mcp {}",if vendor=="openai"{args}else{args.replace("1e400","null")})};
        let echo=|slot|json!({"kind":"echo","text":json!({"content":[{"type":"text","text":format!("ack-{who}-{slot}")}]}).to_string()});
        vec![json!({"kind":"tool","text":tool(2,"\\udfff")}),echo(1),echo(2),json!({"kind":"tool","text":tool(3,"\\ud800")}),echo(3),json!({"kind":"unii","text":"done"})]
    };
    let mut expected_root=vec![json!({"kind":"user","text":"[channel:1 user:2]\ndo work"})];expected_root.extend(expected_trace("ROOT"));
    assert_eq!(root_trace,expected_root,"complete ordered root kind/body, duplicate-ID last-buffer semantics");
    assert_eq!(private_trace,expected_trace("WORKER"),"complete ordered private kind/body");
    drop(root);drop(worker);drop(h);server.abort();let reopened=Memory::open(dir.path().join("state/memory"),128000).unwrap();assert_eq!(native_trace_entries(&reopened,0),root_trace);let private=Memory::open(path,128000).unwrap();assert_eq!(native_trace_entries(&private,0),private_trace);
    eprintln!("MEASURED concurrent native vendor={vendor} ROOT/WORKER A/B stdio/C, duplicate IDs within/across steps, replacement, passing-retry isolation, kind/body/reopen");
    }
}

#[tokio::test]
async fn native_owned_builtin_consumed_scalar_only_and_exact_reopen() {
    use axum::{body::{Body,Bytes},response::IntoResponse};
    for vendor in ["openai","anthropic"] {
        let(dir,mut h,mut run,_mock,old_server)=fixture(vendor,vec![]).await;old_server.abort();std::fs::write(dir.path().join("evidence.txt"),"fixture evidence").unwrap();
        let arguments=[r#"{"path":"evidence.txt","unused":{"x":"\ud800","n":1e400}}"#,r#"{"path":"\udfff","unused":{"n":1e400}}"#];
        let first=if vendor=="openai"{let calls=arguments.iter().map(|input|json!({"type":"function_call","call_id":"same","name":"read","arguments":input})).collect::<Vec<_>>();format!("data: {}\n\n",json!({"type":"response.completed","response":{"status":"completed","output":calls,"usage":{}}}))}else{format!(r#"data: {{"type":"message_start","message":{{"usage":{{}}}}}}

data: {{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"same","name":"read","input":{}}}}}

data: {{"type":"content_block_start","index":1,"content_block":{{"type":"tool_use","id":"same","name":"read","input":{}}}}}

data: {{"type":"message_delta","delta":{{"stop_reason":"tool_use"}}}}

data: {{"type":"message_stop"}}

"#,arguments[0],arguments[1])};
        let last=if vendor=="openai"{format!("data: {}\n\n",json!({"type":"response.completed","response":final_response(vendor,"done")}))}else{"data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n".into()};
        let replies=Arc::new(Mutex::new(VecDeque::from([first,last])));let captured=Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));let seen=captured.clone();
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("http://{}/",listener.local_addr().unwrap());
        let server=tokio::spawn(async move{axum::serve(listener,Router::new().route("/",post(move|bytes:Bytes|{let seen=seen.clone();let replies=replies.clone();async move{seen.lock().await.push(bytes.to_vec());let wire=replies.lock().await.pop_front().unwrap();([("content-type","text/event-stream")],Body::from(wire)).into_response()}}))).await.unwrap();});
        Arc::get_mut(&mut h).unwrap().provider=Provider::mock(url).with_native_transport_for_test();
        h.run_steps(&mut run,vendor,&tools::definitions_with_claude(false,false,false),"fixture","builtin-owned").await.unwrap();
        let requests=captured.lock().await;assert_eq!(requests.len(),2);let next=std::str::from_utf8(&requests[1]).unwrap();assert!(next.contains("fixture evidence"),"irrelevant JS-only fields must not reject read");assert!(next.contains("builtin argument path cannot be represented"),"consumed unpaired path fails explicitly before filesystem use");drop(requests);
        assert_eq!(std::fs::read_to_string(dir.path().join("evidence.txt")).unwrap(),"fixture evidence");let trace=native_trace_entries(&*run.memory.memory.lock().await,0);drop(run);drop(h);server.abort();let reopened=Memory::open(dir.path().join("state/memory"),128000).unwrap();assert_eq!(native_trace_entries(&reopened,0),trace);
        eprintln!("MEASURED builtin vendor={vendor} irrelevant nested UTF16/overflow accepted, consumed path rejected, C actual-http, kind/body/reopen");
    }
}

#[tokio::test]
async fn native_owned_utf16_call_ids_keep_raw_association_back_ids_cancel_and_reopen() {
    use axum::{body::{Body,Bytes},response::IntoResponse};
    use crate::provider::NativeValue;
    for vendor in ["openai","anthropic"] {
        for outcome in ["done","cancel","failure"] {
            let cancel_before_use=outcome=="cancel";let terminal_failure=outcome=="failure";
            let(dir,mut h,mut run,_mock,old_server)=fixture(vendor,vec![]).await;old_server.abort();
            let ids=[r#""\ud800""#,r#""\udfff""#,r#""�""#,r#""\ud800""#,r#""\ud83d\ude00""#,r#""😀""#,r#""ascii""#];
            let names=["unknown1","unknown2","unknown3","unknown4","unknown5","unknown6","�"];
            let mut blocks=Vec::new();
            for(i,id)in ids.iter().enumerate(){let name=if i==6{r#""\ud800""#.to_owned()}else{serde_json::to_string(names[i]).unwrap()};let arguments=format!("{{\"slot\":{}}}",i+1);blocks.push(if vendor=="openai"{format!(r#"{{"type":"function_call","call_id":{id},"name":{name},"arguments":{}}}"#,serde_json::to_string(&arguments).unwrap())}else{format!(r#"{{"type":"tool_use","id":{id},"name":{name},"input":{arguments}}}"#)});}
            let first=if vendor=="openai"{format!("data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\",\"output\":[{}],\"usage\":{{}}}}}}\n\n",blocks.join(","))}else{let mut wire="data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n".to_owned();for(i,b)in blocks.iter().enumerate(){wire.push_str(&format!("data: {{\"type\":\"content_block_start\",\"index\":{i},\"content_block\":{b}}}\n\n"));}wire.push_str("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n");wire};
            let last=if terminal_failure {if vendor=="openai"{"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"refusal\",\"refusal\":\"fixture refusal\"}]}],\"usage\":{}}}\n\n".into()}else{"data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n".into()}}else if vendor=="openai"{format!("data: {}\n\n",json!({"type":"response.completed","response":final_response(vendor,"done")}))}else{"data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"done\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n".into()};
            let captures=Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));let seen=captures.clone();let replies=Arc::new(Mutex::new(VecDeque::from([first,last])));
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("http://{}/",listener.local_addr().unwrap());
            let server=tokio::spawn(async move{axum::serve(listener,Router::new().route("/",post(move|bytes:Bytes|{let seen=seen.clone();let replies=replies.clone();async move{seen.lock().await.push(bytes.to_vec());let wire=replies.lock().await.pop_front().unwrap();([("content-type","text/event-stream")],Body::from(wire)).into_response()}}))).await.unwrap();});
            let owner=Arc::get_mut(&mut h).unwrap();owner.provider=Provider::mock(url).with_native_transport_for_test();let arrived=Arc::new(Notify::new());let release=Arc::new(Notify::new());if cancel_before_use{owner.native_before_use=Some((0,arrived.clone(),release.clone()));}
            let stop=run.cancel.clone();let mut future=Box::pin(h.run_steps(&mut run,vendor,&[],"fixture","utf16-id"));
            if cancel_before_use{tokio::select!{r=&mut future=>panic!("finished before ID cancellation {r:?}"),_=arrived.notified()=>{}}stop.cancel();release.notify_one();}
            let result=future.await;assert_eq!(result.is_err(),cancel_before_use||terminal_failure);
            let tool=|slot:usize|json!({"kind":"tool","text":format!("{} {{\"slot\":{slot}}}",names[slot-1])});
            let echo=|slot:usize|json!({"kind":"echo","text":format!("Error: tool {} is unavailable to this agent",names[slot-1])});
            let mut expected=vec![json!({"kind":"user","text":"[channel:1 user:2]\ndo work"})];
            if cancel_before_use{expected.extend([tool(4),tool(2),tool(3),tool(6),tool(7)]);}else{expected.extend([tool(4),echo(1),tool(2),echo(2),tool(3),echo(3),echo(4),tool(6),echo(5),echo(6),tool(7),echo(7),json!({"kind":"unii","text":"done"})]);}
            if terminal_failure {assert_eq!(expected.pop().unwrap(),json!({"kind":"unii","text":"done"}));}
            let trace=native_trace_entries(&*run.memory.memory.lock().await,0);assert_eq!(trace,expected,"raw-ID equality/buffer association {vendor} cancel={cancel_before_use}");
            let requests=captures.lock().await;assert_eq!(requests.len(),if cancel_before_use{1}else{2});
            if !cancel_before_use{let next=NativeValue::parse(std::str::from_utf8(&requests[1]).unwrap()).unwrap();let backs=if vendor=="openai"{next.field("input").unwrap().elements().into_iter().filter(|i|i["type"]=="function_call_output").map(|i|i.field("call_id").unwrap().canonical()).collect::<Vec<_>>()}else{next.field("messages").unwrap().elements().into_iter().flat_map(|m|m.field("content").map(|v|v.elements()).unwrap_or_default()).filter(|b|b["type"]=="tool_result").map(|b|b.field("tool_use_id").unwrap().canonical()).collect::<Vec<_>>()};let expected_ids=[r#""\ud800""#,r#""\udfff""#,r#""�""#,r#""\ud800""#,r#""😀""#,r#""😀""#,r#""ascii""#].into_iter().map(str::to_owned).collect::<Vec<_>>();assert_eq!(backs,expected_ids,"actual C preserves all seven raw IDs/order, duplicate pairs canonicalize");assert_ne!(backs[0],backs[1]);assert_ne!(backs[0],backs[2]);assert_ne!(backs[1],backs[2]);
                let raw=std::str::from_utf8(&requests[1]).unwrap();assert!(raw.contains(r#""name":"\ud800""#),"unknown native tool name is retained raw in C, not its display projection");let mut previous=0;
                for(i,id)in expected_ids.iter().enumerate(){let text=serde_json::to_string(&format!("Error: tool {} is unavailable to this agent",names[i])).unwrap();let expected_back=if vendor=="openai"{format!(r#"{{"call_id":{id},"output":[{{"text":{text},"type":"input_text"}}],"type":"function_call_output"}}"#)}else{format!(r#"{{"content":[{{"text":{text},"type":"text"}}],"is_error":true,"tool_use_id":{id},"type":"tool_result"}}"#)};let at=raw.find(&expected_back).unwrap_or_else(||panic!("actual C bytes missing exact ID/result pair {expected_back}: {raw}"));assert!(i==0||at>previous,"raw Back byte order");previous=at;}
            }
            let issued:i64=h.store.db.lock().unwrap().query_row("SELECT count(*) FROM tool_runs",[],|r|r.get(0)).unwrap();assert_eq!(issued,if cancel_before_use{0}else{7});
            drop(requests);drop(run);drop(h);server.abort();let reopened=Memory::open(dir.path().join("state/memory"),128000).unwrap();assert_eq!(native_trace_entries(&reopened,0),expected,"inert kind/body reopen {vendor} cancel={cancel_before_use}");
            eprintln!("MEASURED opaque-ID vendor={vendor} outcome={outcome} high/low/replacement distinct, raw duplicates/pairs equivalent, exact pending/back/order/reopen");
        }
    }
}
