// Actual Harness -> Provider -> loopback HTTP -> compactor policy, no model calls.
async fn compaction_parity_fixture(vendor:&str,responses:Vec<Value>)->(Result<String>,Vec<Value>) {
    let (_d,h,_run,mock,server)=fixture(vendor,responses).await;
    mock.release.notify_one();
    let result=h.compress(1,&format!("{vendor}/test"),NodeKey{level:0,index:0},"<chat>frozen</chat>",&"source 🙂 ".repeat(100)).await;
    let requests=mock.requests.lock().await.clone();server.abort();
    (result,requests)
}
#[tokio::test]
async fn compaction_parity_model_refusal_incomplete_and_failed_use_source_or_best() {
    let failures=[
        ("openai",json!({"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"do not store refusal prose"}]}]})),
        ("openai",json!({"status":"incomplete","output":[]})),
        ("openai",json!({"status":"failed","error":{"code":"policy_error"},"output":[]})),
        ("anthropic",json!({"stop_reason":"refusal","content":[{"type":"text","text":"no"}]})),
        ("anthropic",json!({"stop_reason":"max_tokens","content":[{"type":"text","text":"partial never stored"}]})),
    ];
    for (vendor,failure) in failures {
        let (result,requests)=compaction_parity_fixture(vendor,vec![failure.clone()]).await;
        assert_eq!(result.unwrap(),crate::memory::compaction_fallback(&"source 🙂 ".repeat(100)));
        assert_eq!(requests.len(),1);
        let prior="x".repeat(553);
        let (result,requests)=compaction_parity_fixture(vendor,vec![final_response(vendor,&prior),failure]).await;
        assert_eq!(result.unwrap(),prior);assert_eq!(requests.len(),2);
    }
}
#[tokio::test]
async fn compaction_parity_shortest_of_five_first_tie_and_only_latest_note() {
    for vendor in ["openai","anthropic"] {
        let lines=["a".repeat(553),"é".repeat(270),"b".repeat(540),"c".repeat(580),"d".repeat(570)];
        let (result,requests)=compaction_parity_fixture(vendor,lines.iter().map(|s|final_response(vendor,s)).collect()).await;
        assert_eq!(result.unwrap(),lines[1]);assert_eq!(requests.len(),5);
        let field=if vendor=="openai" {"input"} else {"messages"};
        let original=requests[0][field].as_array().unwrap();
        for (n,request) in requests.iter().enumerate().skip(1) {
            let history=request[field].as_array().unwrap();
            assert_eq!(history.len(),original.len());
            assert_eq!(&history[..history.len()-1],&original[..original.len()-1]);
            let blocks=history.last().unwrap()["content"].as_array().unwrap();
            let original_blocks=original.last().unwrap()["content"].as_array().unwrap();
            assert_eq!(&blocks[..original_blocks.len()],original_blocks);
            assert_eq!(blocks.last().unwrap()["text"],crate::memory::compaction_over(&lines[n-1]));
        }
    }
}
#[tokio::test]
async fn compaction_parity_entries_join_exact_utf8_threshold_and_empty_filter() {
    for vendor in ["openai","anthropic"] {
        let response=if vendor=="openai" {
            json!({"status":"completed","output":[
                {"type":"message","content":[{"type":"output_text","text":"\u{feff}"},{"type":"output_text","text":"é".repeat(256)}]},
                {"type":"message","content":[{"type":"output_text","text":" \u{feff}"}]}]})
        } else {
            json!({"stop_reason":"end_turn","content":[{"type":"text","text":"\u{feff}"},{"type":"text","text":"é".repeat(256)}]})
        };
        let (result,requests)=compaction_parity_fixture(vendor,vec![response]).await;
        assert_eq!(result.unwrap(),"é".repeat(256)); assert_eq!(requests.len(),1);
    }
    let response=json!({"status":"completed","output":[
        {"type":"message","content":[{"type":"output_text","text":" a"},{"type":"output_text","text":"b "}]},
        {"type":"message","content":[{"type":"output_text","text":"\u{feff}"}]},
        {"type":"message","content":[{"type":"output_text","text":"\u{0085}c"}]}]});
    let (result,_)=compaction_parity_fixture("openai",vec![response]).await;
    assert_eq!(result.unwrap(),"ab\n\n\u{0085}c");
}
#[tokio::test(start_paused = true)]
async fn compaction_parity_generic_errors_patient_same_correction_then_answer() {
    let unknown=json!({"bogus":"no valid terminal status"});
    let (result,requests)=compaction_parity_fixture("openai",vec![unknown.clone(), final_response("openai","after outage")]).await;
    assert_eq!(result.unwrap(),"after outage");assert_eq!(requests.len(),2);
    assert_eq!(requests[0],requests[1]);
    let prior="x".repeat(553);
    let (result,requests)=compaction_parity_fixture("openai",vec![final_response("openai",&prior),unknown,final_response("openai","short after outage")]).await;
    assert_eq!(result.unwrap(),"short after outage"); assert_eq!(requests.len(),3);
    assert_eq!(requests[1],requests[2]);
    let (result,requests)=compaction_parity_fixture("openai",vec![final_response("openai"," \u{feff}\n")]).await;
    assert_eq!(result.unwrap(),crate::memory::compaction_fallback(&"source 🙂 ".repeat(100)));assert_eq!(requests.len(),1);
}
#[tokio::test]
async fn compaction_parity_pause_turns_preserve_native_context_not_candidate_text() {
    let first=json!([{"type":"thinking","thinking":"private","signature":"sig"},{"type":"text","text":"intermediate"}]);
    let second=json!([{"type":"text","text":"not final either"}]);
    let (result,requests)=compaction_parity_fixture("anthropic",vec![
        json!({"stop_reason":"pause_turn","content":first}),
        json!({"stop_reason":"pause_turn","content":second}),
        final_response("anthropic","only final")]).await;
    assert_eq!(result.unwrap(),"only final");assert_eq!(requests.len(),3);
    assert_eq!(requests[1]["messages"][1],json!({"role":"assistant","content":first}));
    let merged=first.as_array().unwrap().iter().chain(second.as_array().unwrap()).cloned().collect::<Vec<_>>();
    assert_eq!(requests[2]["messages"].as_array().unwrap().len(),2);
    assert_eq!(requests[2]["messages"][1],json!({"role":"assistant","content":merged}));
    assert_eq!(requests[1]["messages"][0],requests[0]["messages"][0]);
}

#[tokio::test]
async fn compaction_actual_upstream_build_differential_selected_line_and_every_block() {
    let contract:Value=serde_json::from_str(include_str!("../tests/fixtures/compactor-contract-0500bf6.json")).unwrap();
    for vendor in ["openai","anthropic"] {
        for case in contract["records"].as_array().unwrap() {
            let replies=case["answers"].as_array().unwrap().iter().map(|end| {
                if end["ok"]==true { final_response(vendor,end["text"].as_str().unwrap()) }
                else if vendor=="openai" { json!({"status":"incomplete","output":[]}) }
                else { json!({"stop_reason":"refusal","content":[]}) }
            }).collect();
            let (_dir,h,_run,mock,server)=fixture(vendor,replies).await;
            mock.release.notify_one();
            let model=if vendor=="openai" {"openai/gpt-6-fixture"} else {"anthropic/fixture"};
            let result=h.compress_with_effort(1,model,"medium",NodeKey{level:0,index:1},contract["context"].as_str().unwrap(),contract["source"].as_str().unwrap()).await.unwrap();
            assert_eq!(result,case["selected"].as_str().unwrap(),"{vendor} {}",case["name"]);
            let requests=mock.requests.lock().await;
            assert_eq!(requests.len(),case["blocks"].as_array().unwrap().len());
            for (request,expected) in requests.iter().zip(case["blocks"].as_array().unwrap()) {
                let field=if vendor=="openai" {"input"} else {"messages"};
                let user=request[field].as_array().unwrap().iter().find(|item|item["role"]=="user").unwrap();
                let actual=user["content"].as_array().unwrap().iter().map(|b|json!({"text":b["text"],"mark":b.get(if vendor=="openai" {"prompt_cache_breakpoint"} else {"cache_control"}).is_some()})).collect::<Vec<_>>();
                assert_eq!(json!(actual),*expected,"{vendor} {}",case["name"]);
                assert!(request["tools"].as_array().is_none_or(Vec::is_empty));
            }
            server.abort();
        }
    }
}

#[tokio::test]
async fn native_turn_oversized_batch_matches_upstream_raw_suffix_not_full_inputs() {
    let (_dir,h,run,_mock,server)=fixture("openai",vec![]).await;
    let context=crate::ui::ReplyContext::request("A");
    let inputs=vec![Input{id:"A".into(),channel:1,user:2,text:"a".repeat(100000)},Input{id:"B".into(),channel:1,user:2,text:"b".repeat(100000)}];
    for input in &inputs {h.store.bind_context(&input.id,&context).unwrap();h.store.admit(input).unwrap();}
    // Suffix boundary is in A; settle only pages before it, not all queued input.
    let mut queued=inputs.clone();
    let future=h.prepare_native_turn(1,&run.memory,&run.cancel,&context,&mut queued);
    tokio::pin!(future);
    let wake=run.memory.changed.notified();tokio::pin!(wake);wake.as_mut().enable();
    tokio::select!{_=&mut wake=>{},result=&mut future=>panic!("must await earlier pages: {result:?}")}
    {
        let mut memory=run.memory.memory.lock().await;
        let first=memory.lookup_source("A").unwrap();let from=memory.whole(first).unwrap();
        let jobs=memory.ready_jobs(usize::MAX);
        for key in jobs.into_iter().filter(|key|key.level==0&&key.index<from){memory.finish(key,"older selected page").unwrap();}
    }
    run.memory.changed.notify_waiters();
    let (view,texts)=future.await.unwrap();
    let contract:Value=serde_json::from_str(include_str!("../tests/fixtures/compactor-contract-0500bf6.json")).unwrap();
    assert_eq!(json!(texts),contract["suffixes"][0]["raw"]);
    assert!(view.contains("older selected page"));
    assert!(!view.contains(&"b".repeat(50)));
    let memory=run.memory.memory.lock().await;
    assert!(!memory.is_settled()); // raw suffix does not require its own summaries
    assert_eq!(memory.say(memory.source_span("A").unwrap().0,memory.source_span("A").unwrap().1).unwrap(),inputs[0].text);
    server.abort();
}

#[tokio::test]
async fn native_turn_arrival_during_settle_recomputes_same_activity_without_duplicates() {
    let (_dir,h,run,_mock,server)=fixture("openai",vec![]).await;
    let old=run.memory.memory.lock().await.append(Kind::Echo,&"slow old leaf".repeat(100)).unwrap();
    let context=crate::ui::ReplyContext::request("A");
    let a=Input{id:"A".into(),channel:1,user:2,text:"fresh A".into()};
    h.store.bind_context(&a.id,&context).unwrap();h.store.admit(&a).unwrap();
    let mut queued=vec![a];
    let future=h.prepare_native_turn(1,&run.memory,&run.cancel,&context,&mut queued);tokio::pin!(future);
    let wake=run.memory.changed.notified();tokio::pin!(wake);wake.as_mut().enable();
    tokio::select!{_=&mut wake=>{},result=&mut future=>panic!("must await old leaf: {result:?}")}
    let b=Input{id:"B".into(),channel:1,user:2,text:"fresh B".into()};
    h.store.bind_context(&b.id,&context).unwrap();h.store.admit(&b).unwrap();run.memory.incoming.notify_one();
    {
        let mut memory=run.memory.memory.lock().await;memory.ready_jobs(usize::MAX);
        memory.finish(NodeKey{level:0,index:old},"settled old leaf").unwrap();
    }
    run.memory.changed.notify_waiters();
    let (view,texts)=future.await.unwrap();
    assert_eq!(texts,vec!["fresh A","fresh B"]);assert!(view.contains("settled old leaf"));
    assert!(!view.contains("fresh A"));assert!(!view.contains("fresh B"));
    let memory=run.memory.memory.lock().await;
    assert_eq!(memory.lookup_source("B"),Some(old+2));
    assert_eq!(memory.stats()["messages"],old+3);
    server.abort();
}

#[tokio::test]
async fn native_channel_worker_sends_exact_selected_view_and_raw_suffix_to_fake_provider() {
    let (_dir,h,run,mock,server)=fixture("openai",vec![final_response("openai","done")]).await;
    h.store.set_settings(1,"openai/gpt-6-fixture","medium").unwrap();
    let context=crate::ui::ReplyContext::request("A");
    let inputs=vec![Input{id:"A".into(),channel:1,user:2,text:"a".repeat(100000)},Input{id:"B".into(),channel:1,user:2,text:"b".repeat(100000)}];
    for input in &inputs {h.store.bind_context(&input.id,&context).unwrap();h.store.admit(input).unwrap();}
    let c=run.memory.clone();let worker_h=h.clone();let worker_c=c.clone();
    let changed=c.changed.notified();tokio::pin!(changed);changed.as_mut().enable();
    let worker=tokio::spawn(async move{worker_h.channel_worker(1,worker_c).await});
    changed.await;
    let expected_view={
        let mut memory=c.memory.lock().await;
        let from=memory.whole(memory.lookup_source("A").unwrap()).unwrap();
        let jobs=memory.ready_jobs(usize::MAX);
        for key in jobs.into_iter().filter(|key|key.level==0&&key.index<from){memory.finish(key,"older selected page").unwrap();}
        memory.render_before(from).unwrap()
    };
    c.changed.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5),mock.started.notified()).await.unwrap();
    let request=mock.requests.lock().await[0].clone();
    let contract:Value=serde_json::from_str(include_str!("../tests/fixtures/compactor-contract-0500bf6.json")).unwrap();
    let raw=contract["suffixes"][0]["raw"].as_array().unwrap().iter().map(|v|v.as_str().unwrap()).collect::<Vec<_>>().join("\n\n");
    let expected=Provider::start("openai",&expected_view,&raw);
    assert_eq!(request["input"].as_array().unwrap().iter().filter(|v|v["role"]=="user").cloned().collect::<Vec<_>>(),expected);
    assert!(raw.len()<inputs.iter().map(|i|i.text.len()).sum::<usize>());
    h.shutdown.cancel();mock.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5),worker).await.unwrap().unwrap().unwrap();
    server.abort();
}

#[tokio::test]
async fn native_turn_cancel_and_native_reopen_preserve_admission_exactly_once() {
    let (dir,h,run,_mock,server)=fixture("openai",vec![]).await;
    let old=run.memory.memory.lock().await.append(Kind::Echo,&"old pending".repeat(100)).unwrap();
    let context=crate::ui::ReplyContext::request("A");
    let a=Input{id:"A".into(),channel:1,user:2,text:"fresh A".into()};
    h.store.bind_context(&a.id,&context).unwrap();h.store.admit(&a).unwrap();
    let mut queued=vec![a.clone()];let stop=CancellationToken::new();
    let future=h.prepare_native_turn(1,&run.memory,&stop,&context,&mut queued);tokio::pin!(future);
    let wake=run.memory.changed.notified();tokio::pin!(wake);wake.as_mut().enable();
    tokio::select!{_=&mut wake=>{},result=&mut future=>panic!("must await old: {result:?}")}
    stop.cancel();assert!(future.await.is_err());
    let before=run.memory.memory.lock().await.zoom(old+1,1).unwrap();
    {
        let mut memory=run.memory.memory.lock().await;
        let old_bank=std::mem::replace(&mut *memory,Memory::open(dir.path().join("temporary-holder"),128000).unwrap());
        drop(old_bank);
        *memory=Memory::open(dir.path().join("memory"),128000).unwrap();
        assert_eq!(memory.zoom(old+1,1).unwrap(),before);
        memory.ready_jobs(usize::MAX);memory.finish(NodeKey{level:0,index:old},"settled old").unwrap();
    }
    let mut queued=vec![a];
    let (_,texts)=h.prepare_native_turn(1,&run.memory,&CancellationToken::new(),&context,&mut queued).await.unwrap();
    assert_eq!(texts,vec!["fresh A"]);
    let memory=run.memory.memory.lock().await;
    assert_eq!(memory.lookup_source("A"),Some(old+1));assert_eq!(memory.stats()["messages"],old+2);
    assert_eq!(memory.zoom(old+1,1).unwrap(),before);
    server.abort();
}

#[tokio::test]
async fn native_channel_worker_cancel_store_and_harness_restart_never_redispatch_input() {
    let (dir,h,run,mock,server)=fixture("openai",vec![]).await;
    let bank=h.config.state_dir.join("chats/1");
    {let mut memory=run.memory.memory.lock().await;*memory=Memory::open(&bank,128000).unwrap();}
    let old=run.memory.memory.lock().await.append(Kind::Echo,&"old pending".repeat(100)).unwrap();
    let context=crate::ui::ReplyContext::request("cancel-A");
    let input=Input{id:"cancel-A".into(),channel:1,user:2,text:"do not dispatch after cancellation".into()};
    h.store.bind_context(&input.id,&context).unwrap();h.store.admit(&input).unwrap();
    let c=run.memory.clone();let worker_h=h.clone();let worker_c=c.clone();
    let changed=c.changed.notified();tokio::pin!(changed);changed.as_mut().enable();
    let worker=tokio::spawn(async move{worker_h.channel_worker(1,worker_c).await});
    changed.await;
    c.cancel.lock().await.as_ref().unwrap().cancel();
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let state:String=h.store.db.lock().unwrap().query_row("SELECT state FROM inbox WHERE id='cancel-A'",[],|r|r.get(0)).unwrap();
            if state=="cancelled" {break;}
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    h.shutdown.cancel();worker.await.unwrap().unwrap();
    assert!(mock.requests.lock().await.is_empty());
    let original={
        let mut memory=c.memory.lock().await;
        memory.ready_jobs(usize::MAX);memory.finish(NodeKey{level:0,index:old},"settled before restart").unwrap();
        assert_eq!(memory.stats()["messages"],2);
        let original=memory.zoom(1,1).unwrap();
        let held=std::mem::replace(&mut *memory,Memory::open(dir.path().join("closed-bank-holder"),128000).unwrap());drop(held);
        original
    };
    let config=h.config.clone();let discord=h.discord.clone();let provider=h.provider.clone();
    drop(run);drop(h); // release all old harness/skill-library writer ownership
    let mut restarted=Harness::new(config,discord,CancellationToken::new()).unwrap();
    Arc::get_mut(&mut restarted).unwrap().provider=provider;
    let reopened=restarted.channel(1).await.unwrap();
    let (state,logged):(String,bool)=restarted.store.db.lock().unwrap().query_row("SELECT state,logged FROM inbox WHERE id='cancel-A'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!((state.as_str(),logged),("cancelled",true));assert!(restarted.store.queued(1).unwrap().is_empty());
    {
        let memory=reopened.memory.lock().await;
        assert_eq!(memory.zoom(1,1).unwrap(),original);assert_eq!(memory.lookup_source("cancel-A"),Some(1));assert_eq!(memory.stats()["messages"],2);
        assert!(memory.is_settled());
    }
    tokio::task::yield_now().await;
    restarted.shutdown.cancel();
    assert!(mock.requests.lock().await.is_empty());
    server.abort();
}

#[tokio::test]
async fn compaction_pause_json_native_normalization_matches_actual_upstream_step() {
    let contract:Value=serde_json::from_str(include_str!("../tests/fixtures/compactor-contract-0500bf6.json")).unwrap();
    let (_dir,h,_run,mock,server)=fixture("anthropic",vec![json!({"stop_reason":"pause_turn","content":contract["pauseJson"]}),final_response("anthropic","final")]).await;
    // Actual Harness now requests Messages SSE; its mock serializes the JSON
    // native output as events. The direct JSON parser path is tested separately.
    mock.release.notify_one();
    let text=h.compress(1,"anthropic/test",NodeKey{level:0,index:0},"<chat>\n</chat>\n","source").await.unwrap();
    assert_eq!(text,"final");
    let requests=mock.requests.lock().await;
    assert_eq!(requests[1]["messages"][1]["content"],contract["pauseNative"]);
    server.abort();
}

fn compaction_journal_bytes(bank: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    for side in ["main", "tree"] {
        for entry in std::fs::read_dir(bank.join(side)).unwrap() {
            let entry = entry.unwrap();
            if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
                files.insert(format!("{side}/{}",entry.file_name().to_str().unwrap()),std::fs::read(entry.path()).unwrap());
            }
        }
    }
    files
}

#[tokio::test]
async fn compactor_shutdown_during_publication_lock_skips_journal_and_native_reopen() {
    let (dir,h,run,mock,server)=fixture("openai",vec![final_response("openai","late summary")]).await;
    let bank=dir.path().join("memory");
    let key={let mut memory=run.memory.memory.lock().await;let id=memory.append(Kind::User,&"long input ".repeat(100)).unwrap();NodeKey{level:0,index:id}};
    let worker=tokio::spawn(h.clone().compactor(GLOBAL_COMPACTOR,run.memory.shared.clone()));
    tokio::time::timeout(Duration::from_secs(5),mock.started.notified()).await.unwrap();
    let held=run.memory.memory.lock().await;
    let original=held.zoom(key.index,1).unwrap();
    let before=held.render();
    let journal=compaction_journal_bytes(&bank);
    mock.release.notify_one();
    // The selected-result hook is immediately before the lock await. Once this
    // wakes, the actual daemon branch cannot publish until we release `held`.
    tokio::time::timeout(Duration::from_secs(5),run.memory.compaction_selected.notified()).await.unwrap();
    h.shutdown.cancel();
    drop(held);
    tokio::time::timeout(Duration::from_secs(5),worker).await.unwrap().unwrap().unwrap();
    assert_eq!(compaction_journal_bytes(&bank),journal);
    {
        let mut memory=run.memory.memory.lock().await;
        assert_eq!(memory.render(),before);
        assert_eq!(memory.zoom(key.index,1).unwrap(),original);
        assert!(!memory.render().contains("late summary"));
        assert_eq!(memory.stats()["journal_healthy"],true);
        let old=std::mem::replace(&mut *memory,Memory::open(dir.path().join("holder"),128000).unwrap());drop(old);
        *memory=Memory::open(&bank,128000).unwrap();
        assert_eq!(memory.render(),before);
        assert_eq!(memory.zoom(key.index,1).unwrap(),original);
        assert!(!memory.render().contains("late summary"));
        assert_eq!(memory.stats()["journal_healthy"],true);
    }
    assert_eq!(compaction_journal_bytes(&bank),journal);
    assert_eq!(mock.requests.lock().await.len(),1);
    server.abort();
}

#[tokio::test]
async fn compactor_committed_publication_before_shutdown_survives_native_reopen() {
    let (dir,h,run,mock,server)=fixture("openai",vec![final_response("openai","committed summary")]).await;
    let bank=dir.path().join("memory");
    let key={let mut memory=run.memory.memory.lock().await;let id=memory.append(Kind::User,&"long input ".repeat(100)).unwrap();NodeKey{level:0,index:id}};
    let original=run.memory.memory.lock().await.zoom(key.index,1).unwrap();
    let before=compaction_journal_bytes(&bank);
    let changed=run.memory.changed.notified();tokio::pin!(changed);changed.as_mut().enable();
    let worker=tokio::spawn(h.clone().compactor(GLOBAL_COMPACTOR,run.memory.shared.clone()));
    tokio::time::timeout(Duration::from_secs(5),mock.started.notified()).await.unwrap();
    mock.release.notify_one();
    // `changed` follows synchronous finish; no scheduling sleep determines order.
    tokio::time::timeout(Duration::from_secs(5),changed).await.unwrap();
    let committed={let memory=run.memory.memory.lock().await;let text=memory.render();assert!(text.contains("committed summary"));text};
    let journal=compaction_journal_bytes(&bank);assert_ne!(journal,before);
    h.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5),worker).await.unwrap().unwrap().unwrap();
    {
        let mut memory=run.memory.memory.lock().await;
        let old=std::mem::replace(&mut *memory,Memory::open(dir.path().join("holder"),128000).unwrap());drop(old);
        *memory=Memory::open(&bank,128000).unwrap();
        assert_eq!(memory.render(),committed);
        assert_eq!(memory.zoom(key.index,1).unwrap(),original);
        assert_eq!(memory.stats()["journal_healthy"],true);
        assert!(memory.is_settled());
    }
    assert_eq!(compaction_journal_bytes(&bank),journal);
    assert_eq!(mock.requests.lock().await.len(),1);
    server.abort();
}
