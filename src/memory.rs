//! OptChat's append-only history, binary summary tree, and incremental view.
//!
//! Callers serialize mutations (the harness uses a Tokio mutex). No provider
//! requests happen here: ready jobs are completed by the background compactor.

use anyhow::{Context, Result, ensure};
use chrono::Local;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
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

    fn end(self) -> Option<u64> {
        self.index.checked_add(1)?.checked_mul(self.count())
    }

    fn parent(self) -> Self {
        Self {
            level: self.level + 1,
            index: self.index / 2,
        }
    }

    fn children(self) -> [Self; 2] {
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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Message {
    i: u64,
    kind: Kind,
    text: String,
    size: usize,
    date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Node {
    l: u32,
    i: u64,
    text: String,
    size: usize,
}

struct DailyLog {
    directory: PathBuf,
    current: Option<(String, File)>,
}

impl DailyLog {
    fn append<T: Serialize>(&mut self, entry: &T) -> Result<()> {
        let day = Local::now().format("%Y-%m-%d").to_string();
        if self.current.as_ref().map(|(d, _)| d) != Some(&day) {
            let path = self.directory.join(format!("{day}.jsonl"));
            let existed = path.exists();
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("open {}", path.display()))?;
            if !existed {
                // The directory entry must survive a power loss too.
                File::open(&self.directory)?.sync_all()?;
            }
            self.current = Some((day, file));
        }
        let mut bytes = serde_json::to_vec(entry)?;
        bytes.push(b'\n');
        let file = &mut self.current.as_mut().expect("opened daily log").1;
        // One write, as specified. A short write is an error, never silently
        // completed by write_all: recovery skips its torn JSON on restart.
        let written = file.write(&bytes)?;
        ensure!(
            written == bytes.len(),
            "short journal write ({written}/{} bytes)",
            bytes.len()
        );
        file.sync_all()?;
        Ok(())
    }
}

pub struct Memory {
    // An advisory flock is released by the kernel on process death, including
    // SIGKILL. Never remove the lock inode: that would let another writer in.
    _lock: File,
    root: Vec<Message>,
    sources: HashMap<String, u64>,
    nodes: BTreeMap<NodeKey, String>,
    ready: BTreeSet<NodeKey>,
    next_leaf: u64,
    view: Vec<NodeKey>,
    mergeable: BTreeSet<NodeKey>,
    view_bytes: usize,
    budget: usize,
    main_log: DailyLog,
    tree_log: DailyLog,
    poisoned: bool,
}

impl Drop for Memory {
    fn drop(&mut self) {
        // Closing the parent descriptor alone can retain flock briefly if a
        // concurrent process spawn inherited the open file description before
        // exec applies CLOEXEC. Explicit unlock releases ownership on a
        // deliberate close even while such a child still holds its descriptor.
        // Kernel close remains the fallback for process death/SIGKILL.
        if let Err(error) = FileExt::unlock(&self._lock) {
            tracing::error!(%error, "failed to release memory writer lock");
        }
    }
}

impl Memory {
    pub fn open(path: impl AsRef<Path>, budget: usize) -> Result<Self> {
        ensure!(budget > 0, "view budget must be positive");
        let path = path.as_ref();
        create_durable_dir(path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.join("lock"))?;
        FileExt::try_lock_exclusive(&lock)
            .with_context(|| format!("chat {} already has a writer", path.display()))?;
        let main_dir = path.join("main");
        let tree_dir = path.join("tree");
        create_durable_dir(&main_dir)?;
        create_durable_dir(&tree_dir)?;
        File::open(path)?.sync_all()?;

        let mut root: Vec<Message> = read_journals(&main_dir)?;
        root.sort_unstable_by_key(|message| message.i);
        let mut sources = HashMap::new();
        for (index, message) in root.iter().enumerate() {
            ensure!(
                message.i == index as u64,
                "non-contiguous history: expected id {index}, found {}; refusing to renumber permanent IDs",
                message.i
            );
            ensure!(
                message.size == message.kind.as_str().len() + 2 + message.text.len(),
                "incorrect size of message {}",
                message.i
            );
            chrono::DateTime::parse_from_rfc3339(&message.date)
                .with_context(|| format!("invalid date on message {}", message.i))?;
            if let Some(source) = &message.source_id {
                ensure!(
                    !source.is_empty(),
                    "empty provenance on message {}",
                    message.i
                );
                ensure!(
                    sources.insert(source.clone(), message.i).is_none(),
                    "duplicate provenance {source:?} in history"
                );
            }
        }
        let records: Vec<Node> = read_journals(&tree_dir)?;
        let mut nodes = BTreeMap::new();
        for node in records {
            let key = NodeKey {
                level: node.l,
                index: node.i,
            };
            ensure!(
                key.count() > 0 && key.end().is_some_and(|end| end <= root.len() as u64),
                "summary {:?} extends outside the history",
                key
            );
            ensure!(
                node.size == node.text.len() && !node.text.trim().is_empty(),
                "invalid summary {:?}",
                key
            );
            ensure!(
                nodes.insert(key, node.text).is_none(),
                "duplicate summary {:?}",
                key
            );
        }
        for key in nodes.keys().filter(|key| key.level > 0) {
            ensure!(
                key.children().iter().all(|child| nodes.contains_key(child)),
                "summary {:?} is missing a child",
                key
            );
        }

        let mut mem = Self {
            _lock: lock,
            root,
            sources,
            nodes,
            ready: BTreeSet::new(),
            next_leaf: 0,
            view: Vec::new(),
            mergeable: BTreeSet::new(),
            view_bytes: 0,
            budget,
            main_log: DailyLog {
                directory: main_dir,
                current: None,
            },
            tree_log: DailyLog {
                directory: tree_dir,
                current: None,
            },
            poisoned: false,
        };
        // Fold in append order; use the replay's length in the age rule rather
        // than today's final length. Never retile or split the live view.
        for index in 0..mem.root.len() {
            let key = NodeKey {
                level: 0,
                index: index as u64,
            };
            mem.view_bytes += mem.part_size(key);
            mem.view.push(key);
            mem.consider_view_pair(key);
            mem.fit(index as u64 + 1);
        }
        mem.update_leaf_ready();
        let built: Vec<NodeKey> = mem.nodes.keys().copied().collect();
        for key in built {
            mem.consider_parent(key);
        }
        mem.build_free_nodes()?;
        Ok(mem)
    }

    /// Every committed append is fsynced before its ID becomes visible.
    pub fn append(&mut self, kind: Kind, text: &str) -> Result<u64> {
        self.append_entry(kind, text, None)
    }

    /// Deduplicate a durable external admission without changing its exact text.
    /// Source IDs are opaque, stable, and unique within this chat directory.
    pub fn append_with_id(&mut self, kind: Kind, text: &str, source_id: &str) -> Result<u64> {
        ensure!(
            !source_id.is_empty(),
            "admission source ID must not be empty"
        );
        if let Some(index) = self.lookup_source(source_id) {
            let existing = &self.root[index as usize];
            ensure!(
                existing.kind == kind && existing.text == text,
                "admission {source_id:?} was already committed with different content"
            );
            return Ok(index);
        }
        self.append_entry(kind, text, Some(source_id))
    }

    pub fn lookup_source(&self, source_id: &str) -> Option<u64> {
        self.sources.get(source_id).copied()
    }

    fn append_entry(&mut self, kind: Kind, text: &str, source_id: Option<&str>) -> Result<u64> {
        self.healthy()?;
        let index = self.root.len() as u64;
        let message = Message {
            i: index,
            kind,
            text: text.to_owned(),
            size: kind.as_str().len() + 2 + text.len(),
            date: Local::now().to_rfc3339(),
            source_id: source_id.map(str::to_owned),
        };
        if let Err(error) = self.main_log.append(&message) {
            self.poisoned = true;
            return Err(error.context("history journal failed; reopen memory before writing again"));
        }
        self.root.push(message);
        if let Some(source_id) = source_id {
            self.sources.insert(source_id.to_owned(), index);
        }
        let key = NodeKey { level: 0, index };
        self.view_bytes += self.part_size(key);
        self.view.push(key);
        self.consider_view_pair(key);
        self.update_leaf_ready();
        self.fit(self.root.len() as u64);
        self.build_free_nodes()?;
        Ok(index)
    }

    pub fn render(&self) -> String {
        let mut out = String::from("<chat>\n");
        for &key in &self.view {
            out.push_str(&self.render_line(key));
            out.push('\n');
        }
        out.push_str("</chat>");
        out
    }

    /// A cacheable, ID-free prefix for the compactor, containing summaries only.
    pub fn compactor_context(&self, key: NodeKey) -> Result<String> {
        self.validate_job(key)?;
        let cutoff = if key.level == 0 {
            key.start()
        } else {
            key.end().unwrap()
        };
        ensure!(
            cutoff <= self.first_unbuilt(),
            "compactor context is not summarized yet"
        );
        let mut out = String::from("<chat>\n");
        for &part in &self.view {
            if part.start() >= cutoff {
                break;
            }
            ensure!(
                part.end().is_some_and(|end| end <= cutoff),
                "context crosses node boundary"
            );
            let text = self
                .nodes
                .get(&part)
                .context("unbuilt line in compactor context")?;
            out.push_str(&flatten(text));
            out.push('\n');
        }
        out.push_str("</chat>");
        Ok(out)
    }

    /// Whole message (never truncated) or precisely the two adjacent children.
    pub fn source(&self, key: NodeKey) -> Result<String> {
        self.validate_job(key)?;
        if key.level == 0 {
            let message = &self.root[key.index as usize];
            Ok(format!("{}: {}", message.kind.as_str(), message.text))
        } else {
            let [left, right] = key.children();
            Ok(format!(
                "{}\n{}",
                flatten(&self.nodes[&left]),
                flatten(&self.nodes[&right])
            ))
        }
    }

    /// Busy/retry exclusion belongs to the async pump, outside this data store.
    pub fn ready_jobs(&self, limit: usize) -> Vec<NodeKey> {
        let first = self.first_unbuilt();
        self.ready
            .iter()
            .copied()
            .filter(|key| self.eligible(*key, first))
            .take(limit)
            .collect()
    }

    /// Persist the shortest trimmed compactor attempt, including slight overshoot.
    pub fn finish(&mut self, key: NodeKey, text: &str) -> Result<()> {
        self.healthy()?;
        self.validate_job(key)?;
        let text = text.trim();
        ensure!(!text.is_empty(), "compactor returned an empty summary");
        ensure!(
            self.eligible(key, self.first_unbuilt()),
            "summary {:?} completed out of order",
            key
        );
        self.save_node(key, text.to_owned())?;
        self.build_free_nodes()
    }

    pub fn is_settled(&self) -> bool {
        self.next_leaf == self.root.len() as u64
    }

    pub fn zoom(&self, id: u64, n: u64) -> Result<String> {
        ensure!(
            n.is_power_of_two()
                && id.is_multiple_of(n)
                && id
                    .checked_add(n)
                    .is_some_and(|end| end <= self.root.len() as u64),
            "No line {id}+{n}."
        );
        if n == 1 {
            let message = &self.root[id as usize];
            return Ok(format!(
                "{id}+0|{}: {}",
                message.kind.as_str(),
                message.text
            ));
        }
        let key = NodeKey {
            level: n.ilog2(),
            index: id / n,
        };
        let [left, right] = key.children();
        ensure!(
            self.nodes.contains_key(&left) && self.nodes.contains_key(&right),
            "No line {id}+{n}."
        );
        Ok(format!(
            "{}\n{}",
            self.render_line(left),
            self.render_line(right)
        ))
    }

    pub fn date(&self, id: u64) -> Result<String> {
        Ok(self
            .root
            .get(id as usize)
            .with_context(|| format!("No message {id}."))?
            .date
            .clone())
    }

    pub fn stats(&self) -> serde_json::Value {
        serde_json::json!({
            "messages": self.root.len(), "summaries": self.nodes.len(),
            "view_lines": self.view.len(), "view_bytes": self.view_size(),
            "view_budget_bytes": self.budget, "settled": self.is_settled(),
            "ready_jobs": self.ready_jobs(usize::MAX).len(), "journal_healthy": !self.poisoned,
        })
    }

    pub fn export_html(&self) -> String {
        let mut out = String::from(
            "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>Pantheon memory</title><style>body{max-width:1000px;margin:2em auto;padding:0 1em;font:16px system-ui;background:#111;color:#eee}pre{white-space:pre-wrap;overflow-wrap:anywhere}summary{cursor:pointer;color:#9cf}small{color:#aaa}details{padding:.4em;border-bottom:1px solid #333}</style><h1>Pantheon memory</h1><h2>Current view</h2><pre>",
        );
        out.push_str(&html_escape(&self.render()));
        out.push_str("</pre><h2>ROOT — original messages</h2>");
        for message in &self.root {
            out.push_str(&format!("<details id=\"message-{}\"><summary>{}+1 · {} · {} · {} bytes</summary><pre>{}</pre></details>",
                message.i, message.i, message.kind.as_str(), html_escape(&message.date), message.size, html_escape(&message.text)));
        }
        let mut level = None;
        for (&key, text) in &self.nodes {
            if level != Some(key.level) {
                out.push_str(&format!("<h2>Tree level {}</h2>", key.level));
                level = Some(key.level);
            }
            let first = &self.root[key.start() as usize];
            let last = &self.root[key.end().unwrap() as usize - 1];
            out.push_str(&format!(
                "<details><summary>{}+{} · {} — {} · {} bytes</summary><pre>{}</pre></details>",
                key.start(),
                key.count(),
                html_escape(&first.date),
                html_escape(&last.date),
                text.len(),
                html_escape(text)
            ));
        }
        out.push_str("</html>");
        out
    }

    fn healthy(&self) -> Result<()> {
        ensure!(
            !self.poisoned,
            "journal state uncertain after an I/O failure; reopen memory"
        );
        Ok(())
    }

    fn validate_job(&self, key: NodeKey) -> Result<()> {
        ensure!(
            key.count() > 0 && key.end().is_some_and(|end| end <= self.root.len() as u64),
            "No node {:?}",
            key
        );
        ensure!(
            !self.nodes.contains_key(&key),
            "summary {:?} already exists",
            key
        );
        if key.level > 0 {
            ensure!(
                key.children()
                    .iter()
                    .all(|child| self.nodes.contains_key(child)),
                "summary children are not ready"
            );
        }
        Ok(())
    }

    fn first_unbuilt(&self) -> u64 {
        // An unbuilt leaf cannot be under a built parent. Hence the oldest
        // unbuilt view line starts exactly at the oldest unbuilt leaf.
        self.next_leaf
    }

    fn eligible(&self, key: NodeKey, first: u64) -> bool {
        let end = if key.level == 0 {
            key.index
        } else {
            key.end().unwrap_or(u64::MAX)
        };
        end <= first
    }

    fn consider_parent(&mut self, key: NodeKey) {
        if key.level >= 63 {
            return;
        }
        let parent = key.parent();
        if parent
            .end()
            .is_some_and(|end| end <= self.root.len() as u64)
            && !self.nodes.contains_key(&parent)
            && parent
                .children()
                .iter()
                .all(|child| self.nodes.contains_key(child))
        {
            self.ready.insert(parent);
        }
    }

    // There is only ever one eligible unbuilt leaf. Keeping the frontier avoids
    // scanning the complete uncompressed tail on every imported-history job.
    fn update_leaf_ready(&mut self) {
        while self.next_leaf < self.root.len() as u64
            && self.nodes.contains_key(&NodeKey {
                level: 0,
                index: self.next_leaf,
            })
        {
            self.next_leaf += 1;
        }
        if self.next_leaf < self.root.len() as u64 {
            self.ready.insert(NodeKey {
                level: 0,
                index: self.next_leaf,
            });
        }
    }

    fn save_node(&mut self, key: NodeKey, text: String) -> Result<()> {
        let node = Node {
            l: key.level,
            i: key.index,
            size: text.len(),
            text,
        };
        if let Err(error) = self.tree_log.append(&node) {
            self.poisoned = true;
            return Err(error.context("tree journal failed; reopen memory before writing again"));
        }
        if self
            .view
            .binary_search_by_key(&key.start(), |part| part.start())
            .is_ok_and(|index| self.view[index] == key)
        {
            self.view_bytes = self.view_bytes - self.part_size(key) + node.size;
        }
        self.nodes.insert(key, node.text);
        self.ready.remove(&key);
        if key.level == 0 {
            self.update_leaf_ready();
        }
        self.consider_parent(key);
        if key.level > 0 {
            self.consider_view_pair(key.children()[0]);
        }
        self.fit(self.root.len() as u64);
        Ok(())
    }

    fn build_free_nodes(&mut self) -> Result<()> {
        loop {
            let first = self.first_unbuilt();
            let mut free = None;
            for &key in &self.ready {
                if !self.eligible(key, first) {
                    continue;
                }
                let source = if key.level == 0 {
                    let message = &self.root[key.index as usize];
                    if message.size > NODE_BYTES {
                        continue;
                    }
                    format!("{}: {}", message.kind.as_str(), message.text)
                } else {
                    let [left, right] = key.children();
                    let a = &self.nodes[&left];
                    let b = &self.nodes[&right];
                    if a.len() + 1 + b.len() > NODE_BYTES {
                        continue;
                    }
                    format!("{a}\n{b}")
                };
                free = Some((key, source));
                break;
            }
            match free {
                Some((key, text)) => self.save_node(key, text)?,
                None => return Ok(()),
            }
        }
    }

    fn part_size(&self, key: NodeKey) -> usize {
        self.nodes.get(&key).map_or(PLACEHOLDER.len(), String::len)
    }

    fn view_size(&self) -> usize {
        self.view_bytes
    }

    fn view_index(&self, key: NodeKey) -> Option<usize> {
        self.view
            .binary_search_by_key(&key.start(), |part| part.start())
            .ok()
            .filter(|&index| self.view[index] == key)
    }

    // Only an append, newly built parent, or merge can enable a view pair.
    // Tracking those pairs avoids repeatedly scanning an uncompressed tail.
    fn consider_view_pair(&mut self, part: NodeKey) {
        if part.level >= 63 {
            return;
        }
        let left = NodeKey {
            level: part.level,
            index: part.index & !1,
        };
        let right = NodeKey {
            level: left.level,
            index: left.index + 1,
        };
        if self.nodes.contains_key(&left.parent())
            && self.view_index(left).is_some()
            && self.view_index(right).is_some()
        {
            self.mergeable.insert(left);
        }
    }

    fn fit(&mut self, total: u64) {
        // Cached on every append, node completion, and replay step. Never sum
        // the potentially very large unsummarized view while fitting.
        let mut size = self.view_bytes;
        while size > self.budget {
            let mut best: Option<(NodeKey, u64)> = None;
            for &a in &self.mergeable {
                let age = total.saturating_sub(a.start());
                // Compare age / 2^(level+2) exactly. u128 protects the cross
                // product; no floating point ties or overflow for long chats.
                let better = best.as_ref().is_none_or(|(previous, previous_age)| {
                    let due = (age as u128) * previous.count() as u128;
                    let previous_due = (*previous_age as u128) * a.count() as u128;
                    due > previous_due || (due == previous_due && a.start() < previous.start())
                });
                if better {
                    best = Some((a, age));
                }
            }
            let Some((child, _)) = best else {
                break;
            };
            let index = self
                .view_index(child)
                .expect("mergeable pair is in the live view");
            let parent = child.parent();
            size = size - self.part_size(self.view[index]) - self.part_size(self.view[index + 1])
                + self.part_size(parent);
            self.view[index] = parent;
            self.view.remove(index + 1);
            self.mergeable.remove(&child);
            self.consider_view_pair(parent);
        }
        self.view_bytes = size;
    }

    fn render_line(&self, key: NodeKey) -> String {
        format!(
            "{}+{}|{}",
            key.start(),
            key.count(),
            self.nodes
                .get(&key)
                .map_or_else(|| PLACEHOLDER.to_owned(), |text| flatten(text))
        )
    }
}

/// Invalid JSON (including torn UTF-8) is reported and skipped. Repair only the
/// missing final newline, without modifying a byte of existing history.
fn read_journals<T: serde::de::DeserializeOwned>(directory: &Path) -> Result<Vec<T>> {
    let mut paths = fs::read_dir(directory)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|path| path.extension().is_some_and(|ext| ext == "jsonl"));
    paths.sort();
    let mut entries = Vec::new();
    for path in paths {
        let mut reader = BufReader::new(File::open(&path)?);
        let mut bytes = Vec::new();
        let mut line = 0;
        let mut missing_newline = false;
        loop {
            bytes.clear();
            if reader.read_until(b'\n', &mut bytes)? == 0 {
                break;
            }
            line += 1;
            missing_newline = !bytes.ends_with(b"\n");
            match serde_json::from_slice(&bytes) {
                Ok(entry) => entries.push(entry),
                Err(error) => {
                    tracing::warn!(path = %path.display(), line, %error, "skipping invalid journal line")
                }
            }
        }
        if missing_newline {
            let mut file = OpenOptions::new().append(true).open(&path)?;
            ensure!(
                file.write(b"\n")? == 1,
                "cannot terminate torn journal line"
            );
            file.sync_all()?;
        }
    }
    Ok(entries)
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
    text.replace(['\n', '\r'], " ")
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Stable Unicode-character breakpoints, always at the previous line ending.
/// Concatenating the returned blocks reproduces the original view byte for byte.
pub fn cache_chunks(view: &str) -> Vec<String> {
    let marks = [50_000, 80_000, 100_000];
    let mut mark = 0;
    let mut last_newline = 0;
    let mut cuts = Vec::new();
    let length = view.chars().count();
    for (count, (byte, character)) in view.char_indices().enumerate() {
        let position = count + 1;
        if character == '\n' {
            last_newline = byte + 1;
        }
        if mark < marks.len() && position == marks[mark] {
            if length >= marks[mark] && last_newline > cuts.last().copied().unwrap_or(0) {
                cuts.push(last_newline);
            }
            mark += 1;
        }
    }
    let mut start = 0;
    let mut chunks = Vec::new();
    for cut in cuts {
        chunks.push(view[start..cut].to_owned());
        start = cut;
    }
    if start < view.len() || chunks.is_empty() {
        chunks.push(view[start..].to_owned());
    }
    chunks
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn key(level: u32, index: u64) -> NodeKey {
        NodeKey { level, index }
    }

    fn drain(mem: &mut Memory) {
        while let Some(job) = mem.ready_jobs(1).first().copied() {
            // Deliberately above half NODE so parents need real compression.
            mem.finish(job, &format!("user: {}", "summary ".repeat(35)))
                .unwrap();
        }
    }

    #[test]
    fn durable_journals_restart_without_recomputing_summaries() {
        let temp = TempDir::new().unwrap();
        let (view, tree_bytes);
        {
            let mut mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
            mem.append(Kind::User, &"大".repeat(700)).unwrap();
            mem.append(Kind::Talk, &"response ".repeat(100)).unwrap();
            drain(&mut mem);
            view = mem.render();
            tree_bytes = fs::read_dir(temp.path().join("tree"))
                .unwrap()
                .map(|entry| fs::read(entry.unwrap().path()).unwrap())
                .collect::<Vec<_>>();
            assert!(mem.is_settled());
        }
        let mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
        assert_eq!(view, mem.render());
        assert_eq!(mem.stats()["messages"], 2);
        assert_eq!(mem.stats()["summaries"], 3);
        assert!(mem.zoom(0, 1).unwrap().ends_with(&"大".repeat(700)));
        assert_eq!(
            tree_bytes,
            fs::read_dir(temp.path().join("tree"))
                .unwrap()
                .map(|entry| fs::read(entry.unwrap().path()).unwrap())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn durable_admission_deduplicates_before_and_after_restart() {
        let temp = TempDir::new().unwrap();
        let text = format!("  exact user paste\n{}\nlast line  ", "λ🦀".repeat(300));
        let journal;
        let before;
        {
            let mut mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
            assert_eq!(
                mem.append_with_id(Kind::User, &text, "discord:123")
                    .unwrap(),
                0
            );
            assert_eq!(
                mem.append_with_id(Kind::User, &text, "discord:123")
                    .unwrap(),
                0
            );
            assert_eq!(mem.lookup_source("discord:123"), Some(0));
            assert_eq!(mem.lookup_source("discord:missing"), None);
            assert!(
                mem.append_with_id(Kind::User, "changed", "discord:123")
                    .is_err()
            );
            assert!(
                mem.append_with_id(Kind::Talk, &text, "discord:123")
                    .is_err()
            );
            assert!(mem.append_with_id(Kind::User, &text, "").is_err());
            journal = fs::read_dir(temp.path().join("main"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            before = fs::read(&journal).unwrap();
            assert_eq!(mem.stats()["messages"], 1);
        }
        let mut mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
        assert_eq!(mem.lookup_source("discord:123"), Some(0));
        assert_eq!(
            mem.append_with_id(Kind::User, &text, "discord:123")
                .unwrap(),
            0
        );
        assert_eq!(mem.zoom(0, 1).unwrap(), format!("0+0|user: {text}"));
        assert_eq!(
            fs::read(&journal).unwrap(),
            before,
            "dedupe must not write another ROOT record"
        );
        assert_eq!(mem.append(Kind::Talk, "ordinary entry").unwrap(), 1);
        let lines: Vec<serde_json::Value> = fs::read_to_string(&journal)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["source_id"], "discord:123");
        assert!(
            lines[1].get("source_id").is_none(),
            "ordinary appends retain the original schema"
        );
        drop(mem);
        let mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
        assert_eq!(mem.stats()["messages"], 2);
        assert_eq!(mem.lookup_source("discord:123"), Some(0));
    }

    #[test]
    fn torn_utf8_is_skipped_preserved_and_terminated() {
        let temp = TempDir::new().unwrap();
        let journal;
        {
            let mut mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
            mem.append(Kind::User, "original").unwrap();
            journal = fs::read_dir(temp.path().join("main"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
        }
        let mut file = OpenOptions::new().append(true).open(&journal).unwrap();
        file.write_all(b"{\"i\":1,\"text\":\"\xf0\x9f").unwrap();
        file.sync_all().unwrap();
        let before = fs::read(&journal).unwrap();
        let mut mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
        assert_eq!(mem.append(Kind::User, "next").unwrap(), 1);
        let after = fs::read(&journal).unwrap();
        assert!(after.starts_with(&before));
        assert_eq!(after[before.len()], b'\n');
        drop(mem);
        let mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
        assert_eq!(mem.zoom(1, 1).unwrap(), "1+0|user: next");
    }

    #[test]
    fn second_writer_is_rejected_and_kernel_lock_is_reusable() {
        let temp = TempDir::new().unwrap();
        let first = Memory::open(temp.path(), VIEW_BYTES).unwrap();
        assert!(Memory::open(temp.path(), VIEW_BYTES).is_err());
        drop(first);
        assert!(Memory::open(temp.path(), VIEW_BYTES).is_ok());
    }

    #[test]
    fn deliberate_close_unlocks_an_inherited_file_description() {
        let temp = TempDir::new().unwrap();
        let first = Memory::open(temp.path(), VIEW_BYTES).unwrap();
        // dup shares the same open file description, exactly as descriptor
        // inheritance during fork does, without forking a multithreaded test.
        let inherited = first._lock.try_clone().unwrap();
        assert!(Memory::open(temp.path(), VIEW_BYTES).is_err());
        drop(first);
        let next = Memory::open(temp.path(), VIEW_BYTES).unwrap();
        assert!(
            inherited.metadata().is_ok(),
            "inherited descriptor stays open"
        );
        assert!(
            Memory::open(temp.path(), VIEW_BYTES).is_err(),
            "the new owner still excludes writers"
        );
        drop(next);
        drop(inherited);
        assert!(Memory::open(temp.path(), VIEW_BYTES).is_ok());
    }

    #[test]
    fn compression_is_ordered_binary_and_has_no_address_markers() {
        let temp = TempDir::new().unwrap();
        let mut mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
        mem.append(Kind::User, &"first original ".repeat(100))
            .unwrap();
        mem.append(Kind::Talk, &"second original ".repeat(100))
            .unwrap();
        assert_eq!(mem.ready_jobs(8), vec![key(0, 0)]);
        assert_eq!(mem.compactor_context(key(0, 0)).unwrap(), "<chat>\n</chat>");
        assert!(mem.source(key(0, 0)).unwrap().contains("first original"));
        assert!(mem.finish(key(0, 1), "premature").is_err());
        let first = format!("user: {}", "first ".repeat(50));
        let second = format!("talk: {}", "second ".repeat(45));
        mem.finish(key(0, 0), &first).unwrap();
        assert_eq!(mem.ready_jobs(8), vec![key(0, 1)]);
        assert_eq!(
            mem.compactor_context(key(0, 1)).unwrap(),
            format!("<chat>\n{}\n</chat>", first.trim())
        );
        mem.finish(key(0, 1), &second).unwrap();
        assert_eq!(mem.ready_jobs(8), vec![key(1, 0)]);
        assert_eq!(
            mem.source(key(1, 0)).unwrap(),
            format!("{}\n{}", first.trim(), second.trim())
        );
        let context = mem.compactor_context(key(1, 0)).unwrap();
        assert!(!context.contains("0+1|"));
        assert!(context.find("user:").unwrap() < context.find("talk:").unwrap());
        mem.finish(key(1, 0), "user: first; talk: second").unwrap();
        assert!(mem.is_settled());
    }

    #[test]
    fn short_messages_are_free_and_zoom_preserves_exact_original() {
        let temp = TempDir::new().unwrap();
        let mut mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
        mem.append(Kind::User, "keep\nverbatim λ").unwrap();
        mem.append(Kind::Talk, "understood").unwrap();
        assert!(mem.ready_jobs(8).is_empty());
        assert_eq!(
            mem.nodes[&key(1, 0)],
            "user: keep\nverbatim λ\ntalk: understood"
        );
        assert_eq!(mem.zoom(0, 1).unwrap(), "0+0|user: keep\nverbatim λ");
        assert_eq!(
            mem.zoom(0, 2).unwrap(),
            "0+1|user: keep verbatim λ\n1+1|talk: understood"
        );
        assert!(mem.zoom(1, 2).is_err());
        assert!(mem.zoom(0, 3).is_err());
        assert!(mem.zoom(u64::MAX, 1).is_err());
        assert!(mem.date(0).unwrap().contains('T'));
    }

    #[test]
    fn view_only_coarsens_and_preserves_prefix_while_under_budget() {
        let temp = TempDir::new().unwrap();
        // A tiny budget can be smaller than the irreducible binary tiling
        // (popcount(T) parts), so leave room for seven parts at this scale.
        let mut mem = Memory::open(temp.path(), 3000).unwrap();
        for _ in 0..96 {
            let previous = mem.view.clone();
            let old_prefix = mem.render().trim_end_matches("</chat>").to_owned();
            let was_small = mem.view_size() + 400 < mem.budget;
            mem.append(Kind::User, &"input ".repeat(120)).unwrap();
            drain(&mut mem);
            assert!(mem.view_size() <= mem.budget);
            for old in previous {
                let replacement = mem
                    .view
                    .iter()
                    .find(|new| {
                        new.start() <= old.start() && new.end().unwrap() >= old.end().unwrap()
                    })
                    .unwrap();
                assert!(replacement.level >= old.level, "a live view part split");
            }
            if was_small {
                assert!(mem.render().starts_with(&old_prefix));
            }
            assert_eq!(
                mem.view_bytes,
                mem.view
                    .iter()
                    .map(|&key| mem.part_size(key))
                    .sum::<usize>()
            );
        }
        assert!(mem.view.first().unwrap().level > mem.view.last().unwrap().level);
    }

    #[test]
    fn startup_fold_matches_reference_age_rule_at_every_append() {
        let temp = TempDir::new().unwrap();
        {
            let mut mem = Memory::open(temp.path(), 3000).unwrap();
            for _ in 0..128 {
                mem.append(Kind::User, &"original ".repeat(80)).unwrap();
                drain(&mut mem);
            }
        }
        let mut mem = Memory::open(temp.path(), 3000).unwrap();
        let reopened = mem.view.clone();
        mem.view.clear();
        mem.mergeable.clear();
        mem.view_bytes = 0;
        let mut expected = Vec::<NodeKey>::new();
        for index in 0..mem.root.len() {
            let total = index as u64 + 1;
            let leaf = key(0, index as u64);
            expected.push(leaf);
            loop {
                let bytes: usize = expected.iter().map(|key| mem.nodes[key].len()).sum();
                if bytes <= mem.budget {
                    break;
                }
                let mut best: Option<(usize, f64)> = None;
                for (index, pair) in expected.windows(2).enumerate() {
                    let (a, b) = (pair[0], pair[1]);
                    if a.level == b.level
                        && a.index % 2 == 0
                        && b.index == a.index + 1
                        && mem.nodes.contains_key(&a.parent())
                    {
                        let due = (total - a.start()) as f64 / (4 * a.count()) as f64;
                        if best.is_none_or(|(_, old_due)| due > old_due) {
                            best = Some((index, due));
                        }
                    }
                }
                let Some((index, _)) = best else {
                    break;
                };
                expected[index] = expected[index].parent();
                expected.remove(index + 1);
            }
            mem.view_bytes += mem.part_size(leaf);
            mem.view.push(leaf);
            mem.consider_view_pair(leaf);
            mem.fit(total);
            assert_eq!(mem.view, expected, "cached fold differs at message {index}");
            let pairs: BTreeSet<_> = mem
                .view
                .windows(2)
                .filter_map(|pair| {
                    let (a, b) = (pair[0], pair[1]);
                    (a.level == b.level
                        && a.index % 2 == 0
                        && b.index == a.index + 1
                        && mem.nodes.contains_key(&a.parent()))
                    .then_some(a)
                })
                .collect();
            assert_eq!(
                mem.mergeable, pairs,
                "merge frontier differs at message {index}"
            );
        }
        assert_eq!(reopened, expected);
    }

    #[test]
    fn unsummarized_import_maintains_only_one_leaf_job() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join("main")).unwrap();
        let mut journal = Vec::new();
        let text = "large imported message ".repeat(30);
        for i in 0..2_000 {
            let message = Message {
                i,
                kind: Kind::Note,
                size: 6 + text.len(),
                text: text.clone(),
                date: "2026-01-01T00:00:00+00:00".into(),
                source_id: None,
            };
            serde_json::to_writer(&mut journal, &message).unwrap();
            journal.push(b'\n');
        }
        fs::write(temp.path().join("main/2026-01-01.jsonl"), journal).unwrap();
        let mut mem = Memory::open(temp.path(), 2000).unwrap();
        assert_eq!(mem.ready_jobs(usize::MAX), vec![key(0, 0)]);
        assert!(mem.mergeable.is_empty());
        assert!(!mem.is_settled());
        mem.finish(key(0, 0), &"note: summarized ".repeat(20))
            .unwrap();
        assert_eq!(mem.ready_jobs(usize::MAX), vec![key(0, 1)]);
        assert!(
            mem.compactor_context(key(0, 1))
                .unwrap()
                .contains("note: summarized")
        );
    }

    #[test]
    fn cache_marks_count_characters_and_preserve_every_byte() {
        let view = format!(
            "<chat>\n{}</chat>",
            "λ".repeat(49980)
                + "\n"
                + &"中".repeat(29000)
                + "\n"
                + &"x".repeat(19000)
                + "\n"
                + &"z".repeat(7000)
                + "\n"
        );
        let chunks = cache_chunks(&view);
        assert_eq!(chunks.concat(), view);
        assert_eq!(chunks.len(), 4);
        assert!(chunks[..3].iter().all(|chunk| chunk.ends_with('\n')));
        assert!(chunks[0].chars().count() <= 50_000);
        assert!(chunks[..2].concat().chars().count() <= 80_000);
        assert!(chunks[..3].concat().chars().count() <= 100_000);
        assert_eq!(cache_chunks("<chat>\n</chat>"), vec!["<chat>\n</chat>"]);
    }

    #[test]
    fn tool_cap_keeps_unicode_head_tail_and_counts_notice() {
        let text = format!("head{}tail", "🦀".repeat(40_000));
        let capped = cap_tool_result(&text);
        assert_eq!(capped.chars().count(), TOOL_CHARS);
        assert!(capped.starts_with("head"));
        assert!(capped.ends_with("tail"));
        assert!(capped.contains("characters omitted"));
        assert_eq!(cap_tool_result("small"), "small");
    }

    #[test]
    fn html_export_escapes_untrusted_history() {
        let temp = TempDir::new().unwrap();
        let mut mem = Memory::open(temp.path(), VIEW_BYTES).unwrap();
        mem.append(Kind::User, "<script>alert('x')</script>")
            .unwrap();
        let html = mem.export_html();
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("Tree level 0"));
        assert!(html.contains("ROOT"));
    }
}
