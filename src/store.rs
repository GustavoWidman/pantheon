//! Operational durability is separate from the immutable chat log.
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Mutex};

pub struct Store {
    pub(crate) db: Mutex<Connection>,
}
#[derive(Clone, Debug)]
pub struct Input {
    pub id: String,
    pub channel: u64,
    pub user: u64,
    pub text: String,
}
#[derive(Debug)]
pub struct Outbound {
    pub id: String,
    pub channel: u64,
    pub user: Option<u64>,
    pub text: String,
    pub nonce: String,
    pub receipt: Option<String>,
    pub reply_to: Option<u64>,
}
#[derive(Clone, Debug)]
pub struct Job {
    pub id: String,
    pub channel: u64,
    pub user: u64,
    pub kind: String,
    pub payload: Value,
    pub due: i64,
    pub interval: Option<i64>,
}
#[derive(Debug)]
pub struct AgentRecord {
    pub channel: u64,
    pub user: u64,
    pub task: String,
    pub report: String,
    pub model: String,
    pub reasoning: String,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
        CREATE TABLE IF NOT EXISTS inbox(id TEXT PRIMARY KEY,channel TEXT NOT NULL,user TEXT NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'queued',logged INTEGER NOT NULL DEFAULT 0,created INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS outbox(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,channel TEXT NOT NULL,user TEXT,text TEXT NOT NULL,nonce TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'queued',receipt TEXT,attempts INTEGER NOT NULL DEFAULT 0,next_try INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS settings(channel TEXT PRIMARY KEY,model TEXT NOT NULL,reasoning TEXT NOT NULL,usage TEXT NOT NULL DEFAULT '{}');
        CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY,channel TEXT NOT NULL,user TEXT NOT NULL,kind TEXT NOT NULL,payload TEXT NOT NULL,due INTEGER NOT NULL,interval INTEGER,state TEXT NOT NULL DEFAULT 'active',last TEXT);
        CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY,batch TEXT NOT NULL,channel TEXT NOT NULL,user TEXT NOT NULL,task TEXT NOT NULL,state TEXT NOT NULL,report TEXT);
        CREATE TABLE IF NOT EXISTS tool_runs(id TEXT PRIMARY KEY,channel TEXT NOT NULL,name TEXT NOT NULL,state TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS inbox_pending ON inbox(channel,created) WHERE state='queued';
        CREATE INDEX IF NOT EXISTS inbox_unlogged ON inbox(channel) WHERE logged=0;
        CREATE INDEX IF NOT EXISTS outbox_pending ON outbox(channel,seq) WHERE state='queued';
        CREATE INDEX IF NOT EXISTS jobs_due ON jobs(due) WHERE state='active';
        CREATE INDEX IF NOT EXISTS tasks_batch ON tasks(batch,state);
        CREATE TABLE IF NOT EXISTS agent_settings(id TEXT PRIMARY KEY,model TEXT NOT NULL,reasoning TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS agent_runs(id TEXT PRIMARY KEY,owner TEXT NOT NULL,state TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS agent_inbox(id TEXT PRIMARY KEY,owner TEXT NOT NULL,channel TEXT NOT NULL,user TEXT NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'queued',created INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS agent_inbox_pending ON agent_inbox(owner,created) WHERE state='queued';
        CREATE TABLE IF NOT EXISTS shell_runs(id TEXT PRIMARY KEY,owner TEXT NOT NULL,channel TEXT NOT NULL,user TEXT NOT NULL,command TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'running',background INTEGER NOT NULL DEFAULT 0,output TEXT);
        PRAGMA user_version=2;")?;
        crate::ui::initialize(&db)?;
        Ok(Self { db: Mutex::new(db) })
    }
    pub fn admit(&self, input: &Input) -> Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let admitted = tx.execute(
            "INSERT OR IGNORE INTO inbox(id,channel,user,text,created) VALUES(?1,?2,?3,?4,?5)",
            params![
                input.id,
                input.channel.to_string(),
                input.user.to_string(),
                input.text,
                now()
            ],
        )? == 1;
        if admitted {
            let context = tx.query_row("SELECT reply_to,activity FROM ui_contexts WHERE source=?1",[&input.id],|r|Ok(crate::ui::ReplyContext{reply_to:r.get::<_,Option<String>>(0)?.and_then(|s|s.parse().ok()),activity:r.get(1)?})).optional()?.or(tx.query_row("SELECT reply_to,activity FROM ui_sessions WHERE channel=?1 AND closed=0 ORDER BY rowid DESC LIMIT 1",[input.channel.to_string()],|r|Ok(crate::ui::ReplyContext{reply_to:r.get::<_,Option<String>>(0)?.and_then(|s|s.parse().ok()),activity:r.get(1)?})).optional()?).unwrap_or_else(||crate::ui::ReplyContext::request(&input.id));
            tx.execute("INSERT OR IGNORE INTO ui_contexts(source,reply_to,activity,is_prompt) VALUES(?1,?2,?3,1)",params![input.id,context.reply_to.map(|id|id.to_string()),context.activity])?;
            tx.execute(
                "UPDATE ui_contexts SET is_prompt=1 WHERE source=?1",
                [&input.id],
            )?;
            tx.execute("INSERT OR IGNORE INTO ui_sessions(activity,channel,reply_to,closed) VALUES(?1,?2,?3,0)",params![context.activity,input.channel.to_string(),context.reply_to.map(|id|id.to_string())])?;
            if input.id.parse::<u64>().is_ok_and(|n| n > 0) {
                tx.execute(
                    "INSERT OR IGNORE INTO ui_reactions(message,channel,activity) VALUES(?1,?2,?3)",
                    params![input.id, input.channel.to_string(), context.activity],
                )?;
            }
        }
        tx.commit()?;
        Ok(admitted)
    }
    pub fn queued(&self, channel: u64) -> Result<Vec<Input>> {
        let db = self.db.lock().unwrap();
        let mut stmt=db.prepare("SELECT id,user,text FROM inbox WHERE channel=?1 AND state='queued' ORDER BY created,rowid")?;
        let rows = stmt.query_map([channel.to_string()], |r| {
            Ok(Input {
                id: r.get(0)?,
                channel,
                user: r.get::<_, String>(1)?.parse().unwrap_or(0),
                text: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
    pub fn queued_channels(&self) -> Result<Vec<u64>> {
        let db = self.db.lock().unwrap();
        let mut stmt=db.prepare("SELECT DISTINCT channel FROM inbox WHERE state='queued' OR (logged=0 AND state!='queued')")?;
        Ok(stmt
            .query_map([], |r| Ok(r.get::<_, String>(0)?.parse().unwrap_or(0)))?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn mark_logged(&self, id: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("UPDATE inbox SET logged=1 WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn unlogged(&self, channel: u64) -> Result<Vec<Input>> {
        let db = self.db.lock().unwrap();
        let mut stmt=db.prepare("SELECT id,user,text FROM inbox WHERE channel=?1 AND logged=0 AND state!='queued' ORDER BY created,rowid")?;
        Ok(stmt
            .query_map([channel.to_string()], |r| {
                Ok(Input {
                    id: r.get(0)?,
                    channel,
                    user: r.get::<_, String>(1)?.parse().unwrap_or(0),
                    text: r.get(2)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn complete_turn(
        &self,
        inputs: &[String],
        id: &str,
        channel: u64,
        user: u64,
        chunks: &[String],
    ) -> Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let context = match inputs.first() {
            Some(id) => tx
                .query_row(
                    "SELECT reply_to,activity FROM ui_contexts WHERE source=?1",
                    [id],
                    |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?,
            None => None,
        };
        let queued: i64 = tx.query_row(
            "SELECT count(*) FROM inbox i LEFT JOIN ui_contexts c ON c.source=i.id WHERE i.channel=?1 AND i.state='queued' AND (c.activity=?2 OR c.is_prompt=1 OR c.source IS NULL)",
            params![channel.to_string(),context.as_ref().map(|(_,activity)|activity)],
            |r| r.get(0),
        )?;
        if queued != 0 {
            return Ok(false);
        }
        let recipient: String = context
            .as_ref()
            .and_then(|(reply, _)| reply.as_ref())
            .map(|id| {
                tx.query_row("SELECT user FROM inbox WHERE id=?1", [id], |r| {
                    r.get::<_, String>(0)
                })
                .optional()
            })
            .transpose()?
            .flatten()
            .unwrap_or_else(|| user.to_string());
        let activity = context
            .as_ref()
            .map(|(_, activity)| activity.as_str())
            .unwrap_or("");
        let background = crate::ui::background_count(&tx, activity)?;
        crate::ui::close_segments(&tx, channel)?;
        if background == 0 {
            tx.execute(
                "UPDATE ui_sessions SET closed=1 WHERE activity=?1",
                [activity],
            )?;
        }

        for (i, text) in chunks.iter().enumerate() {
            let id = format!("{id}:{i}");
            let hash = Sha256::digest(id.as_bytes());
            let nonce = u64::from_le_bytes(hash[..8].try_into().unwrap()).to_string();
            tx.execute(
                "INSERT OR IGNORE INTO outbox(id,channel,user,text,nonce,reply_to) VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    id,
                    channel.to_string(),
                    if i == 0 && background==0 { Some(recipient.clone()) } else { None },
                    text,
                    nonce,
                    if i==0 && background==0 {context.as_ref().and_then(|(reply,_)|reply.as_deref())} else {None}
                ],
            )?;
        }
        for id in inputs {
            tx.execute("UPDATE inbox SET state='done' WHERE id=?1", [id])?;
        }
        tx.commit()?;
        Ok(true)
    }
    pub fn input_state(&self, id: &str, state: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("UPDATE inbox SET state=?2 WHERE id=?1", params![id, state])?;
        Ok(())
    }
    pub fn enqueue(&self, id: &str, channel: u64, user: Option<u64>, text: &str) -> Result<()> {
        let hash = Sha256::digest(id.as_bytes());
        let nonce = u64::from_le_bytes(hash[..8].try_into().unwrap()).to_string();
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        crate::ui::close_segments(&tx, channel)?;
        tx.execute(
            "INSERT INTO outbox(id,channel,user,text,nonce) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET text=excluded.text,state='queued',next_try=0 WHERE outbox.text!=excluded.text",
            params![
                id,
                channel.to_string(),
                user.map(|u| u.to_string()),
                text,
                nonce
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn next_outbound(&self) -> Result<Option<Outbound>> {
        self.next_outbound_excluding(&[])
    }
    pub fn next_outbound_excluding(&self, channels: &[u64]) -> Result<Option<Outbound>> {
        self.next_outbound_with_ui_budget(channels, &[])
    }
    pub fn next_outbound_with_ui_budget(
        &self,
        channels: &[u64],
        ui_throttled: &[u64],
    ) -> Result<Option<Outbound>> {
        // New messages preserve order and can pass edits of existing activity.
        Ok(self.db.lock().unwrap().query_row("SELECT id,channel,user,text,nonce,receipt,reply_to FROM outbox o WHERE state='queued' AND next_try<=?1 AND channel NOT IN (SELECT value FROM json_each(?2)) AND (o.id NOT LIKE '%:activity:%' OR o.receipt IS NULL OR channel NOT IN (SELECT value FROM json_each(?3))) AND NOT EXISTS(SELECT 1 FROM outbox p WHERE p.channel=o.channel AND p.state='queued' AND p.seq<o.seq AND (p.id NOT LIKE '%:activity:%' OR p.receipt IS NULL)) ORDER BY seq LIMIT 1",params![now(),serde_json::to_string(&channels.iter().map(|c|c.to_string()).collect::<Vec<_>>())?,serde_json::to_string(&ui_throttled.iter().map(|c|c.to_string()).collect::<Vec<_>>())?],|r|Ok(Outbound{id:r.get(0)?,channel:r.get::<_,String>(1)?.parse().unwrap_or(0),user:r.get::<_,Option<String>>(2)?.and_then(|s|s.parse().ok()),text:r.get(3)?,nonce:r.get(4)?,receipt:r.get(5)?,reply_to:r.get::<_,Option<String>>(6)?.and_then(|s|s.parse().ok())})).optional()?)
    }
    pub fn delivered(&self, item: &Outbound, receipt: &str) -> Result<()> {
        self.db.lock().unwrap().execute("UPDATE outbox SET receipt=?2,state=CASE WHEN text=?3 THEN 'sent' ELSE 'queued' END,next_try=0 WHERE id=?1",params![item.id,receipt,item.text])?;
        Ok(())
    }
    pub fn sent(&self, id: &str, receipt: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE outbox SET state='sent',receipt=?2 WHERE id=?1",
            params![id, receipt],
        )?;
        Ok(())
    }
    pub fn retry_outbound(&self, id: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE outbox SET attempts=attempts+1,next_try=?2+MIN(60,2*(attempts+1)) WHERE id=?1",
            params![id, now()],
        )?;
        Ok(())
    }
    pub fn settings(&self, channel: u64, model: &str, reasoning: &str) -> Result<(String, String)> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR IGNORE INTO settings(channel,model,reasoning) VALUES(?1,?2,?3)",
            params![channel.to_string(), model, reasoning],
        )?;
        Ok(db.query_row(
            "SELECT model,reasoning FROM settings WHERE channel=?1",
            [channel.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }
    pub fn set_settings(&self, channel: u64, model: &str, reasoning: &str) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO settings(channel,model,reasoning) VALUES(?1,?2,?3) ON CONFLICT(channel) DO UPDATE SET model=excluded.model,reasoning=excluded.reasoning",params![channel.to_string(),model,reasoning])?;
        Ok(())
    }
    pub fn usage(&self, channel: u64, model: &str, usage: &Value) -> Result<()> {
        let mut usage = usage.clone();
        usage["_pantheon_model"] = serde_json::json!(model);
        self.db.lock().unwrap().execute(
            "UPDATE settings SET usage=?2 WHERE channel=?1",
            params![channel.to_string(), usage.to_string()],
        )?;
        Ok(())
    }
    pub fn stats(&self, channel: u64) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let pending: i64 = db.query_row(
            "SELECT count(*) FROM inbox WHERE channel=?1 AND state='queued'",
            [channel.to_string()],
            |r| r.get(0),
        )?;
        let out: i64 = db.query_row(
            "SELECT count(*) FROM outbox WHERE channel=?1 AND state='queued'",
            [channel.to_string()],
            |r| r.get(0),
        )?;
        let usage: String = db
            .query_row(
                "SELECT usage FROM settings WHERE channel=?1",
                [channel.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or("{}".into());
        let shells: i64 = db.query_row(
            "SELECT count(*) FROM shell_runs WHERE channel=?1 AND state='running'",
            [channel.to_string()],
            |r| r.get(0),
        )?;
        let worker_queue: i64 = db.query_row(
            "SELECT count(*) FROM agent_inbox WHERE channel=?1 AND state='queued'",
            [channel.to_string()],
            |r| r.get(0),
        )?;
        let mut stmt = db.prepare(
            "SELECT name,phase,owner FROM ui_agents WHERE channel=?1 AND active=1 ORDER BY rowid",
        )?;
        let agents=stmt.query_map([channel.to_string()],|r|Ok(serde_json::json!({"name":r.get::<_,String>(0)?,"phase":r.get::<_,String>(1)?,"id":r.get::<_,String>(2)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        Ok(
            serde_json::json!({"queued_prompts":pending,"pending_delivery":out,"running_shells":shells,"queued_worker_messages":worker_queue,"active_agents":agents,"last_request_usage":serde_json::from_str::<Value>(&usage)?}),
        )
    }
    pub fn add_job(&self, job: &Job) -> Result<()> {
        ensure!(
            job.interval.is_none_or(|n| n >= 5),
            "minimum interval is 5 seconds"
        );
        self.db.lock().unwrap().execute("INSERT INTO jobs(id,channel,user,kind,payload,due,interval) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![job.id,job.channel.to_string(),job.user.to_string(),job.kind,job.payload.to_string(),job.due,job.interval])?;
        Ok(())
    }
    pub fn jobs(&self, channel: Option<u64>, due_only: bool) -> Result<Vec<Job>> {
        let db = self.db.lock().unwrap();
        let mut s=db.prepare("SELECT id,channel,user,kind,payload,due,interval FROM jobs WHERE state='active' AND (?1 IS NULL OR channel=?1) AND (?2=0 OR due<=?3) ORDER BY due")?;
        let rows = s.query_map(
            params![channel.map(|c| c.to_string()), due_only, now()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                ))
            },
        )?;
        rows.map(|r| {
            let (id, c, u, k, p, d, i) = r?;
            Ok(Job {
                id,
                channel: c.parse()?,
                user: u.parse()?,
                kind: k,
                payload: serde_json::from_str(&p)?,
                due: d,
                interval: i,
            })
        })
        .collect()
    }
    pub fn cancel_job(&self, channel: u64, id: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().execute(
            "UPDATE jobs SET state='cancelled' WHERE id=?1 AND channel=?2 AND state='active'",
            params![id, channel.to_string()],
        )? == 1)
    }
    pub fn fire_wakeup(&self, job: &Job, text: &str) -> Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let id = format!("wake:{}:{}", job.id, job.due);
        if !job_active(&tx, job)? {
            return Ok(false);
        }
        crate::ui::copy_context(&tx, &job.id, &id)?;
        deliver_event(
            &tx,
            &id,
            job.payload["_owner"]
                .as_str()
                .unwrap_or(&format!("channel:{}", job.channel)),
            job.channel,
            job.user,
            text,
        )?;
        advance(&tx, job)?;
        tx.commit()?;
        Ok(true)
    }
    pub fn monitor_result(&self, job: &Job, value: &str) -> Result<bool> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        if !job_active(&tx, job)? {
            return Ok(false);
        }
        let previous: Option<String> =
            tx.query_row("SELECT last FROM jobs WHERE id=?1", [&job.id], |r| r.get(0))?;
        let changed = previous.as_deref() != Some(value);
        if changed {
            if !job_active(&tx, job)? {
                return Ok(false);
            }
            crate::ui::copy_context(&tx, &job.id, &format!("monitor:{}:{}", job.id, job.due))?;
            deliver_event(
                &tx,
                &format!("monitor:{}:{}", job.id, job.due),
                job.payload["_owner"]
                    .as_str()
                    .unwrap_or(&format!("channel:{}", job.channel)),
                job.channel,
                job.user,
                &format!(
                    "[monitor {}] {}\n{}",
                    job.id,
                    job.payload["prompt"]
                        .as_str()
                        .unwrap_or("Monitor output changed"),
                    value
                ),
            )?;
        }
        tx.execute(
            "UPDATE jobs SET last=?2 WHERE id=?1",
            params![job.id, value],
        )?;
        advance(&tx, job)?;
        tx.commit()?;
        Ok(changed)
    }
    pub fn add_task(
        &self,
        id: &str,
        batch: &str,
        channel: u64,
        user: u64,
        task: &str,
    ) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO tasks(id,batch,channel,user,task,state) VALUES(?1,?2,?3,?4,?5,'running')",
            params![id, batch, channel.to_string(), user.to_string(), task],
        )?;
        Ok(())
    }
    pub fn finish_task(&self, id: &str, report: &str) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        finish_task_transaction(&tx, id, report)?;
        tx.commit()?;
        Ok(())
    }
    pub fn tasks(&self, channel: u64) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let mut s = db.prepare(
            "SELECT t.id,CASE WHEN u.active=1 THEN 'working' WHEN EXISTS(SELECT 1 FROM shell_runs r WHERE r.owner=t.id AND r.state='running') THEN 'background shell' ELSE t.state END,t.task,COALESCE(u.name,'Worker'),COALESCE(a.model,'Unknown'),COALESCE(a.reasoning,'Unknown') FROM tasks t LEFT JOIN ui_agents u ON u.owner=t.id LEFT JOIN agent_settings a ON a.id=t.id WHERE t.channel=?1 ORDER BY t.rowid DESC LIMIT 100",
        )?;
        Ok(Value::Array(s.query_map([channel.to_string()],|r|Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"state":r.get::<_,String>(1)?,"task":r.get::<_,String>(2)?,"name":r.get::<_,String>(3)?,"model":r.get::<_,String>(4)?,"reasoning":r.get::<_,String>(5)?})))?.collect::<std::result::Result<_,_>>()?))
    }
    pub fn tool_start(&self, id: &str, channel: u64, name: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO tool_runs(id,channel,name,state) VALUES(?1,?2,?3,'running')",
            params![id, channel.to_string(), name],
        )?;
        Ok(())
    }
    pub fn tool_done(&self, id: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("UPDATE tool_runs SET state='done' WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn register_agent(&self, id: &str, model: &str, reasoning: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO agent_settings(id,model,reasoning) VALUES(?1,?2,?3)",
            params![id, model, reasoning],
        )?;
        Ok(())
    }
    pub fn agent(&self, id: &str) -> Result<Option<AgentRecord>> {
        Ok(self.db.lock().unwrap().query_row("SELECT t.channel,t.user,t.task,COALESCE(t.report,''),a.model,a.reasoning FROM tasks t JOIN agent_settings a ON a.id=t.id WHERE t.id=?1",[id],|r|Ok(AgentRecord{channel:r.get::<_,String>(0)?.parse().unwrap_or(0),user:r.get::<_,String>(1)?.parse().unwrap_or(0),task:r.get(2)?,report:r.get(3)?,model:r.get(4)?,reasoning:r.get(5)?})).optional()?)
    }
    pub fn admit_event(&self, input: &Input, owner: &str) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        deliver_event(
            &tx,
            &input.id,
            owner,
            input.channel,
            input.user,
            &input.text,
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn agent_events(&self, owner: &str) -> Result<Vec<Input>> {
        let db = self.db.lock().unwrap();
        let mut stmt=db.prepare("SELECT id,channel,user,text FROM agent_inbox WHERE owner=?1 AND state='queued' ORDER BY created,rowid")?;
        Ok(stmt
            .query_map([owner], |r| {
                Ok(Input {
                    id: r.get(0)?,
                    channel: r.get::<_, String>(1)?.parse().unwrap_or(0),
                    user: r.get::<_, String>(2)?.parse().unwrap_or(0),
                    text: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn agent_event_done(&self, id: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("UPDATE agent_inbox SET state='done' WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn pending_agents(&self) -> Result<Vec<String>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare("SELECT DISTINCT owner FROM agent_inbox WHERE state='queued'")?;
        Ok(stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn agent_run_start(&self, id: &str, owner: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO agent_runs(id,owner,state) VALUES(?1,?2,'running')",
            params![id, owner],
        )?;
        Ok(())
    }
    pub fn finish_agent(&self, id: &str, report: &str, delivery_id: &str) -> Result<()> {
        self.finish_agent_turn(id, report, delivery_id, true)
    }
    pub fn finish_agent_turn(
        &self,
        id: &str,
        report: &str,
        delivery_id: &str,
        successful: bool,
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let finished: bool = tx
            .query_row(
                "SELECT state!='running' FROM agent_runs WHERE id=?1",
                [delivery_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if finished {
            return Ok(());
        }
        let (state, channel, user): (String, String, String) = tx.query_row(
            "SELECT state,channel,user FROM tasks WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM shell_runs WHERE owner=?1 AND state='running') OR EXISTS(SELECT 1 FROM agent_inbox WHERE owner=?1 AND state='queued')",[id],|r|r.get(0))?;
        if successful && pending {
            // A model turn ending does not complete an agent with detached work.
            // Persist its private continuation, release its permit, and let its
            // durable inbox resume it. The coordinator receives no early report.
            tx.execute(
                "UPDATE tasks SET report=?2 WHERE id=?1",
                params![id, report],
            )?;
            tx.execute(
                "UPDATE ui_agents SET active=0,phase='Waiting for background work' WHERE owner=?1",
                [id],
            )?;
            tx.execute(
                "UPDATE agent_runs SET state='done' WHERE id=?1",
                [delivery_id],
            )?;
            tx.commit()?;
            return Ok(());
        }
        if state == "running" {
            finish_task_transaction(&tx, id, report)?;
        } else {
            tx.execute(
                "UPDATE tasks SET report=?2 WHERE id=?1",
                params![id, report],
            )?;
            crate::ui::copy_context(&tx, id, delivery_id)?;
            crate::ui::worker_report(&tx, id, delivery_id, channel.parse()?)?;
            deliver_event(
                &tx,
                delivery_id,
                &format!("channel:{channel}"),
                channel.parse()?,
                user.parse()?,
                &format!("[{id}] {report}"),
            )?;
        }
        tx.execute(
            "UPDATE agent_runs SET state='done' WHERE id=?1",
            [delivery_id],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn shell_start(
        &self,
        id: &str,
        owner: &str,
        channel: u64,
        user: u64,
        command: &str,
    ) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO shell_runs(id,owner,channel,user,command) VALUES(?1,?2,?3,?4,?5)",
            params![id, owner, channel.to_string(), user.to_string(), command],
        )?;
        Ok(())
    }
    pub fn shell_detach(&self, id: &str) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute("UPDATE shell_runs SET background=1 WHERE id=?1", [id])?;
        shell_delivery(&tx, id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn shell_finish(&self, id: &str, output: &str, cancelled: bool) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE shell_runs SET state=?2,output=?3 WHERE id=?1",
            params![id, if cancelled { "cancelled" } else { "done" }, output],
        )?;
        shell_delivery(&tx, id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn recover(&self) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE ui_agents SET active=0,phase='Interrupted' WHERE active=1",
            [],
        )?;
        tx.execute(
            "UPDATE ui_events SET status='error' WHERE status='running'",
            [],
        )?;
        let affected = {
            let mut s =
                tx.prepare("SELECT DISTINCT channel,user FROM inbox WHERE state='running'")?;
            s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (c, u) in affected {
            let id = format!("recovery:{}:{c}", uuid::Uuid::new_v4());
            let hash = Sha256::digest(id.as_bytes());
            let reply_to: Option<String> = tx
                .query_row(
                    "SELECT reply_to FROM ui_contexts WHERE source=?1",
                    [format!("channel:{c}")],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            tx.execute("INSERT INTO outbox(id,channel,user,text,nonce,reply_to) VALUES(?1,?2,?3,?4,?5,?6)",params![id,c,u,"Pantheon restarted during a turn. That turn was interrupted; completed tool effects may remain. Send a message to continue after inspecting the saved history.",u64::from_le_bytes(hash[..8].try_into().unwrap()).to_string(),reply_to])?;
        }
        tx.execute(
            "UPDATE inbox SET state='interrupted' WHERE state='running'",
            [],
        )?;
        tx.execute(
            "UPDATE tool_runs SET state='unknown' WHERE state='running'",
            [],
        )?;
        let shells = {
            let mut stmt = tx.prepare("SELECT id FROM shell_runs WHERE state='running'")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for id in shells {
            tx.execute("UPDATE shell_runs SET state='done',background=1,output='Interrupted by harness restart. Inspect command effects before retrying; the command was not replayed.' WHERE id=?1",[&id])?;
            shell_delivery(&tx, &id)?;
        }
        let active_agents = {
            let mut stmt=tx.prepare("SELECT r.id,r.owner,t.channel,t.user,t.state FROM agent_runs r JOIN tasks t ON t.id=r.owner WHERE r.state='running'")?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (run, owner, channel, user, initial_state) in active_agents {
            if initial_state != "running" {
                let report = "A resumed worker turn was interrupted by harness restart. Inspect its saved trace and tool effects before retrying; its provider conversation was not replayed.";
                tx.execute(
                    "UPDATE tasks SET report=?2 WHERE id=?1",
                    params![owner, report],
                )?;
                crate::ui::copy_context(&tx, &owner, &format!("recovery:{run}"))?;
                deliver_event(
                    &tx,
                    &format!("recovery:{run}"),
                    &format!("channel:{channel}"),
                    channel.parse()?,
                    user.parse()?,
                    &format!("[{owner}] {report}"),
                )?;
            }
            tx.execute(
                "UPDATE agent_runs SET state='interrupted' WHERE id=?1",
                [run],
            )?;
        }
        let unfinished = {
            let mut s = tx.prepare("SELECT id FROM tasks WHERE state='running'")?;
            s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        tx.commit()?;
        drop(db);
        for id in unfinished {
            self.finish_task(
                &id,
                "Interrupted by harness restart. Inspect tool effects before retrying.",
            )?;
        }
        Ok(())
    }
}
fn finish_task_transaction(tx: &rusqlite::Transaction<'_>, id: &str, report: &str) -> Result<()> {
    let changed = tx.execute(
        "UPDATE tasks SET state='done',report=?2 WHERE id=?1 AND state='running'",
        params![id, report],
    )?;
    if changed == 0 {
        return Ok(());
    }
    let (channel, user): (String, String) =
        tx.query_row("SELECT channel,user FROM tasks WHERE id=?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let delivery = format!("report:{id}:initial");
    crate::ui::copy_context(tx, id, &delivery)?;
    crate::ui::worker_report(tx, id, &delivery, channel.parse()?)?;
    deliver_event(
        tx,
        &delivery,
        &format!("channel:{channel}"),
        channel.parse()?,
        user.parse()?,
        &format!("[{id}] {report}"),
    )?;

    Ok(())
}

fn job_active(tx: &rusqlite::Transaction<'_>, job: &Job) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT state='active' AND due=?2 FROM jobs WHERE id=?1",
        params![job.id, job.due],
        |r| r.get(0),
    )?)
}
fn deliver_event(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    owner: &str,
    channel: u64,
    user: u64,
    text: &str,
) -> Result<()> {
    crate::ui::copy_context(tx, owner, id)?;
    if owner == format!("channel:{channel}") {
        tx.execute(
            "INSERT OR IGNORE INTO inbox(id,channel,user,text,created) VALUES(?1,?2,?3,?4,?5)",
            params![id, channel.to_string(), user.to_string(), text, now()],
        )?;
    } else {
        tx.execute("INSERT OR IGNORE INTO agent_inbox(id,owner,channel,user,text,created) VALUES(?1,?2,?3,?4,?5,?6)",params![id,owner,channel.to_string(),user.to_string(),text,now()])?;
    }
    Ok(())
}
fn shell_delivery(tx: &rusqlite::Transaction<'_>, id: &str) -> Result<()> {
    let result:Option<(String,String,String,String)>=tx.query_row("SELECT owner,channel,user,output FROM shell_runs WHERE id=?1 AND state='done' AND background=1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    if let Some((owner, channel, user, output)) = result {
        crate::ui::copy_context(tx, id, &format!("shell:{id}"))?;
        deliver_event(
            tx,
            &format!("shell:{id}"),
            &owner,
            channel.parse()?,
            user.parse()?,
            &format!("[shell {id}] {output}"),
        )?;
    }
    Ok(())
}

fn advance(tx: &rusqlite::Transaction<'_>, job: &Job) -> Result<()> {
    // Collapse missed intervals into one wake rather than flooding the inbox after downtime.
    if let Some(interval) = job.interval {
        tx.execute(
            "UPDATE jobs SET due=?2 WHERE id=?1",
            params![job.id, now() + interval],
        )?;
    } else {
        tx.execute("UPDATE jobs SET state='fired' WHERE id=?1", [&job.id])?;
    }
    Ok(())
}
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumed_agent_interruption_is_reported_once_after_consuming_its_event() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("db");
        let store = Store::open(&path).unwrap();
        store.add_task("child", "batch", 1, 2, "work").unwrap();
        store.finish_task("child", "initial report").unwrap();
        store.agent_run_start("resumed", "child").unwrap();
        store
            .admit_event(
                &Input {
                    id: "event".into(),
                    channel: 1,
                    user: 2,
                    text: "completion".into(),
                },
                "child",
            )
            .unwrap();
        store.agent_event_done("event").unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        store.recover().unwrap();
        store.recover().unwrap();
        let reports = store.queued(1).unwrap();
        assert_eq!(reports.len(), 2);
        assert!(reports[1].text.contains("resumed worker turn"));
        assert!(store.agent_events("child").unwrap().is_empty());
    }
    #[test]
    fn owned_schedules_route_to_child_and_honor_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("db")).unwrap();
        let job = Job {
            id: "wake".into(),
            channel: 1,
            user: 2,
            kind: "wakeup".into(),
            payload: serde_json::json!({"_owner":"child","prompt":"check"}),
            due: now(),
            interval: None,
        };
        store.add_job(&job).unwrap();
        store.fire_wakeup(&job, "wake child").unwrap();
        store.fire_wakeup(&job, "duplicate").unwrap();
        assert!(store.queued(1).unwrap().is_empty());
        assert_eq!(store.agent_events("child").unwrap().len(), 1);
        assert_eq!(store.agent_events("child").unwrap()[0].text, "wake child");
        let monitor = Job {
            id: "monitor".into(),
            kind: "monitor".into(),
            interval: Some(5),
            ..job
        };
        store.add_job(&monitor).unwrap();
        assert!(store.monitor_result(&monitor, "changed").unwrap());
        store.cancel_job(1, "monitor").unwrap();
        assert!(!store.monitor_result(&monitor, "later").unwrap());
        assert_eq!(store.agent_events("child").unwrap().len(), 2);
    }
    #[test]
    fn shell_completion_is_once_in_both_detach_race_orders() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("db")).unwrap();
        for id in ["first", "second", "foreground", "cancelled"] {
            store.shell_start(id, "child", 1, 2, "test").unwrap();
        }
        store.shell_detach("first").unwrap();
        store.shell_finish("first", "first result", false).unwrap();
        store.shell_detach("first").unwrap();
        store
            .shell_finish("second", "second result", false)
            .unwrap();
        store.shell_detach("second").unwrap();
        store
            .shell_finish("foreground", "inline result", false)
            .unwrap();
        store.shell_detach("cancelled").unwrap();
        store.shell_finish("cancelled", "stopped", true).unwrap();
        let events = store.agent_events("child").unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].text.contains("first result"));
        assert!(events[1].text.contains("second result"));
        assert!(store.queued(1).unwrap().is_empty());
    }
    #[test]
    fn interrupted_shell_is_reported_after_restart_and_never_replayed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("db");
        let store = Store::open(&path).unwrap();
        store
            .shell_start("command", "child", 1, 2, "irreversible command")
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        store.recover().unwrap();
        store.recover().unwrap();
        let events = store.agent_events("child").unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].text.contains("not replayed"));
    }
    #[test]
    fn admission_and_delivery_survive_restart() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("db");
        {
            let s = Store::open(&p).unwrap();
            let i = Input {
                id: "1".into(),
                channel: 1,
                user: 2,
                text: "go".into(),
            };
            assert!(s.admit(&i).unwrap());
            assert!(!s.admit(&i).unwrap());
            s.enqueue("o", 1, Some(2), "reply").unwrap();
        }
        let s = Store::open(&p).unwrap();
        assert_eq!(s.queued(1).unwrap().len(), 1);
        let o = s.next_outbound().unwrap().unwrap();
        s.sent(&o.id, "receipt").unwrap();
        assert!(s.next_outbound().unwrap().is_none());
    }
    #[test]
    fn each_worker_report_is_immediate_and_once() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("db")).unwrap();
        s.add_task("a", "b", 1, 2, "task").unwrap();
        s.add_task("c", "b", 1, 2, "task").unwrap();
        s.finish_task("a", "A").unwrap();
        let first = s.queued(1).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, "report:a:initial");
        assert_eq!(first[0].text, "[a] A");
        s.finish_task("c", "C").unwrap();
        s.finish_task("c", "C").unwrap();
        let q = s.queued(1).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].text, "[a] A");
        assert_eq!(q[1].text, "[c] C");
        drop(s);
        let s = Store::open(&d.path().join("db")).unwrap();
        s.finish_task("a", "duplicate").unwrap();
        assert_eq!(s.queued(1).unwrap().len(), 2);
    }
    #[test]
    fn recovery_never_reexecutes_interrupted_tool_turn() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("db")).unwrap();
        s.admit(&Input {
            id: "a".into(),
            channel: 1,
            user: 2,
            text: "side effect".into(),
        })
        .unwrap();
        s.input_state("a", "running").unwrap();
        s.recover().unwrap();
        assert!(s.queued(1).unwrap().is_empty());
        assert!(s.next_outbound().unwrap().is_some());
    }
    #[test]
    fn tool_row_revision_survives_send_ack_race() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("db")).unwrap();
        s.enqueue("tool", 1, None, "running").unwrap();
        let running = s.next_outbound().unwrap().unwrap();
        s.enqueue("tool", 1, None, "done").unwrap();
        s.delivered(&running, "123").unwrap();
        let update = s.next_outbound().unwrap().unwrap();
        assert_eq!(update.text, "done");
        assert_eq!(update.receipt.as_deref(), Some("123"));
        s.delivered(&update, "123").unwrap();
        assert!(s.next_outbound().unwrap().is_none());
        s.enqueue("tool", 1, None, "done").unwrap();
        assert!(s.next_outbound().unwrap().is_none());
    }
    #[test]
    fn pending_channel_cannot_block_another_channel() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("db")).unwrap();
        s.enqueue("a", 1, None, "first").unwrap();
        s.enqueue("b", 1, None, "second").unwrap();
        s.enqueue("c", 2, None, "independent").unwrap();
        assert_eq!(s.next_outbound_excluding(&[1]).unwrap().unwrap().id, "c");
        s.retry_outbound("a").unwrap();
        assert_eq!(s.next_outbound().unwrap().unwrap().id, "c");
    }
    #[test]
    fn final_completion_and_delivery_commit_together_with_steering_barrier() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("db")).unwrap();
        let first = Input {
            id: "first".into(),
            channel: 1,
            user: 2,
            text: "task".into(),
        };
        s.admit(&first).unwrap();
        s.input_state(&first.id, "running").unwrap();
        s.admit(&Input {
            id: "steer".into(),
            channel: 1,
            user: 2,
            text: "change".into(),
        })
        .unwrap();
        assert!(
            !s.complete_turn(
                std::slice::from_ref(&first.id),
                "final",
                1,
                2,
                &["reply".into()]
            )
            .unwrap()
        );
        assert!(s.next_outbound().unwrap().is_none());
        s.input_state("steer", "running").unwrap();
        assert!(
            s.complete_turn(
                &[first.id, "steer".into()],
                "final",
                1,
                2,
                &["reply".into()]
            )
            .unwrap()
        );
        s.recover().unwrap();
        let out = s.next_outbound().unwrap().unwrap();
        assert_eq!(out.id, "final:0");
        s.sent(&out.id, "123").unwrap();
        assert!(s.next_outbound().unwrap().is_none());
    }
    #[test]
    fn accepted_input_can_be_reconciled_without_reexecuting_it() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("db")).unwrap();
        s.admit(&Input {
            id: "gap".into(),
            channel: 1,
            user: 2,
            text: "exact user input".into(),
        })
        .unwrap();
        s.input_state("gap", "running").unwrap();
        s.recover().unwrap();
        assert!(s.queued(1).unwrap().is_empty());
        assert_eq!(s.queued_channels().unwrap(), vec![1]);
        let unlogged = s.unlogged(1).unwrap();
        assert_eq!(unlogged[0].text, "exact user input");
        s.mark_logged("gap").unwrap();
        assert!(s.unlogged(1).unwrap().is_empty());
    }
    #[test]
    fn worker_report_waits_for_owned_shells_and_inbox_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let store = Store::open(&path).unwrap();
        let context = crate::ui::ReplyContext::request("100");
        store
            .add_task("worker", "batch", 1, 2, "say hi after waiting")
            .unwrap();
        store
            .present_agent("worker", 1, &context, "Greeter", "codex/test")
            .unwrap();
        store.agent_run_start("first", "worker").unwrap();
        store.bind_context("shell", &context).unwrap();
        store
            .shell_start("shell", "worker", 1, 2, "sleep then hi")
            .unwrap();
        store.shell_detach("shell").unwrap();
        store
            .finish_agent_turn("worker", "Hi too early", "first", true)
            .unwrap();
        assert!(store.queued(1).unwrap().is_empty());
        assert!(store.next_outbound().unwrap().is_none());
        assert_eq!(
            store
                .db
                .lock()
                .unwrap()
                .query_row("SELECT state FROM tasks WHERE id='worker'", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "running"
        );
        drop(store);
        let store = Store::open(&path).unwrap();
        store.shell_finish("shell", "Hi", false).unwrap();
        assert_eq!(store.agent_events("worker").unwrap().len(), 1);
        store.agent_run_start("second", "worker").unwrap();
        store
            .finish_agent_turn("worker", "Hi before inbox digestion", "second", true)
            .unwrap();
        assert!(store.queued(1).unwrap().is_empty());
        for event in store.agent_events("worker").unwrap() {
            store.agent_event_done(&event.id).unwrap();
        }
        // A future monitor is not current work and must not hold the report forever.
        store
            .add_job(&Job {
                id: "future".into(),
                channel: 1,
                user: 2,
                kind: "monitor".into(),
                payload: serde_json::json!({"_owner":"worker","command":"true"}),
                due: now() + 600,
                interval: Some(600),
            })
            .unwrap();
        store.agent_run_start("third", "worker").unwrap();
        store
            .finish_agent_turn("worker", "Hi", "third", true)
            .unwrap();
        store
            .finish_agent_turn("worker", "duplicate", "third", true)
            .unwrap();
        let reports = store.queued(1).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].text, "[worker] Hi");
        let out = store.next_outbound().unwrap().unwrap();
        assert_eq!(
            out.text
                .matches("↙ incoming agent message from Greeter")
                .count(),
            1
        );
        assert!(!out.text.contains("shell"));
    }
    #[test]
    fn failed_worker_reports_are_not_held_by_detached_work() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        store.add_task("worker", "batch", 1, 2, "work").unwrap();
        store.agent_run_start("failed", "worker").unwrap();
        store.shell_start("shell", "worker", 1, 2, "work").unwrap();
        store
            .finish_agent_turn("worker", "Task stopped: provider failure", "failed", false)
            .unwrap();
        let reports = store.queued(1).unwrap();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].text.contains("provider failure"));
    }
}
