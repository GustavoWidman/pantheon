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

pub const INDEX: &str = "Skills are an evolving library. The complete active catalogue below is a frozen system-prefix snapshot; appended curator notifications announce later approved revisions. Use skill(action=\"preview\") for a short introduction, skill(action=\"load\") for a relevant guide before using it, and skill(action=\"list\") for current metadata. Invocation and refinement counts describe history, not proven success. Invoke explicit_only guides only when the user requests that workflow. Skills guide authorized work; they do not grant permissions or override user instructions.";
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
    /// Reviewed explanation delivered to the orchestrator and dashboard.
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub purpose: String,
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
pub struct QueuedFork {
    pub id: String,
    pub channel: String,
    pub generation: String,
    pub payload: Value,
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
            CREATE TABLE IF NOT EXISTS attempts(id TEXT PRIMARY KEY,channel TEXT NOT NULL,status TEXT NOT NULL,proposal TEXT,report TEXT NOT NULL DEFAULT '',usage TEXT NOT NULL DEFAULT '[]',review_context TEXT NOT NULL DEFAULT '',created INTEGER NOT NULL,finished INTEGER);
            CREATE INDEX IF NOT EXISTS attempts_created ON attempts(created);
            CREATE TABLE IF NOT EXISTS curator_forks(id TEXT PRIMARY KEY,channel TEXT NOT NULL,generation TEXT NOT NULL,payload TEXT NOT NULL,status TEXT NOT NULL,phase TEXT NOT NULL,reviewers_spawned INTEGER NOT NULL DEFAULT 0,reviewers_running INTEGER NOT NULL DEFAULT 0,reviewers_finished INTEGER NOT NULL DEFAULT 0,created INTEGER NOT NULL,finished INTEGER,UNIQUE(channel,generation));
            CREATE UNIQUE INDEX IF NOT EXISTS curator_one_active ON curator_forks(channel) WHERE status='active';
            CREATE UNIQUE INDEX IF NOT EXISTS curator_one_pending ON curator_forks(channel) WHERE status='queued';
            CREATE TABLE IF NOT EXISTS curator_fork_settings(id TEXT PRIMARY KEY,model TEXT NOT NULL,reasoning TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS curator_fork_controls(id TEXT PRIMARY KEY,requested INTEGER NOT NULL DEFAULT 0,cancelled INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS skill_notifications(id TEXT PRIMARY KEY,channel TEXT NOT NULL,payload TEXT NOT NULL,created INTEGER NOT NULL,acknowledged INTEGER NOT NULL DEFAULT 0,presented INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS skill_counters(id TEXT PRIMARY KEY,invocations INTEGER NOT NULL DEFAULT 0,refinements INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS skill_invocations(skill TEXT NOT NULL,owner TEXT NOT NULL,PRIMARY KEY(skill,owner));
            CREATE TABLE IF NOT EXISTS skill_catalogues(channel TEXT PRIMARY KEY,text TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS skill_channel_settled(channel TEXT PRIMARY KEY,settled INTEGER NOT NULL);
            UPDATE curator_forks SET status='interrupted',phase='interrupted',reviewers_running=0,finished=strftime('%s','now') WHERE status='active';
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
        let mut query = db.prepare("SELECT h.id,h.revision,h.origin,r.files,COALESCE(c.invocations,0),COALESCE(c.refinements,0) FROM heads h JOIN revisions r USING(id,revision) LEFT JOIN skill_counters c ON c.id=h.id WHERE h.retired=0 ORDER BY h.id")?;
        let rows = query.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut skills = Skills::empty();
        for row in rows {
            let (id, revision, origin, files, invocations, refinements) = row?;
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
            skill.invocations = invocations;
            skill.refinements = refinements;
        }
        let skills = Arc::new(skills);
        *cache = Some(skills.clone());
        Ok(skills)
    }
    /// One durable pending snapshot per channel; superseded generations remain deduplicated.
    pub fn enqueue_fork(&self, channel: &str, generation: &str, payload: &Value) -> Result<bool> {
        ensure!(
            !channel.is_empty()
                && channel.len() <= 128
                && !generation.is_empty()
                && generation.len() <= 256,
            "invalid fork identity"
        );
        let payload = serde_json::to_string(payload)?;
        ensure!(
            payload.len() <= 16_000_000,
            "fork exceeds durable context budget"
        );
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM curator_forks WHERE channel=?1 AND generation=?2)",
            params![channel, generation],
            |r| r.get(0),
        )?;
        if exists {
            return Ok(false);
        }
        let requested: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM curator_forks f JOIN curator_fork_controls c USING(id) WHERE f.channel=?1 AND f.status='queued' AND c.requested=1)",[channel],|r|r.get(0))?;
        tx.execute("UPDATE curator_forks SET status='superseded',payload='null',finished=?2 WHERE channel=?1 AND status='queued'",params![channel,crate::store::now()])?;
        let id = uuid::Uuid::new_v4().to_string();
        tx.execute("INSERT INTO curator_forks(id,channel,generation,payload,status,phase,created) VALUES(?1,?2,?3,?4,'queued','waiting',?5)",params![id,channel,generation,payload,crate::store::now()])?;
        if requested {
            tx.execute(
                "INSERT INTO curator_fork_controls(id,requested,cancelled) VALUES(?1,1,0)",
                [&id],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }
    /// Manual requests refer to an existing eligible snapshot; they cannot invent work.
    pub fn request_channel_fork(&self, channel: &str) -> Result<bool> {
        let changed = self.db.lock().unwrap().execute(
            "INSERT INTO curator_fork_controls(id,requested,cancelled) SELECT id,1,0 FROM curator_forks WHERE channel=?1 AND status='queued' ON CONFLICT(id) DO UPDATE SET requested=1",
            [channel],
        )?;
        Ok(changed > 0)
    }
    pub fn fork_requested(&self, channel: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM curator_forks f JOIN curator_fork_controls c USING(id) WHERE f.channel=?1 AND f.status='queued' AND c.requested=1)",[channel],|r|r.get(0))?)
    }
    /// Pending snapshots are discarded. Active jobs keep ownership until their
    /// cancellation token finishes, but publication is immediately forbidden.
    pub fn cancel_channel_forks(&self, channel: &str) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute("INSERT INTO curator_fork_controls(id,requested,cancelled) SELECT id,0,1 FROM curator_forks WHERE channel=?1 AND status IN ('active','queued') ON CONFLICT(id) DO UPDATE SET requested=0,cancelled=1",[channel])?;
        tx.execute("UPDATE curator_forks SET status='cancelled',phase='cancelled',payload='null',finished=?2 WHERE channel=?1 AND status='queued'",params![channel,crate::store::now()])?;
        tx.commit()?;
        Ok(())
    }
    pub fn queued_channels(&self) -> Result<Vec<String>> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT channel FROM curator_forks f WHERE status='queued' AND NOT EXISTS(SELECT 1 FROM curator_forks a WHERE a.channel=f.channel AND a.status='active') ORDER BY created,rowid")?;
        Ok(q.query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn take_fork(&self, channel: &str) -> Result<Option<QueuedFork>> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM curator_forks WHERE channel=?1 AND status='active')",
            [channel],
            |r| r.get(0),
        )?;
        if active {
            return Ok(None);
        }
        let raw = tx.query_row("SELECT id,generation,payload FROM curator_forks WHERE channel=?1 AND status='queued' ORDER BY created,rowid LIMIT 1",[channel],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional()?;
        let Some((id, generation, payload)) = raw else {
            return Ok(None);
        };
        tx.execute("UPDATE curator_forks SET status='active',phase='drafting',reviewers_spawned=0,reviewers_running=0,reviewers_finished=0 WHERE id=?1",[&id])?;
        tx.execute("INSERT INTO attempts(id,channel,status,created) VALUES(?1,?2,'running',?3) ON CONFLICT(id) DO UPDATE SET status='running',finished=NULL",params![id,channel,crate::store::now()])?;
        tx.execute(
            "UPDATE curator_fork_controls SET requested=0 WHERE id=?1",
            [&id],
        )?;
        tx.commit()?;
        Ok(Some(QueuedFork {
            id,
            channel: channel.into(),
            generation,
            payload: serde_json::from_str(&payload)?,
        }))
    }
    /// Resolve once before provider work; later configuration changes cannot
    /// rewrite the durable identity of an already-running job or its reviewers.
    pub fn pin_fork_settings(&self, id: &str, model: &str, reasoning: &str) -> Result<()> {
        ensure!(
            !model.trim().is_empty()
                && model.len() <= 256
                && !reasoning.trim().is_empty()
                && reasoning.len() <= 64,
            "invalid pinned curator settings"
        );
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM curator_forks WHERE id=?1 AND status='active')",
            [id],
            |r| r.get(0),
        )?;
        ensure!(active, "curation fork is no longer active");
        let old = tx
            .query_row(
                "SELECT model,reasoning FROM curator_fork_settings WHERE id=?1",
                [id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((old_model, old_reasoning)) = old {
            ensure!(
                old_model == model && old_reasoning == reasoning,
                "curation fork settings are already pinned"
            );
        } else {
            tx.execute(
                "INSERT INTO curator_fork_settings(id,model,reasoning) VALUES(?1,?2,?3)",
                params![id, model, reasoning],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn update_fork_phase(
        &self,
        id: &str,
        phase: &str,
        spawned: usize,
        running: usize,
        finished: usize,
    ) -> Result<()> {
        ensure!(
            matches!(phase, "drafting" | "review")
                && running + finished <= spawned
                && spawned <= 64,
            "invalid review phase/counters"
        );
        let changed = self.db.lock().unwrap().execute("UPDATE curator_forks SET phase=?2,reviewers_spawned=?3,reviewers_running=?4,reviewers_finished=?5 WHERE id=?1 AND status='active'",params![id,phase,spawned as i64,running as i64,finished as i64])?;
        ensure!(changed == 1, "curation fork is no longer active");
        Ok(())
    }
    pub fn finish_fork(
        &self,
        id: &str,
        status: &str,
        report: &Value,
        usage: &[Value],
    ) -> Result<()> {
        ensure!(
            matches!(
                status,
                "denied"
                    | "no_change"
                    | "failed"
                    | "interrupted"
                    | "rejected"
                    | "completed"
                    | "cancelled"
            ),
            "invalid final curator status"
        );
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        ensure!(tx.execute("UPDATE curator_forks SET status=?2,phase=?2,payload='null',reviewers_running=0,finished=?3 WHERE id=?1 AND status='active'",params![id,status,crate::store::now()])? == 1,"curation fork is no longer active");
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
        tx.commit()?;
        Ok(())
    }
    /// Registry updates, completed attempt and notification outbox commit together.
    pub fn publish_fork(
        &self,
        id: &str,
        proposal: &Proposal,
        report: &Value,
        usage: &[Value],
    ) -> Result<()> {
        validate_proposal(proposal)?;
        ensure!(
            proposal
                .changes
                .iter()
                .all(|c| !c.summary.trim().is_empty() && !c.purpose.trim().is_empty()),
            "publication requires a summary and purpose for each skill change"
        );
        let mut cache = self.snapshot_cache.lock().unwrap();
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let channel: String = tx
            .query_row(
                "SELECT channel FROM curator_forks f WHERE id=?1 AND status='active' AND NOT EXISTS(SELECT 1 FROM curator_fork_controls c WHERE c.id=f.id AND c.cancelled=1)",
                [id],
                |r| r.get(0),
            )
            .context("curation fork is no longer active")?;
        apply_changes(&tx, proposal, Some(id))?;
        for change in &proposal.changes {
            let revision = change.expected_revision + 1;
            let kind = if change.retire {
                "retire"
            } else if change.expected_revision == 0 {
                "add"
            } else {
                "modify"
            };
            let note = json!({"id":format!("{id}:{}",change.id),"attempt":id,"channel":channel,"skill":change.id,"revision":revision,"kind":kind,"summary":change.summary,"purpose":change.purpose});
            tx.execute(
                "INSERT INTO skill_notifications(id,channel,payload,created) VALUES(?1,?2,?3,?4)",
                params![
                    note["id"].as_str().unwrap(),
                    channel,
                    note.to_string(),
                    crate::store::now()
                ],
            )?;
        }
        tx.execute("UPDATE curator_forks SET status='published',phase='finished',payload='null',reviewers_running=0,finished=?2 WHERE id=?1",params![id,crate::store::now()])?;
        tx.execute("UPDATE attempts SET status='published',proposal=?2,report=?3,usage=?4,finished=?5 WHERE id=?1",params![id,serde_json::to_string(proposal)?,serde_json::to_string(report)?,serde_json::to_string(usage)?,crate::store::now()])?;
        tx.commit()?;
        *cache = None;
        Ok(())
    }
    pub fn notifications(&self, channel: &str) -> Result<Vec<Value>> {
        self.notification_rows(channel, false)
    }
    pub fn notification_channels(&self) -> Result<Vec<String>> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare(
            "SELECT DISTINCT channel FROM skill_notifications WHERE presented=0 ORDER BY channel",
        )?;
        Ok(q.query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn pending_skill_events(&self, channel: &str) -> Result<Vec<Value>> {
        self.notification_rows(channel, true)
    }
    fn notification_rows(&self, channel: &str, events: bool) -> Result<Vec<Value>> {
        let db = self.db.lock().unwrap();
        let mut q=db.prepare(if events {"SELECT payload FROM skill_notifications WHERE channel=?1 AND presented=0 ORDER BY created,rowid"}else{"SELECT payload FROM skill_notifications WHERE channel=?1 AND acknowledged=0 ORDER BY created,rowid"})?;
        let raw = q
            .query_map([channel], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        raw.into_iter()
            .map(|s| Ok(serde_json::from_str(&s)?))
            .collect()
    }
    pub fn acknowledge_notifications(&self, channel: &str, ids: &[String]) -> Result<()> {
        self.mark_notifications(channel, ids, false)
    }
    pub fn mark_skill_events_presented(&self, channel: &str, ids: &[String]) -> Result<()> {
        self.mark_notifications(channel, ids, true)
    }
    fn mark_notifications(&self, channel: &str, ids: &[String], presented: bool) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(if presented {"UPDATE skill_notifications SET presented=1 WHERE channel=?1 AND id IN (SELECT value FROM json_each(?2))"}else{"UPDATE skill_notifications SET acknowledged=1 WHERE channel=?1 AND id IN (SELECT value FROM json_each(?2))"},params![channel,serde_json::to_string(ids)?])?;
        Ok(())
    }
    /// Freeze exact system catalogue bytes until the caller's idle refresh boundary.
    pub fn note_channel_settled(&self, channel: &str, timestamp: i64) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO skill_channel_settled(channel,settled) VALUES(?1,?2) ON CONFLICT(channel) DO UPDATE SET settled=max(settled,excluded.settled)",params![channel,timestamp])?;
        Ok(())
    }
    pub fn last_settled(&self, channel: &str) -> Result<Option<i64>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT settled FROM skill_channel_settled WHERE channel=?1",
                [channel],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn catalogue_refresh_due(
        &self,
        channel: &str,
        idle_seconds: u64,
        now: i64,
    ) -> Result<bool> {
        Ok(self.last_settled(channel)?.is_some_and(|last| {
            now.saturating_sub(last) >= i64::try_from(idle_seconds).unwrap_or(i64::MAX)
        }))
    }
    pub fn cache_catalogue(&self, channel: &str, candidate: &str, refresh: bool) -> Result<String> {
        ensure!(candidate.len() <= 1_000_000, "catalogue exceeds budget");
        let db = self.db.lock().unwrap();
        db.execute("INSERT INTO skill_catalogues(channel,text) VALUES(?1,?2) ON CONFLICT(channel) DO UPDATE SET text=excluded.text WHERE ?3",params![channel,candidate,refresh])?;
        Ok(db.query_row(
            "SELECT text FROM skill_catalogues WHERE channel=?1",
            [channel],
            |r| r.get(0),
        )?)
    }
    pub fn catalogue(&self, description_chars: usize) -> Result<String> {
        self.snapshot()?.catalogue(description_chars)
    }
    pub fn record_invocation(&self, skill_id: &str, owner_turn_id: &str) -> Result<()> {
        ensure!(
            !owner_turn_id.is_empty() && owner_turn_id.len() <= 256,
            "invalid invocation owner"
        );
        let mut cache = self.snapshot_cache.lock().unwrap();
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM heads WHERE id=?1)",
            [skill_id],
            |r| r.get(0),
        )?;
        ensure!(exists, "unknown skill invocation");
        if tx.execute(
            "INSERT OR IGNORE INTO skill_invocations(skill,owner) VALUES(?1,?2)",
            params![skill_id, owner_turn_id],
        )? > 0
        {
            tx.execute("INSERT INTO skill_counters(id,invocations,refinements) VALUES(?1,1,0) ON CONFLICT(id) DO UPDATE SET invocations=invocations+1",[skill_id])?;
            *cache = None;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn channel_status(&self, channel: &str) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let queued: i64 = db.query_row(
            "SELECT count(*) FROM curator_forks WHERE channel=?1 AND status='queued'",
            [channel],
            |r| r.get(0),
        )?;
        let requested: bool=db.query_row("SELECT EXISTS(SELECT 1 FROM curator_forks f JOIN curator_fork_controls c USING(id) WHERE f.channel=?1 AND f.status='queued' AND c.requested=1)",[channel],|r|r.get(0))?;
        let latest=db.query_row("SELECT f.id,f.generation,f.status,f.phase,f.reviewers_spawned,f.reviewers_running,f.reviewers_finished,f.created,f.finished,a.report,a.usage,s.model,s.reasoning FROM curator_forks f LEFT JOIN attempts a USING(id) LEFT JOIN curator_fork_settings s USING(id) WHERE f.channel=?1 ORDER BY (f.status='active') DESC,f.created DESC,f.rowid DESC LIMIT 1",[channel],|r|Ok(json!({"id":r.get::<_,String>(0)?,"generation":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?,"phase":r.get::<_,String>(3)?,"reviewers_spawned":r.get::<_,i64>(4)?,"reviewers_running":r.get::<_,i64>(5)?,"reviewers_finished":r.get::<_,i64>(6)?,"created":r.get::<_,i64>(7)?,"finished":r.get::<_,Option<i64>>(8)?,"report":r.get::<_,Option<String>>(9)?,"usage":r.get::<_,Option<String>>(10)?,"model":r.get::<_,Option<String>>(11)?,"reasoning":r.get::<_,Option<String>>(12)?}))).optional()?;
        Ok(json!({"channel":channel,"queued_count":queued,"requested":requested,"latest":latest}))
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
        let mut q = db.prepare("SELECT h.id,revision,retired,origin,COALESCE(c.invocations,0),COALESCE(c.refinements,0) FROM heads h LEFT JOIN skill_counters c ON c.id=h.id ORDER BY h.id")?;
        let rows = q.query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"revision":r.get::<_,i64>(1)?,"retired":r.get::<_,bool>(2)?,"origin":r.get::<_,String>(3)?,"invocations":r.get::<_,i64>(4)?,"refinements":r.get::<_,i64>(5)?})))?;
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
                summary: String::new(),
                purpose: String::new(),
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
            proposals.push(json!({"id":id,"status":status,"task_family":p.task_family,"reason":p.reason,"changes":p.changes.iter().map(|c|json!({"id":c.id,"expected_revision":c.expected_revision,"retire":c.retire,"summary":c.summary,"purpose":c.purpose})).collect::<Vec<_>>()}));
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
        ensure!(
            change.summary.chars().count() <= 1000 && change.purpose.chars().count() <= 1000,
            "change summary/purpose exceeds 1000 characters"
        );
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
        if attempt.is_some() && actual > 0 && !change.retire {
            tx.execute("INSERT INTO skill_counters(id,invocations,refinements) VALUES(?1,0,1) ON CONFLICT(id) DO UPDATE SET refinements=refinements+1",[&change.id])?;
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

fn validate_library(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let (count,bytes):(i64,i64)=tx.query_row("SELECT count(*),COALESCE(sum(length(r.files)),0) FROM heads h JOIN revisions r USING(id,revision) WHERE h.retired=0",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(count <= 256, "skill library exceeds 256 active entries");
    ensure!(bytes <= 32_000_000, "active skill library exceeds 32 MB");
    Ok(())
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
                summary: "Updated reusable deployment checks.".into(),
                purpose: "Improve deployment verification.".into(),
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
            summary: "Updated reusable deployment checks.".into(),
            purpose: "Improve deployment verification.".into(),
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
    fn queues_serialize_per_channel_coalesce_and_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SkillsConfig::default();
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        assert!(
            library
                .enqueue_fork("a", "1", &json!({"context":"a1"}))
                .unwrap()
        );
        assert!(
            library
                .enqueue_fork("a", "2", &json!({"context":"a2"}))
                .unwrap()
        );
        assert!(
            !library
                .enqueue_fork("a", "1", &json!({"context":"duplicate"}))
                .unwrap()
        );
        let a = library.take_fork("a").unwrap().unwrap();
        assert_eq!(a.generation, "2");
        assert_eq!(a.payload["context"], "a2");
        assert!(library.enqueue_fork("a", "3", &json!({})).unwrap());
        assert!(library.take_fork("a").unwrap().is_none());
        library.enqueue_fork("b", "1", &json!({})).unwrap();
        let b = library.take_fork("b").unwrap().unwrap();
        library.update_fork_phase(&a.id, "review", 2, 1, 1).unwrap();
        assert_eq!(
            library.channel_status("a").unwrap()["latest"]["phase"],
            "review"
        );
        assert_eq!(
            library.channel_status("a").unwrap()["latest"]["reviewers_spawned"],
            2
        );
        assert!(library.queued_channels().unwrap().is_empty());
        library
            .finish_fork(&b.id, "no_change", &json!({}), &[])
            .unwrap();
        library.note_channel_settled("a", 100).unwrap();
        assert!(!library.catalogue_refresh_due("a", 300, 399).unwrap());
        assert!(library.catalogue_refresh_due("a", 300, 400).unwrap());
        drop(library);
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        assert_eq!(
            library.channel_status("a").unwrap()["latest"]["status"],
            "queued"
        );
        assert_eq!(library.last_settled("a").unwrap(), Some(100));
        assert_eq!(library.queued_channels().unwrap(), vec!["a"]);
        assert_eq!(library.take_fork("a").unwrap().unwrap().generation, "3");
        assert!(!library.enqueue_fork("a", "2", &json!({})).unwrap());
    }
    #[test]
    fn fork_publication_notifications_and_counters_are_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SkillsConfig::default();
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        library.enqueue_fork("a", "1", &json!({})).unwrap();
        let fork = library.take_fork("a").unwrap().unwrap();
        let mut stale = proposal("engineering", 1, "new engineering");
        stale
            .changes
            .push(proposal("research", 99, "new research").changes.remove(0));
        assert!(
            library
                .publish_fork(&fork.id, &stale, &json!({}), &[])
                .is_err()
        );
        assert_eq!(
            library.snapshot().unwrap().entries["engineering"].revision,
            1
        );
        assert!(library.notifications("a").unwrap().is_empty());
        assert_eq!(
            library.channel_status("a").unwrap()["latest"]["status"],
            "active"
        );
        let p = proposal("engineering", 1, "new engineering");
        library
            .publish_fork(&fork.id, &p, &json!({"approved":true}), &[])
            .unwrap();
        let notes = library.notifications("a").unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["kind"], "modify");
        assert_eq!(notes[0]["revision"], 2);
        assert_eq!(notes[0]["purpose"], p.changes[0].purpose);
        assert!(library.notifications("b").unwrap().is_empty());
        let ids = vec![notes[0]["id"].as_str().unwrap().to_owned()];
        library.mark_skill_events_presented("a", &ids).unwrap();
        assert!(library.pending_skill_events("a").unwrap().is_empty());
        assert_eq!(library.notifications("a").unwrap().len(), 1);
        library.acknowledge_notifications("b", &ids).unwrap();
        assert_eq!(library.notifications("a").unwrap().len(), 1);
        drop(library);
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        assert_eq!(library.notifications("a").unwrap(), notes);
        assert_eq!(
            library
                .heads()
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["id"] == "engineering")
                .unwrap()["refinements"],
            1
        );
        library.acknowledge_notifications("a", &ids).unwrap();
        assert!(library.notifications("a").unwrap().is_empty());
        assert!(library.publish_fork(&fork.id, &p, &json!({}), &[]).is_err());
    }
    #[test]
    fn full_catalogue_and_counts_remain_frozen_until_idle_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let library = SkillLibrary::open(&SkillsConfig::default(), dir.path()).unwrap();
        for n in 0..20 {
            library
                .publish(&proposal(&format!("guide-{n}"), 0, "checks"))
                .unwrap();
        }
        let before = library.catalogue(180).unwrap();
        assert!(before.contains("guide-19"));
        assert!(before.contains("guide-9"));
        assert!(before.contains("browser-activities"));
        let pinned = library.snapshot().unwrap();
        assert_eq!(
            library.cache_catalogue("a", &before, false).unwrap(),
            before
        );
        library.record_invocation("engineering", "turn-1").unwrap();
        library.record_invocation("engineering", "turn-1").unwrap();
        library.record_invocation("engineering", "turn-2").unwrap();
        assert_eq!(pinned.entries["engineering"].invocations, 0);
        assert_eq!(
            library.snapshot().unwrap().entries["engineering"].invocations,
            2
        );
        let after = library.catalogue(180).unwrap();
        assert_ne!(before, after);
        assert_eq!(library.cache_catalogue("a", &after, false).unwrap(), before);
        assert_eq!(library.cache_catalogue("a", &after, true).unwrap(), after);
        drop(library);
        let library = SkillLibrary::open(&SkillsConfig::default(), dir.path()).unwrap();
        assert_eq!(
            library
                .cache_catalogue("a", "should not replace", false)
                .unwrap(),
            after
        );
        assert_eq!(
            library.snapshot().unwrap().entries["engineering"].invocations,
            2
        );
    }
    #[test]
    fn manual_requests_and_cancellation_are_channel_scoped_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SkillsConfig::default();
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        assert!(!library.request_channel_fork("a").unwrap());
        library.enqueue_fork("a", "1", &json!({})).unwrap();
        assert!(library.request_channel_fork("a").unwrap());
        library.enqueue_fork("a", "2", &json!({})).unwrap();
        assert!(library.fork_requested("a").unwrap());
        drop(library);
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        assert!(library.fork_requested("a").unwrap());
        let a = library.take_fork("a").unwrap().unwrap();
        assert_eq!(a.generation, "2");
        assert!(!library.fork_requested("a").unwrap());
        library
            .save_proposal(&a.id, &proposal("fresh", 0, "fresh procedure"))
            .unwrap();
        library.update_fork_phase(&a.id, "review", 2, 2, 0).unwrap();
        library
            .save_review_context(&a.id, &json!({"phase":"review"}))
            .unwrap();
        library.enqueue_fork("a", "3", &json!({})).unwrap();
        library.enqueue_fork("b", "1", &json!({})).unwrap();
        library.cancel_channel_forks("a").unwrap();
        assert_eq!(library.channel_status("a").unwrap()["queued_count"], 0);
        assert_eq!(library.queued_channels().unwrap(), vec!["b"]);
        assert!(
            library
                .publish_fork(
                    &a.id,
                    &proposal("fresh", 0, "fresh procedure"),
                    &json!({}),
                    &[]
                )
                .is_err()
        );
        assert!(library.notifications("a").unwrap().is_empty());
        library
            .finish_fork(&a.id, "cancelled", &json!({}), &[])
            .unwrap();
        assert!(library.take_fork("a").unwrap().is_none());
        assert_eq!(
            library.channel_status("a").unwrap()["latest"]["reviewers_running"],
            0
        );
        assert!(!library.enqueue_fork("a", "3", &json!({})).unwrap());
        let b = library.take_fork("b").unwrap().unwrap();
        library
            .publish_fork(
                &b.id,
                &proposal("fresh", 0, "fresh procedure"),
                &json!({}),
                &[],
            )
            .unwrap();
        assert_eq!(library.notification_channels().unwrap(), vec!["b"]);
        let note = library.pending_skill_events("b").unwrap().remove(0);
        library
            .mark_skill_events_presented("b", &[note["id"].as_str().unwrap().into()])
            .unwrap();
        assert!(library.notification_channels().unwrap().is_empty());
        assert_eq!(library.snapshot().unwrap().entries["fresh"].refinements, 0);
    }
    #[test]
    fn interrupted_fork_proposals_survive_privately_without_publication() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SkillsConfig::default();
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        library
            .enqueue_fork("7", "generation", &json!({"memory":"snapshot"}))
            .unwrap();
        let fork = library.take_fork("7").unwrap().unwrap();
        library
            .save_proposal(&fork.id, &proposal("fresh", 0, "checks"))
            .unwrap();
        library
            .update_fork_phase(&fork.id, "review", 2, 2, 0)
            .unwrap();
        library
            .save_review_context(&fork.id, &json!({"recorded":"evidence"}))
            .unwrap();
        assert_eq!(
            library.proposal("7", &fork.id).unwrap().changes[0].id,
            "fresh"
        );
        drop(library);
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        assert_eq!(
            library.channel_status("7").unwrap()["latest"]["status"],
            "interrupted"
        );
        assert_eq!(
            library.channel_status("7").unwrap()["latest"]["reviewers_running"],
            0
        );
        assert_eq!(library.proposals("7").unwrap().as_array().unwrap().len(), 1);
        assert!(library.proposal("8", &fork.id).is_err());
        assert!(library.notifications("7").unwrap().is_empty());
        assert!(
            library
                .snapshot()
                .unwrap()
                .execute(&json!({"action":"load","id":"fresh"}))
                .is_err()
        );
        assert!(
            library
                .publish_fork(&fork.id, &proposal("fresh", 0, "checks"), &json!({}), &[])
                .is_err()
        );
    }
    #[test]
    fn job_model_and_reasoning_are_pinned_durably() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SkillsConfig::default();
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        library.enqueue_fork("a", "1", &json!({})).unwrap();
        assert!(
            library
                .pin_fork_settings("missing", "gpt-6-luna", "low")
                .is_err()
        );
        let fork = library.take_fork("a").unwrap().unwrap();
        library
            .pin_fork_settings(&fork.id, "gpt-6-luna", "low")
            .unwrap();
        library
            .pin_fork_settings(&fork.id, "gpt-6-luna", "low")
            .unwrap();
        assert!(
            library
                .pin_fork_settings(&fork.id, "gpt-6.1-sol", "high")
                .is_err()
        );
        library
            .update_fork_phase(&fork.id, "review", 2, 2, 0)
            .unwrap();
        let status = library.channel_status("a").unwrap();
        assert_eq!(status["latest"]["model"], "gpt-6-luna");
        assert_eq!(status["latest"]["reasoning"], "low");
        assert_eq!(status["requested"], false);
        drop(library);
        let library = SkillLibrary::open(&cfg, dir.path()).unwrap();
        let status = library.channel_status("a").unwrap();
        assert_eq!(status["latest"]["status"], "interrupted");
        assert_eq!(status["latest"]["model"], "gpt-6-luna");
        assert_eq!(status["latest"]["reasoning"], "low");
        assert!(
            library
                .pin_fork_settings(&fork.id, "gpt-6-luna", "low")
                .is_err()
        );
    }
}
