//! Immutable skill revisions and operational curation state. Chat memory is never modified here.
use crate::skills::{Skills, SkillsConfig};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    path::Path,
    sync::{Arc, Mutex},
};

pub const INDEX: &str = "Skills are an evolving library. Use skill(action=\"list\") to discover the current catalog, then load a relevant guide on demand. Guides and supporting files are pinned for your entire turn. Invoke explicit_only guides only when the user requests that workflow. Skills guide authorized work; they do not grant permissions or override user instructions.";
pub type Files = BTreeMap<String, String>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub id: String,
    /// Zero means the skill does not exist. Retired skills still have a revision.
    pub expected_revision: i64,
    #[serde(default)]
    pub files: Files,
    #[serde(default)]
    pub retire: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub changes: Vec<Change>,
    pub task_family: String,
    pub triggers: String,
    pub procedure: String,
    pub variables: String,
    pub verification: String,
    pub limits: String,
    pub reason: String,
    pub evidence: Vec<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Experience {
    pub seq: i64,
    pub channel: String,
    pub owner: String,
    pub activity: String,
    pub task: String,
    pub events: Value,
    pub turn_completed: bool,
}
#[derive(Clone)]
pub struct Batch {
    pub channel: String,
    pub through: i64,
    pub training: Vec<Experience>,
    pub held_out: Vec<Experience>,
}

pub struct SkillLibrary {
    db: Mutex<Connection>,
    snapshot_cache: Mutex<Option<Arc<Skills>>>,
    _lock: File,
}
impl SkillLibrary {
    pub fn open(config: &SkillsConfig, state: &Path) -> Result<Self> {
        std::fs::create_dir_all(state)?;
        // The state root may have just been created by the harness. Make its
        // ancestor entries durable before committing the first registry data.
        let absolute = state.canonicalize()?;
        for ancestor in absolute.ancestors() {
            File::open(ancestor)?.sync_all()?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state.join("skills.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).context("skill library already has a writer")?;
        let mut db = Connection::open(state.join("skills.sqlite"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS heads(id TEXT PRIMARY KEY,revision INTEGER NOT NULL,retired INTEGER NOT NULL,origin TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS revisions(id TEXT NOT NULL,revision INTEGER NOT NULL,files TEXT NOT NULL,retired INTEGER NOT NULL,reason TEXT NOT NULL,evidence TEXT NOT NULL,created INTEGER NOT NULL,PRIMARY KEY(id,revision));
            CREATE TABLE IF NOT EXISTS seed_baselines(id TEXT PRIMARY KEY,hash TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS seeds(id TEXT NOT NULL,hash TEXT NOT NULL,files TEXT NOT NULL,current INTEGER NOT NULL,PRIMARY KEY(id,hash));
            CREATE TABLE IF NOT EXISTS experiences(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,channel TEXT NOT NULL,owner TEXT NOT NULL,activity TEXT NOT NULL,task TEXT NOT NULL,events TEXT NOT NULL,successful INTEGER NOT NULL,created INTEGER NOT NULL,reviewed INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS experiences_pending ON experiences(channel,seq) WHERE reviewed=0;
            CREATE TABLE IF NOT EXISTS attempts(id TEXT PRIMARY KEY,channel TEXT NOT NULL,status TEXT NOT NULL,proposal TEXT,report TEXT NOT NULL DEFAULT '',usage TEXT NOT NULL DEFAULT '[]',review_context TEXT NOT NULL DEFAULT '',created INTEGER NOT NULL,finished INTEGER);
            CREATE TABLE IF NOT EXISTS reviewed_activities(channel TEXT NOT NULL,activity TEXT NOT NULL,PRIMARY KEY(channel,activity));
            CREATE INDEX IF NOT EXISTS attempts_created ON attempts(created);
            CREATE TABLE IF NOT EXISTS curator_state(key TEXT PRIMARY KEY,value INTEGER NOT NULL);
            UPDATE attempts SET status='interrupted',report='Process stopped before curation completed',finished=strftime('%s','now') WHERE status='running';")?;
        let seeds = Skills::load(config)?;
        let tx = db.transaction()?;
        // Changed package seeds are offers, never replacements for learned heads.
        tx.execute("UPDATE seeds SET current=0", [])?;
        for (id, skill) in &seeds.entries {
            let mut files = Files::from([("SKILL.md".into(), seeds.main_text(id)?.into())]);
            if let Some(root) = &skill.root {
                collect_files(root, root, &mut files)?;
            }
            validate_files(id, &files)?;
            let serialized = serde_json::to_string(&files)?;
            let hash = hex::encode(Sha256::digest(serialized.as_bytes()));
            tx.execute("INSERT INTO seeds(id,hash,files,current) VALUES(?1,?2,?3,1) ON CONFLICT(id,hash) DO UPDATE SET current=1", params![id,hash,serialized])?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM heads WHERE id=?1)",
                [id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO seed_baselines VALUES(?1,?2)",
                params![id, if exists { "" } else { &hash }],
            )?;
            if !exists {
                tx.execute("INSERT INTO heads VALUES(?1,1,0,'seed')", [id])?;
                tx.execute(
                    "INSERT INTO revisions VALUES(?1,1,?2,0,'Initial seed','[]',?3)",
                    params![id, serialized, crate::store::now()],
                )?;
            }
        }
        validate_library(&tx)?;
        tx.commit()?;
        File::open(state)?.sync_all()?;
        Ok(Self {
            db: Mutex::new(db),
            snapshot_cache: Mutex::new(None),
            _lock: lock,
        })
    }
    pub fn snapshot(&self) -> Result<Arc<Skills>> {
        let mut cache = self.snapshot_cache.lock().unwrap();
        if let Some(snapshot) = cache.as_ref() {
            return Ok(snapshot.clone());
        }
        let db = self.db.lock().unwrap();
        let mut query = db.prepare("SELECT h.id,h.revision,h.origin,r.files FROM heads h JOIN revisions r USING(id,revision) WHERE h.retired=0 ORDER BY h.id")?;
        let rows = query.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut skills = Skills::empty();
        for row in rows {
            let (id, revision, origin, files) = row?;
            let files: Files = serde_json::from_str(&files)?;
            skills.insert(
                &id,
                files
                    .get("SKILL.md")
                    .context("revision lacks SKILL.md")?
                    .clone(),
                None,
            )?;
            let skill = skills.entries.get_mut(&id).unwrap();
            skill.resources = Some(files);
            skill.revision = revision;
            skill.origin = origin;
        }
        let skills = Arc::new(skills);
        *cache = Some(skills.clone());
        Ok(skills)
    }
    pub fn history(&self, id: &str, offset: usize) -> Result<Value> {
        ensure!(offset <= 100_000, "history offset too large");
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT revision,retired,reason,evidence,created FROM revisions WHERE id=?1 ORDER BY revision DESC LIMIT 9 OFFSET ?2")?;
        let rows = q.query_map(params![id,offset as i64], |r| Ok(json!({"revision":r.get::<_,i64>(0)?,"retired":r.get::<_,bool>(1)?,"reason":r.get::<_,String>(2)?.chars().take(800).collect::<String>(),"evidence":r.get::<_,String>(3)?,"created":r.get::<_,i64>(4)?})))?;
        let mut items: Vec<Value> = rows.collect::<std::result::Result<_, _>>()?;
        let more = items.len() > 8;
        items.truncate(8);
        Ok(json!({"id":id,"revisions":items,"next_offset":more.then_some(offset+8)}))
    }
    pub fn revision_files(&self, id: &str, revision: i64) -> Result<Files> {
        ensure!(revision > 0, "revision must be positive");
        let db = self.db.lock().unwrap();
        let files: String = db
            .query_row(
                "SELECT files FROM revisions WHERE id=?1 AND revision=?2",
                params![id, revision],
                |r| r.get(0),
            )
            .context("unknown skill revision")?;
        Ok(serde_json::from_str(&files)?)
    }
    pub fn heads(&self) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT id,revision,retired,origin FROM heads ORDER BY id")?;
        let rows = q.query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"revision":r.get::<_,i64>(1)?,"retired":r.get::<_,bool>(2)?,"origin":r.get::<_,String>(3)?})))?;
        Ok(Value::Array(rows.collect::<std::result::Result<_, _>>()?))
    }
    pub fn seed_offers(&self) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT s.id,s.hash FROM seeds s JOIN seed_baselines b USING(id) JOIN heads h USING(id) JOIN revisions r ON r.id=h.id AND r.revision=h.revision WHERE s.current=1 AND s.hash<>b.hash AND s.files<>r.files ORDER BY s.id")?;
        let rows = q.query_map([], |r| {
            Ok(json!({"id":r.get::<_,String>(0)?,"hash":r.get::<_,String>(1)?}))
        })?;
        Ok(Value::Array(rows.collect::<std::result::Result<_, _>>()?))
    }
    pub fn seed(&self, id: &str, hash: &str) -> Result<Files> {
        let db = self.db.lock().unwrap();
        let files: String = db
            .query_row(
                "SELECT files FROM seeds WHERE id=?1 AND hash=?2",
                params![id, hash],
                |r| r.get(0),
            )
            .context("unknown seed")?;
        Ok(serde_json::from_str(&files)?)
    }
    /// Compare-and-swap all changes together; never publish half a merge/split.
    pub fn publish(&self, proposal: &Proposal) -> Result<()> {
        validate_proposal(proposal)?;
        let mut cache = self.snapshot_cache.lock().unwrap();
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        apply_changes(&tx, proposal, None)?;
        tx.commit()?;
        *cache = None;
        Ok(())
    }
    pub fn publish_attempt(
        &self,
        proposal: &Proposal,
        id: &str,
        report: &Value,
        usage: &[Value],
        batch: &Batch,
    ) -> Result<()> {
        validate_proposal(proposal)?;
        let mut cache = self.snapshot_cache.lock().unwrap();
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let running: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND status='running')",
            [id],
            |r| r.get(0),
        )?;
        ensure!(running, "curation attempt is no longer active");
        apply_changes(&tx, proposal, Some(id))?;
        tx.execute(
            "UPDATE attempts SET status='published',report=?2,usage=?3,finished=?4 WHERE id=?1",
            params![
                id,
                serde_json::to_string(report)?,
                serde_json::to_string(usage)?,
                crate::store::now()
            ],
        )?;
        mark_batch(&tx, batch)?;
        tx.execute("DELETE FROM curator_state WHERE key='requested'", [])?;
        tx.commit()?;
        *cache = None;
        Ok(())
    }
    pub fn rollback(&self, id: &str, revision: i64) -> Result<()> {
        let heads = self.heads()?;
        let head = heads
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == id)
            .context("unknown skill")?;
        let files = self.revision_files(id, revision)?;
        let retired = {
            let db = self.db.lock().unwrap();
            db.query_row(
                "SELECT retired FROM revisions WHERE id=?1 AND revision=?2",
                params![id, revision],
                |r| r.get::<_, bool>(0),
            )?
        };
        self.publish(&Proposal {
            changes: vec![Change {
                id: id.into(),
                expected_revision: head["revision"].as_i64().unwrap(),
                files,
                retire: retired,
            }],
            task_family: "User-requested rollback".into(),
            triggers: "Explicit rollback request".into(),
            procedure: "Restore a recorded revision".into(),
            variables: "Selected skill and revision".into(),
            verification: "Stored immutable revision".into(),
            limits: "Historical guidance may be outdated".into(),
            reason: format!("User rollback to revision {revision}"),
            evidence: vec![],
        })
    }
    pub fn record(
        &self,
        id: &str,
        channel: u64,
        owner: &str,
        task: &str,
        events: &Value,
        successful: bool,
    ) -> Result<()> {
        // Bounded excerpts are evidence, never a replacement for the original journal.
        let task: String = task.chars().take(8000).collect();
        let activity = events["activity"].as_str().unwrap_or(id).to_owned();
        let events = serde_json::to_string(events)?;
        ensure!(events.len() <= 64_000, "experience exceeds capture budget");
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute("INSERT OR IGNORE INTO experiences(id,channel,owner,activity,task,events,successful,created) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![id,channel.to_string(),owner,activity,task,events,successful,crate::store::now()])?;
        tx.execute("DELETE FROM experiences WHERE seq NOT IN (SELECT seq FROM experiences ORDER BY seq DESC LIMIT 256)", [])?;
        tx.commit()?;
        Ok(())
    }
    pub fn batch(&self, minimum: usize) -> Result<Option<Batch>> {
        let db = self.db.lock().unwrap();
        // Root turns supply independent cases; worker turns remain supporting evidence.
        let channel: Option<String> = db.query_row("SELECT channel FROM experiences WHERE reviewed=0 AND owner LIKE 'channel:%' AND NOT EXISTS(SELECT 1 FROM reviewed_activities a WHERE a.channel=experiences.channel AND a.activity=experiences.activity) GROUP BY channel HAVING count(DISTINCT activity)>=?1 ORDER BY min(seq) LIMIT 1", [minimum as i64], |r|r.get(0)).optional()?;
        let Some(channel) = channel else {
            return Ok(None);
        };
        let mut q = db.prepare("SELECT seq,channel,owner,activity,task,events,successful FROM experiences WHERE channel=?1 AND reviewed=0 AND owner LIKE 'channel:%' AND NOT EXISTS(SELECT 1 FROM reviewed_activities a WHERE a.channel=experiences.channel AND a.activity=experiences.activity) AND seq IN (SELECT max(seq) FROM experiences WHERE channel=?1 AND owner LIKE 'channel:%' GROUP BY activity) ORDER BY seq DESC LIMIT 8")?;
        let rows = q.query_map([&channel], experience_row)?;
        let mut items: Vec<Experience> = rows.collect::<std::result::Result<_, _>>()?;
        // Rehearsal starts from the original task, including when the last turn
        // merely delivered a background worker report.
        for case in &mut items {
            case.task=db.query_row("SELECT task FROM experiences WHERE channel=?1 AND activity=?2 AND owner LIKE 'channel:%' ORDER BY seq LIMIT 1",params![case.channel,case.activity],|r|r.get(0))?;
        }
        let through: i64 = db.query_row(
            "SELECT max(seq) FROM experiences WHERE channel=?1",
            [&channel],
            |r| r.get(0),
        )?;
        let mut held_out = vec![items.remove(0)];
        let activities: Vec<_> = items.iter().map(|e| e.activity.clone()).collect();
        let representative_ids: Vec<_> = items.iter().map(|e| e.seq).collect();
        let mut q=db.prepare("SELECT seq,channel,owner,activity,task,events,successful FROM experiences WHERE channel=?1 AND activity IN (SELECT value FROM json_each(?2)) AND seq NOT IN (SELECT value FROM json_each(?3)) ORDER BY seq DESC LIMIT 8")?;
        let supporting = q
            .query_map(
                params![
                    channel,
                    serde_json::to_string(&activities)?,
                    serde_json::to_string(&representative_ids)?
                ],
                experience_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        items.extend(supporting);
        // All records from the withheld activity stay out of drafting, including
        // private worker traces and earlier root turns from that same task.
        let mut q=db.prepare("SELECT seq,channel,owner,activity,task,events,successful FROM experiences WHERE channel=?1 AND activity=?2 AND seq<>?3 ORDER BY seq DESC LIMIT 4")?;
        let supporting = q
            .query_map(
                params![channel, held_out[0].activity, held_out[0].seq],
                experience_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        held_out[0].events = json!({"root":held_out[0].events,"supporting":supporting.iter().map(|e|json!({"seq":e.seq,"owner":e.owner,"events":bounded_json(&e.events,6000)})).collect::<Vec<_>>()});
        Ok(Some(Batch {
            channel,
            through,
            training: items,
            held_out,
        }))
    }
    pub fn proposals(&self, channel: &str) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let mut q=db.prepare("SELECT id,status,proposal FROM attempts WHERE channel=?1 AND proposal IS NOT NULL ORDER BY created DESC,rowid DESC LIMIT 4")?;
        let rows = q.query_map([channel], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut proposals = vec![];
        for row in rows {
            let (id, status, proposal) = row?;
            let p: Proposal = serde_json::from_str(&proposal)?;
            proposals.push(json!({"id":id,"status":status,"task_family":p.task_family,"reason":p.reason,"changes":p.changes.iter().map(|c|json!({"id":c.id,"expected_revision":c.expected_revision,"retire":c.retire})).collect::<Vec<_>>()}));
        }
        Ok(json!(proposals))
    }
    pub fn proposal(&self, channel: &str, attempt: &str) -> Result<Proposal> {
        let db = self.db.lock().unwrap();
        let raw: String = db
            .query_row(
                "SELECT proposal FROM attempts WHERE id=?1 AND channel=?2 AND proposal IS NOT NULL",
                params![attempt, channel],
                |r| r.get(0),
            )
            .context("unknown proposal in this channel")?;
        Ok(serde_json::from_str(&raw)?)
    }
    pub fn load_revision(&self, id: &str, revision: i64, args: &Value) -> Result<Value> {
        let files = self.revision_files(id, revision)?;
        let mut skills = Skills::empty();
        skills.insert(id, files["SKILL.md"].clone(), None)?;
        let skill = skills.entries.get_mut(id).unwrap();
        skill.revision = revision;
        skill.resources = Some(files);
        let mut args = args.clone();
        args["action"] = json!("load");
        args["id"] = json!(id);
        skills.execute(&args)
    }
    pub fn request_pass(&self) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO curator_state VALUES('requested',1) ON CONFLICT(key) DO UPDATE SET value=1",[])?;
        Ok(())
    }
    pub fn pass_requested(&self) -> Result<bool> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM curator_state WHERE key='requested'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            != 0)
    }
    pub fn begin_attempt(&self, id: &str, channel: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO attempts(id,channel,status,created) VALUES(?1,?2,'running',?3)",
            params![id, channel, crate::store::now()],
        )?;
        db.execute("INSERT INTO curator_state VALUES('last_attempt',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [crate::store::now()])?;
        Ok(())
    }
    pub fn save_proposal(&self, id: &str, proposal: &Proposal) -> Result<()> {
        validate_proposal(proposal)?;
        self.db.lock().unwrap().execute(
            "UPDATE attempts SET proposal=?2 WHERE id=?1 AND status='running'",
            params![id, serde_json::to_string(proposal)?],
        )?;
        Ok(())
    }
    pub fn save_review_context(&self, id: &str, context: &Value) -> Result<()> {
        let context = serde_json::to_string(context)?;
        ensure!(
            context.len() <= 1_000_000,
            "review context exceeds durable budget"
        );
        self.db.lock().unwrap().execute(
            "UPDATE attempts SET review_context=?2 WHERE id=?1 AND status='running'",
            params![id, context],
        )?;
        Ok(())
    }
    pub fn finish_attempt(
        &self,
        id: &str,
        status: &str,
        report: &Value,
        usage: &[Value],
        batch: Option<&Batch>,
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE attempts SET status=?2,report=?3,usage=?4,finished=?5 WHERE id=?1",
            params![
                id,
                status,
                serde_json::to_string(report)?,
                serde_json::to_string(usage)?,
                crate::store::now()
            ],
        )?;
        if let Some(batch) = batch {
            mark_batch(&tx, batch)?;
        }
        if status != "interrupted" {
            tx.execute("DELETE FROM curator_state WHERE key='requested'", [])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn status(&self) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let latest = db.query_row("SELECT id,status,created,finished,report,usage,proposal IS NOT NULL FROM attempts ORDER BY created DESC,rowid DESC LIMIT 1", [], |r| Ok(json!({"id":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,"created":r.get::<_,i64>(2)?,"finished":r.get::<_,Option<i64>>(3)?,"report":r.get::<_,String>(4)?,"usage":r.get::<_,String>(5)?,"has_proposal":r.get::<_,bool>(6)?}))).optional()?;
        let pending: i64 = db.query_row(
            "SELECT count(DISTINCT channel||':'||activity) FROM experiences WHERE reviewed=0 AND owner LIKE 'channel:%' AND NOT EXISTS(SELECT 1 FROM reviewed_activities a WHERE a.channel=experiences.channel AND a.activity=experiences.activity)",
            [],
            |r| r.get(0),
        )?;
        let last: i64 = db
            .query_row(
                "SELECT value FROM curator_state WHERE key='last_attempt'",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let settled: i64 = db.query_row(
            "SELECT COALESCE(max(created),0) FROM experiences",
            [],
            |r| r.get(0),
        )?;
        Ok(
            json!({"pending_tasks":pending,"last_attempt":last,"last_settled":settled,"latest":latest}),
        )
    }
}
impl Drop for SkillLibrary {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self._lock);
    }
}

pub fn validate_proposal(p: &Proposal) -> Result<()> {
    ensure!(
        !p.changes.is_empty() && p.changes.len() <= 4,
        "one logical proposal may change 1–4 skills"
    );
    for field in [
        &p.task_family,
        &p.triggers,
        &p.procedure,
        &p.variables,
        &p.verification,
        &p.limits,
        &p.reason,
    ] {
        ensure!(
            !field.trim().is_empty() && field.len() <= 4000,
            "proposal must explain scope, procedure, variation, verification and limits"
        );
    }
    ensure!(p.evidence.len() <= 16, "too many evidence references");
    let mut ids = std::collections::HashSet::new();
    for change in &p.changes {
        ensure!(ids.insert(&change.id), "duplicate change for a skill");
        ensure!(
            change.expected_revision >= 0,
            "expected_revision must be nonnegative"
        );
        // Validate IDs even for retirements.
        if change.retire {
            validate_files(
                &change.id,
                &Files::from([(
                    "SKILL.md".into(),
                    "---\nname: retired\ndescription: Retired guide.\n---\nRetired.".into(),
                )]),
            )?;
        } else {
            validate_files(&change.id, &change.files)?;
        }
    }
    Ok(())
}
fn validate_files(id: &str, files: &Files) -> Result<()> {
    ensure!(
        !files.is_empty() && files.len() <= 64,
        "skill must contain 1–64 text files"
    );
    ensure!(
        files.values().map(String::len).sum::<usize>() <= 2_000_000,
        "skill exceeds 2 MB"
    );
    for (path, text) in files {
        ensure!(
            path.len() <= 256
                && !path.is_empty()
                && Path::new(path)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "invalid skill resource path"
        );
        ensure!(text.len() <= 512_000, "skill resource exceeds 512000 bytes");
    }
    let mut skills = Skills::empty();
    skills.insert(
        id,
        files
            .get("SKILL.md")
            .context("skill needs SKILL.md")?
            .clone(),
        None,
    )?;
    Ok(())
}
fn collect_files(root: &Path, directory: &Path, files: &mut Files) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if matches!(entry.file_name().to_str(), Some(".git" | ".hg" | ".svn")) {
            continue;
        }
        let path = entry.path();
        // Reject links rather than allowing cycles or mutable external assets.
        ensure!(
            !entry.file_type()?.is_symlink(),
            "seed supporting resources cannot be symlinks"
        );
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else if path.is_file() {
            ensure!(files.len() < 64, "too many seed supporting files");
            let name = path
                .strip_prefix(root)?
                .to_str()
                .context("non-UTF8 resource path")?
                .to_owned();
            if name != "SKILL.md" {
                files.insert(name, crate::skills::read_bounded(&path)?);
            }
        }
    }
    Ok(())
}

fn apply_changes(
    tx: &rusqlite::Transaction<'_>,
    proposal: &Proposal,
    attempt: Option<&str>,
) -> Result<()> {
    for change in &proposal.changes {
        let head = tx
            .query_row(
                "SELECT revision,origin FROM heads WHERE id=?1",
                [&change.id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        let actual = head.as_ref().map(|h| h.0).unwrap_or(0);
        ensure!(
            actual == change.expected_revision,
            "skill {} changed during curation",
            change.id
        );
        ensure!(
            !change.retire || actual > 0,
            "cannot retire an unknown skill"
        );
        let files = if change.retire {
            tx.query_row(
                "SELECT files FROM revisions WHERE id=?1 AND revision=?2",
                params![change.id, actual],
                |r| r.get::<_, String>(0),
            )?
        } else {
            serde_json::to_string(&change.files)?
        };
        if actual > 0 {
            let (old_files, old_retired): (String, bool) = tx.query_row(
                "SELECT files,retired FROM revisions WHERE id=?1 AND revision=?2",
                params![change.id, actual],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            ensure!(
                old_files != files || old_retired != change.retire,
                "proposal does not change skill {}",
                change.id
            );
        }
        let revision = actual.checked_add(1).context("revision overflow")?;
        tx.execute(
            "INSERT INTO revisions VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                change.id,
                revision,
                files,
                change.retire,
                proposal.reason,
                json!({"cases":proposal.evidence,"attempt":attempt}).to_string(),
                crate::store::now()
            ],
        )?;
        tx.execute("INSERT INTO heads VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,retired=excluded.retired", params![change.id,revision,change.retire,head.map(|h|h.1).unwrap_or_else(||"curator".into())])?;
    }
    validate_library(tx)?;
    Ok(())
}

fn mark_batch(tx: &rusqlite::Transaction<'_>, batch: &Batch) -> Result<()> {
    let activities: std::collections::BTreeSet<_> = batch
        .training
        .iter()
        .chain(&batch.held_out)
        .map(|e| &e.activity)
        .collect();
    for activity in &activities {
        tx.execute(
            "INSERT OR IGNORE INTO reviewed_activities VALUES(?1,?2)",
            params![batch.channel, activity],
        )?;
    }
    tx.execute("UPDATE experiences SET reviewed=1 WHERE channel=?1 AND activity IN (SELECT value FROM json_each(?2)) AND seq<=?3",params![batch.channel,serde_json::to_string(&activities)?,batch.through])?;
    Ok(())
}
fn validate_library(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let (count,bytes):(i64,i64)=tx.query_row("SELECT count(*),COALESCE(sum(length(r.files)),0) FROM heads h JOIN revisions r USING(id,revision) WHERE h.retired=0",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(count <= 256, "skill library exceeds 256 active entries");
    ensure!(bytes <= 32_000_000, "active skill library exceeds 32 MB");
    Ok(())
}
fn experience_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Experience> {
    let raw: String = r.get(5)?;
    let events = serde_json::from_str(&raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(Experience {
        seq: r.get(0)?,
        channel: r.get(1)?,
        owner: r.get(2)?,
        activity: r.get(3)?,
        task: r.get(4)?,
        events,
        turn_completed: r.get(6)?,
    })
}
/// Preserve valid JSON and explicit truncation instead of cutting a JSON object.
pub fn bounded_json(value: &Value, limit: usize) -> Value {
    let raw = value.to_string();
    let total = raw.chars().count();
    if total <= limit {
        return value.clone();
    }
    let mut budget = limit / 2;
    loop {
        let head: String = raw.chars().take(budget / 2).collect();
        let tail: String = raw.chars().skip(total - budget / 2).collect();
        let result = json!({"truncated":true,"head":head,"tail":tail,"total_chars":total});
        if result.to_string().chars().count() <= limit {
            return result;
        }
        budget /= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn files(method: &str) -> Files {
        Files::from([
            (
                "SKILL.md".into(),
                format!(
                    "---\nname: workflow\ndescription: Repeatable deployment checks.\n---\n{method}"
                ),
            ),
            ("references/check.md".into(), method.into()),
        ])
    }
    fn proposal(id: &str, expected: i64, method: &str) -> Proposal {
        Proposal {
            changes: vec![Change {
                id: id.into(),
                expected_revision: expected,
                files: files(method),
                retire: false,
            }],
            task_family: "Service deployment".into(),
            triggers: "Replacing an existing service".into(),
            procedure: "Inspect, replace, verify".into(),
            variables: "Service, package and state paths".into(),
            verification: "Observe process and gateway health".into(),
            limits: "No unobserved success claims".into(),
            reason: "Grounded verification procedure".into(),
            evidence: vec![1],
        }
    }
    #[test]
    fn seed_revisions_retirements_and_rollback_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SkillsConfig::default();
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        let old = library.snapshot().unwrap();
        library
            .publish(&proposal(
                "engineering",
                1,
                "Discover the service and verify its running executable.",
            ))
            .unwrap();
        assert_eq!(
            old.execute(&json!({"action":"load","id":"engineering"}))
                .unwrap()["revision"],
            1
        );
        assert_eq!(
            library
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"engineering"}))
                .unwrap()["revision"],
            2
        );
        assert!(
            library
                .seed_offers()
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
        let mut retire = proposal("research", 1, "unused");
        retire.changes[0].retire = true;
        library.publish(&retire).unwrap();
        drop(library);
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        assert!(
            library
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"research"}))
                .is_err()
        );
        assert_eq!(
            library
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"engineering"}))
                .unwrap()["revision"],
            2
        );
        library.rollback("research", 1).unwrap();
        assert_eq!(
            library
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"research"}))
                .unwrap()["revision"],
            3
        );
        library.rollback("engineering", 1).unwrap();
        assert_eq!(
            library
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"engineering"}))
                .unwrap()["text"],
            old.execute(&json!({"action":"load","id":"engineering"}))
                .unwrap()["text"]
        );
    }
    #[test]
    fn support_files_and_changed_package_seeds_never_replace_a_pinned_revision() {
        let state = tempfile::tempdir().unwrap();
        let seeds = tempfile::tempdir().unwrap();
        let root = seeds.path().join("workflow");
        std::fs::create_dir_all(root.join("references")).unwrap();
        for (name, text) in files("old checks") {
            std::fs::write(root.join(name), text).unwrap();
        }
        let cfg = SkillsConfig {
            bundled: false,
            directories: vec![root.clone()],
        };
        let library = SkillLibrary::open(&cfg, state.path()).unwrap();
        let old = library.snapshot().unwrap();
        library
            .publish(&proposal("workflow", 1, "learned checks"))
            .unwrap();
        let learned = library.snapshot().unwrap();
        for (name, text) in files("new package checks") {
            std::fs::write(root.join(name), text).unwrap();
        }
        assert_eq!(
            old.execute(&json!({"action":"load","id":"workflow","file":"references/check.md"}))
                .unwrap()["text"],
            "old checks"
        );
        drop(library);
        let library = SkillLibrary::open(&cfg, state.path()).unwrap();
        assert_eq!(
            library
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"workflow","file":"references/check.md"}))
                .unwrap()["text"],
            "learned checks"
        );
        assert_eq!(
            learned
                .execute(&json!({"action":"load","id":"workflow"}))
                .unwrap()["revision"],
            2
        );
        assert_eq!(library.seed_offers().unwrap().as_array().unwrap().len(), 1);
        assert!(Arc::ptr_eq(
            &library.snapshot().unwrap(),
            &library.snapshot().unwrap()
        ));
    }
    #[test]
    fn a_new_package_seed_collision_is_offered_without_overwriting_a_learned_skill() {
        let state = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let cfg = SkillsConfig {
            bundled: false,
            directories: vec![],
        };
        let library = SkillLibrary::open(&cfg, state.path()).unwrap();
        library
            .publish(&proposal("workflow", 0, "learned checks"))
            .unwrap();
        drop(library);
        let root = source.path().join("workflow");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/private-config"), [255, 0, 255]).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            files("package checks")["SKILL.md"].clone(),
        )
        .unwrap();
        let library = SkillLibrary::open(
            &SkillsConfig {
                bundled: false,
                directories: vec![root],
            },
            state.path(),
        )
        .unwrap();
        let skill = library
            .snapshot()
            .unwrap()
            .execute(&json!({"action":"load","id":"workflow"}))
            .unwrap();
        assert_eq!(skill["origin"], "curator");
        assert!(skill["text"].as_str().unwrap().contains("learned checks"));
        assert_eq!(library.seed_offers().unwrap().as_array().unwrap().len(), 1);
    }
    #[test]
    fn stale_multi_skill_proposal_is_atomic_and_resources_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let library = SkillLibrary::open(&SkillsConfig::default(), dir.path()).unwrap();
        let mut change = proposal("fresh", 0, "new checks");
        change.changes.push(Change {
            id: "engineering".into(),
            expected_revision: 99,
            files: files("stale"),
            retire: false,
        });
        assert!(library.publish(&change).is_err());
        assert!(
            library
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"fresh"}))
                .is_err()
        );
        let mut invalid = proposal("fresh", 0, "checks");
        invalid.changes[0]
            .files
            .insert("../outside".into(), "bad".into());
        assert!(library.publish(&invalid).is_err());
        let original = library.revision_files("engineering", 1).unwrap();
        let mut unchanged = proposal("engineering", 1, "placeholder");
        unchanged.changes[0].files = original;
        assert!(library.publish(&unchanged).is_err());
        assert!(SkillLibrary::open(&SkillsConfig::default(), dir.path()).is_err());
    }
    #[test]
    fn interrupted_proposals_survive_without_advancing_the_review_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SkillsConfig::default();
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        for n in 1..=4 {
            library
                .record(
                    &format!("turn-{n}"),
                    7,
                    "channel:7",
                    "deploy",
                    &json!({"events":[]}),
                    true,
                )
                .unwrap();
        }
        library.begin_attempt("attempt", "7").unwrap();
        library
            .save_proposal("attempt", &proposal("fresh", 0, "checks"))
            .unwrap();
        drop(library);
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        assert_eq!(library.status().unwrap()["latest"]["status"], "interrupted");
        assert_eq!(library.status().unwrap()["pending_tasks"], 4);
        assert_eq!(library.proposals("7").unwrap().as_array().unwrap().len(), 1);
        assert!(library.proposal("8", "attempt").is_err());
        assert!(
            library
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"fresh"}))
                .is_err()
        );
    }
    #[test]
    fn task_activities_keep_worker_evidence_private_and_do_not_count_wakeups_as_new_cases() {
        let dir = tempfile::tempdir().unwrap();
        let library = SkillLibrary::open(&SkillsConfig::default(), dir.path()).unwrap();
        for n in 0..4 {
            library
                .record(
                    &format!("same-task-{n}"),
                    1,
                    "channel:1",
                    "Continue the original task",
                    &json!({"activity":"one-task"}),
                    true,
                )
                .unwrap();
        }
        assert!(library.batch(4).unwrap().is_none());
        assert_eq!(library.status().unwrap()["pending_tasks"], 1);
        for n in 2..=4 {
            library
                .record(
                    &format!("root-{n}"),
                    1,
                    "channel:1",
                    &format!("Distinct task {n}"),
                    &json!({"activity":format!("task-{n}")}),
                    true,
                )
                .unwrap();
        }
        library
            .record(
                "training-worker",
                1,
                "worker",
                "training task",
                &json!({"activity":"task-2","observation":"TRAINING_PRIVATE_RESULT"}),
                true,
            )
            .unwrap();
        library
            .record(
                "withheld-worker",
                1,
                "worker",
                "withheld task",
                &json!({"activity":"task-4","observation":"WITHHELD_PRIVATE_RESULT"}),
                true,
            )
            .unwrap();
        let batch = library.batch(4).unwrap().unwrap();
        assert!(
            serde_json::to_string(&batch.training)
                .unwrap()
                .contains("TRAINING_PRIVATE_RESULT")
        );
        assert!(
            !serde_json::to_string(&batch.training)
                .unwrap()
                .contains("WITHHELD_PRIVATE_RESULT")
        );
        assert!(
            serde_json::to_string(&batch.held_out)
                .unwrap()
                .contains("WITHHELD_PRIVATE_RESULT")
        );
        library.begin_attempt("pass", "1").unwrap();
        library
            .finish_attempt("pass", "no_change", &json!({}), &[], Some(&batch))
            .unwrap();
        library
            .record(
                "late-wakeup",
                1,
                "channel:1",
                "Same task woke again",
                &json!({"activity":"one-task"}),
                true,
            )
            .unwrap();
        assert_eq!(library.status().unwrap()["pending_tasks"], 0);
        assert!(library.batch(4).unwrap().is_none());
    }
    #[test]
    fn publication_and_cursor_commit_together_and_workers_do_not_supply_held_out_cases() {
        let dir = tempfile::tempdir().unwrap();
        let library = SkillLibrary::open(&SkillsConfig::default(), dir.path()).unwrap();
        for n in 0..5 {
            library
                .record(
                    &format!("worker-{n}"),
                    7,
                    "worker",
                    "private supporting task",
                    &json!({}),
                    true,
                )
                .unwrap();
        }
        assert!(library.batch(4).unwrap().is_none());
        for n in 1..=4 {
            library
                .record(
                    &format!("root-{n}"),
                    7,
                    "channel:7",
                    "deploy",
                    &json!({}),
                    true,
                )
                .unwrap();
        }
        let batch = library.batch(4).unwrap().unwrap();
        assert_eq!(batch.training.len(), 3);
        assert_eq!(batch.held_out.len(), 1);
        assert!(
            !batch
                .training
                .iter()
                .any(|e| e.seq == batch.held_out[0].seq)
        );
        library.begin_attempt("attempt", "7").unwrap();
        let p = proposal("fresh", 0, "checks");
        library.save_proposal("attempt", &p).unwrap();
        library
            .publish_attempt(
                &p,
                "attempt",
                &json!({"review":"accepted"}),
                &[json!({"input_tokens":10})],
                &batch,
            )
            .unwrap();
        assert_eq!(library.status().unwrap()["latest"]["status"], "published");
        assert_eq!(library.status().unwrap()["pending_tasks"], 0);
        assert!(
            library
                .publish_attempt(&p, "attempt", &json!({}), &[], &batch)
                .is_err()
        );
    }
}
