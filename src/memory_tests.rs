use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn key(l: u32, i: u64) -> NodeKey {
    NodeKey { level: l, index: i }
}
fn digest(s: &str) -> Value {
    json!({"bytes":s.len(),"utf16":s.encode_utf16().count(),"sha256":hex::encode(Sha256::digest(s.as_bytes()))})
}
fn fixtures() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/uniichat-0500bf6.json")).unwrap()
}
fn state(mem: &Memory, label: &str) -> Value {
    let refs = |v: &View| {
        v.parts
            .iter()
            .map(|k| format!("{}+{}", k.start(), k.count()))
            .collect::<Vec<_>>()
    };
    let jobs=mem.ready_jobs(usize::MAX).into_iter().map(|k|json!({"span":format!("{}+{}",k.start(),k.count()),"context":digest(&mem.compactor_context(k).unwrap())})).collect::<Vec<_>>();
    json!({"label":label,"count":mem.count,"open":mem.open,"view":refs(&mem.view),"viewBatch":mem.view.batch,"viewSize":part_bytes(&mem.view.parts,&mem.nodes),
        "seen":refs(&mem.seen),"seenBatch":mem.seen.batch,"seenSize":part_bytes(&mem.seen.parts,&mem.nodes),"jobs":jobs,"rendered":digest(&mem.render())})
}
fn drain(mem: &mut Memory) {
    while let Some(k) = mem.ready_jobs(1).first().copied() {
        mem.finish(k, &format!("summary {}+{}", k.start(), k.count()))
            .unwrap()
    }
}
#[test]
fn actual_upstream_scheduler_gating_and_completion_order_differential() {
    let temp = TempDir::new().unwrap();
    let mut mem = Memory::open(temp.path(), 4096).unwrap();
    let mut events = Vec::new();
    for k in 0..70 {
        mem.append(Kind::User, &format!("L{k}:{}", "x".repeat(900)))
            .unwrap();
        if [0, 1, 63, 64, 69].contains(&k) {
            events.push(state(&mem, &format!("append-{k}")))
        }
    }
    for i in (0..64).rev() {
        mem.finish(key(0, i), &"s".repeat(512)).unwrap();
        if [63, 1, 0].contains(&i) {
            events.push(state(&mem, &format!("finish-{i}")))
        }
    }
    for i in 64..70 {
        mem.finish(key(0, i), &"s".repeat(512)).unwrap();
    }
    events.push(state(&mem, "all-leaves"));
    for wave in 0..8 {
        let mut busy = mem.ready_jobs(usize::MAX);
        if busy.is_empty() {
            break;
        }
        busy.sort_by_key(|k| std::cmp::Reverse(k.index));
        for k in busy {
            mem.finish(k, &"s".repeat(512)).unwrap()
        }
        events.push(state(&mem, &format!("wave-{wave}")));
    }
    let expected = fixtures()[0]["events"].as_array().unwrap().clone();
    for (got, want) in events.iter().zip(&expected) {
        assert_eq!(got, want, "event {}", got["label"])
    }
    assert_eq!(events.len(), expected.len());
}
#[test]
fn actual_upstream_free_chains_and_open_groups_differential() {
    let temp = TempDir::new().unwrap();
    let mut mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
    for k in 0..20 {
        mem.push(Kind::User, &format!("short {k}"), &[], k < 2)
            .unwrap();
        assert_eq!(
            state(&mem, &format!("short-{k}")),
            fixtures()[1]["events"][k]
        );
    }
}
fn page_inputs() -> Vec<String> {
    vec![
        String::new(),
        "a".repeat(30000),
        "a".repeat(30001),
        format!("\n{}", "a".repeat(30000)),
        format!("{}\nZ", "a".repeat(29999)),
        format!("{}😀Z", "a".repeat(29999)),
        "é".repeat(30001),
        "e\u{0301}".repeat(15001),
    ]
}
#[test]
fn actual_upstream_utf16_page_boundaries_differential() {
    for (i, text) in page_inputs().iter().enumerate() {
        let pages = clip_pages(text);
        assert_eq!(pages.concat(), *text);
        assert_eq!(
            json!(pages.iter().map(|p| digest(p)).collect::<Vec<_>>()),
            fixtures()[2]["inputs"][i]["pages"]
        );
    }
}
#[test]
fn actual_upstream_images_right_aligned_differential() {
    for input in fixtures()[3]["inputs"].as_array().unwrap() {
        let dir = TempDir::new().unwrap();
        let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
        let text = if input["long"].as_bool().unwrap() {
            "z".repeat(30001)
        } else {
            "short".to_owned()
        };
        let images: Vec<String> = serde_json::from_value(input["images"].clone()).unwrap();
        mem.push(Kind::User, &text, &images, false).unwrap();
        let pages = (0..mem.count)
            .map(|id| {
                let msg = mem.message(id).unwrap();
                json!({"text":digest(&msg.text),"images":msg.images,"more":msg.more})
            })
            .collect::<Vec<_>>();
        assert_eq!(json!(pages), input["pages"]);
        assert_eq!(mem.say(0, mem.count).unwrap(), text);
    }
}
#[test]
fn restarted_hidden_group_is_abandoned_before_unrelated_append() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    mem.append(Kind::User, "visible").unwrap();
    mem.push(Kind::Echo, "unfinished A", &[], true).unwrap();
    let before = mem.render();
    drop(mem);
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    assert_eq!((mem.count, mem.open), (1, 0));
    assert_eq!(mem.render(), before);
    assert!(mem.zoom(1, 1).is_err());
    assert_eq!(mem.push(Kind::Echo, "unrelated B", &[], false).unwrap(), 1);
    assert_eq!((mem.count, mem.open), (2, 0));
    assert_eq!(mem.say(1, 2).unwrap(), "unrelated B");
    assert!(!mem.render().contains("unfinished A"));
    drop(mem);
    let mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    assert_eq!(mem.say(1, 2).unwrap(), "unrelated B");
    let raw = fs::read_to_string(fs::read_dir(dir.path().join("main")).unwrap().next().unwrap().unwrap().path()).unwrap();
    assert!(raw.contains("unfinished A"), "unpublished physical record is retained");
    assert!(raw.contains("unrelated B"));
}
#[test]
fn indexed_originals_are_immutable_and_unicode_zoom_cursors_are_native() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    let text = format!("{}😀Z", "a".repeat(29999));
    mem.append(Kind::User, &text).unwrap();
    assert_eq!(mem.count, 2);
    assert_eq!(mem.say(0, 2).unwrap(), text);
    assert!(mem.zoom(0, 1).unwrap().ends_with(&"a".repeat(29999)));
    assert_eq!(mem.zoom(1, 1).unwrap(), "1+0|user: 😀Z");
    assert!(mem.zoom_at(1, 1, 1).is_err());
    assert_eq!(mem.zoom_at(1, 1, 2).unwrap(), "1+0|user: Z");
    let before = mem.zoom(0, 1).unwrap();
    drain(&mut mem);
    assert_eq!(mem.zoom(0, 1).unwrap(), before);
}
#[test]
fn restart_preserves_exact_partitions_and_no_completed_node_is_reasked() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), 4096).unwrap();
    for k in 0..140 {
        mem.append(Kind::User, &format!("{k} {}", "x".repeat(900)))
            .unwrap();
    }
    // Distinct arbitrary completion order before saving an unfinished batch.
    for k in (0..30).rev() {
        mem.finish(key(0, k), &"s".repeat(512)).unwrap();
    }
    let parts = mem.view.clone();
    let seen = mem.seen.clone();
    let serial = mem.serial;
    let before = mem.render();
    drop(mem);
    let mut mem = Memory::open(dir.path(), 4096).unwrap();
    assert_eq!(mem.view.parts, parts.parts);
    assert_eq!(mem.view.batch, parts.batch);
    assert_eq!(mem.seen.parts, seen.parts);
    assert_eq!(mem.render(), before);
    assert_eq!(mem.serial, serial);
    assert!(
        !mem.ready_jobs(64)
            .iter()
            .any(|k| k.level == 0 && k.index < 30)
    );
    drain(&mut mem);
    let before = mem.render();
    let count = mem.count;
    let serial = mem.serial;
    drop(mem);
    let mem = Memory::open(dir.path(), 4096).unwrap();
    assert_eq!(mem.render(), before);
    assert_eq!(mem.count, count);
    assert_eq!(mem.serial, serial);
}
#[test]
fn separate_compactor_view_frontier_and_frozen_launch_context_no_future_sentinel() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    for k in 0..100 {
        mem.append(Kind::User, &format!("{k} {}", "x".repeat(900)))
            .unwrap();
        mem.finish(key(0, k), &"s".repeat(512)).unwrap();
    }
    // Completing all internal nodes lets the seen partition fall to 16KB,
    // independently of the 128KB main view, without rebuilding its prefix.
    drain(&mut mem);
    assert!(part_bytes(&mem.seen.parts, &mem.nodes) <= COMPACTOR_BYTES);
    assert!(mem.view.parts.len() > mem.seen.parts.len());
    let id = mem
        .append(Kind::User, &"before sentinel ".repeat(90))
        .unwrap();
    let k = key(0, id);
    let context = mem.compactor_context(k).unwrap();
    mem.append(Kind::User, "FUTURE_ONLY_SENTINEL").unwrap();
    assert_eq!(mem.compactor_context(k).unwrap(), context);
    assert!(!context.contains("FUTURE_ONLY_SENTINEL"));
}
#[test]
fn ready_window_never_exceeds_64_and_merge_wins_ctx_ties() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    for _ in 0..100 {
        mem.append(Kind::User, &"x".repeat(900)).unwrap();
    }
    assert_eq!(mem.ready_jobs(1000).len(), 64);
    assert_eq!(mem.ready_jobs(1000).last().unwrap().index, 63);
    mem.finish(key(0, 0), &"s".repeat(512)).unwrap();
    mem.finish(key(0, 1), &"s".repeat(512)).unwrap();
    let jobs = mem.ready_jobs(1000);
    assert_eq!(jobs.len(), 64);
    let first_at2 = jobs.iter().find(|k| ctx(**k) == 2).unwrap();
    assert_eq!(*first_at2, key(1, 0));
    assert!(!mem.is_settled());
}
#[test]
fn sources_deduplicate_paged_admissions_across_restart_with_exact_content() {
    let dir = TempDir::new().unwrap();
    let text = "é😀".repeat(17000);
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    assert_eq!(mem.append_with_id(Kind::User, &text, "input:a").unwrap(), 0);
    let count = mem.count;
    drop(mem);
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    assert_eq!(mem.append_with_id(Kind::User, &text, "input:a").unwrap(), 0);
    assert_eq!(mem.count, count);
    assert!(
        mem.append_with_id(Kind::User, "different", "input:a")
            .is_err()
    );
    assert!(mem.append_with_id(Kind::Talk, &text, "input:a").is_err());
}
#[test]
fn snapshots_freeze_original_and_tree_frontiers_without_cloning_history() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    for _ in 0..8 {
        let i = mem.append(Kind::User, &"original".repeat(100)).unwrap();
        mem.finish(key(0, i), &"s".repeat(512)).unwrap();
    }
    let snap = mem.snapshot().unwrap();
    let serialized = serde_json::to_vec(&snap).unwrap();
    assert!(serialized.len() < 10_000);
    let snap: MemorySnapshot = serde_json::from_slice(&serialized).unwrap();
    let original = snap.zoom(0, 1).unwrap();
    let parent = snap.zoom(0, 8).unwrap();
    let id = mem.append(Kind::User, "FUTURE_ONLY_SENTINEL").unwrap();
    drain(&mut mem);
    assert_eq!(snap.zoom(0, 1).unwrap(), original);
    assert_eq!(snap.zoom(0, 8).unwrap(), parent);
    assert!(snap.zoom(id, 1).is_err());
    assert!(snap.date(id).is_err());
    assert!(!snap.render().contains("FUTURE_ONLY_SENTINEL"));
    drop(mem);
    let _mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    assert_eq!(snap.zoom(0, 1).unwrap(), original);
}
#[test]
fn snapshots_require_settled_leaves_but_not_finished_internal_tree() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    mem.append(Kind::User, &"x".repeat(900)).unwrap();
    assert!(mem.snapshot().is_err());
    mem.finish(key(0, 0), "summary").unwrap();
    assert!(mem.snapshot().is_ok());
}
#[test]
fn summary_text_pruned_after_both_views_pass_but_disk_zoom_survives() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), 4096).unwrap();
    for _ in 0..160 {
        let id = mem.append(Kind::User, &"x".repeat(900)).unwrap();
        mem.finish(key(0, id), &"s".repeat(512)).unwrap();
        drain(&mut mem);
    }
    assert!(
        mem.nodes.len() < mem.serial as usize * 3 / 4,
        "{} / {}",
        mem.nodes.len(),
        mem.serial
    );
    assert!(!mem.nodes.contains_key(&key(0, 0)));
    assert!(mem.zoom(0, 2).unwrap().contains(&"s".repeat(512)));
    assert!(mem.zoom(0, 1).unwrap().contains(&"x".repeat(900)));
}
#[test]
fn generic_native_schema_validation_is_read_only_before_opening_writer() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("unexpected"), b"immutable payload").unwrap();
    assert!(Memory::open(dir.path(), VIEW_BYTES).is_err());
    assert!(!dir.path().join("lock").exists());
    assert!(!dir.path().join("format").exists());
    assert_eq!(
        fs::read(dir.path().join("unexpected")).unwrap(),
        b"immutable payload"
    );
    fs::write(dir.path().join("format"), "unsupported-native-schema\n").unwrap();
    assert!(Memory::open(dir.path(), VIEW_BYTES).is_err());
    assert!(!dir.path().join("lock").exists());
}
#[test]
fn exclusive_writer_lock_releases_without_deleting_lock_inode() {
    let dir = TempDir::new().unwrap();
    let first = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    assert!(Memory::open(dir.path(), VIEW_BYTES).is_err());
    drop(first);
    assert!(Memory::open(dir.path(), VIEW_BYTES).is_ok());
}
#[test]
fn journal_failure_poisoning_prevents_visibility_or_followup_writes() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    fs::remove_dir(dir.path().join("main")).unwrap();
    fs::write(dir.path().join("main"), b"blocked").unwrap();
    assert!(mem.append(Kind::User, "must not appear").is_err());
    assert_eq!(mem.count, 0);
    assert!(mem.append(Kind::User, "no retry").is_err());
    assert!(mem.snapshot().is_err());
}
#[test]
fn tree_failure_does_not_publish_summary_and_poisoning_survives_to_caller() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    mem.append(Kind::User, &"x".repeat(900)).unwrap();
    fs::remove_dir(dir.path().join("tree")).unwrap();
    fs::write(dir.path().join("tree"), b"blocked").unwrap();
    assert!(mem.finish(key(0, 0), "summary").is_err());
    assert_eq!(mem.serial, 0);
    assert!(!mem.is_settled());
    assert!(mem.append(Kind::User, "no retry").is_err());
}
#[test]
fn checkpoint_failure_is_fail_closed_and_committed_original_recovers() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    fs::create_dir(dir.path().join("view.json.new")).unwrap();
    assert!(
        mem.append(Kind::User, "durable before failed checkpoint")
            .is_err()
    );
    assert!(mem.append(Kind::User, "no retry").is_err());
    drop(mem);
    fs::remove_dir(dir.path().join("view.json.new")).unwrap();
    let mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    assert_eq!(mem.count, 1);
    assert!(mem.zoom(0, 1).unwrap().contains("durable before"));
}
#[test]
fn torn_hidden_page_tail_is_abandoned_and_only_unpublished_position_is_reused() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    mem.push(Kind::User, "durable hidden", &[], true).unwrap();
    drop(mem);
    let path = fs::read_dir(dir.path().join("main"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"i\":1,\"text\":\"torn")
        .unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    assert_eq!((mem.count, mem.open), (0, 0));
    assert!(mem.zoom(0, 1).is_err());
    assert_eq!(mem.push(Kind::User, "close", &[], false).unwrap(), 0);
    assert_eq!(mem.say(0, 1).unwrap(), "close");
    drop(mem);
    assert_eq!(
        Memory::open(dir.path(), VIEW_BYTES)
            .unwrap()
            .say(0, 1)
            .unwrap(),
        "close"
    );
}
#[test]
fn cache_blocks_use_four_lines_and_one_near_tail_mark_without_losing_bytes() {
    for n in 0..10 {
        let view = format!(
            "<chat>\n{}</chat>\n",
            (0..n).map(|i| format!("{i}+1|é😀\n")).collect::<String>()
        );
        let blocks = cache_chunks(&view);
        assert_eq!(blocks.concat(), view);
        assert_eq!(blocks.len(), n / 4 + 2);
    }
}
#[test]
fn ruler_and_cutoff_are_neutral_and_target_not_hard_cap_matches_upstream() {
    let task = compaction_task(key(1, 0), "a\nb");
    assert!(task.contains(&"-".repeat(512)));
    assert!(task.contains("0+1 and 1+1"));
    assert!(!task.contains("Thoth"));
    let line = "😀".repeat(200);
    assert_eq!(compaction_cut(&line).len(), 512);
    assert!(compaction_over(&line).contains("800 bytes"));
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    mem.append(Kind::User, &"x".repeat(900)).unwrap();
    mem.finish(key(0, 0), &"s".repeat(600)).unwrap();
    assert_eq!(mem.node(key(0, 0)).unwrap().unwrap().len(), 600);
}
#[test]
fn html_export_escapes_untrusted_originals() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    mem.append(Kind::User, "<script>danger</script>").unwrap();
    let html = mem.export_html();
    assert!(html.contains("&lt;script&gt;"));
    assert!(!html.contains("<script>"));
}
#[test]
fn actual_upstream_flat_uses_javascript_whitespace_not_rust_whitespace() {
    for input in fixtures()[4]["inputs"].as_array().unwrap() {
        assert_eq!(
            flatten(input["text"].as_str().unwrap()),
            input["flat"].as_str().unwrap()
        );
    }
    for input in fixtures()[4]["inputs"].as_array().unwrap() {
        assert_eq!(
            summary_trim(input["text"].as_str().unwrap()),
            input["trim"].as_str().unwrap()
        );
    }
    assert!(compaction_over("a\nb").ends_with("a\nb| ← LIMIT"));
    assert_eq!(compaction_fallback("a\nb"), "a b");
}
#[test]
fn complete_native_tail_without_newline_recovers_consistently_on_every_restart() {
    let dir = TempDir::new().unwrap();
    let mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    drop(mem);
    let msg = Message {
        i: 0,
        kind: Kind::User,
        text: "whole recovered".into(),
        size: 21,
        date: Local::now().to_rfc3339(),
        source_id: None,
        more: false,
        images: vec![],
    };
    let path = dir
        .path()
        .join("main")
        .join(Local::now().format("%Y-%m-%d.jsonl").to_string());
    fs::write(&path, serde_json::to_vec(&msg).unwrap()).unwrap();
    for _ in 0..2 {
        let mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
        assert_eq!(mem.count, 1);
        assert!(mem.zoom(0, 1).unwrap().contains("whole recovered"));
    }
}
#[test]
fn invalid_native_checkpoint_partition_is_rejected_without_renumbering() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    mem.append(Kind::User, "keep exact ID").unwrap();
    drop(mem);
    let mut view: Value =
        serde_json::from_slice(&fs::read(dir.path().join("view.json")).unwrap()).unwrap();
    view["view"]["parts"] = json!([]);
    fs::write(
        dir.path().join("view.json"),
        serde_json::to_vec(&view).unwrap(),
    )
    .unwrap();
    assert!(Memory::open(dir.path(), VIEW_BYTES).is_err());
}
#[test]
fn fsync_failure_and_index_failure_poison_without_false_admission_then_recover_once() {
    for sync in [true, false] {
        let dir = TempDir::new().unwrap();
        let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
        mem.main_log.fault.sync = sync;
        mem.main_log.fault.index = !sync;
        assert!(
            mem.append_with_id(Kind::User, "uncertain durable append", "input:a")
                .is_err()
        );
        assert_eq!(mem.count, 0);
        assert!(!mem.has_source("input:a"));
        assert!(mem.append(Kind::Talk, "must not write").is_err());
        drop(mem);
        let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
        assert_eq!(mem.count, 1);
        assert_eq!(
            mem.append_with_id(Kind::User, "uncertain durable append", "input:a")
                .unwrap(),
            0
        );
        assert_eq!(mem.count, 1);
    }
}
#[test]
fn interrupted_same_source_admission_rewrites_whole_unpublished_group() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    let text = "a".repeat(30001);
    // Exactly one native page completes before an injected short group write.
    // The incomplete suffix is a torn JSON record, not another admitted ID.
    let date = "2026-10-19T00:00:00+00:00";
    let msg = Message {
        i: 0,
        kind: Kind::User,
        text: "a".repeat(30000),
        size: 30006,
        date: date.into(),
        source_id: Some("input:a".into()),
        more: true,
        images: vec![],
    };
    // append_with_id chooses its own date, whose serialized width may differ;
    // inject a prefix long enough for page one and a torn page two.
    mem.main_log.fault.short = Some(serde_json::to_vec(&msg).unwrap().len() + 30);
    assert!(mem.append_with_id(Kind::User, &text, "input:a").is_err());
    assert_eq!(mem.count, 0);
    assert!(mem.snapshot().is_err());
    drop(mem);
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    assert_eq!((mem.count, mem.open), (0, 0));
    assert!(!mem.has_source("input:a"));
    assert_eq!(mem.append_with_id(Kind::User, &text, "input:a").unwrap(), 0);
    assert_eq!((mem.count, mem.open), (2, 0));
    assert_eq!(mem.say(0, 2).unwrap(), text);
    assert_eq!(mem.date(0).unwrap(), mem.date(1).unwrap());
    drop(mem);
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    assert_eq!(mem.append_with_id(Kind::User, &text, "input:a").unwrap(), 0);
    assert_eq!(mem.count, 2);
}
#[test]
fn synced_tree_with_failed_index_is_recovered_without_reasking_node() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    mem.append(Kind::User, &"x".repeat(900)).unwrap();
    mem.tree_log.fault.index = true;
    assert!(mem.finish(key(0, 0), "durable summary").is_err());
    assert_eq!(mem.serial, 0);
    drop(mem);
    let mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    assert!(mem.is_settled());
    assert!(mem.ready_jobs(64).is_empty());
    assert!(mem.render().contains("durable summary"));
}
#[test]
fn finish_rejects_unscheduled_future_work_outside_bounded_frontier() {
    let dir = TempDir::new().unwrap();
    let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
    for _ in 0..70 {
        mem.append(Kind::User, &"x".repeat(900)).unwrap();
    }
    let before = mem.serial;
    assert!(mem.finish(key(0, 69), "unscheduled").is_err());
    assert_eq!(mem.serial, before);
}

#[test]
fn actual_upstream_restart_discards_open_accumulation_differential() {
    let fixtures=fixtures();
    let fixture=fixtures.as_array().unwrap().iter().find(|v|v["name"]=="restart-open-group").unwrap();
    for input in fixture["inputs"].as_array().unwrap() {
        let dir=TempDir::new().unwrap();
        let mut mem=Memory::open(dir.path(),VIEW_BYTES).unwrap();
        mem.push(Kind::User,"unfinished A",&[],true).unwrap();
        let mut events=vec![state(&mem,"open-A")];
        drop(mem);
        let mut mem=Memory::open(dir.path(),VIEW_BYTES).unwrap();
        events.push(state(&mem,"reopen"));
        mem.append(Kind::User,if input["matching"]==true {"unfinished A"} else {"unrelated B"}).unwrap();
        events.push(state(&mem,"close"));
        assert_eq!(json!(events),input["events"]);
        let text=(0..mem.count).map(|i|mem.message(i).unwrap().text).collect::<Vec<_>>();
        assert_eq!(json!(text),input["text"]);
    }
}

#[test]
fn restarted_unkeyed_talk_and_echo_groups_never_join_unrelated_keyed_input() {
    for kind in [Kind::Talk, Kind::Echo] {
        let dir = TempDir::new().unwrap();
        let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
        mem.append_with_id(Kind::User, "committed", "input:committed").unwrap();
        let committed = mem.zoom(0, 1).unwrap();
        mem.push(kind, "abandoned private prefix", &[], true).unwrap();
        drop(mem);
        for _ in 0..2 {
            let mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
            assert_eq!((mem.count, mem.open), (1, 0));
            assert_eq!(mem.zoom(0, 1).unwrap(), committed);
            assert!(mem.zoom(1,1).is_err());
        }
        let mut mem = Memory::open(dir.path(), VIEW_BYTES).unwrap();
        assert_eq!(mem.append_with_id(Kind::User, "unrelated B", "input:b").unwrap(), 1);
        assert_eq!(mem.say(0,2).unwrap(), "committedunrelated B");
        assert_eq!(mem.zoom(0,1).unwrap(), committed);
        drop(mem);
        let mem=Memory::open(dir.path(),VIEW_BYTES).unwrap();
        assert_eq!(mem.lookup_source("input:b"), Some(1));
        assert!(!mem.say(0,2).unwrap().contains("abandoned"));
    }
}

#[test]
fn partial_replacement_and_different_source_retry_only_reuse_unpublished_ids() {
    let dir=TempDir::new().unwrap();
    let mut mem=Memory::open(dir.path(),VIEW_BYTES).unwrap();
    mem.append_with_id(Kind::User,"committed","input:committed").unwrap();
    let committed=mem.zoom(0,1).unwrap();
    mem.push(Kind::Talk,"A never closed",&[],true).unwrap();
    drop(mem);
    let mut mem=Memory::open(dir.path(),VIEW_BYTES).unwrap();
    mem.main_log.fault.short=Some(31000);
    assert!(mem.append_with_id(Kind::User,&"b".repeat(60001),"input:b").is_err());
    assert!(mem.snapshot().is_err());
    drop(mem);
    let mut mem=Memory::open(dir.path(),VIEW_BYTES).unwrap();
    assert_eq!((mem.count,mem.open),(1,0));
    assert!(!mem.has_source("input:b"));
    assert_eq!(mem.zoom(0,1).unwrap(),committed);
    assert_eq!(mem.append_with_id(Kind::User,"replacement C","input:c").unwrap(),1);
    assert_eq!(mem.say(1,2).unwrap(),"replacement C");
    drop(mem);
    let mut mem=Memory::open(dir.path(),VIEW_BYTES).unwrap();
    assert_eq!(mem.lookup_source("input:b"),None);
    assert_eq!(mem.lookup_source("input:c"),Some(1));
    // A previously unpublished source may arrive later as a complete, new
    // native admission. It cannot overwrite C's now-committed ID.
    assert_eq!(mem.append_with_id(Kind::User,"complete B","input:b").unwrap(),2);
    assert_eq!(mem.zoom(0,1).unwrap(),committed);
    assert_eq!(mem.say(1,3).unwrap(),"replacement Ccomplete B");
}

#[test]
fn native_scan_never_permits_superseding_a_published_position() {
    let dir=TempDir::new().unwrap();
    let mut mem=Memory::open(dir.path(),VIEW_BYTES).unwrap();
    mem.append(Kind::User,"committed").unwrap();
    let msg=mem.message(0).unwrap();
    drop(mem);
    let path=fs::read_dir(dir.path().join("main")).unwrap().next().unwrap().unwrap().path();
    let mut bytes=serde_json::to_vec(&msg).unwrap();bytes.push(b'\n');
    OpenOptions::new().append(true).open(path).unwrap().write_all(&bytes).unwrap();
    assert!(Memory::open(dir.path(),VIEW_BYTES).is_err());
}

#[test]
fn actual_upstream_whole_suffix_limits_differential() {
    let f:Value=serde_json::from_str(include_str!("../tests/fixtures/compactor-contract-0500bf6.json")).unwrap();
    for case in f["suffixes"].as_array().unwrap() {
        let dir=TempDir::new().unwrap();let mut mem=Memory::open(dir.path(),VIEW_BYTES).unwrap();
        let mut groups=Vec::new();
        for (text,pics) in case["texts"].as_array().unwrap().iter().zip(case["images"].as_array().unwrap()) {
            let start=mem.count;
            mem.push(Kind::User,text.as_str().unwrap(),&(0..pics.as_u64().unwrap()).map(|n|format!("image{n}")).collect::<Vec<_>>(),false).unwrap();
            groups.push((start,mem.count));
        }
        let from=mem.whole(0).unwrap();
        assert_eq!(from,case["from"].as_u64().unwrap(),"{}",case["name"]);
        assert_eq!(mem.count,case["pages"].as_u64().unwrap());
        let raw=groups.into_iter().filter(|(_,end)|*end>from).map(|(start,end)|mem.say(start.max(from),end).unwrap()).collect::<Vec<_>>();
        assert_eq!(json!(raw),case["raw"]);
        assert!(mem.settled_before(0).unwrap());
        assert!(mem.whole(mem.count+1).is_err());
    }
}
