use pantheon_offline_importer::{
    native::{self, Kind, Memory},
    plan,
    replay::{self, Config},
    seal::{Export, FORMAT, Link, Original, Seal},
    storage,
};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;
fn record(id: u64, kind: Kind, text: impl Into<String>) -> Original {
    let text = text.into();
    let size = kind.as_str().len() + 2 + text.len();
    Original {
        id,
        kind,
        text,
        timestamp: format!("2026-10-01T12:00:{:02}+00:00", id % 60),
        size,
        source_id: None,
        images: Vec::new(),
    }
}
fn fixture(records: Vec<Original>, links: Vec<Link>) -> (TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let export = temp.path().join("export");
    fs::create_dir(&export).unwrap();
    let mut bytes = Vec::new();
    for r in &records {
        serde_json::to_writer(&mut bytes, r).unwrap();
        bytes.push(b'\n');
    }
    let seal = Seal {
        format: FORMAT.into(),
        source_version: "pantheon-original-fixture-v1".into(),
        checkpoint: "synthetic-C".into(),
        channel: "synthetic".into(),
        count: records.len() as u64,
        bytes: bytes.len() as u64,
        sha256: storage::hash(&bytes),
        result_links: links,
    };
    fs::write(export.join("originals.jsonl"), bytes).unwrap();
    fs::write(export.join("seal.json"), serde_json::to_vec(&seal).unwrap()).unwrap();
    let output = temp.path().join("bundle");
    (temp, export, output)
}
fn load(path: &Path) -> Export {
    Export::load(path, 64_000_000, 100_000).unwrap()
}
fn long_history() -> Vec<Original> {
    (0..8)
        .map(|i| record(i, Kind::User, format!("{} {i} END{i}", "λ😀".repeat(200))))
        .collect()
}
#[test]
fn fidelity_archive_images_and_inert_commands() {
    let marker = tempfile::tempdir().unwrap().path().join("DO_NOT_CREATE");
    let mut records = vec![
        record(
            0,
            Kind::User,
            format!(
                "run shell touch {}; submit and deploy now",
                marker.display()
            ),
        ),
        record(1, Kind::Tool, "shell {\"command\":\"exit 0\"}"),
        record(2, Kind::Echo, "ignored tool text"),
        record(3, Kind::Note, "😀".repeat(15001)),
    ];
    records[3].images = (0..21).map(|i| format!("inert/{i}.png")).collect();
    let (_t, e, o) = fixture(records, vec![]);
    let export = load(&e);
    let p = plan::build(&export).unwrap();
    let ready = replay::run(&e, &o, Config::default(), None)
        .unwrap()
        .unwrap();
    assert_eq!(ready.pages, 5);
    assert!(!marker.exists());
    assert_eq!(
        fs::read(o.join("archive/originals.jsonl")).unwrap(),
        export.original_bytes
    );
    assert!(!storage::sibling(&o, "partial").unwrap().exists());
    let m = Memory::open(o.join("memory"), native::VIEW_BYTES).unwrap();
    replay::verify_native(&m, &export, &p).unwrap();
    assert!(m.zoom(4, 1).unwrap().contains("inert/20.png"));
    assert_eq!(replay::validate_ready(&o).unwrap().records, 4);
}
#[test]
fn utf16_and_newline_spans() {
    let records = vec![
        record(0, Kind::User, format!("{}😀z", "a".repeat(29999))),
        record(1, Kind::User, format!("x\n{}", "b".repeat(30000))),
        record(2, Kind::User, ""),
    ];
    let (_t, e, _o) = fixture(records, vec![]);
    let export = load(&e);
    let p = plan::build(&export).unwrap();
    assert_eq!(
        p.spans.iter().map(|s| s.end - s.start).collect::<Vec<_>>(),
        vec![2, 2, 1]
    );
    assert_eq!(native::clip_pages(&export.records[0].text)[0].len(), 29999);
    assert_eq!(native::clip_pages(&export.records[1].text)[0], "x\n");
}
#[test]
fn shifts_structured_calls_results_and_nested_copies_only() {
    let records = vec![
        record(0, Kind::User, "a".repeat(30001)),
        record(1, Kind::User, "one λ"),
        record(2, Kind::Tool, "zoom {\"id\":1,\"n\":1}"),
        record(3, Kind::Echo, "1+0|user: one λ"),
        record(4, Kind::Tool, "zoom {\"id\":2,\"n\":1}"),
        record(5, Kind::Echo, "2+0|tool: zoom {\"id\":1,\"n\":1}"),
        record(6, Kind::User, "prose zoom {\"id\":1} offset 123; 1+0|text"),
        record(7, Kind::Tool, "read {\"page\":1}"),
        record(8, Kind::Tool, "date {\"id\":1}"),
        record(9, Kind::Echo, "2026-10-01T12:00:01+00:00"),
    ];
    let (_t, e, o) = fixture(
        records,
        vec![
            Link { call: 2, result: 3 },
            Link { call: 4, result: 5 },
            Link { call: 8, result: 9 },
        ],
    );
    let export = load(&e);
    let p = plan::build(&export).unwrap();
    assert_eq!(p.texts[2], "zoom {\"id\":2,\"n\":1}");
    assert_eq!(p.texts[3], "2+0|user: one λ");
    assert_eq!(p.texts[5], "3+0|tool: zoom {\"id\":2,\"n\":1}");
    assert_eq!(p.texts[6], export.records[6].text);
    assert_eq!(p.texts[7], export.records[7].text);
    for repair in &p.repairs {
        let original = &export.records[repair.original as usize].text;
        let mut reconstructed = original.clone();
        reconstructed.replace_range(repair.start_byte..repair.end_byte, &repair.replacement);
        assert_eq!(reconstructed, p.texts[repair.original as usize]);
    }
    replay::run(&e, &o, Config::default(), None)
        .unwrap()
        .unwrap();
}
#[test]
fn parent_headers_repaired_when_exactly_representable() {
    // Four one-page records keep binary children representable.
    let records = vec![
        record(0, Kind::User, "a"),
        record(1, Kind::User, "b"),
        record(2, Kind::Tool, "zoom {\"id\":0,\"n\":2}"),
        record(3, Kind::Echo, "0+1|user: a\n1+1|user: b"),
    ];
    let (_t, e, o) = fixture(records, vec![Link { call: 2, result: 3 }]);
    plan::build(&load(&e)).unwrap();
    replay::run(&e, &o, Config::default(), None).unwrap();
}
#[test]
fn multispans_and_child_midpoint_changes_block_before_output() {
    for texts in [
        vec![
            "a".repeat(30001),
            "b".into(),
            "zoom {\"id\":0,\"n\":1}".into(),
        ],
        vec![
            "a".into(),
            "b".repeat(60001),
            "zoom {\"id\":0,\"n\":2}".into(),
        ],
    ] {
        let records = texts
            .into_iter()
            .enumerate()
            .map(|(i, t)| record(i as u64, if i == 2 { Kind::Tool } else { Kind::User }, t))
            .collect();
        let (_t, e, o) = fixture(records, vec![]);
        assert!(replay::run(&e, &o, Config::default(), None).is_err());
        assert!(!o.exists());
        assert!(!storage::sibling(&o, "partial").unwrap().exists());
    }
}
#[test]
fn malformed_future_duplicate_negative_and_unsupported_cursor_block() {
    for text in [
        "zoom {",
        "zoom {\"id\":0,\"id\":1,\"n\":1}",
        "zoom {\"id\":-1,\"n\":1}",
        "zoom {\"id\":9,\"n\":1}",
        "zoom {\"id\":0,\"n\":3}",
        "zoom {\"id\":0,\"n\":1,\"at\":2}",
        "date {\"id\":2}",
        "zoom\t{\"id\":0,\"n\":1}",
    ] {
        let (_t, e, _o) = fixture(
            vec![record(0, Kind::User, "a"), record(1, Kind::Tool, text)],
            vec![],
        );
        assert!(plan::build(&load(&e)).is_err(), "accepted {text}");
    }
}
#[test]
fn ambiguous_unpaired_or_mismatched_results_block() {
    for (text, links) in [
        ("0+0|user: a", vec![]),
        ("0+0|user: wrong", vec![Link { call: 1, result: 2 }]),
    ] {
        let (_t, e, _o) = fixture(
            vec![
                record(0, Kind::User, "a"),
                record(1, Kind::Tool, "zoom {\"id\":0,\"n\":1}"),
                record(2, Kind::Echo, text),
            ],
            links,
        );
        assert!(plan::build(&load(&e)).is_err());
    }
}
#[test]
fn sealed_fidelity_validation() {
    let (_t, e, _o) = fixture(vec![record(0, Kind::User, "a")], vec![]);
    fs::write(e.join("originals.jsonl"), b"tampered\n").unwrap();
    assert!(Export::load(&e, 10000, 100).is_err());
    let (_t, e, _o) = fixture(vec![record(1, Kind::User, "gap")], vec![]);
    assert!(Export::load(&e, 10000, 100).is_err());
    let (_t, e, _o) = fixture(
        vec![record(0, Kind::User, "a"), record(0, Kind::User, "dupe")],
        vec![],
    );
    assert!(Export::load(&e, 10000, 100).is_err());
    let (_t, e, _o) = fixture(vec![record(0, Kind::User, "a")], vec![]);
    assert!(Export::load(&e, 1, 100).is_err());
    assert!(Export::load(&e, 10000, 0).is_err());
}
#[test]
fn checkpoint_resume_reproduces_events_and_native_text() {
    for reverse in [false, true] {
        let (_t, e, o) = fixture(long_history(), vec![]);
        let c = Config {
            reverse_completions: reverse,
            ..Default::default()
        };
        let full = o.with_file_name("full");
        replay::run(&e, &full, c.clone(), None).unwrap().unwrap();
        assert!(replay::run(&e, &o, c.clone(), Some(3)).unwrap().is_none());
        assert!(!o.exists());
        assert!(replay::validate_ready(&o).is_err());
        replay::run(&e, &o, c.clone(), None).unwrap().unwrap();
        assert_eq!(
            fs::read(full.join("events.json")).unwrap(),
            fs::read(o.join("events.json")).unwrap()
        );
        let a = Memory::open(full.join("memory"), native::VIEW_BYTES).unwrap();
        let b = Memory::open(o.join("memory"), native::VIEW_BYTES).unwrap();
        assert_eq!(a.render(), b.render());
        assert!(replay::run(&e, &o, c, None).unwrap().is_some());
    }
}
#[test]
fn crash_discards_uncommitted_work_and_orphans() {
    let (_t, e, o) = fixture(long_history(), vec![]);
    replay::run(&e, &o, Config::default(), Some(3)).unwrap();
    let stage = storage::sibling(&o, "partial").unwrap();
    fs::write(stage.join("work/format"), "torn work").unwrap();
    fs::create_dir(stage.join("checkpoints/000000000004.tmp")).unwrap();
    fs::write(
        stage.join("checkpoints/000000000004.tmp/torn"),
        "incomplete",
    )
    .unwrap();
    fs::write(stage.join("CURRENT.next"), "torn pointer").unwrap();
    replay::run(&e, &o, Config::default(), None)
        .unwrap()
        .unwrap();
    replay::validate_ready(&o).unwrap();
}
#[test]
fn corrupt_checkpoint_fails_closed_without_ready() {
    let (_t, e, o) = fixture(long_history(), vec![]);
    replay::run(&e, &o, Config::default(), Some(2)).unwrap();
    let stage = storage::sibling(&o, "partial").unwrap();
    fs::write(stage.join("checkpoints/000000000002/state.json"), "corrupt").unwrap();
    assert!(replay::run(&e, &o, Config::default(), None).is_err());
    assert!(!o.exists());
}
#[test]
fn budgets_stop_before_summary_admission_and_no_policy_change_on_resume() {
    let (_t, e, o) = fixture(long_history(), vec![]);
    let c = Config {
        max_jobs: 0,
        ..Default::default()
    };
    assert!(replay::run(&e, &o, c.clone(), None).is_err());
    assert!(!o.exists());
    assert!(replay::run(&e, &o, Config::default(), None).is_err());
    let (_t, e, o) = fixture(long_history(), vec![]);
    let c = Config {
        max_input_bytes: 0,
        ..Default::default()
    };
    assert!(replay::run(&e, &o, c, None).is_err());
    assert!(!o.exists());
    let (_t, e, o) = fixture(long_history(), vec![]);
    let c = Config {
        max_staging_bytes: 1,
        ..Default::default()
    };
    assert!(replay::run(&e, &o, c, None).is_err());
    assert!(!o.exists());
}
#[test]
fn output_isolation_symlinks_and_preexisting_dirs() {
    use std::os::unix::fs::symlink;
    let (_t, e, o) = fixture(vec![record(0, Kind::User, "a")], vec![]);
    assert!(replay::run(&e, &e.join("nested"), Config::default(), None).is_err());
    let other = o.with_file_name("elsewhere");
    fs::create_dir(&other).unwrap();
    symlink(&other, &o).unwrap();
    assert!(replay::run(&e, &o, Config::default(), None).is_err());
    fs::remove_file(&o).unwrap();
    fs::create_dir(&o).unwrap();
    fs::write(o.join("untouched"), "preserve").unwrap();
    assert!(replay::run(&e, &o, Config::default(), None).is_err());
    assert_eq!(fs::read_to_string(o.join("untouched")).unwrap(), "preserve");
}
#[test]
fn ready_tamper_rejected() {
    let (_t, e, o) = fixture(vec![record(0, Kind::User, "a")], vec![]);
    replay::run(&e, &o, Config::default(), None).unwrap();
    fs::write(o.join("events.json"), "tamper").unwrap();
    assert!(replay::validate_ready(&o).is_err());
}
#[test]
fn pending_launches_are_causal_and_bounded_at_native_64() {
    let records = (0..66)
        .map(|i| {
            record(
                i,
                Kind::User,
                format!("{} SENTINEL_{i}_ONLY", "L".repeat(600)),
            )
        })
        .collect();
    let (_t, e, o) = fixture(records, vec![]);
    let c = Config {
        latency_ticks: 100,
        ..Default::default()
    };
    replay::run(&e, &o, c.clone(), Some(66)).unwrap();
    let stage = storage::sibling(&o, "partial").unwrap();
    let state: serde_json::Value =
        storage::read_json(&stage.join("checkpoints/000000000066/state.json")).unwrap();
    let pending = state["pending"].as_array().unwrap();
    assert_eq!(pending.len(), 64);
    for p in pending {
        let key = p["key"]["index"].as_u64().unwrap();
        let ctx = p["context"].as_str().unwrap();
        for future in key..66 {
            assert!(!ctx.contains(&format!("SENTINEL_{future}_ONLY")));
        }
    }
    replay::run(&e, &o, c, None).unwrap().unwrap();
}
#[test]
fn exclusive_writer_and_resume_identity_bindings() {
    use fs2::FileExt;
    let (_t, e, o) = fixture(long_history(), vec![]);
    let lock_path = storage::sibling(&o, "import-lock").unwrap();
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .unwrap();
    file.try_lock_exclusive().unwrap();
    assert!(replay::run(&e, &o, Config::default(), None).is_err());
    FileExt::unlock(&file).unwrap();
    replay::run(&e, &o, Config::default(), Some(2)).unwrap();
    let stage = storage::sibling(&o, "partial").unwrap();
    fs::write(stage.join("OWNER"), "different-code-or-export").unwrap();
    assert!(replay::run(&e, &o, Config::default(), None).is_err());
    assert!(!o.exists());
}
#[test]
fn export_schema_rejects_derived_nodes_duplicate_keys_and_invalid_unicode() {
    for raw in [
        r#"{"i":0,"kind":"user","text":"a","size":7,"date":"2026-10-01T12:00:00Z","summary":"not an original"}"#,
        r#"{"i":0,"i":0,"kind":"user","text":"a","size":7,"date":"2026-10-01T12:00:00Z"}"#,
        r#"{"i":0,"kind":"user","text":"\uD800","size":7,"date":"2026-10-01T12:00:00Z"}"#,
    ] {
        let (_t, e, _o) = fixture(vec![record(0, Kind::User, "a")], vec![]);
        let bytes = format!("{raw}\n").into_bytes();
        let mut seal: Seal = storage::read_json(&e.join("seal.json")).unwrap();
        seal.bytes = bytes.len() as u64;
        seal.sha256 = storage::hash(&bytes);
        fs::write(e.join("originals.jsonl"), bytes).unwrap();
        fs::write(e.join("seal.json"), serde_json::to_vec(&seal).unwrap()).unwrap();
        assert!(Export::load(&e, 10000, 100).is_err());
    }
}
#[test]
fn wrong_native_size_and_extra_files_are_rejected() {
    let mut r = record(0, Kind::User, "λ");
    r.size = 0;
    let (_t, e, _o) = fixture(vec![r], vec![]);
    assert!(Export::load(&e, 10000, 100).is_err());
    let (_t, e, _o) = fixture(vec![record(0, Kind::User, "a")], vec![]);
    fs::write(e.join("tree.jsonl"), "derived summary store").unwrap();
    assert!(Export::load(&e, 10000, 100).is_err());
}
#[test]
fn self_reference_uses_logged_call_visibility_not_accidental_prior_only_cutoff() {
    let records = vec![
        record(0, Kind::User, "a".repeat(30001)),
        record(1, Kind::Tool, "zoom {\"id\":1,\"n\":1}"),
        record(2, Kind::Echo, "1+0|tool: zoom {\"id\":1,\"n\":1}"),
        record(3, Kind::Tool, "date {\"id\":3}"),
    ];
    let (_t, e, o) = fixture(records, vec![Link { call: 1, result: 2 }]);
    let p = plan::build(&load(&e)).unwrap();
    assert_eq!(p.texts[1], "zoom {\"id\":2,\"n\":1}");
    assert_eq!(p.texts[2], "2+0|tool: zoom {\"id\":2,\"n\":1}");
    assert_eq!(p.texts[3], "date {\"id\":4}");
    replay::run(&e, &o, Config::default(), None).unwrap();
}
#[test]
fn parent_results_shift_by_representable_aligned_offset() {
    let records = vec![
        record(0, Kind::User, "a".repeat(60001)),
        record(1, Kind::User, "b"),
        record(2, Kind::User, "c"),
        record(3, Kind::User, "d"),
        record(4, Kind::Tool, "zoom {\"id\":2,\"n\":2}"),
        record(
            5,
            Kind::Echo,
            "2+1|historical child C\n3+1|historical child D",
        ),
    ];
    let (_t, e, o) = fixture(records, vec![Link { call: 4, result: 5 }]);
    let p = plan::build(&load(&e)).unwrap();
    assert_eq!(p.texts[4], "zoom {\"id\":4,\"n\":2}");
    assert_eq!(p.texts[5], "4+1|historical child C\n5+1|historical child D");
    replay::run(&e, &o, Config::default(), None).unwrap();
    let m = Memory::open(o.join("memory"), native::VIEW_BYTES).unwrap();
    assert!(
        !m.render()
            .contains("historical child C\nhistorical child D")
    );
}
#[test]
fn parent_with_equally_split_children_preserves_exact_child_ranges() {
    let records = vec![
        record(0, Kind::User, "a".repeat(30001)),
        record(1, Kind::User, "b".repeat(30001)),
        record(2, Kind::Tool, "zoom {\"id\":0,\"n\":2}"),
        record(3, Kind::Echo, "0+1|observed A\n1+1|observed B"),
    ];
    let (_t, e, o) = fixture(records, vec![Link { call: 2, result: 3 }]);
    let p = plan::build(&load(&e)).unwrap();
    assert_eq!(p.texts[2], "zoom {\"id\":0,\"n\":4}");
    assert_eq!(p.texts[3], "0+2|observed A\n2+2|observed B");
    replay::run(&e, &o, Config::default(), None).unwrap();
}
#[test]
fn completed_checkpoint_can_publish_at_exact_tick_budget_without_extra_tick() {
    let (_t, e, o) = fixture(vec![record(0, Kind::User, "small free leaf")], vec![]);
    let c = Config {
        max_ticks: 1,
        ..Default::default()
    };
    assert!(replay::run(&e, &o, c.clone(), Some(1)).unwrap().is_none());
    assert!(replay::run(&e, &o, c, None).unwrap().is_some());
}
