//! Native OptMem engine following UniiChat 0500bf6.
//! Algorithm/task adaptation: Victor Taelin, Apache-2.0; see THIRD_PARTY_NOTICES.md.
//!
//! Callers serialize mutations (the harness uses a Tokio mutex). No provider
//! requests happen here: ready jobs are completed by the background compactor.

use anyhow::{Context, Result, ensure};
use chrono::{Local, NaiveDate};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::FileExt as UnixFileExt;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

pub const NODE_BYTES: usize = 512;
pub const VIEW_BYTES: usize = 128_000;
pub const TOOL_CHARS: usize = 30_000;
pub const COMPACT: &str = include_str!("compact.txt");
const PLACEHOLDER: &str = "(not summarized yet: zoom it)";
pub const COMPACTOR_BYTES: usize = 32_000;
pub const COMPACTOR_JOBS: usize = 64;
const FORMAT: &str = "pantheon-optmem-uniichat-0500bf6-v1\n";
const INDEX_SLOT: u64 = 24;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    User,
    Talk,
    Tool,
    Echo,
    Note,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Talk => "talk",
            Self::Tool => "tool",
            Self::Echo => "echo",
            Self::Note => "note",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct NodeKey {
    pub level: u32,
    pub index: u64,
}

impl NodeKey {
    pub fn count(self) -> u64 {
        1_u64.checked_shl(self.level).unwrap_or(0)
    }

    pub fn start(self) -> u64 {
        self.index.saturating_mul(self.count())
    }

    pub fn end(self) -> Option<u64> {
        self.index.checked_add(1)?.checked_mul(self.count())
    }

    fn parent(self) -> Self {
        Self {
            level: self.level + 1,
            index: self.index / 2,
        }
    }

    pub fn children(self) -> [Self; 2] {
        [
            Self {
                level: self.level - 1,
                index: self.index * 2,
            },
            Self {
                level: self.level - 1,
                index: self.index * 2 + 1,
            },
        ]
    }
}

// Each ID is one native page, not one UI message. `more` makes the whole
// page group visible only after its last journal record is durable.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Message {
    i: u64,
    kind: Kind,
    #[serde(default, skip_serializing_if = "is_false")]
    work: bool,
    text: String,
    size: usize,
    date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_id: Option<String>,
    #[serde(default)]
    more: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<String>,
}
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Default)]
struct MessageOptions<'a> {
    date: Option<&'a str>,
    work: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct Node {
    l: u32,
    i: u64,
    text: String,
    size: usize,
    serial: u64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct View {
    parts: Vec<NodeKey>,
    batch: bool,
}
#[derive(Deserialize, Serialize)]
struct SavedView {
    version: u32,
    messages: u64,
    view: View,
    seen: View,
}

// Rebuildable sparse indexes contain day, byte offset and record length. They
// never contain text. Tree slots use UniiChat's (2*i+1)*2^l-1 layout. Journals
// are authoritative; an index write failure poisons the writer too.
#[cfg(test)]
#[derive(Default)]
struct WriteFault {
    short: Option<usize>,
    sync: bool,
    index: bool,
}
struct Journal {
    #[cfg(test)]
    fault: WriteFault,
    directory: PathBuf,
    index: File,
    current: Option<(String, File)>,
}
impl Journal {
    fn open(path: &Path, stream: &str) -> Result<Self> {
        Ok(Self {
            #[cfg(test)]
            fault: WriteFault::default(),
            directory: path.join(stream),
            index: OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(path.join(format!("{stream}.index")))?,
            current: None,
        })
    }
    fn slot(key: NodeKey) -> u64 {
        ((2 * key.index + 1) << key.level) - 1
    }
    fn position(&self, slot: u64) -> Result<Option<[u64; 3]>> {
        let mut bytes = [0; 24];
        let mut read = 0;
        while read < bytes.len() {
            let n = self
                .index
                .read_at(&mut bytes[read..], slot * INDEX_SLOT + read as u64)?;
            if n == 0 {
                return Ok(None);
            }
            read += n;
        }
        let p = std::array::from_fn(|i| {
            u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())
        });
        Ok((p[2] != 0).then_some(p))
    }
    fn place(&self, slot: u64, pos: [u64; 3]) -> Result<()> {
        let bytes: Vec<u8> = pos.into_iter().flat_map(u64::to_le_bytes).collect();
        self.index.write_all_at(&bytes, slot * INDEX_SLOT)?;
        Ok(())
    }
    fn day_path(&self, day: u64) -> Result<PathBuf> {
        let date = NaiveDate::from_num_days_from_ce_opt(day as i32).context("invalid index day")?;
        Ok(self.directory.join(format!("{date}.jsonl")))
    }
    fn read<T: serde::de::DeserializeOwned>(&self, slot: u64) -> Result<Option<T>> {
        let Some([day, offset, len]) = self.position(slot)? else {
            return Ok(None);
        };
        ensure!(len < 128_000_000, "corrupt index record length");
        let file = File::open(self.day_path(day)?)?;
        let mut bytes = vec![0; len as usize];
        file.read_exact_at(&mut bytes, offset)?;
        Ok(Some(
            serde_json::from_slice(&bytes).context("invalid indexed journal record")?,
        ))
    }
    // A group is one append + fsync. IDs/indexes become visible only afterwards.
    fn append<T: Serialize>(&mut self, records: &[(u64, T)]) -> Result<()> {
        use chrono::Datelike;
        let now = Local::now();
        let day = now.format("%Y-%m-%d").to_string();
        if self.current.as_ref().map(|(d, _)| d) != Some(&day) {
            let path = self.directory.join(format!("{day}.jsonl"));
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            File::open(&self.directory)?.sync_all()?;
            self.current = Some((day, file));
        }
        let file = &mut self.current.as_mut().unwrap().1;
        let mut bytes = Vec::new();
        let mut places = Vec::new();
        let base = file.metadata()?.len();
        for (slot, rec) in records {
            let offset = bytes.len();
            serde_json::to_writer(&mut bytes, rec)?;
            places.push((
                *slot,
                [
                    now.date_naive().num_days_from_ce() as u64,
                    base + offset as u64,
                    (bytes.len() - offset) as u64,
                ],
            ));
            bytes.push(b'\n');
        }
        #[cfg(test)]
        let write_len = self.fault.short.unwrap_or(bytes.len()).min(bytes.len());
        #[cfg(not(test))]
        let write_len = bytes.len();
        let n = file.write(&bytes[..write_len])?;
        ensure!(
            n == bytes.len(),
            "short journal write ({n}/{})",
            bytes.len()
        );
        #[cfg(test)]
        if self.fault.sync {
            anyhow::bail!("injected fsync failure");
        }
        file.sync_all()?;
        #[cfg(test)]
        if self.fault.index {
            anyhow::bail!("injected index failure after journal fsync");
        }
        for (slot, pos) in places {
            self.place(slot, pos)?;
        }
        self.index.sync_all()?;
        Ok(())
    }
    // Stream originals once on recovery; never retain their bodies in RAM.
    // Strict parsing prevents hiding a missing permanent ID after corruption.
    fn scan<T: serde::de::DeserializeOwned>(
        &self,
        mut visit: impl FnMut(T, [u64; 3]) -> Result<()>,
    ) -> Result<()> {
        use chrono::Datelike;
        let mut paths = fs::read_dir(&self.directory)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.retain(|p| p.extension().is_some_and(|s| s == "jsonl"));
        paths.sort();
        for path in paths {
            let date = NaiveDate::parse_from_str(
                path.file_stem()
                    .unwrap()
                    .to_str()
                    .context("invalid journal name")?,
                "%Y-%m-%d",
            )?;
            let mut reader = BufReader::new(File::open(&path)?);
            let mut offset = 0;
            let mut bytes = Vec::new();
            loop {
                bytes.clear();
                let n = reader.read_until(b'\n', &mut bytes)?;
                if n == 0 {
                    break;
                }
                // A torn final write is retained and terminated, never interpreted.
                if !bytes.ends_with(b"\n") {
                    let mut file = OpenOptions::new().append(true).open(&path)?;
                    file.write_all(b"\n")?;
                    file.sync_all()?;
                    bytes.push(b'\n');
                }
                if let Ok(rec) = serde_json::from_slice::<T>(&bytes) {
                    visit(
                        rec,
                        [
                            date.num_days_from_ce() as u64,
                            offset,
                            (bytes.len() - 1) as u64,
                        ],
                    )?;
                } else {
                    tracing::warn!(path=%path.display(), offset, "skipped invalid journal record");
                }
                offset += bytes.len() as u64;
            }
        }
        Ok(())
    }
}

pub struct Memory {
    _lock: File,
    directory: PathBuf,
    count: u64,
    open: u64,
    serial: u64,
    sources: HashMap<String, u64>,
    // Only lines of either view and their ancestors live here. Descendants are
    // looked up on disk when zoomed; originals never live here.
    nodes: BTreeMap<NodeKey, String>,
    ready: BTreeSet<NodeKey>,
    busy: BTreeMap<NodeKey, String>,
    view: View,
    seen: View,
    budget: usize,
    main_log: Journal,
    tree_log: Journal,
    poisoned: bool,
}

// A frozen partition with immutable journal frontiers. Private readers cannot
// see future originals or summaries completed after this snapshot. It serializes
// paths/frontiers and view lines, not an O(history) copy of the whole bank.
#[derive(Clone, Serialize, Deserialize)]
pub struct MemorySnapshot {
    version: u32,
    directory: PathBuf,
    count: u64,
    serial: u64,
    rendered: String,
}
impl MemorySnapshot {
    pub fn message_count(&self) -> u64 {
        self.count
    }
    pub fn render(&self) -> String {
        self.rendered.clone()
    }
    fn journals(&self) -> Result<(Journal, Journal)> {
        ensure!(self.version == 2, "unsupported memory snapshot format");
        ensure!(
            fs::read_to_string(self.directory.join("format"))? == FORMAT,
            "snapshot memory format changed"
        );
        // Read-only: never create files or alter indexes in a private reader.
        let open = |stream: &str| -> Result<Journal> {
            Ok(Journal {
                #[cfg(test)]
                fault: WriteFault::default(),
                directory: self.directory.join(stream),
                index: File::open(self.directory.join(format!("{stream}.index")))?,
                current: None,
            })
        };
        Ok((open("main")?, open("tree")?))
    }
    pub fn zoom(&self, id: u64, n: u64) -> Result<String> {
        self.zoom_at(id, n, 0)
    }
    pub fn zoom_at(&self, id: u64, n: u64, at: usize) -> Result<String> {
        let (main, tree) = self.journals()?;
        Ok(String::from_utf16_lossy(&zoom(
            &main,
            &tree,
            self.count,
            self.serial,
            id,
            n,
            at,
        )?))
    }
    pub fn zoom_units_at(&self, id: u64, n: u64, at: usize) -> Result<Vec<u16>> {
        let (main, tree) = self.journals()?;
        zoom(&main, &tree, self.count, self.serial, id, n, at)
    }
    pub fn date(&self, id: u64) -> Result<String> {
        if id >= self.count {
            return Ok(format!("No message {id}."));
        }
        let (main, _) = self.journals()?;
        display_date(&main.read::<Message>(id)?.context("missing original")?.date)
    }
}
impl Drop for Memory {
    fn drop(&mut self) {
        if let Err(e) = FileExt::unlock(&self._lock) {
            tracing::error!(error=%e, "memory unlock failed");
        }
    }
}
impl Memory {
    /// Validate native schema without creating locks, indexes, or journals.
    pub fn validate_namespace(path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        match fs::symlink_metadata(path) {
            Ok(metadata) => ensure!(
                metadata.is_dir(),
                "memory namespace must be a real directory"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        if path.join("format").try_exists()? {
            ensure!(
                fs::symlink_metadata(path.join("format"))?
                    .file_type()
                    .is_file()
                    && fs::read_to_string(path.join("format"))? == FORMAT,
                "unsupported memory format"
            );
        } else {
            ensure!(
                fs::read_dir(path)?.next().is_none(),
                "memory directory is nonempty but has no native format marker"
            );
        }
        Ok(())
    }
    pub fn open(path: impl AsRef<Path>, budget: usize) -> Result<Self> {
        ensure!(budget > 0, "view budget must be positive");
        let path = path.as_ref();
        // Must precede mkdir, lock creation, index repair, or checkpoint writes.
        Self::validate_namespace(path)?;
        create_durable_dir(path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.join("lock"))?;
        FileExt::try_lock_exclusive(&lock).context("memory already has a writer")?;
        if !path.join("format").exists() {
            let mut marker = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path.join("format"))?;
            marker.write_all(FORMAT.as_bytes())?;
            marker.sync_all()?;
            File::open(path)?.sync_all()?;
        }
        create_durable_dir(&path.join("main"))?;
        create_durable_dir(&path.join("tree"))?;
        let main_log = Journal::open(path, "main")?;
        let tree_log = Journal::open(path, "tree")?;
        // Sidecars are disposable. Always rebuild them from fsynced journals;
        // never renumber IDs or treat an index as an authoritative journal.
        main_log.index.set_len(0)?;
        tree_log.index.set_len(0)?;
        let mut count = 0;
        let mut visible = 0;
        let mut sources = HashMap::new();
        let mut pending_sources = Vec::new();
        main_log.scan::<Message>(|msg, pos| {
            // Native positional IDs are published only by a closing page.
            // Restarted writers may shadow an unfinished group's positions,
            // never a visible ID. Physical journal records remain immutable.
            if msg.i != count {
                ensure!(
                    count > visible && msg.i == visible,
                    "original ID may only restart at unfinished frontier {visible}, got {}",
                    msg.i
                );
                for source in pending_sources.drain(..) {
                    sources.remove(&source);
                }
                count = visible;
            }
            ensure!(msg.i == count, "non-contiguous original IDs");
            ensure!(msg.size == show(&msg, true).len(), "invalid original size");
            chrono::DateTime::parse_from_rfc3339(&msg.date)?;
            if let Some(source) = &msg.source_id {
                ensure!(
                    !source.is_empty() && sources.insert(source.clone(), msg.i).is_none(),
                    "duplicate/empty source ID"
                );
                pending_sources.push(source.clone());
            }
            main_log.place(msg.i, pos)?;
            count += 1;
            if !msg.more {
                visible = count;
                pending_sources.clear();
            }
            Ok(())
        })?;
        // Match UniiChat load: incomplete trailing groups do not survive as
        // active `open` pages. Their fsynced records stay in the journal, but
        // their unpublished positions/source IDs may be replaced on append.
        for source in pending_sources {
            sources.remove(&source);
        }
        let open = 0;
        let count = visible;
        let mut serial = 0;
        tree_log.scan::<Node>(|node, pos| {
            let key = NodeKey {
                level: node.l,
                index: node.i,
            };
            ensure!(
                key.count() > 0 && key.end().is_some_and(|e| e <= count),
                "tree extends outside originals"
            );
            ensure!(
                node.serial == serial && node.size == node.text.len() && !node.text.is_empty(),
                "invalid tree record"
            );
            ensure!(
                tree_log.position(Journal::slot(key))?.is_none(),
                "duplicate summary"
            );
            if key.level > 0 {
                for half in key.children() {
                    ensure!(
                        tree_log.position(Journal::slot(half))?.is_some(),
                        "summary child missing"
                    );
                }
            }
            tree_log.place(Journal::slot(key), pos)?;
            serial += 1;
            Ok(())
        })?;
        main_log.index.sync_all()?;
        tree_log.index.sync_all()?;
        let mut mem = Self {
            _lock: lock,
            directory: fs::canonicalize(path)?,
            count,
            open,
            serial,
            sources,
            nodes: BTreeMap::new(),
            ready: BTreeSet::new(),
            busy: BTreeMap::new(),
            view: View::default(),
            seen: View::default(),
            budget,
            main_log,
            tree_log,
            poisoned: false,
        };
        // Discover only missing subtrees from maximal roots. Built ancestors
        // shortcut traversal, just like Memory.todo/down in the reference.
        for root in roots(count) {
            mem.todo(root)?;
        }
        let checkpoint = path.join("view.json");
        if checkpoint.exists() {
            let saved: SavedView = serde_json::from_slice(&fs::read(checkpoint)?)
                .context("invalid saved memory view")?;
            ensure!(
                saved.version == 2 && saved.messages <= count,
                "incompatible saved memory view"
            );
            mem.validate_view(&saved.view, saved.messages)?;
            ensure!(
                saved.view.parts.last().and_then(|k| k.end()).unwrap_or(0) == saved.messages,
                "saved main view does not cover its exact original frontier"
            );
            mem.validate_view(&saved.seen, saved.messages)?;
            mem.view = saved.view;
            mem.seen = saved.seen;
        } else {
            for root in roots(count) {
                let parts = mem.down(root)?;
                mem.view.parts.extend(parts);
            }
            mem.seen = mem.view.clone();
        }
        mem.load_view_nodes()?;
        mem.fit(false)?;
        mem.pump_free()?;
        mem.checkpoint()?;
        Ok(mem)
    }
    fn healthy(&self) -> Result<()> {
        ensure!(!self.poisoned, "journal state uncertain; reopen memory");
        Ok(())
    }
    fn message(&self, id: u64) -> Result<Message> {
        ensure!(id < self.count, "No message {id}.");
        self.main_log.read(id)?.context("missing original")
    }
    fn node(&self, key: NodeKey) -> Result<Option<String>> {
        if let Some(text) = self.nodes.get(&key) {
            return Ok(Some(text.clone()));
        }
        Ok(self
            .tree_log
            .read::<Node>(Journal::slot(key))?
            .map(|n| n.text))
    }
    fn built(&self, key: NodeKey) -> bool {
        self.nodes.contains_key(&key)
    }
    fn todo(&mut self, key: NodeKey) -> Result<()> {
        if let Some(text) = self.node(key)? {
            self.nodes.insert(key, text);
            return Ok(());
        }
        if key.level == 0 {
            self.ready.insert(key);
            return Ok(());
        }
        for half in key.children() {
            self.todo(half)?;
        }
        if key.children().iter().all(|k| self.built(*k)) {
            self.ready.insert(key);
        }
        Ok(())
    }
    fn down(&mut self, key: NodeKey) -> Result<Vec<NodeKey>> {
        if self.built(key) || key.level == 0 {
            return Ok(vec![key]);
        }
        let mut out = Vec::new();
        for half in key.children() {
            out.extend(self.down(half)?)
        }
        Ok(out)
    }
    fn load_view_nodes(&mut self) -> Result<()> {
        let parts: Vec<_> = self
            .view
            .parts
            .iter()
            .chain(&self.seen.parts)
            .copied()
            .collect();
        for part in parts {
            let mut key = part;
            while key.end().is_some_and(|e| e <= self.count) {
                if let Some(text) = self.node(key)? {
                    self.nodes.insert(key, text);
                }
                if key.level >= 62 {
                    break;
                }
                key = key.parent();
            }
        }
        Ok(())
    }
    pub fn append(&mut self, kind: Kind, text: &str) -> Result<u64> {
        self.append_entry(kind, text, &[], false, None, MessageOptions::default())
    }
    pub fn append_with_id(&mut self, kind: Kind, text: &str, source_id: &str) -> Result<u64> {
        self.append_sourced(kind, text, source_id, false)
    }
    pub fn append_work_with_id(&mut self, text: &str, source_id: &str) -> Result<u64> {
        self.append_sourced(Kind::User, text, source_id, true)
    }
    fn append_sourced(
        &mut self,
        kind: Kind,
        text: &str,
        source_id: &str,
        work: bool,
    ) -> Result<u64> {
        ensure!(!source_id.is_empty(), "empty source ID");
        if let Some(id) = self.sources.get(source_id).copied() {
            let mut joined = String::new();
            let mut i = id;
            loop {
                let msg = self.message(i)?;
                ensure!(
                    msg.kind == kind && msg.work == work,
                    "admission kind mismatch"
                );
                joined.push_str(&msg.text);
                i += 1;
                if !msg.more {
                    break;
                }
            }
            ensure!(
                joined == text,
                "admission {source_id:?} already committed with different content"
            );
            return Ok(id);
        }
        self.append_entry(
            kind,
            text,
            &[],
            false,
            Some(source_id),
            MessageOptions {
                work,
                ..Default::default()
            },
        )
    }
    pub fn has_source(&self, source_id: &str) -> bool {
        self.lookup_source(source_id).is_some()
    }
    pub fn lookup_source(&self, source_id: &str) -> Option<u64> {
        self.sources
            .get(source_id)
            .copied()
            .filter(|id| *id < self.count)
    }
    /// Native streaming append: every text/image page has its own ID. `more`
    /// holds the whole trailing group invisible. Restart abandons the active
    /// trailing group (not its journal records), matching native Unii semantics.
    /// Images are immutable names managed by the attachment owner; the memory
    /// engine stores/retrieves their references, never decodes or executes them.
    pub fn push(&mut self, kind: Kind, text: &str, images: &[String], more: bool) -> Result<u64> {
        self.append_entry(kind, text, images, more, None, MessageOptions::default())
    }
    fn append_entry(
        &mut self,
        kind: Kind,
        text: &str,
        images: &[String],
        more: bool,
        source_id: Option<&str>,
        options: MessageOptions<'_>,
    ) -> Result<u64> {
        self.healthy()?;
        let first = self.count + self.open;
        let texts = clip_pages(text);
        let n = texts.len().max(images.len().div_ceil(20));
        let date = options
            .date
            .map(str::to_owned)
            .unwrap_or_else(|| Local::now().to_rfc3339());
        let records: Vec<_> = (0..n)
            .map(|k| {
                let i = first + k as u64;
                let text = if k >= n - texts.len() {
                    texts[k - (n - texts.len())].clone()
                } else {
                    String::new()
                };
                let end = images.len().saturating_sub((n - 1 - k) * 20);
                let mut msg = Message {
                    i,
                    kind,
                    work: options.work,
                    size: 0,
                    text,
                    date: date.clone(),
                    source_id: if k == 0 {
                        source_id.map(str::to_owned)
                    } else {
                        None
                    },
                    more: k + 1 < n || more,
                    images: images[end.saturating_sub(20)..end].to_vec(),
                };
                msg.size = show(&msg, true).len();
                (i, msg)
            })
            .collect();
        if let Err(e) = self.main_log.append(&records) {
            self.poisoned = true;
            return Err(e.context("original journal failed"));
        }
        let previous = self.count;
        if more {
            self.open += n as u64;
        } else {
            self.count += self.open + n as u64;
            self.open = 0;
        }
        if let Some(id) = source_id {
            self.sources.insert(id.to_owned(), first);
        }
        for i in previous..self.count {
            self.ready.insert(NodeKey { level: 0, index: i });
        }
        self.fit(true)?;
        self.pump_free()?;
        self.checkpoint()?;
        Ok(first)
    }
    /// Rejoin complete native pages without separators (not a zoom operation).
    pub fn say(&self, start: u64, end: u64) -> Result<String> {
        ensure!(
            start <= end && end <= self.count,
            "invalid native page range"
        );
        let mut out = String::new();
        for id in start..end {
            out.push_str(&self.message(id)?.text)
        }
        Ok(out)
    }
    /// Earliest page of the fresh raw suffix, following Memory.whole. Payload
    /// includes author/image labels, with the final page always admitted.
    pub fn whole(&self, from: u64) -> Result<u64> {
        ensure!(from <= self.count, "invalid native suffix start");
        let (mut at, mut size, mut pics) = (self.count, 0usize, 0usize);
        while at > from {
            let msg = self.message(at - 1)?;
            size = size.saturating_add(msg.size);
            pics = pics.saturating_add(msg.images.len());
            if at < self.count && (size > VIEW_BYTES || pics > 20) {
                return Ok(at);
            }
            at -= 1;
        }
        Ok(at)
    }
    pub fn source_span(&self, source: &str) -> Result<(u64, u64)> {
        let start = self
            .lookup_source(source)
            .context("missing native source")?;
        let mut end = start;
        loop {
            let page = self.message(end)?;
            end += 1;
            if !page.more {
                return Ok((start, end));
            }
        }
    }
    pub fn settled_before(&self, end: u64) -> Result<bool> {
        ensure!(end <= self.count, "invalid native settle frontier");
        Ok(self.first_unbuilt() >= end)
    }
    pub fn render_before(&self, end: u64) -> Result<String> {
        ensure!(end <= self.count, "invalid native render frontier");
        Ok(render_parts(&self.view.parts, &self.nodes, end))
    }
    pub fn render(&self) -> String {
        render_parts(&self.view.parts, &self.nodes, self.count)
    }
    pub fn snapshot(&self) -> Result<MemorySnapshot> {
        self.healthy()?;
        ensure!(
            self.is_settled(),
            "memory snapshot requires settled summaries"
        );
        Ok(MemorySnapshot {
            version: 2,
            directory: self.directory.clone(),
            count: self.count,
            serial: self.serial,
            rendered: self.render(),
        })
    }
    pub fn compactor_context(&self, key: NodeKey) -> Result<String> {
        self.validate_job(key)?;
        self.busy
            .get(&key)
            .cloned()
            .context("node has not been scheduled")
    }
    pub fn source(&self, key: NodeKey) -> Result<String> {
        self.validate_job(key)?;
        if key.level == 0 {
            let msg = self.message(key.index)?;
            Ok(show(&msg, true))
        } else {
            let [a, b] = key.children();
            Ok(format!(
                "{}\n{}",
                flatten(&self.node(a)?.context("missing child")?),
                flatten(&self.node(b)?.context("missing child")?)
            ))
        }
    }
    fn validate_job(&self, key: NodeKey) -> Result<()> {
        ensure!(
            key.count() > 0 && key.end().is_some_and(|e| e <= self.count),
            "No node {key:?}"
        );
        ensure!(self.node(key)?.is_none(), "summary already exists");
        if key.level > 0 {
            for half in key.children() {
                ensure!(self.node(half)?.is_some(), "summary children not ready")
            }
        }
        Ok(())
    }
    fn first_unbuilt(&self) -> u64 {
        self.ready
            .iter()
            .find(|k| k.level == 0)
            .map_or(self.count, |k| k.index)
    }
    fn candidates(&self, limit: usize) -> Vec<NodeKey> {
        let end = self
            .ready
            .iter()
            .filter(|k| k.level == 0)
            .nth(COMPACTOR_JOBS - 1)
            .map_or(self.count, |k| k.index);
        let mut ready: Vec<_> = self
            .ready
            .iter()
            .copied()
            .filter(|k| ctx(*k) <= end)
            .collect();
        ready.sort_by_key(|k| (ctx(*k), std::cmp::Reverse(k.level)));
        ready.truncate(limit);
        ready
    }
    /// Jobs reserved by the immediate pump. The async owner dispatches each at
    /// most once; failed provider attempts remain reserved (patient retry).
    pub fn ready_jobs(&self, limit: usize) -> Vec<NodeKey> {
        let mut jobs: Vec<_> = self.busy.keys().copied().collect();
        jobs.sort_by_key(|k| (ctx(*k), std::cmp::Reverse(k.level)));
        jobs.truncate(limit);
        jobs
    }
    pub fn finish(&mut self, key: NodeKey, text: &str) -> Result<()> {
        self.healthy()?;
        self.validate_job(key)?;
        ensure!(self.busy.contains_key(&key), "node has not been scheduled");
        let text = summary_trim(text);
        ensure!(!text.is_empty(), "empty summary");
        // Concurrent jobs may finish in any order inside the 64-leaf frontier.
        // Their summaries are durable immediately; publication is separately gated.
        self.store(vec![(key, text.to_owned())])?;
        self.busy.remove(&key);
        self.pump_free()?;
        self.fit(false)?;
        self.checkpoint()
    }
    fn store(&mut self, parts: Vec<(NodeKey, String)>) -> Result<()> {
        let records: Vec<_> = parts
            .iter()
            .enumerate()
            .map(|(n, (k, t))| {
                (
                    Journal::slot(*k),
                    Node {
                        l: k.level,
                        i: k.index,
                        text: t.clone(),
                        size: t.len(),
                        serial: self.serial + n as u64,
                    },
                )
            })
            .collect();
        if let Err(e) = self.tree_log.append(&records) {
            self.poisoned = true;
            return Err(e.context("tree journal failed"));
        }
        self.serial += records.len() as u64;
        for (key, text) in &parts {
            self.nodes.insert(*key, text.clone());
            self.ready.remove(key);
        }
        for (key, _) in parts {
            let parent = key.parent();
            if parent.end().is_some_and(|e| e <= self.count)
                && self.node(parent)?.is_none()
                && parent.children().iter().all(|k| self.built(*k))
            {
                self.ready.insert(parent);
            }
        }
        Ok(())
    }
    fn pump_free(&mut self) -> Result<()> {
        loop {
            let mut free = Vec::new();
            for key in self.candidates(usize::MAX) {
                if self.busy.len() >= COMPACTOR_JOBS {
                    break;
                }
                if self.busy.contains_key(&key) {
                    continue;
                }
                let text = self.source(key)?;
                if text.len() <= NODE_BYTES {
                    free.push((key, text));
                } else {
                    let end = ctx(key).min(self.first_unbuilt());
                    let context = render_parts(&self.seen.parts, &self.nodes, end);
                    self.busy.insert(key, context);
                }
            }
            if free.is_empty() {
                break;
            }
            self.store(free)?;
        }
        self.fit(false)
    }
    pub fn is_settled(&self) -> bool {
        self.first_unbuilt() >= self.count
    }
    fn validate_view(&self, view: &View, count: u64) -> Result<()> {
        let mut end = 0;
        for key in &view.parts {
            ensure!(key.count() > 0 && key.start() == end, "view gap/overlap");
            end = key.end().context("view overflow")?;
            ensure!(end <= count, "view exceeds originals");
            if key.level > 0 {
                ensure!(self.node(*key)?.is_some(), "view summary missing")
            }
        }
        Ok(())
    }
    fn fit(&mut self, next: bool) -> Result<()> {
        let frontier = self.first_unbuilt();
        let mut view = std::mem::take(&mut self.view);
        let mut seen = std::mem::take(&mut self.seen);
        self.fit_view(&mut view, self.count, self.budget, next)?;
        self.fit_view(&mut seen, frontier, COMPACTOR_BYTES, next)?;
        self.view = view;
        self.seen = seen;
        // Prune anything strictly below both partitions, retaining ancestors.
        let view = &self.view;
        let seen = &self.seen;
        self.nodes
            .retain(|key, _| held(view, *key) || held(seen, *key));
        Ok(())
    }
    fn fit_view(&self, view: &mut View, end: u64, high: usize, next: bool) -> Result<()> {
        view.parts.retain(|k| k.end().is_some_and(|e| e <= end));
        let at = view.parts.last().and_then(|k| k.end()).unwrap_or(0);
        view.parts
            .extend((at..end).map(|index| NodeKey { level: 0, index }));
        let size = part_bytes(&view.parts, &self.nodes);
        if size > high || (next && view.batch) {
            let merged = merge(view, end, high, &self.nodes);
            let merged_size = part_bytes(&merged, &self.nodes);
            if next || merged_size <= std::cmp::max(high / 2, size.saturating_sub(high)) {
                view.parts = merged;
            }
            view.batch = part_bytes(&view.parts, &self.nodes) > high / 2;
        }
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<()> {
        self.healthy()?;
        let result = (|| -> Result<()> {
            let bytes = serde_json::to_vec(&SavedView {
                version: 2,
                messages: self.count,
                view: self.view.clone(),
                seen: self.seen.clone(),
            })?;
            let path = self.directory.join("view.json");
            if fs::read(&path).ok().as_deref() == Some(bytes.as_slice()) {
                return Ok(());
            }
            let tmp = self.directory.join("view.json.new");
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(tmp, path)?;
            File::open(&self.directory)?.sync_all()?;
            Ok(())
        })();
        if let Err(e) = result {
            self.poisoned = true;
            return Err(e.context("view checkpoint failed"));
        }
        Ok(())
    }
    pub fn zoom(&self, id: u64, n: u64) -> Result<String> {
        self.zoom_at(id, n, 0)
    }
    pub fn zoom_at(&self, id: u64, n: u64, at: usize) -> Result<String> {
        Ok(String::from_utf16_lossy(&self.zoom_units_at(id, n, at)?))
    }
    pub fn zoom_units_at(&self, id: u64, n: u64, at: usize) -> Result<Vec<u16>> {
        zoom(
            &self.main_log,
            &self.tree_log,
            self.count,
            self.serial,
            id,
            n,
            at,
        )
    }
    pub fn date(&self, id: u64) -> Result<String> {
        if id >= self.count {
            return Ok(format!("No message {id}."));
        }
        display_date(&self.message(id)?.date)
    }
    pub fn stats(&self) -> serde_json::Value {
        serde_json::json!({"format":"uniichat-0500bf6-v1","messages":self.count,"open_pages":self.open,"summaries":self.serial,
            "resident_nodes":self.nodes.len(),"view_lines":self.view.parts.len(),"view_bytes":part_bytes(&self.view.parts,&self.nodes),
            "view_budget_bytes":self.budget,"compactor_view_bytes":part_bytes(&self.seen.parts,&self.nodes),
            "compactor_budget_bytes":COMPACTOR_BYTES,"compactor_frontier":self.first_unbuilt(),"settled":self.is_settled(),"ready_jobs":self.ready.len(),"journal_healthy":!self.poisoned})
    }
    /// Read a worker's original log without taking its writer lock or changing
    /// indexes. The atomic checkpoint bounds the reader to published originals.
    pub fn agent_log(directory: &Path, at: usize) -> Result<String> {
        Ok(String::from_utf16_lossy(&Self::agent_log_units(
            directory, at,
        )?))
    }
    pub fn agent_log_units(directory: &Path, at: usize) -> Result<Vec<u16>> {
        let saved: SavedView = serde_json::from_slice(&fs::read(directory.join("view.json"))?)?;
        let main = Journal {
            #[cfg(test)]
            fault: WriteFault::default(),
            directory: directory.join("main"),
            index: File::open(directory.join("main.index"))?,
            current: None,
        };
        Ok(clip_page_units(&original_log(&main, saved.messages)?, at))
    }
    pub fn original_log(&self) -> Result<String> {
        original_log(&self.main_log, self.count)
    }
    pub fn export_html(&self) -> String {
        // An operator export deliberately materializes its output, not live RAM.
        let mut out = String::from(
            "<!doctype html><html><meta charset=\"utf-8\"><title>Pantheon memory</title><h1>Current view</h1><pre>",
        );
        out.push_str(&html_escape(&self.render()));
        out.push_str("</pre><h2>Original pages</h2>");
        for id in 0..self.count {
            match self.message(id) {
                Ok(msg) => out.push_str(&format!(
                    "<details><summary>{id}+1 · {}</summary><pre>{}</pre></details>",
                    html_escape(&msg.date),
                    html_escape(&msg.text)
                )),
                Err(e) => out.push_str(&format!(
                    "<pre>Read failed: {}</pre>",
                    html_escape(&e.to_string())
                )),
            }
        }
        out.push_str("</html>");
        out
    }
}
fn ctx(key: NodeKey) -> u64 {
    if key.level == 0 {
        key.index
    } else {
        key.end().unwrap_or(u64::MAX)
    }
}
fn roots(count: u64) -> Vec<NodeKey> {
    let mut out = Vec::new();
    let mut at = 0;
    for l in (0..63).rev() {
        let n = 1u64 << l;
        if count - at >= n {
            out.push(NodeKey {
                level: l,
                index: at / n,
            });
            at += n
        }
    }
    out
}
fn held(view: &View, key: NodeKey) -> bool {
    view.parts
        .iter()
        .find(|p| p.end().is_some_and(|e| e > key.start()))
        .is_none_or(|p| p.level <= key.level)
}
fn part_bytes(parts: &[NodeKey], nodes: &BTreeMap<NodeKey, String>) -> usize {
    parts
        .iter()
        .filter_map(|k| nodes.get(k))
        .map(String::len)
        .sum()
}
fn render_parts(parts: &[NodeKey], nodes: &BTreeMap<NodeKey, String>, end: u64) -> String {
    let mut out = String::from("<chat>\n");
    for key in parts.iter().filter(|k| k.end().is_some_and(|e| e <= end)) {
        if let Some(text) = nodes.get(key) {
            out.push_str(&format!(
                "{}+{}|{}\n",
                key.start(),
                key.count(),
                flatten(text)
            ))
        }
    }
    out.push_str("</chat>\n");
    out
}
// Same normalized pair age and oldest-on-tie ordering as Memory.merge. Scan
// candidates after each merge; deterministic reference tests pin the policy.
fn merge(view: &View, end: u64, high: usize, nodes: &BTreeMap<NodeKey, String>) -> Vec<NodeKey> {
    let mut parts = view.parts.clone();
    let mut size = part_bytes(&parts, nodes);
    while size > high / 2 {
        let mut best: Option<usize> = None;
        for k in 0..parts.len().saturating_sub(1) {
            let a = parts[k];
            let b = parts[k + 1];
            if !a.index.is_multiple_of(2)
                || a.level != b.level
                || b.index != a.index + 1
                || !nodes.contains_key(&a.parent())
            {
                continue;
            }
            if best.is_none_or(|j| {
                let old = parts[j];
                let age = (end as u128 + 1) - (a.index as u128 * a.count() as u128);
                let previous = (end as u128 + 1) - (old.index as u128 * old.count() as u128);
                age * old.count() as u128 > previous * a.count() as u128
            }) {
                best = Some(k)
            }
        }
        let Some(k) = best else { break };
        let a = parts[k];
        let b = parts[k + 1];
        let up = a.parent();
        size = size - nodes.get(&a).map_or(0, String::len) - nodes.get(&b).map_or(0, String::len)
            + nodes[&up].len();
        parts[k] = up;
        parts.remove(k + 1);
    }
    parts
}
fn zoom(
    main: &Journal,
    tree: &Journal,
    count: u64,
    serial: u64,
    id: u64,
    n: u64,
    at: usize,
) -> Result<Vec<u16>> {
    if !(n.is_power_of_two()
        && id.is_multiple_of(n)
        && id.checked_add(n).is_some_and(|e| e <= count))
    {
        let roots = roots(count)
            .iter()
            .map(|key| format!("{}+{}", key.start(), key.count()))
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(format!(
            "No line {id}+{n}: n is a power of 2, id a multiple of n. The tree's roots: {roots}."
        )
        .encode_utf16()
        .collect());
    }
    if n == 1 {
        let msg: Message = main.read(id)?.context("missing original")?;
        let mut page = msg.clone();
        page.text.clear();
        let prefix = format!("{id}+0|{}", show(&page, false));
        let mut out = prefix.encode_utf16().collect::<Vec<_>>();
        out.extend(clip_page_units(&msg.text, at));
        if at == 0 {
            let shown = show(&page, true);
            out.extend(shown[show(&page, false).len()..].encode_utf16());
        }
        return Ok(out);
    }
    let key = NodeKey {
        level: n.ilog2(),
        index: id / n,
    };
    let mut lines = Vec::new();
    for half in key.children() {
        let node = tree
            .read::<Node>(Journal::slot(half))?
            .filter(|n| n.serial < serial);
        let text = node.map_or_else(|| PLACEHOLDER.to_owned(), |n| flatten(&n.text));
        lines.push(format!("{}+{}|{text}", half.start(), half.count()));
    }
    Ok(lines.join("\n").encode_utf16().collect())
}

fn display_date(date: &str) -> Result<String> {
    Ok(chrono::DateTime::parse_from_rfc3339(date)?
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string())
}

fn original_log(main: &Journal, count: u64) -> Result<String> {
    let mut lines = Vec::new();
    let mut page = 0;
    while page < count {
        let mut original: Message = main.read(page)?.context("missing worker original")?;
        page += 1;
        while original.more && page < count {
            let next: Message = main.read(page)?.context("missing worker continuation")?;
            ensure!(
                original.kind == next.kind,
                "worker entry kind changed within its pages"
            );
            original.text.push_str(&next.text);
            original.images.extend(next.images);
            original.more = next.more;
            page += 1;
        }
        ensure!(!original.more, "worker entry is unpublished");
        lines.push(format!("{}|{}", lines.len(), show(&original, true)));
    }
    Ok(lines.join("\n\n"))
}

/// JavaScript Clip.clip_end: count UTF-16 code units, prefer the last newline
/// within CAP, otherwise stop on a Unicode scalar boundary (never half a pair).
fn page_end(text: &str, start: usize) -> usize {
    let mut units = 0;
    let mut end = text.len();
    let mut newline = None;
    for (relative, ch) in text[start..].char_indices() {
        if units + ch.len_utf16() > TOOL_CHARS {
            end = start + relative;
            break;
        }
        units += ch.len_utf16();
        if ch == '\n' {
            newline = Some(start + relative + 1)
        }
    }
    if end == text.len() {
        end
    } else {
        newline.unwrap_or(end)
    }
}
pub fn clip_pages(text: &str) -> Vec<String> {
    let mut pages = Vec::new();
    let mut at = 0;
    loop {
        let end = page_end(text, at);
        pages.push(text[at..end].to_owned());
        at = end;
        if at == text.len() {
            break;
        }
    }
    pages
}
pub(crate) fn clip_page_units(text: &str, at: usize) -> Vec<u16> {
    let units = text.encode_utf16().collect::<Vec<_>>();
    let start = at.min(units.len());
    let end = if units.len() - start <= TOOL_CHARS {
        units.len()
    } else {
        let cap = start + TOOL_CHARS;
        units[start..cap]
            .iter()
            .rposition(|c| *c == 10)
            .map(|i| start + i + 1)
            .unwrap_or(cap - usize::from((units[cap] >> 10) == 0x37))
    };
    let mut out = units[start..end].to_vec();
    if end < units.len() {
        out.extend(
            format!(
                "\n[characters {}-{end} of {}; go on with at {end}]",
                at + 1,
                units.len()
            )
            .encode_utf16(),
        );
    }
    out
}

/// Four-line blocks, with a separate opening and incomplete tail. Provider
/// adapters mark only the last whole block, or the opener if there is none.
/// Concatenation reproduces the view exactly. Never split original text here.
pub fn cache_chunks(view: &str) -> Vec<String> {
    let Some(inner) = view.strip_prefix("<chat>\n").and_then(|s| {
        s.strip_suffix("</chat>\n")
            .or_else(|| s.strip_suffix("</chat>"))
    }) else {
        return vec![view.to_owned()];
    };
    let lines: Vec<_> = inner.split_inclusive('\n').collect();
    let whole = lines.len() / 4;
    let mut blocks = vec!["<chat>\n".to_owned()];
    for k in 0..whole {
        blocks.push(lines[k * 4..(k + 1) * 4].concat())
    }
    blocks.push(format!(
        "{}{suffix}",
        lines[whole * 4..].concat(),
        suffix = if view.ends_with("</chat>\n") {
            "</chat>\n"
        } else {
            "</chat>"
        }
    ));
    blocks
}
// A newly created chat directory must survive along with its contents. Sync
// every new component's parent, including the case of a nested new state path.
fn create_durable_dir(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    if path.exists() {
        ensure!(path.is_dir(), "{} is not a directory", path.display());
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_durable_dir(parent)?;
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
        Err(error) => return Err(error.into()),
    }
    File::open(path)?.sync_all()?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn flatten(text: &str) -> String {
    if !text.contains('\n') {
        text.to_owned()
    } else {
        static NEWLINE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"[\u0009-\u000d\u0020\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000\ufeff]*\n[\u0009-\u000d\u0020\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000\ufeff]*").unwrap()
        });
        NEWLINE.replace_all(text, " ").into_owned()
    }
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Retain equal head/tail portions. The omission notice counts toward CAP, so
/// both the permanent echo and all later steps contain at most 30,000 chars.
pub fn cap_tool_result(text: &str) -> String {
    let count = text.chars().count();
    if count <= TOOL_CHARS {
        return text.to_owned();
    }
    let mut omitted = count - TOOL_CHARS;
    let (notice, head, tail) = loop {
        let notice = format!("\n[... {omitted} characters omitted; head and tail preserved ...]\n");
        let remaining = TOOL_CHARS - notice.chars().count();
        let head = remaining / 2;
        let tail = remaining - head;
        let actual = count - head - tail;
        if actual == omitted {
            break (notice, head, tail);
        }
        omitted = actual;
    };
    let head_end = text
        .char_indices()
        .nth(head)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len());
    let tail_start = text
        .char_indices()
        .nth(count - tail)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len());
    format!("{}{}{}", &text[..head_end], notice, &text[tail_start..])
}

/// ECMAScript TrimString whitespace, not Rust's Unicode White_Space set.
/// In particular JS trims BOM but does not trim U+0085 NEXT LINE.
pub fn summary_trim(text: &str) -> &str {
    text.trim_matches(|ch|matches!(ch, '\u{0009}'..='\u{000d}'|' '| '\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'))
}

/// Native task/ruler from UniiChat Memory.task. Target, not a hard cap:
/// shortest of five answers may remain oversized, as in pinned upstream.
pub fn compaction_task(key: NodeKey, source: &str) -> String {
    let ruler = "-".repeat(NODE_BYTES);
    let task = if key.level == 0 {
        format!(
            "Compaction: compress message {} into one line of at most 512 bytes\n(about 70 words), the length of this ruler:\n{ruler}\n<input>\n",
            key.start()
        )
    } else {
        let [a, b] = key.children();
        format!(
            "Compaction: merge lines {}+{} and {}+{}, adjacent, into one line of at most\n512 bytes (about 70 words), the length of this ruler:\n{ruler}\n<chat> may hold their messages, {} to {}, in more detail: take details\nof them from there too.\n<input>\n",
            a.start(),
            a.count(),
            b.start(),
            b.count(),
            key.start(),
            key.end().unwrap() - 1
        )
    };
    format!("{task}{source}\n</input>")
}
pub fn compaction_cut(text: &str) -> String {
    let mut end = text.len().min(NODE_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1
    }
    // Node's cut removes replacement characters left by a split UTF-8 scalar.
    text[..end].trim_end_matches('\u{fffd}').to_owned()
}
pub fn compaction_over(line: &str) -> String {
    format!(
        "Too long: your last line for this <input> was {} bytes,\nover the 512-byte limit. Write the whole line again, cutting just\nenough of the least valuable items to fit before this cut:\n{}| ← LIMIT",
        line.len(),
        compaction_cut(line)
    )
}

fn show(msg: &Message, images: bool) -> String {
    let mut out = format!(
        "{}: {}",
        if msg.work { "work" } else { msg.kind.as_str() },
        msg.text
    );
    if images {
        for name in &msg.images {
            out.push_str(&format!("\n[image docs/images/{name}]"))
        }
    }
    out
}

/// Empty/refused answer fallback is flat source, unlike overlength feedback,
/// which cuts the actual last answer before any flattening.
pub fn compaction_fallback(source: &str) -> String {
    compaction_cut(&flatten(source))
}

#[cfg(test)]
mod tests {
    include!("memory_tests.rs");
}
