use crate::{
    native::{COMPACT, Memory, NodeKey, VIEW_BYTES, clip_pages, compaction_task},
    plan::{self, Plan},
    seal::Export,
    storage::{self, Inventory},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub max_source_bytes: u64,
    pub max_records: usize,
    pub max_jobs: u64,
    pub max_input_bytes: u64,
    pub max_output_bytes: u64,
    pub max_ticks: u64,
    pub max_checkpoint_bytes: u64,
    pub max_staging_bytes: u64,
    pub latency_ticks: u64,
    pub reverse_completions: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_source_bytes: 64_000_000,
            max_records: 100_000,
            max_jobs: 100_000,
            max_input_bytes: 1_000_000_000,
            max_output_bytes: 64_000_000,
            max_ticks: 1_000_000,
            max_checkpoint_bytes: 512_000_000,
            max_staging_bytes: 2_000_000_000,
            latency_ticks: 2,
            reverse_completions: false,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Pending {
    key: NodeKey,
    due: u64,
    source: String,
    context: String,
    summary: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct State {
    identity: String,
    implementation: String,
    config: Config,
    tick: u64,
    next: usize,
    jobs: u64,
    input_bytes: u64,
    output_bytes: u64,
    pending: Vec<Pending>,
    events: Vec<serde_json::Value>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pointer {
    generation: u64,
    files_sha256: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub format: String,
    pub export_identity: String,
    pub implementation: String,
    pub fake_summaries: bool,
    pub records: u64,
    pub pages: u64,
    pub policy: String,
    pub files: Inventory,
}
pub fn implementation_identity() -> String {
    let mut bytes = Vec::new();
    for source in [
        include_bytes!("../Cargo.toml").as_slice(),
        include_bytes!("../Cargo.lock").as_slice(),
        include_bytes!("lib.rs").as_slice(),
        include_bytes!("native.rs").as_slice(),
        include_bytes!("compact.txt").as_slice(),
        include_bytes!("plan.rs").as_slice(),
        include_bytes!("seal.rs").as_slice(),
        include_bytes!("storage.rs").as_slice(),
        include_bytes!("replay.rs").as_slice(),
    ] {
        bytes.extend_from_slice(source);
        bytes.push(0);
    }
    storage::hash(&bytes)
}
fn event(state: &mut State, kind: &str, data: serde_json::Value) {
    state
        .events
        .push(serde_json::json!({"tick":state.tick,"kind":kind,"data":data}));
}
fn checkpoints(stage: &Path) -> std::path::PathBuf {
    stage.join("checkpoints")
}
fn generation(stage: &Path, n: u64) -> std::path::PathBuf {
    checkpoints(stage).join(format!("{n:012}"))
}
fn commit(stage: &Path, state: &State, memory: &Path) -> Result<()> {
    let n = state.tick;
    let temp = checkpoints(stage).join(format!("{n:012}.tmp"));
    if temp.exists() {
        storage::plain_tree(&temp)?;
        fs::remove_dir_all(&temp)?;
    }
    let memory_files = storage::inventory(memory)?;
    let state_bytes = storage::json(state)?;
    let mut expected: Inventory = memory_files
        .iter()
        .map(|(path, e)| (format!("memory/{path}"), e.clone()))
        .collect();
    expected.insert(
        "state.json".into(),
        storage::Entry {
            sha256: storage::hash(&state_bytes),
            bytes: state_bytes.len() as u64,
        },
    );
    let bytes = storage::json(&expected)?;
    let size = expected.values().try_fold(bytes.len() as u64, |sum, e| {
        sum.checked_add(e.bytes).context("snapshot overflow")
    })?;
    ensure!(
        size <= state.config.max_checkpoint_bytes,
        "checkpoint byte budget exceeded"
    );
    let existing = storage::inventory(stage)?
        .values()
        .try_fold(0_u64, |sum, e| {
            sum.checked_add(e.bytes).context("staging overflow")
        })?;
    // Reserve a bounded pointer payload in addition to the exact generation data.
    let projected = existing
        .checked_add(size)
        .and_then(|n| n.checked_add(512))
        .context("staging accounting overflow")?;
    ensure!(
        projected <= state.config.max_staging_bytes,
        "staging byte budget exceeded"
    );
    storage::mkdir(&temp)?;
    storage::copy_tree(memory, &temp.join("memory"))?;
    storage::write_new(&temp.join("state.json"), &state_bytes)?;
    storage::verify_inventory(&temp, &expected)?;
    storage::write_new(&temp.join("files.json"), &bytes)?;
    storage::sync_dir(&temp)?;
    let dest = generation(stage, n);
    // A complete orphan after a crash before CURRENT is safe to replace only
    // under the owned staging directory and the exclusive importer lock.
    if dest.exists() {
        storage::plain_tree(&dest)?;
        fs::remove_dir_all(&dest)?;
    }
    fs::rename(&temp, &dest)?;
    storage::sync_dir(&checkpoints(stage))?;
    storage::atomic_replace(
        &stage.join("CURRENT"),
        &storage::json(&Pointer {
            generation: n,
            files_sha256: storage::hash(&bytes),
        })?,
    )?;
    if n >= 2 {
        let old = generation(stage, n - 2);
        if old.exists() {
            storage::plain_tree(&old)?;
            fs::remove_dir_all(old)?;
            storage::sync_dir(&checkpoints(stage))?;
        }
    }
    Ok(())
}
fn load_checkpoint(stage: &Path) -> Result<(State, std::path::PathBuf)> {
    let p: Pointer = storage::read_json(&stage.join("CURRENT"))?;
    let root = generation(stage, p.generation);
    let bytes = fs::read(root.join("files.json"))?;
    ensure!(
        storage::hash(&bytes) == p.files_sha256,
        "checkpoint manifest hash mismatch"
    );
    let expected: Inventory = serde_json::from_slice(&bytes)?;
    let mut actual = storage::inventory(&root)?;
    actual.remove("files.json");
    ensure!(actual == expected, "checkpoint content mismatch");
    let s: State = storage::read_json(&root.join("state.json"))?;
    ensure!(s.tick == p.generation, "checkpoint tick mismatch");
    Ok((s, root.join("memory")))
}
fn reserve(memory: &Memory, state: &mut State) -> Result<()> {
    let existing = state.pending.iter().map(|p| p.key).collect::<BTreeSet<_>>();
    let available = memory.ready_jobs(64);
    ensure!(
        existing.iter().all(|k| available.contains(k)),
        "resumed pending job not present in native scheduler"
    );
    for key in available.into_iter().filter(|k| !existing.contains(k)) {
        let source = memory.source(key)?;
        let context = memory.compactor_context(key)?;
        let input = (COMPACT.len() + context.len() + compaction_task(key, &source).len()) as u64;
        let summary = format!(
            "fake:{}:{}:{}",
            key.level,
            key.index,
            storage::hash(format!("{source}\0{context}").as_bytes())
        );
        ensure!(
            state.jobs < state.config.max_jobs,
            "job budget exhausted before admission"
        );
        let next_input = state
            .input_bytes
            .checked_add(input)
            .context("input accounting overflow")?;
        let next_output = state
            .output_bytes
            .checked_add(summary.len() as u64)
            .context("output accounting overflow")?;
        ensure!(
            next_input <= state.config.max_input_bytes
                && next_output <= state.config.max_output_bytes,
            "summary byte budget exhausted before admission"
        );
        state.jobs += 1;
        state.input_bytes = next_input;
        state.output_bytes = next_output;
        event(
            state,
            "launch",
            serde_json::json!({"key":key,"source_sha256":storage::hash(source.as_bytes()),"context_sha256":storage::hash(context.as_bytes()),"input_bytes":input}),
        );
        state.pending.push(Pending {
            key,
            due: state
                .tick
                .checked_add(state.config.latency_ticks)
                .context("virtual time overflow")?,
            source,
            context,
            summary,
        });
    }
    Ok(())
}
fn complete(memory: &mut Memory, state: &mut State) -> Result<()> {
    state.pending.sort_by_key(|p| (p.due, p.key));
    let mut due = Vec::new();
    let mut future = Vec::new();
    for p in std::mem::take(&mut state.pending) {
        if p.due <= state.tick {
            due.push(p);
        } else {
            future.push(p);
        }
    }
    if state.config.reverse_completions {
        due.reverse();
    }
    state.pending = future;
    for job in due {
        memory.finish(job.key, &job.summary)?;
        event(
            state,
            "complete",
            serde_json::json!({"key":job.key,"summary_sha256":storage::hash(job.summary.as_bytes())}),
        );
    }
    Ok(())
}
pub fn verify_native(memory: &Memory, export: &Export, plan: &Plan) -> Result<()> {
    ensure!(
        memory.stats()["messages"].as_u64() == plan.spans.last().map(|s| s.end),
        "native page count mismatch"
    );
    for ((r, text), span) in export.records.iter().zip(&plan.texts).zip(&plan.spans) {
        ensure!(
            memory.say(span.start, span.end)? == *text,
            "native concatenation fidelity failed for {}",
            r.id
        );
        for id in span.start..span.end {
            ensure!(
                memory.date(id)? == r.timestamp,
                "native timestamp fidelity failed"
            );
        }
    }
    Ok(())
}
fn verify_pages(path: &Path, export: &Export, plan: &Plan) -> Result<()> {
    let mut pages = Vec::new();
    for entry in fs::read_dir(path.join("main"))? {
        let bytes = fs::read(entry?.path())?;
        for line in bytes
            .strip_suffix(b"\n")
            .context("unterminated native journal")?
            .split(|b| *b == b'\n')
        {
            pages.push(serde_json::from_slice::<serde_json::Value>(line)?);
        }
    }
    pages.sort_by_key(|p| p["i"].as_u64());
    ensure!(
        pages.len() as u64 == plan.spans.last().context("empty plan")?.end,
        "journal count mismatch"
    );
    for ((r, text), span) in export.records.iter().zip(&plan.texts).zip(&plan.spans) {
        let texts = clip_pages(text);
        let n = (span.end - span.start) as usize;
        for k in 0..n {
            let page = &pages[span.start as usize + k];
            let t = if k >= n - texts.len() {
                texts[k - (n - texts.len())].as_str()
            } else {
                ""
            };
            let end = r.images.len().saturating_sub((n - 1 - k) * 20);
            let images = &r.images[end.saturating_sub(20)..end];
            let actual_images = page.get("images").cloned().unwrap_or(serde_json::json!([]));
            ensure!(
                page["i"] == span.start + k as u64
                    && page["text"] == t
                    && page["date"] == r.timestamp
                    && page["kind"] == r.kind.as_str()
                    && page["more"] == (k + 1 < n)
                    && actual_images == serde_json::json!(images),
                "native page/image/order fidelity failed"
            );
        }
    }
    Ok(())
}
pub fn validate_ready(output: &Path) -> Result<Ready> {
    storage::plain_tree(output)?;
    let ready: Ready = storage::read_json(&output.join("READY.json"))?;
    ensure!(
        ready.format == "pantheon-fixture-import-bundle-v1" && ready.fake_summaries,
        "unsupported completion receipt"
    );
    let mut files = storage::inventory(output)?;
    files.remove("READY.json");
    ensure!(files == ready.files, "ready bundle integrity mismatch");
    Ok(ready)
}
fn publish(
    stage: &Path,
    output: &Path,
    state: &State,
    export: &Export,
    plan: &Plan,
) -> Result<Ready> {
    let publish = stage.join("publish");
    if publish.exists() {
        storage::plain_tree(&publish)?;
        fs::remove_dir_all(&publish)?;
    }
    let existing = storage::inventory(stage)?
        .values()
        .try_fold(0_u64, |sum, e| {
            sum.checked_add(e.bytes).context("staging overflow")
        })?;
    let native_bytes = storage::inventory(&stage.join("work"))?
        .values()
        .try_fold(0_u64, |sum, e| {
            sum.checked_add(e.bytes).context("publication overflow")
        })?;
    let payload = native_bytes
        .checked_add(export.original_bytes.len() as u64)
        .and_then(|n| n.checked_add(export.manifest_bytes.len() as u64))
        .and_then(|n| n.checked_add(storage::json(&plan.repairs).ok()?.len() as u64))
        .and_then(|n| n.checked_add(storage::json(&state.events).ok()?.len() as u64))
        // Reserve 1 MB for config, accounting and the small final inventory.
        .and_then(|n| n.checked_add(1_000_000))
        .context("publication accounting overflow")?;
    ensure!(
        existing
            .checked_add(payload)
            .is_some_and(|n| n <= state.config.max_staging_bytes),
        "publication staging byte budget exceeded"
    );
    storage::mkdir(&publish)?;
    storage::copy_tree(&stage.join("work"), &publish.join("memory"))?;
    storage::mkdir(&publish.join("archive"))?;
    storage::write_new(&publish.join("archive/seal.json"), &export.manifest_bytes)?;
    storage::write_new(
        &publish.join("archive/originals.jsonl"),
        &export.original_bytes,
    )?;
    // Audits are outside native memory. No positional lookup map is emitted.
    storage::write_new(
        &publish.join("transformations.json"),
        &storage::json(&plan.repairs)?,
    )?;
    storage::write_new(&publish.join("events.json"), &storage::json(&state.events)?)?;
    storage::write_new(&publish.join("policy.json"), &storage::json(&state.config)?)?;
    storage::write_new(
        &publish.join("accounting.json"),
        &storage::json(
            &serde_json::json!({"jobs":state.jobs,"input_bytes_reserved":state.input_bytes,"output_bytes_reserved":state.output_bytes,"real_calls":0,"fixed_point_iterations":plan.iterations}),
        )?,
    )?;
    let ready=Ready{format:"pantheon-fixture-import-bundle-v1".into(),export_identity:export.identity(),implementation:implementation_identity(),fake_summaries:true,records:export.seal.count,pages:plan.spans.last().context("empty plan")?.end,policy:"one closed original per tick; append, scheduled completions, launch; drain only at EOF; fake-only-v1".into(),files:storage::inventory(&publish)?};
    storage::write_new(&publish.join("READY.json"), &storage::json(&ready)?)?;
    storage::sync_dir(&publish)?;
    ensure!(!output.exists(), "output appeared during publication");
    fs::rename(&publish, output)?;
    storage::sync_dir(output.parent().context("missing parent")?)?;
    validate_ready(output)
}
/// Only synthetic fixtures are authorized for this implementation. The caller
/// supplies a sealed export; no live-state discovery or remote source exists.
/// `stop_after` simulates interruption immediately after a coherent checkpoint.
pub fn run(
    export_dir: &Path,
    output: &Path,
    config: Config,
    stop_after: Option<u64>,
) -> Result<Option<Ready>> {
    ensure!(
        config.latency_ticks > 0 && config.latency_ticks <= 10000 && config.max_ticks > 0,
        "invalid scheduling/budget policy"
    );
    ensure!(
        export_dir.is_absolute() && output.is_absolute(),
        "absolute export/output paths required"
    );
    storage::plain_ancestors(export_dir)?;
    storage::plain_ancestors(output.parent().context("output parent required")?)?;
    let source = fs::canonicalize(export_dir)?;
    let parent = fs::canonicalize(output.parent().context("output parent required")?)?;
    ensure!(
        output.file_name().is_some() && !output.ends_with(".") && !output.ends_with(".."),
        "invalid output name"
    );
    let output = parent.join(output.file_name().context("output name")?);
    ensure!(
        !output.starts_with(&source) && !source.starts_with(&output),
        "export/output overlap"
    );
    let stage = storage::sibling(&output, "partial")?;
    ensure!(
        !source.starts_with(&stage) && !stage.starts_with(&source),
        "export/staging overlap"
    );
    let lock_path = storage::sibling(&output, "import-lock")?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)?;
    FileExt::try_lock_exclusive(&lock).context("another importer owns output")?;
    let export = Export::load(&source, config.max_source_bytes, config.max_records)?;
    let plan = plan::build(&export)?; // Fail before creating staging on any unresolved ref.
    if output.exists() {
        let ready = validate_ready(&output)?;
        ensure!(
            ready.export_identity == export.identity()
                && ready.implementation == implementation_identity(),
            "output belongs to another export"
        );
        let old: Config = storage::read_json(&output.join("policy.json"))?;
        ensure!(old == config, "output policy mismatch");
        return Ok(Some(ready));
    }
    if !stage.exists() {
        storage::mkdir(&stage)?;
        storage::write_new(
            &stage.join("OWNER"),
            format!("{}:{}", export.identity(), implementation_identity()).as_bytes(),
        )?;
        storage::mkdir(&checkpoints(&stage))?;
    }
    storage::plain_tree(&stage)?;
    ensure!(
        fs::read(stage.join("OWNER"))?
            == format!("{}:{}", export.identity(), implementation_identity()).as_bytes(),
        "staging ownership mismatch"
    );
    let work = stage.join("work");
    let mut state = if stage.join("CURRENT").exists() {
        let (s, snapshot) = load_checkpoint(&stage)?;
        ensure!(
            s.identity == export.identity()
                && s.implementation == implementation_identity()
                && s.config == config,
            "resume export/policy mismatch"
        );
        if work.exists() {
            fs::remove_dir_all(&work)?;
        }
        storage::copy_tree(&snapshot, &work)?;
        s
    } else {
        if work.exists() {
            fs::remove_dir_all(&work)?;
        }
        State {
            identity: export.identity(),
            implementation: implementation_identity(),
            config: config.clone(),
            tick: 0,
            next: 0,
            jobs: 0,
            input_bytes: 0,
            output_bytes: 0,
            pending: Vec::new(),
            events: Vec::new(),
        }
    };
    let mut memory = Memory::open(&work, VIEW_BYTES)?;
    if !stage.join("CURRENT").exists() {
        commit(&stage, &state, &work)?;
    }
    loop {
        if state.next == export.records.len()
            && state.pending.is_empty()
            && memory.ready_jobs(64).is_empty()
            && memory.is_settled()
        {
            break;
        }
        ensure!(
            state.tick < config.max_ticks,
            "virtual tick budget exhausted"
        );
        state.tick += 1;
        if state.next < export.records.len() {
            let r = &export.records[state.next];
            let first =
                memory.push_record_at(r.kind, &plan.texts[state.next], &r.images, &r.timestamp)?;
            ensure!(
                first == plan.spans[state.next].start,
                "native page plan drift"
            );
            let text_hash = storage::hash(plan.texts[state.next].as_bytes());
            event(
                &mut state,
                "append",
                serde_json::json!({"original":r.id,"first":first,"text_sha256":text_hash}),
            );
            state.next += 1;
        }
        complete(&mut memory, &mut state)?;
        reserve(&memory, &mut state)?;
        commit(&stage, &state, &work)?;
        if stop_after.is_some_and(|n| state.tick >= n) {
            return Ok(None);
        }
        if state.next == export.records.len()
            && state.pending.is_empty()
            && memory.ready_jobs(64).is_empty()
            && memory.is_settled()
        {
            break;
        }
    }
    verify_native(&memory, &export, &plan)?;
    verify_pages(&work, &export, &plan)?;
    drop(memory);
    let ready = publish(&stage, &output, &state, &export, &plan)?;
    // Completed summaries/receipts and source/audit live in the bundle. Drop
    // temporary mappings/checkpoints. Failure here is reported, not replayed.
    fs::remove_dir_all(&stage)?;
    storage::sync_dir(&parent)?;
    drop(lock);
    Ok(Some(ready))
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    #[test]
    fn partial_checkpoint_pointer_and_ready_writes_never_publish_then_resume() {
        for name in ["state.json", "CURRENT.next", "READY.json"] {
            let temp = tempfile::tempdir().unwrap();
            let export = temp.path().join("export");
            fs::create_dir(&export).unwrap();
            fs::write(
                export.join("seal.json"),
                include_bytes!("../tests/synthetic-export/seal.json"),
            )
            .unwrap();
            fs::write(
                export.join("originals.jsonl"),
                include_bytes!("../tests/synthetic-export/originals.jsonl"),
            )
            .unwrap();
            let output = temp.path().join("bundle");
            storage::fail_next_write(name);
            assert!(
                run(&export, &output, Config::default(), None).is_err(),
                "fault not triggered: {name}"
            );
            assert!(!output.exists());
            assert!(validate_ready(&output).is_err());
            assert!(
                run(&export, &output, Config::default(), None)
                    .unwrap()
                    .is_some()
            );
            validate_ready(&output).unwrap();
        }
    }
}
