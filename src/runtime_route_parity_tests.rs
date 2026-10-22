// U3 actual production entry points, local native wire and native disk only.
fn route_contains(value: &Value, text: &str) -> bool {
    match value {
        Value::String(s) => s.contains(text),
        Value::Array(a) => a.iter().any(|v| route_contains(v, text)),
        Value::Object(m) => m.values().any(|v| route_contains(v, text)),
        _ => false,
    }
}
fn route_tool_output(vendor: &str, request: &Value, id: &str) -> (String, bool) {
    if vendor == "openai" {
        let items = request["input"].as_array().unwrap();
        let found = items
            .iter()
            .filter(|v| v["type"] == "function_call_output" && v["call_id"] == id)
            .collect::<Vec<_>>();
        assert_eq!(found.len(), 1, "unique result {id}");
        let parts = found[0]["output"]
            .as_array()
            .expect("native OpenAI Back output must be content-parts array");
        assert_eq!(parts.len(), 1, "text-only route output {id}");
        assert_eq!(parts[0]["type"], "input_text");
        (parts[0]["text"].as_str().unwrap().to_owned(), false)
    } else {
        let items = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|v| v["content"].as_array().into_iter().flatten());
        let found = items
            .filter(|v| v["type"] == "tool_result" && v["tool_use_id"] == id)
            .collect::<Vec<_>>();
        assert_eq!(found.len(), 1, "unique result {id}");
        let parts = found[0]["content"]
            .as_array()
            .expect("native Anthropic Back content must be content-parts array");
        assert_eq!(parts.len(), 1, "text-only route content {id}");
        assert_eq!(parts[0]["type"], "text");
        (
            parts[0]["text"].as_str().unwrap().to_owned(),
            found[0]["is_error"].as_bool().unwrap(),
        )
    }
}
#[tokio::test]
async fn native_production_spawn_worker_freezes_prompt_but_zooms_live_root_and_reopens_trace() {
    for vendor in ["openai", "anthropic"] {
        let tools = [
            ("old-page", "zoom", json!({"id":1,"n":1})),
            ("future-page", "zoom", json!({"id":2,"n":1})),
            ("future-date", "date", json!({"id":2})),
        ];
        let first = if vendor == "openai" {
            json!({"status":"completed","output":tools.iter().map(|(id,name,args)|json!({"type":"function_call","call_id":id,"name":name,"arguments":args.to_string()})).collect::<Vec<_>>(),"usage":{}})
        } else {
            json!({"stop_reason":"tool_use","content":tools.iter().map(|(id,name,args)|json!({"type":"tool_use","id":id,"name":name,"input":args})).collect::<Vec<_>>(),"usage":{}})
        };
        let (dir, mut h, mut run, mock, server) = fixture(
            vendor,
            vec![first, final_response(vendor, "worker completed")],
        )
        .await;
        let probe: crate::provider::NativeCacheProbe = Arc::default();
        let owner = Arc::get_mut(&mut h).unwrap();
        owner.provider = owner
            .provider
            .clone()
            .with_native_transport_for_test()
            .with_native_cache_probe(probe.clone());
        if vendor == "openai" {
            run.settings.0 = "openai/gpt-6-fixture".into();
        }
        let page = format!("{}{}", "x".repeat(29970), "EXACT_WORKER_OLD_PAGE_SENTINEL");
        let expected_zoom = {
            let mut m = run.memory.memory.lock().await;
            assert_eq!(m.append(Kind::User, &page).unwrap(), 1);
            while let Some(job) = m.ready_jobs(1).first().copied() {
                m.finish(job, "launch summary").unwrap();
            }
            m.zoom(1, 1).unwrap()
        };
        let spawned = h.execute_tool(&mut run, &crate::provider::ToolCall {
            id: "spawn-real-worker".into(), name: "spawn".into(),
            arguments: json!({"tasks":[{"name":"Route Probe","task":"PRIVATE_ROUTE_TASK"}]}).into()}).await.unwrap();
        let spawned: Value = serde_json::from_str(&spawned).unwrap();
        let id = spawned["ids"][0].as_str().unwrap().to_owned();
        assert!(
            h.store.agent(&id).unwrap().is_some(),
            "production registration"
        );
        tokio::time::timeout(Duration::from_secs(10), mock.started.notified())
            .await
            .unwrap();
        let (root_after_arrival, future_zoom, future_date) = {
            let mut m = run.memory.memory.lock().await;
            assert_eq!(
                m.append(Kind::User, "FUTURE_ROOT_ARRIVAL_MUST_NOT_LEAK")
                    .unwrap(),
                2
            );
            (
                m.zoom(0, 1).unwrap() + &m.zoom(1, 1).unwrap() + &m.zoom(2, 1).unwrap(),
                m.zoom(2, 1).unwrap(),
                m.date(2).unwrap(),
            )
        };
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !h.children.lock().await.contains_key(&id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let requests = mock.requests.lock().await;
        assert_eq!(
            requests.len(),
            2,
            "{vendor} one call batch and final request"
        );
        assert!(route_contains(&requests[0], "PRIVATE_ROUTE_TASK"));
        assert!(
            route_contains(&requests[1], &expected_zoom),
            "{vendor} full old page delivered"
        );
        assert!(
            !route_contains(&requests[0], "FUTURE_ROOT_ARRIVAL_MUST_NOT_LEAK"),
            "{vendor} launch prompt is frozen"
        );
        assert!(
            route_contains(&requests[1], "FUTURE_ROOT_ARRIVAL_MUST_NOT_LEAK"),
            "{vendor} numeric zoom deliberately reads live root, like upstream Tools.call"
        );
        let backs = if vendor == "openai" {
            requests[1]["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v["type"] == "function_call_output")
                .cloned()
                .collect::<Vec<_>>()
        } else {
            requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|v| v["content"].as_array().into_iter().flatten())
                .filter(|v| v["type"] == "tool_result")
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(backs.len(), 3);
        assert_eq!(
            route_tool_output(vendor, &requests[1], "old-page"),
            (expected_zoom.clone(), false)
        );
        assert_eq!(
            route_tool_output(vendor, &requests[1], "future-page"),
            (future_zoom.clone(), false)
        );
        assert_eq!(
            route_tool_output(vendor, &requests[1], "future-date"),
            (future_date.clone(), false)
        );
        assert!(
            route_contains(&backs[1], "FUTURE_ROOT_ARRIVAL_MUST_NOT_LEAK"),
            "live zoom result {vendor}"
        );
        assert!(
            !route_contains(&backs[2], "Error:"),
            "live date result {vendor}"
        );
        if vendor == "anthropic" {
            assert_eq!(backs[1]["is_error"], false);
            assert_eq!(backs[2]["is_error"], false);
        }
        assert_eq!(
            requests[0][if vendor == "openai" {
                "max_output_tokens"
            } else {
                "max_tokens"
            }],
            128000
        );
        assert_ne!(
            h.store.cache_affinity(&id).unwrap(),
            h.store.cache_affinity("channel:1").unwrap(),
            "private cache domain"
        );
        let observed = probe.lock().unwrap().clone();
        assert_eq!(observed.len(), 2);
        for (actual, request) in observed.iter().zip(requests.iter()) {
            assert_eq!(&actual.body, request);
            assert_eq!(actual.identity, h.store.cache_affinity(&id).unwrap());
            assert_eq!(actual.model, run.settings.0);
            assert!(!actual.keys.is_empty());
        }
        assert_eq!(
            observed[0].keys, observed[1].keys,
            "same marked launch prefix across tool steps"
        );
        drop(observed);
        drop(requests);
        let m = run.memory.memory.lock().await;
        assert_eq!(m.stats()["messages"], 3, "worker must not append to root");
        assert_eq!(
            m.zoom(0, 1).unwrap() + &m.zoom(1, 1).unwrap() + &m.zoom(2, 1).unwrap(),
            root_after_arrival
        );
        drop(m);
        let path = h.config.state_dir.join("subagents").join(&id);
        let trace = Memory::open(&path, 128000).unwrap();
        let typed = native_trace_entries(&trace, 0);
        // The full ASCII zoom result includes its native author prefix; its
        // 30,000-unit boundary creates two durable Echo pages, not one Entry.
        assert!(expected_zoom.is_ascii() && expected_zoom.len() > 30000);
        let expected_trace = vec![
            json!({"kind":"user","text":"PRIVATE_ROUTE_TASK"}),
            json!({"kind":"tool","text":"zoom {\"id\":1,\"n\":1}"}),
            json!({"kind":"echo","text":&expected_zoom[..30000]}),
            json!({"kind":"echo","text":&expected_zoom[30000..]}),
            json!({"kind":"tool","text":"zoom {\"id\":2,\"n\":1}"}),
            json!({"kind":"echo","text":future_zoom}),
            json!({"kind":"tool","text":"date {\"id\":2}"}),
            json!({"kind":"echo","text":future_date}),
            json!({"kind":"unii","text":"worker completed"}),
        ];
        assert_eq!(
            typed, expected_trace,
            "entire worker trace kind/body {vendor}"
        );
        eprintln!(
            "MEASURED worker vendor={vendor} requests=2 trace_pages={} echo_pages=4 live_zoom_date=true frozen_prompt=true",
            typed.len()
        );
        drop(trace);
        let reopened = Memory::open(&path, 128000).unwrap();
        assert_eq!(native_trace_entries(&reopened, 0), typed);
        drop(reopened);
        drop(run);
        drop(h);
        server.abort();
        drop(dir);
    }
}

#[tokio::test]
async fn native_production_root_actor_profiles_actual_wire_and_owned_stop() {
    for vendor in ["openai", "anthropic"] {
        let (dir, mut h, run, mock, server) =
            fixture(vendor, vec![final_response(vendor, "done")]).await;
        let probe: crate::provider::NativeCacheProbe = Arc::default();
        let provider = h
            .provider
            .clone()
            .with_native_transport_for_test()
            .with_native_cache_probe(probe.clone());
        Arc::get_mut(&mut h).unwrap().provider = provider;
        if vendor == "openai" {
            h.store
                .set_settings(1, "openai/gpt-6-fixture", "medium")
                .unwrap();
        }
        h.store.input_state("first", "done").unwrap();
        let context = crate::ui::ReplyContext::request("route-root");
        let input = Input {
            id: "route-root".into(),
            channel: 1,
            user: 2,
            text: "ROOT_ROUTE_INPUT".into(),
        };
        h.store.bind_context(&input.id, &context).unwrap();
        h.admit_prompt(&input).unwrap();
        let c = run.memory.clone();
        let owner = h.clone();
        let root = c.clone();
        let actor = tokio::spawn(async move { owner.channel_worker(1, root).await });
        tokio::time::timeout(Duration::from_secs(10), mock.started.notified())
            .await
            .unwrap();
        let request = mock.requests.lock().await[0].clone();
        let (view, raw) = {
            let m = c.memory.lock().await;
            let from = m.whole(m.lookup_source("route-root").unwrap()).unwrap();
            (
                m.render_before(from).unwrap(),
                vec![
                    m.say(from, m.stats()["messages"].as_u64().unwrap())
                        .unwrap(),
                ],
            )
        };
        let history = Provider::start(vendor, &view, &raw.join("\n\n"));
        let field = if vendor == "openai" {
            "input"
        } else {
            "messages"
        };
        assert_eq!(
            request[field]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v["role"] == "user")
                .cloned()
                .collect::<Vec<_>>(),
            history
        );
        assert_eq!(
            request[if vendor == "openai" {
                "max_output_tokens"
            } else {
                "max_tokens"
            }],
            128000
        );
        assert_eq!(request["stream"], true);
        if vendor == "openai" {
            assert_eq!(request["input"][0]["role"], "developer");
        } else {
            assert_eq!(
                request["thinking"]["type"], "enabled",
                "unknown synthetic model keeps explicit capability fallback"
            );
        }
        let observed = probe.lock().unwrap().clone();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].body, request);
        assert_eq!(
            observed[0].identity,
            h.store.cache_affinity("channel:1").unwrap()
        );
        assert!(!observed[0].keys.is_empty());
        drop(observed);
        let cancel = c.cancel.lock().await.as_ref().unwrap().clone();
        cancel.cancel();
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if c.cancel.lock().await.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !h.shutdown.is_cancelled(),
            "owned root stop does not cancel harness"
        );
        assert_eq!(mock.requests.lock().await.len(), 1);
        h.shutdown.cancel();
        actor.await.unwrap().unwrap();
        server.abort();
        let persisted = native_trace_entries(&*c.memory.lock().await, 0);
        assert_eq!(
            persisted,
            vec![
                json!({"kind":"user","text":"do work"}),
                json!({"kind":"user","text":"ROOT_ROUTE_INPUT"}),
                json!({"kind":"unii","text":"Turn stopped: cancelled. Tool effects already completed may remain."})
            ],
            "root stop entire trace {vendor}"
        );
        drop(c);
        drop(run);
        drop(h);
        let reopened = Memory::open(dir.path().join("memory"), 128000).unwrap();
        assert_eq!(native_trace_entries(&reopened, 0), persisted);
        eprintln!("MEASURED root-owned-stop vendor={vendor} requests=1 exact_trace_reopen=true");
    }
}

#[tokio::test]
async fn native_production_curator_and_reviewer_use_private_snapshot_read_routes() {
    for vendor in ["openai", "anthropic"] {
        let calls = |specs: Vec<(&str, &str, Value)>| {
            if vendor == "openai" {
                json!({"status":"completed","output":specs.into_iter().map(|(id,name,args)|json!({"type":"function_call","call_id":id,"name":name,"arguments":args.to_string()})).collect::<Vec<_>>(),"usage":{}})
            } else {
                json!({"stop_reason":"tool_use","content":specs.into_iter().map(|(id,name,args)|json!({"type":"tool_use","id":id,"name":name,"input":args})).collect::<Vec<_>>(),"usage":{}})
            }
        };
        let reads = || {
            vec![
                ("old", "zoom", json!({"id":1,"n":1})),
                ("new", "zoom", json!({"id":2,"n":1})),
                ("date-new", "date", json!({"id":2})),
            ]
        };
        let mut draft = reads();
        draft.push(("stage","create_skill",json!({"id":"route-proof","name":"Route Proof","description":"Inspect native memory routes","body":"Read exact source before reporting.","summary":"Added observed check","purpose":"Native route proof"})));
        let responses = vec![
            calls(draft),
            final_response(vendor, "staged"),
            calls(reads()),
            calls(vec![(
                "verdict",
                "decide",
                json!({"approve":false,"reason":"Synthetic route probe only","scope_fit":"unsupported","evidence":"No publication is intended"}),
            )]),
        ];
        let (dir, mut h, run, mock, server) = fixture(vendor, responses).await;
        let probe: crate::provider::NativeCacheProbe = Arc::default();
        let owner = Arc::get_mut(&mut h).unwrap();
        owner.provider = owner
            .provider
            .clone()
            .with_native_transport_for_test()
            .with_native_cache_probe(probe.clone());
        owner.config.curator.reviewers = 1;
        if vendor == "openai" {
            owner.config.agent.model = "openai/gpt-6-fixture".into();
        }
        h.store.input_state("first", "done").unwrap();
        let expected = {
            let mut m = run.memory.memory.lock().await;
            m.append(
                Kind::User,
                &format!("{}CURATOR_EXACT_OLD_PAGE", "q".repeat(29000)),
            )
            .unwrap();
            while let Some(job) = m.ready_jobs(1).first().copied() {
                m.finish(job, "curator launch summary").unwrap();
            }
            m.zoom(1, 1).unwrap()
        };
        h.skills
            .enqueue_fork("1", "route-fork", &json!({"task":"synthetic native route"}))
            .unwrap();
        h.skills.request_channel_fork("1").unwrap();
        let maintenance = tokio::spawn(h.clone().curator_worker());
        h.curator_changed.notify_one();
        tokio::time::timeout(Duration::from_secs(10), mock.started.notified())
            .await
            .unwrap();
        let root_after = {
            let mut m = run.memory.memory.lock().await;
            m.append(Kind::User, "CURATOR_FUTURE_ROOT_MUST_STAY_PRIVATE")
                .unwrap();
            native_trace_entries(&m, 0)
        };
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = h.skills.channel_status("1").unwrap();
                if status["latest"]["status"] == "denied" {
                    break;
                } else if status["latest"]["status"] == "failed" {
                    panic!("route failed: {status}");
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 4, "draft two + reviewer two {vendor}");
        for (index, request) in requests.iter().enumerate() {
            assert!(
                !route_contains(request, "CURATOR_FUTURE_ROOT_MUST_STAY_PRIVATE"),
                "{vendor} snapshot leak at {index}"
            );
            assert_eq!(
                request[if vendor == "openai" {
                    "max_output_tokens"
                } else {
                    "max_tokens"
                }],
                128000
            );
            assert_eq!(request["stream"], true);
        }
        assert!(
            route_contains(&requests[1], &expected),
            "draft Session::read full page {vendor}"
        );
        assert!(
            route_contains(&requests[3], &expected),
            "reviewer Session::read full page {vendor}"
        );
        for (index, label) in [(1, "draft"), (3, "reviewer")] {
            for (id, expected_value, error) in [
                ("old", json!({"text":expected}), false),
                ("new", json!({"error":"No line 2+1."}), true),
                ("date-new", json!({"error":"No message 2."}), true),
            ] {
                let (text, is_error) = route_tool_output(vendor, &requests[index], id);
                assert_eq!(
                    serde_json::from_str::<Value>(&text).unwrap(),
                    expected_value,
                    "{vendor} {label} {id}"
                );
                if vendor == "anthropic" {
                    assert_eq!(is_error, error, "{vendor} {label} {id}");
                }
            }
        }
        eprintln!(
            "MEASURED private-curator-reviewer vendor={vendor} requests=4 bound_result_ids=6 root_writes=0 snapshot_future_rejected=true"
        );
        let observed = probe.lock().unwrap().clone();
        assert_eq!(observed.len(), 4);
        for (index, (actual, request)) in observed.iter().zip(requests.iter()).enumerate() {
            assert_eq!(&actual.body, request);
            assert_eq!(
                actual.identity,
                h.store
                    .cache_affinity(if index < 2 {
                        "curator:1"
                    } else {
                        "curator-reviewer:1:0"
                    })
                    .unwrap()
            );
            assert!(!actual.keys.is_empty());
        }
        assert_eq!(observed[0].keys, observed[1].keys);
        assert_eq!(observed[2].keys, observed[3].keys);
        assert_ne!(observed[0].keys, observed[2].keys);
        drop(observed);
        assert_ne!(
            h.store.cache_affinity("curator:1").unwrap(),
            h.store.cache_affinity("curator-reviewer:1:0").unwrap()
        );
        assert_ne!(
            h.store.cache_affinity("curator:1").unwrap(),
            h.store.cache_affinity("channel:1").unwrap()
        );
        drop(requests);
        assert_eq!(
            native_trace_entries(&*run.memory.memory.lock().await, 0),
            root_after,
            "private research/review no root append"
        );
        assert!(
            h.skills
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"route-proof"}))
                .is_err(),
            "denied synthetic proposal not installed"
        );
        h.shutdown.cancel();
        maintenance.await.unwrap();
        drop(run);
        drop(h);
        server.abort();
        let reopened = Memory::open(dir.path().join("memory"), 128000).unwrap();
        assert_eq!(native_trace_entries(&reopened, 0), root_after);
    }
}

#[tokio::test]
async fn native_production_worker_owned_cancel_delivers_once_without_root_write() {
    for vendor in ["openai", "anthropic"] {
        let (dir, mut h, mut run, mock, server) = fixture(
            vendor,
            vec![final_response(vendor, "late must not publish")],
        )
        .await;
        let provider = h.provider.clone().with_native_transport_for_test();
        Arc::get_mut(&mut h).unwrap().provider = provider;
        let before = native_trace_entries(&*run.memory.memory.lock().await, 0);
        let spawned=h.execute_tool(&mut run,&crate::provider::ToolCall{id:"cancel-spawn".into(),name:"spawn".into(),arguments:json!({"tasks":[{"name":"Owned Cancel","task":"CANCEL_PRIVATE_TASK"}]}).into()}).await.unwrap();
        let spawned: Value = serde_json::from_str(&spawned).unwrap();
        let id = spawned["ids"][0].as_str().unwrap().to_owned();
        tokio::time::timeout(Duration::from_secs(10), mock.started.notified())
            .await
            .unwrap();
        h.children.lock().await.get(&id).unwrap().cancel.cancel();
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !h.children.lock().await.contains_key(&id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !run.cancel.is_cancelled() && !h.shutdown.is_cancelled(),
            "worker-owned stop {vendor}"
        );
        assert_eq!(mock.requests.lock().await.len(), 1);
        let report: String = h
            .store
            .db
            .lock()
            .unwrap()
            .query_row("SELECT report FROM tasks WHERE id=?1", [&id], |r| r.get(0))
            .unwrap();
        assert!(report.starts_with("Task stopped:"));
        let deliveries: i64 = h
            .store
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM inbox WHERE text LIKE ?1",
                [format!("[{id}] %")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(deliveries, 1, "one durable root report");
        assert_eq!(
            native_trace_entries(&*run.memory.memory.lock().await, 0),
            before
        );
        let trace = Memory::open(h.config.state_dir.join("subagents").join(&id), 128000).unwrap();
        assert_eq!(
            native_trace_entries(&trace, 0),
            vec![json!({"kind":"user","text":"CANCEL_PRIVATE_TASK"})]
        );
        drop(trace);
        drop(run);
        drop(h);
        server.abort();
        let reopened = Memory::open(dir.path().join("memory"), 128000).unwrap();
        assert_eq!(native_trace_entries(&reopened, 0), before);
        eprintln!(
            "MEASURED worker-owned-cancel vendor={vendor} requests=1 root_writes=0 reports=1"
        );
    }
}

#[tokio::test]
async fn native_production_curator_owned_cancel_keeps_root_and_pending_proposal_private() {
    for vendor in ["openai", "anthropic"] {
        let (dir, mut h, run, mock, server) =
            fixture(vendor, vec![final_response(vendor, "late curator text")]).await;
        let provider = h.provider.clone().with_native_transport_for_test();
        Arc::get_mut(&mut h).unwrap().provider = provider;
        h.store.input_state("first", "done").unwrap();
        let before = native_trace_entries(&*run.memory.memory.lock().await, 0);
        h.skills
            .enqueue_fork("1", "owned-cancel-route", &json!({"task":"cancel route"}))
            .unwrap();
        h.skills.request_channel_fork("1").unwrap();
        let maintenance = tokio::spawn(h.clone().curator_worker());
        h.curator_changed.notify_one();
        tokio::time::timeout(Duration::from_secs(10), mock.started.notified())
            .await
            .unwrap();
        h.cancel_curator(1).await.unwrap();
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !h.curators.lock().await.contains_key(&1) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            h.skills.channel_status("1").unwrap()["latest"]["status"],
            "interrupted"
        );
        assert_eq!(mock.requests.lock().await.len(), 1);
        assert!(
            !run.cancel.is_cancelled() && !h.shutdown.is_cancelled(),
            "curator-owned stop {vendor}"
        );
        assert!(h.skills.notifications("1").unwrap().is_empty());
        assert_eq!(
            native_trace_entries(&*run.memory.memory.lock().await, 0),
            before
        );
        h.shutdown.cancel();
        maintenance.await.unwrap();
        drop(run);
        drop(h);
        server.abort();
        let reopened = Memory::open(dir.path().join("memory"), 128000).unwrap();
        assert_eq!(native_trace_entries(&reopened, 0), before);
        eprintln!(
            "MEASURED curator-owned-cancel vendor={vendor} requests=1 root_writes=0 notifications=0"
        );
    }
}

#[tokio::test]
async fn native_production_curator_cancel_during_actual_reviewer_never_installs_proposal() {
    for vendor in ["openai", "anthropic"] {
        let args = json!({"id":"review-cancel-proof","name":"Review Cancel Proof","description":"Inspect private reviewer cancellation","body":"Read evidence before reporting.","summary":"Synthetic route","purpose":"Test owned cancellation"});
        let draft = if vendor == "openai" {
            json!({"status":"completed","output":[{"type":"function_call","call_id":"stage","name":"create_skill","arguments":args.to_string()}],"usage":{}})
        } else {
            json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"stage","name":"create_skill","input":args}],"usage":{}})
        };
        let (dir, mut h, run, mock, old_server) = fixture(
            vendor,
            vec![
                draft,
                final_response(vendor, "staged"),
                final_response(vendor, "late verdict"),
            ],
        )
        .await;
        old_server.abort();
        let arrived = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let mark = arrived.clone();
        let gate = release.clone();
        let seen = captured.clone();
        let transport = mock.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    post(move |Json(body): Json<Value>| {
                        let mark = mark.clone();
                        let gate = gate.clone();
                        let seen = seen.clone();
                        let transport = transport.clone();
                        async move {
                            seen.lock().await.push(body.clone());
                            if body["tools"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|v| v["name"] == "decide")
                            {
                                mark.notify_one();
                                gate.notified().await;
                            }
                            respond(State(transport), Json(body)).await
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let owner = Arc::get_mut(&mut h).unwrap();
        owner.provider = Provider::mock(endpoint).with_native_transport_for_test();
        owner.config.curator.reviewers = 1;
        h.store.input_state("first", "done").unwrap();
        let before = native_trace_entries(&*run.memory.memory.lock().await, 0);
        h.skills
            .enqueue_fork(
                "1",
                "reviewer-cancel-route",
                &json!({"task":"reviewer cancel"}),
            )
            .unwrap();
        h.skills.request_channel_fork("1").unwrap();
        mock.release.notify_one();
        let maintenance = tokio::spawn(h.clone().curator_worker());
        h.curator_changed.notify_one();
        tokio::time::timeout(Duration::from_secs(10), arrived.notified())
            .await
            .unwrap();
        assert_eq!(
            h.skills.channel_status("1").unwrap()["latest"]["phase"],
            "review"
        );
        h.cancel_curator(1).await.unwrap();
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !h.curators.lock().await.contains_key(&1) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            captured.lock().await.len(),
            3,
            "two draft plus actual reviewer HTTP request"
        );
        assert_eq!(
            h.skills.channel_status("1").unwrap()["latest"]["status"],
            "interrupted"
        );
        assert!(!run.cancel.is_cancelled() && !h.shutdown.is_cancelled());
        assert!(h.skills.notifications("1").unwrap().is_empty());
        assert!(
            h.skills
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"review-cancel-proof"}))
                .is_err()
        );
        assert_eq!(
            native_trace_entries(&*run.memory.memory.lock().await, 0),
            before
        );
        h.shutdown.cancel();
        maintenance.await.unwrap();
        drop(run);
        drop(h);
        server.abort();
        let reopened = Memory::open(dir.path().join("memory"), 128000).unwrap();
        assert_eq!(native_trace_entries(&reopened, 0), before);
        eprintln!(
            "MEASURED reviewer-owned-cancel vendor={vendor} requests=3 root_writes=0 skill_installs=0"
        );
    }
}
