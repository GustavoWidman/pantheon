//! Durable Discord presentation, separate from the agent's canonical memory.
use crate::store::Store;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ReplyContext {
    pub reply_to: Option<u64>,
    pub activity: String,
}
impl ReplyContext {
    pub fn request(id: &str) -> Self {
        Self {
            reply_to: id.parse::<u64>().ok().filter(|id| *id > 0),
            activity: format!("request:{id}"),
        }
    }
}
pub(crate) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS ui_contexts(source TEXT PRIMARY KEY,reply_to TEXT,activity TEXT NOT NULL,is_prompt INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS ui_sessions(activity TEXT PRIMARY KEY,channel TEXT NOT NULL,reply_to TEXT,settled INTEGER NOT NULL DEFAULT 1);
        CREATE TABLE IF NOT EXISTS ui_events(seq INTEGER PRIMARY KEY AUTOINCREMENT,activity TEXT NOT NULL,event TEXT NOT NULL,label TEXT NOT NULL,status TEXT NOT NULL,started INTEGER NOT NULL,elapsed INTEGER NOT NULL,UNIQUE(activity,event));
        CREATE TABLE IF NOT EXISTS ui_agents(owner TEXT PRIMARY KEY,activity TEXT NOT NULL,channel TEXT NOT NULL,name TEXT NOT NULL,model TEXT NOT NULL,active INTEGER NOT NULL DEFAULT 0,phase TEXT NOT NULL DEFAULT 'Idle');
        CREATE INDEX IF NOT EXISTS ui_events_activity ON ui_events(activity,seq);
        CREATE INDEX IF NOT EXISTS ui_agents_active ON ui_agents(channel,active);")?;
    let exists: bool = db
        .prepare("PRAGMA table_info(outbox)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == "reply_to");
    if !exists {
        db.execute("ALTER TABLE outbox ADD COLUMN reply_to TEXT", [])?;
    }
    Ok(())
}
pub(crate) fn copy_context(db: &Connection, from: &str, to: &str) -> Result<()> {
    db.execute("INSERT OR IGNORE INTO ui_contexts(source,reply_to,activity) SELECT ?2,reply_to,activity FROM ui_contexts WHERE source=?1", params![from,to])?;
    Ok(())
}
impl Store {
    pub fn reply_context(&self, source: &str) -> Result<ReplyContext> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT reply_to,activity FROM ui_contexts WHERE source=?1",
                [source],
                |r| {
                    Ok(ReplyContext {
                        reply_to: r.get::<_, Option<String>>(0)?.and_then(|s| s.parse().ok()),
                        activity: r.get(1)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_else(|| ReplyContext::request(source)))
    }
    pub fn bind_context(&self, source: &str, context: &ReplyContext) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO ui_contexts(source,reply_to,activity) VALUES(?1,?2,?3) ON CONFLICT(source) DO UPDATE SET reply_to=excluded.reply_to,activity=excluded.activity",params![source,context.reply_to.map(|id|id.to_string()),context.activity])?;
        Ok(())
    }
    pub fn is_prompt(&self, source: &str) -> Result<bool> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT is_prompt=1 FROM ui_contexts WHERE source=?1",
                [source],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }
    pub fn agent_label(&self, owner: &str) -> Result<String> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT name FROM ui_agents WHERE owner=?1", [owner], |r| {
                r.get(0)
            })
            .optional()?
            .unwrap_or_else(|| {
                if owner.starts_with("channel:") {
                    "Coordinator".into()
                } else {
                    format!("Worker {}", short_id(owner))
                }
            }))
    }
    pub fn present_agent(
        &self,
        owner: &str,
        channel: u64,
        context: &ReplyContext,
        name: &str,
        model: &str,
    ) -> Result<()> {
        self.bind_context(owner, context)?;
        let db = self.db.lock().unwrap();
        db.execute("INSERT INTO ui_sessions(activity,channel,reply_to) VALUES(?1,?2,?3) ON CONFLICT(activity) DO NOTHING",params![context.activity,channel.to_string(),context.reply_to.map(|id|id.to_string())])?;
        db.execute("INSERT INTO ui_agents(owner,activity,channel,name,model) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(owner) DO UPDATE SET activity=excluded.activity,channel=excluded.channel,name=excluded.name,model=excluded.model",params![owner,context.activity,channel.to_string(),clean(name,48),model])?;
        Ok(())
    }
    pub fn agent_phase(&self, owner: &str, active: bool, phase: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE ui_agents SET active=?2,phase=?3 WHERE owner=?1",
            params![owner, active, phase],
        )?;
        if let Some(activity) = db
            .query_row(
                "SELECT activity FROM ui_agents WHERE owner=?1",
                [owner],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            render_activity(&db, &activity)?;
        }
        Ok(())
    }
    pub fn activity_event(
        &self,
        context: &ReplyContext,
        channel: u64,
        id: &str,
        label: &str,
        status: &str,
        elapsed: Duration,
    ) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR IGNORE INTO ui_sessions(activity,channel,reply_to) VALUES(?1,?2,?3)",
            params![
                context.activity,
                channel.to_string(),
                context.reply_to.map(|id| id.to_string())
            ],
        )?;
        db.execute("INSERT INTO ui_events(activity,event,label,status,started,elapsed) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(activity,event) DO UPDATE SET label=excluded.label,status=excluded.status,elapsed=excluded.elapsed",params![context.activity,id,clean(label,110),status,chrono::Utc::now().timestamp_millis(),elapsed.as_millis().min(i64::MAX as u128) as i64])?;
        render_activity(&db, &context.activity)?;
        Ok(())
    }
    pub fn refresh_activities(&self, channel: u64, settled: bool) -> Result<bool> {
        let db = self.db.lock().unwrap();
        let mut stmt=db.prepare("SELECT s.activity FROM ui_sessions s WHERE channel=?1 AND (s.settled=0 OR s.activity=(SELECT activity FROM ui_contexts WHERE source=?2) OR EXISTS(SELECT 1 FROM ui_agents a WHERE a.activity=s.activity AND active=1) OR EXISTS(SELECT 1 FROM shell_runs r JOIN ui_contexts c ON c.source=r.id WHERE c.activity=s.activity AND r.state='running') OR EXISTS(SELECT 1 FROM agent_inbox i JOIN ui_contexts c ON c.source=i.id WHERE c.activity=s.activity AND i.state='queued'))")?;
        let activities = stmt
            .query_map(
                params![channel.to_string(), format!("channel:{channel}")],
                |r| r.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut busy = !settled;
        for activity in activities {
            db.execute(
                "UPDATE ui_sessions SET settled=?2 WHERE activity=?1",
                params![activity, settled],
            )?;
            busy |= render_activity(&db, &activity)?;
        }
        Ok(busy)
    }
    pub fn enqueue_reply(
        &self,
        id: &str,
        channel: u64,
        user: Option<u64>,
        text: &str,
        reply_to: Option<u64>,
    ) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO outbox(id,channel,user,text,nonce,reply_to) VALUES(?1,?2,?3,?4,?1,?5) ON CONFLICT(id) DO UPDATE SET text=excluded.text,state='queued' WHERE outbox.text!=excluded.text",params![id,channel.to_string(),user.map(|u|u.to_string()),text,reply_to.map(|id|id.to_string())])?;
        Ok(())
    }
}
fn render_activity(db: &Connection, activity: &str) -> Result<bool> {
    let (channel, reply_to, settled): (String, Option<String>, bool) = db.query_row(
        "SELECT channel,reply_to,settled FROM ui_sessions WHERE activity=?1",
        [activity],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let mut agents = db.prepare(
        "SELECT name,phase FROM ui_agents WHERE activity=?1 AND active=1 ORDER BY rowid",
    )?;
    let active = agents
        .query_map([activity], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let shells:i64=db.query_row("SELECT count(*) FROM shell_runs r JOIN ui_contexts c ON c.source=r.id WHERE c.activity=?1 AND r.state='running'",[activity],|r|r.get(0))?;
    let pending:i64=db.query_row("SELECT (SELECT count(*) FROM inbox i JOIN ui_contexts c ON c.source=i.id WHERE c.activity=?1 AND i.state='queued')+(SELECT count(*) FROM agent_inbox i JOIN ui_contexts c ON c.source=i.id WHERE c.activity=?1 AND i.state='queued')",[activity],|r|r.get(0))?;
    let busy = !active.is_empty() || shells > 0 || pending > 0 || !settled;
    let status = if !active.is_empty() {
        "Working"
    } else if shells > 0 {
        "Background work running"
    } else if pending > 0 {
        "Processing incoming reports"
    } else if !settled {
        "Updating context"
    } else {
        "Settled"
    };
    let mut events = db.prepare(
        "SELECT label,status,started,elapsed FROM ui_events WHERE activity=?1 ORDER BY seq",
    )?;
    let now = chrono::Utc::now().timestamp_millis();
    let lines = events
        .query_map([activity], |r| {
            let label: String = r.get(0)?;
            let state: String = r.get(1)?;
            let elapsed = if state == "running" {
                now - r.get::<_, i64>(2)?
            } else {
                r.get(3)?
            };
            Ok(if state == "event" {
                label
            } else {
                format!(
                    "{} {label} · {:.1}s",
                    match state.as_str() {
                        "running" => "◌",
                        "done" => "✓",
                        "background" => "↗",
                        "skipped" => "–",
                        _ => "✗",
                    },
                    elapsed.max(0) as f64 / 1000.0
                )
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let phases = active
        .iter()
        .map(|(name, phase)| format!("{name}: {phase}"))
        .collect::<Vec<_>>()
        .join(" · ");
    let footer = clean(
        &if phases.is_empty() {
            if shells > 0 {
                format!("{shells} background shell jobs running")
            } else if pending > 0 {
                "Processing incoming reports".into()
            } else if !settled {
                "Preparing context for the next turn".into()
            } else {
                "All work settled".into()
            }
        } else {
            phases
        },
        240,
    );
    let lines = if lines.is_empty() {
        vec!["◌ Preparing your request".into()]
    } else {
        lines
    };
    // Fixed row groups keep page boundaries stable when durations/statuses change.
    let pages = lines.chunks(12).collect::<Vec<_>>();
    for (index, page) in pages.iter().enumerate() {
        let current = index + 1 == pages.len();
        let heading = if current { status } else { "Activity" };
        let footer = if current {
            footer.as_str()
        } else {
            "Activity continues below"
        };
        let text = format!(
            "**Pantheon · {heading}**\n```text\n{}\n```\n{footer}",
            page.join("\n")
        );
        let id = format!("{activity}:activity:{index}");
        db.execute("INSERT INTO outbox(id,channel,text,nonce,reply_to) VALUES(?1,?2,?3,?1,?4) ON CONFLICT(id) DO UPDATE SET text=excluded.text,state='queued' WHERE outbox.text!=excluded.text",params![id,channel,text,reply_to])?;
    }
    Ok(busy)
}
pub fn short_id(id: &str) -> &str {
    &id[..id.char_indices().nth(8).map(|(i, _)| i).unwrap_or(id.len())]
}
pub fn truncate(text: &str, max: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max
        })
        .collect()
}
pub fn clean(text: &str, max: usize) -> String {
    truncate(
        &text
            .chars()
            .map(|c| if c == '`' || c.is_control() { ' ' } else { c })
            .collect::<String>(),
        max,
    )
}

pub fn card(
    title: &str,
    description: &str,
    fields: Vec<(&str, String, bool)>,
    error: bool,
) -> Value {
    let mut budget = 5600usize.saturating_sub(title.encode_utf16().count().min(256));
    let description = truncate(description, 3500.min(budget));
    budget = budget.saturating_sub(description.encode_utf16().count());
    let mut rows = Vec::new();
    for (name, value, inline) in fields.into_iter().take(25) {
        let name = clean(name, 200);
        let value = truncate(
            &value,
            900.min(budget.saturating_sub(name.encode_utf16().count())),
        );
        if value.is_empty() {
            continue;
        }
        budget = budget.saturating_sub(name.encode_utf16().count() + value.encode_utf16().count());
        rows.push(json!({"name":name,"value":value,"inline":inline}));
    }
    json!({"content":"","embeds":[{"title":clean(title,200),"description":description,"color":if error{0xED4245}else{0x7C83FD},"fields":rows,"footer":{"text":"Pantheon • This channel"},"timestamp":chrono::Utc::now().to_rfc3339()}],"allowed_mentions":{"parse":[],"replied_user":false}})
}

/// Context capacity is display metadata, never an inference limit override.
pub fn model_window(config: &crate::config::Config, model: &str) -> Option<(u64, &'static str)> {
    if let Some(tokens) = config
        .agent
        .context_windows
        .get(model)
        .copied()
        .filter(|n| *n > 0)
    {
        return Some((tokens, "Configured model limit"));
    }
    let slug = model.strip_prefix("codex/")?;
    use std::io::Read;
    let file = std::fs::File::open(config.auth.home().ok()?.join("models_cache.json")).ok()?;
    let mut bytes = Vec::new();
    file.take(4_194_305).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 4_194_304 {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let tokens = value["models"]
        .as_array()?
        .iter()
        .find(|m| m["slug"] == slug)?["context_window"]
        .as_u64()
        .filter(|n| *n > 0)?;
    Some((tokens, "Codex model metadata"))
}
fn number(n: u64) -> String {
    let digits = n.to_string();
    let mut result = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(ch);
    }
    result
}
/// Allocate cells by cumulative boundaries, so rounding cannot overfill the grid.
fn squares(parts: &[(u64, &str)], capacity: u64, cells: usize) -> String {
    let mut result = String::from("```\n");
    let mut boundary = 0usize;
    let mut total = 0u128;
    for (count, glyph) in parts {
        total += u128::from(*count);
        let next = ((total.min(u128::from(capacity)) * cells as u128)
            .div_ceil(u128::from(capacity.max(1))) as usize)
            .min(cells);
        for i in boundary..next {
            result.push_str(glyph);
            if (i + 1).is_multiple_of(10) {
                result.push('\n');
            }
        }
        boundary = next;
    }
    for i in boundary..cells {
        result.push('⬛');
        if (i + 1).is_multiple_of(10) {
            result.push('\n');
        }
    }
    result.push_str("```");
    result
}
pub fn context_card(
    config: &crate::config::Config,
    model: &str,
    reasoning: &str,
    memory: &Value,
    usage: &Value,
) -> Value {
    let recorded_model = usage["_pantheon_model"].as_str();
    let cached = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| usage["cache_read_input_tokens"].as_u64())
        .unwrap_or(0);
    let input = usage["input_tokens"].as_u64().unwrap_or(0).saturating_add(
        if usage.get("cache_read_input_tokens").is_some() {
            cached.saturating_add(usage["cache_creation_input_tokens"].as_u64().unwrap_or(0))
        } else {
            0
        },
    );
    let output = usage["output_tokens"].as_u64().unwrap_or(0);
    let total = input.saturating_add(output);
    let fresh = input.saturating_sub(cached);
    let window_model = recorded_model.unwrap_or(model);
    let capacity = model_window(config, window_model);
    let has_usage = usage.get("input_tokens").is_some();
    let mut fields = vec![("Channel model", format!("`{model}` · {reasoning}"), false)];
    let window = match capacity {
        Some((limit, source)) if recorded_model.is_some() && has_usage => format!("{}\n**{} / {} tokens · {:.1}%**\nLast recorded request · {source}{}",squares(&[(cached.min(input),"🟦"),(fresh,"🟪"),(output,"🟧")],limit,100),number(total),number(limit),total as f64 / limit as f64 * 100.0,if total>limit {" · exceeds displayed limit"}else{""}),
        Some((limit, source)) => format!("{}\n**{}-token window** · {source}\n⬜ Usage unavailable · {}",squares(&[],limit,100).replace("⬛","⬜"),number(limit),if has_usage {"Previous usage has no recorded model; occupancy unavailable."}else{"No request recorded yet."}),
        None => "Model capacity unavailable. Set `agent.context_windows` for this model to enable its window grid.".into(),
    };
    fields.push(("Model window", window, false));
    if has_usage {
        fields.push(("Request breakdown",format!("🟦 Cached input  **{}**\n🟪 Fresh input  **{}**\n🟧 Output  **{}**{}\nInput total  **{}**",number(cached.min(input)),number(fresh),number(output),if recorded_model.is_some(){capacity.map(|(limit,_)|format!("\n⬛ Remaining  **{}**",number(limit.saturating_sub(total)))).unwrap_or_default()}else{String::new()},number(input)),true));
        fields.push((
            "Prompt cache",
            if input > 0 {
                format!(
                    "**{:.1}%** of input reused\nCached input occupies context normally.",
                    cached.min(input) as f64 / input as f64 * 100.0
                )
            } else {
                "No input usage recorded".into()
            },
            true,
        ));
        if let Some(last) = recorded_model.filter(|last| *last != model) {
            fields.push((
                "Recorded request model",
                format!("`{last}`\nChannel model has changed since this request."),
                false,
            ));
        }
    }
    let view = memory["view_bytes"].as_u64().unwrap_or(0);
    let budget = memory["view_budget_bytes"].as_u64().unwrap_or(1).max(1);
    fields.push(("Memory view",format!("{}\n🟩 **{:.1} / {:.1} KiB · {:.1}%**\nCompacted view for the next fresh turn · byte budget",squares(&[(view,"🟩")],budget,20),view as f64 / 1024.0,budget as f64 / 1024.0,view as f64 / budget as f64 * 100.0),false));
    fields.push((
        "Durable history",
        format!(
            "{} messages · {} summaries\n{} loaded view lines",
            memory["messages"], memory["summaries"], memory["view_lines"]
        ),
        true,
    ));
    fields.push((
        "Memory state",
        if memory["settled"] == true {
            "Settled".into()
        } else {
            format!("Compacting · {} jobs ready", memory["ready_jobs"])
        },
        true,
    ));
    let mut result = card(
        "Context",
        "Last recorded coordinator request and the memory view for the next turn. Model squares use rounded 1% cells; token counts below come from provider reports.",
        fields,
        false,
    );
    result["embeds"][0]["color"] = json!(0x5865F2);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn field<'a>(card: &'a Value, name: &str) -> &'a str {
        card["embeds"][0]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .unwrap()["value"]
            .as_str()
            .unwrap()
    }
    #[test]
    fn context_grid_accounts_for_anthropic_cache_and_retains_recorded_model() {
        let mut config = crate::config::Config::default();
        config
            .agent
            .context_windows
            .insert("anthropic/old".into(), 1000);
        config
            .agent
            .context_windows
            .insert("openai/new".into(), 9000);
        let usage = json!({"_pantheon_model":"anthropic/old","input_tokens":100,"cache_read_input_tokens":200,"cache_creation_input_tokens":50,"output_tokens":50});
        let memory = json!({"view_bytes":256,"view_budget_bytes":1024,"messages":4,"summaries":2,"view_lines":2,"settled":true});
        let card = context_card(&config, "openai/new", "low", &memory, &usage);
        let window = field(&card, "Model window");
        assert!(window.contains("400 / 1,000 tokens · 40.0%"));
        assert_eq!(window.matches("🟦").count(), 20);
        assert_eq!(window.matches("🟪").count(), 15);
        assert_eq!(window.matches("🟧").count(), 5);
        assert_eq!(window.matches("⬛").count(), 60);
        assert!(field(&card, "Request breakdown").contains("Remaining  **600**"));
        assert!(field(&card, "Recorded request model").contains("anthropic/old"));
        assert!(field(&card, "Memory view").contains("25.0%"));
    }
    #[test]
    fn context_metadata_fallback_and_legacy_usage_do_not_invent_occupancy() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.auth.codex_home = Some(dir.path().into());
        std::fs::write(
            dir.path().join("models_cache.json"),
            r#"{"models":[{"slug":"test","context_window":2000,"max_context_window":9000}]}"#,
        )
        .unwrap();
        assert_eq!(
            model_window(&config, "codex/test"),
            Some((2000, "Codex model metadata"))
        );
        assert_eq!(model_window(&config, "openai/test"), None);
        let card = context_card(
            &config,
            "codex/test",
            "low",
            &json!({}),
            &json!({"input_tokens":100}),
        );
        let window = field(&card, "Model window");
        assert!(window.contains("occupancy unavailable"));
        assert_eq!(
            window.split("```").nth(1).unwrap().matches("⬜").count(),
            100
        );
        config
            .agent
            .context_windows
            .insert("codex/test".into(), 3000);
        assert_eq!(
            model_window(&config, "codex/test"),
            Some((3000, "Configured model limit"))
        );
        config.agent.context_windows.clear();
        std::fs::write(dir.path().join("models_cache.json"), "invalid").unwrap();
        assert_eq!(model_window(&config, "codex/test"), None);
    }
    #[test]
    fn context_grid_clamps_overflow_without_losing_reported_counts() {
        let mut config = crate::config::Config::default();
        config
            .agent
            .context_windows
            .insert("openai/test".into(), 100);
        let card = context_card(
            &config,
            "openai/test",
            "low",
            &json!({}),
            &json!({"_pantheon_model":"openai/test","input_tokens":200,"output_tokens":10}),
        );
        let grid = field(&card, "Model window");
        assert_eq!(grid.matches("🟪").count(), 100);
        assert_eq!(grid.matches("⬛").count(), 0);
        assert!(grid.contains("210 / 100"));
        assert!(grid.contains("exceeds displayed limit"));
        assert!(field(&card, "Request breakdown").contains("Remaining  **0**"));
    }
    #[test]
    fn activity_accumulates_updates_and_preserves_reply_and_receipt_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sqlite");
        let context = ReplyContext::request("123456789012345678");
        let store = Store::open(&path).unwrap();
        store
            .present_agent("scout", 1, &context, "Docs Scout", "codex/test")
            .unwrap();
        store.agent_phase("scout", true, "Thinking").unwrap();
        store
            .activity_event(
                &context,
                1,
                "call",
                "Docs Scout / web_search",
                "running",
                Duration::ZERO,
            )
            .unwrap();
        let first = store.next_outbound().unwrap().unwrap();
        assert_eq!(first.reply_to, context.reply_to);
        assert_eq!(first.user, None);
        assert!(first.text.contains("```text\n"));
        store.delivered(&first, "discord-receipt").unwrap();
        store
            .activity_event(
                &context,
                1,
                "call",
                "Docs Scout / web_search",
                "done",
                Duration::from_millis(1500),
            )
            .unwrap();
        store
            .activity_event(
                &context,
                1,
                "report",
                "-> incoming agent message from Docs Scout [scout]",
                "event",
                Duration::ZERO,
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let revised = store.next_outbound().unwrap().unwrap();
        assert_eq!(first.id, revised.id);
        assert_eq!(revised.receipt.as_deref(), Some("discord-receipt"));
        assert!(revised.text.contains("✓ Docs Scout / web_search · 1.5s"));
        assert!(
            revised
                .text
                .contains("-> incoming agent message from Docs Scout")
        );
        assert!(!revised.text.contains("◌ Docs Scout / web_search"));
    }
    #[test]
    fn unicode_fenced_pages_and_embeds_stay_within_discord_limits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        let context = ReplyContext::request("99");
        for i in 0..30 {
            store
                .activity_event(
                    &context,
                    1,
                    &i.to_string(),
                    &"🦀".repeat(200),
                    "running",
                    Duration::ZERO,
                )
                .unwrap();
        }
        let mut pages = 0;
        while let Some(out) = store.next_outbound().unwrap() {
            assert!(out.text.encode_utf16().count() <= 2000);
            assert_eq!(out.text.matches("```").count(), 2);
            store.sent(&out.id, "receipt").unwrap();
            pages += 1;
        }
        assert_eq!(pages, 3);
        let fields = (0..25)
            .map(|_| ("Field", "🦀".repeat(2000), true))
            .collect();
        let payload = card("Context", &"🦀".repeat(3000), fields, false);
        let embed = &payload["embeds"][0];
        let total = embed["title"].as_str().unwrap().encode_utf16().count()
            + embed["description"]
                .as_str()
                .unwrap()
                .encode_utf16()
                .count()
            + embed["footer"]["text"]
                .as_str()
                .unwrap()
                .encode_utf16()
                .count()
            + embed["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    f["name"].as_str().unwrap().encode_utf16().count()
                        + f["value"].as_str().unwrap().encode_utf16().count()
                })
                .sum::<usize>();
        assert!(total <= 6000);
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
    }
    #[test]
    fn late_worker_and_schedule_delivery_keep_the_original_request_anchor() {
        use crate::store::{Input, Job};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        let original = ReplyContext::request("100");
        let newer = ReplyContext::request("200");
        store.add_task("scout", "batch", 1, 2, "research").unwrap();
        store.register_agent("scout", "codex/test", "low").unwrap();
        store.bind_context("scout", &original).unwrap();
        store.bind_context("channel:1", &newer).unwrap();
        store.finish_task("scout", "source found").unwrap();
        assert_eq!(
            store.reply_context("batch:batch").unwrap().reply_to,
            Some(100)
        );
        let job = Job {
            id: "timer".into(),
            channel: 1,
            user: 2,
            kind: "wakeup".into(),
            payload: json!({"_owner":"channel:1"}),
            due: crate::store::now(),
            interval: None,
        };
        store.bind_context("timer", &original).unwrap();
        store.add_job(&job).unwrap();
        store.fire_wakeup(&job, "wake").unwrap();
        let id = format!("wake:timer:{}", job.due);
        assert_eq!(store.reply_context(&id).unwrap().reply_to, Some(100));
        store
            .admit(&Input {
                id: "200".into(),
                channel: 1,
                user: 2,
                text: "new request".into(),
            })
            .unwrap();
        assert!(
            !store
                .complete_turn(
                    &["batch:batch".into()],
                    "old-final",
                    1,
                    2,
                    &["old report".into()]
                )
                .unwrap()
        );
    }

    #[test]
    fn queued_worker_events_keep_status_busy_and_completion_quiet() {
        use crate::store::Input;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        let context = ReplyContext::request("99");
        store
            .admit(&Input {
                id: "99".into(),
                channel: 1,
                user: 2,
                text: "work".into(),
            })
            .unwrap();
        store.input_state("99", "running").unwrap();
        store
            .present_agent("worker", 1, &context, "Shell Runner", "codex/test")
            .unwrap();
        store.bind_context("completion", &context).unwrap();
        store.db.lock().unwrap().execute("INSERT INTO agent_inbox(id,owner,channel,user,text,created) VALUES('completion','worker','1','2','shell complete',0)",[]).unwrap();
        assert!(store.refresh_activities(1, true).unwrap());
        let activity = store.next_outbound().unwrap().unwrap();
        assert!(activity.text.contains("Processing incoming reports"));
        store.sent(&activity.id, "receipt").unwrap();
        assert!(
            store
                .complete_turn(
                    &["99".into()],
                    "ack",
                    1,
                    2,
                    &["Waiting for worker report".into()]
                )
                .unwrap()
        );
        let ack = store.next_outbound().unwrap().unwrap();
        assert_eq!(ack.reply_to, Some(99));
        assert_eq!(ack.user, None);
    }

    #[test]
    fn completion_replies_pass_throttled_activity_and_updates_preserve_retry_backoff() {
        use crate::store::Input;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        let context = ReplyContext::request("99");
        store
            .admit(&Input {
                id: "99".into(),
                channel: 1,
                user: 2,
                text: "work".into(),
            })
            .unwrap();
        store.input_state("99", "running").unwrap();
        store
            .activity_event(
                &context,
                1,
                "call",
                "Coordinator / spawn",
                "running",
                Duration::ZERO,
            )
            .unwrap();
        let activity = store.next_outbound().unwrap().unwrap();
        store.retry_outbound(&activity.id).unwrap();
        store
            .activity_event(
                &context,
                1,
                "call",
                "Coordinator / spawn",
                "done",
                Duration::from_secs(1),
            )
            .unwrap();
        assert!(
            store
                .complete_turn(&["99".into()], "final", 1, 2, &["Finished".into()])
                .unwrap()
        );
        let final_reply = store
            .next_outbound_with_ui_budget(&[], &[1])
            .unwrap()
            .unwrap();
        assert_eq!(final_reply.id, "final:0");
        assert_eq!(final_reply.reply_to, Some(99));
        store.sent(&final_reply.id, "reply").unwrap();
        assert!(store.next_outbound().unwrap().is_none());
    }
}
