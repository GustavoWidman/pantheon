//! Durable diagnostics only: no conversation retention or provider cache-policy changes.
use crate::store::Store;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input: Option<u64>,
    pub cached: Option<u64>,
    pub written: Option<u64>,
    pub output: Option<u64>,
}
impl Usage {
    pub fn parse(v: &Value) -> Self {
        let cached = v["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| v["cache_read_input_tokens"].as_u64());
        let written = v["input_tokens_details"]["cache_write_tokens"]
            .as_u64()
            .or_else(|| v["cache_creation_input_tokens"].as_u64());
        let input = v["input_tokens"].as_u64().map(|n| {
            if v.get("cache_read_input_tokens").is_some()
                || v.get("cache_creation_input_tokens").is_some()
            {
                n.saturating_add(cached.unwrap_or(0))
                    .saturating_add(written.unwrap_or(0))
            } else {
                n
            }
        });
        Self {
            input,
            cached: cached.map(|n| input.map_or(n, |i| n.min(i))),
            written,
            output: v["output_tokens"].as_u64(),
        }
    }
    fn ratio(&self) -> String {
        match (self.input, self.cached) {
            (Some(i), Some(c)) if i > 0 => format!(
                "{:.1}% · {} / {} input tokens",
                c as f64 / i as f64 * 100.,
                crate::ui::number(c),
                crate::ui::number(i)
            ),
            _ => "Cache reuse not reported".into(),
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Snapshot {
    model: String,
    reasoning: String,
    fixed: String,
    lines: Vec<(usize, String)>,
    #[serde(default)]
    closing_bytes: usize,
}
pub(crate) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS cache_views(channel TEXT PRIMARY KEY,snapshot TEXT NOT NULL,trace TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS cache_usage(seq INTEGER PRIMARY KEY AUTOINCREMENT,channel TEXT NOT NULL,model TEXT NOT NULL,phase TEXT NOT NULL,usage TEXT NOT NULL,created INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS cache_usage_channel ON cache_usage(channel,seq);")?;
    Ok(())
}
impl Store {
    pub fn cache_turn(
        &self,
        channel: u64,
        model: &str,
        reasoning: &str,
        system: &str,
        defs: &[Value],
        view: &str,
    ) -> Result<()> {
        let mut hash = Sha256::new();
        hash.update(system.as_bytes());
        hash.update([0]);
        hash.update(serde_json::to_vec(defs)?);
        let body = view.strip_suffix("</chat>").unwrap_or(view);
        let snapshot = Snapshot {
            model: model.into(),
            reasoning: reasoning.into(),
            fixed: hex::encode(hash.finalize()),
            closing_bytes: view.len() - body.len(),
            lines: body
                .split_inclusive('\n')
                .map(|l| (l.len(), hex::encode(Sha256::digest(l.as_bytes()))))
                .collect(),
        };
        let db = self.db.lock().unwrap();
        let old = db
            .query_row(
                "SELECT snapshot FROM cache_views WHERE channel=?1",
                [channel.to_string()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|s| serde_json::from_str::<Snapshot>(&s))
            .transpose()?;
        let previous_bytes: usize = old
            .as_ref()
            .map(|s| s.lines.iter().map(|l| l.0).sum::<usize>() + s.closing_bytes)
            .unwrap_or(0);
        let mut shared: usize = old
            .as_ref()
            .map(|s| {
                s.lines
                    .iter()
                    .zip(&snapshot.lines)
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a.0)
                    .sum()
            })
            .unwrap_or(0);
        if old
            .as_ref()
            .is_some_and(|o| o.lines == snapshot.lines && o.closing_bytes == snapshot.closing_bytes)
        {
            shared += snapshot.closing_bytes;
        }
        let compatible = old.as_ref().is_some_and(|o| {
            o.model == model && o.reasoning == reasoning && o.fixed == snapshot.fixed
        });
        let reason = match &old {
            None => "First observed fresh turn",
            Some(o) if o.model != model || o.reasoning != reasoning => "Model or reasoning changed",
            Some(o) if o.fixed != snapshot.fixed => "System instructions or tool schemas changed",
            Some(_) if shared == previous_bytes && view.len() == previous_bytes => {
                "Memory view unchanged"
            }
            Some(o)
                if shared == previous_bytes.saturating_sub(o.closing_bytes)
                    && view.len() > previous_bytes =>
            {
                "Memory view grew by appending"
            }
            Some(_) => "Memory view coarsened or changed",
        };
        let trace = json!({"reason":reason,"view_bytes":view.len(),"previous_view_bytes":previous_bytes,"shared_view_bytes":shared,"fixed_prefix_compatible":compatible,"observed_at":crate::store::now()});
        db.execute("INSERT INTO cache_views(channel,snapshot,trace) VALUES(?1,?2,?3) ON CONFLICT(channel) DO UPDATE SET snapshot=excluded.snapshot,trace=excluded.trace",params![channel.to_string(),serde_json::to_string(&snapshot)?,trace.to_string()])?;
        Ok(())
    }
    pub fn observed_usage(
        &self,
        channel: u64,
        model: &str,
        usage: &Value,
        phase: &str,
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let mut raw = usage.clone();
        if !raw.is_object() {
            raw = json!({});
        }
        raw["_pantheon_model"] = json!(model);
        tx.execute("INSERT INTO settings(channel,model,reasoning,usage) VALUES(?1,?2,'medium',?3) ON CONFLICT(channel) DO UPDATE SET usage=excluded.usage",params![channel.to_string(),model,raw.to_string()])?;
        tx.execute(
            "INSERT INTO cache_usage(channel,model,phase,usage,created) VALUES(?1,?2,?3,?4,?5)",
            params![
                channel.to_string(),
                model,
                phase,
                serde_json::to_string(&Usage::parse(usage))?,
                crate::store::now()
            ],
        )?;
        tx.execute("DELETE FROM cache_usage WHERE channel=?1 AND seq NOT IN (SELECT seq FROM cache_usage WHERE channel=?1 ORDER BY seq DESC LIMIT 100)",[channel.to_string()])?;
        tx.commit()?;
        Ok(())
    }
    pub fn cache_card(&self, channel: u64) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let trace = db
            .query_row(
                "SELECT trace FROM cache_views WHERE channel=?1",
                [channel.to_string()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|s| serde_json::from_str::<Value>(&s))
            .transpose()?;
        let rows=db.prepare("SELECT model,phase,usage,created FROM cache_usage WHERE channel=?1 ORDER BY seq DESC LIMIT 100")?.query_map([channel.to_string()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let rows = rows
            .into_iter()
            .map(|(m, p, u, t)| Ok((m, p, serde_json::from_str::<Usage>(&u)?, t)))
            .collect::<Result<Vec<_>>>()?;
        let mut total = 0u64;
        let mut cached = 0u64;
        let mut known = 0usize;
        let mut writes = 0u64;
        let mut write_reports = 0usize;
        for (_, _, u, _) in &rows {
            if let (Some(i), Some(c)) = (u.input, u.cached) {
                total = total.saturating_add(i);
                cached = cached.saturating_add(c);
                known += 1;
            }
            if let Some(w) = u.written {
                writes = writes.saturating_add(w);
                write_reports += 1;
            }
        }
        let mut fields = vec![];
        if let Some((model, phase, u, time)) = rows.first() {
            fields.push((
                "Last root request",
                format!(
                    "`{model}` · {}\n{}\nCache writes: {}\n<t:{time}:R>",
                    phase.replace('_', " "),
                    u.ratio(),
                    u.written
                        .map(crate::ui::number)
                        .unwrap_or_else(|| "not reported".into())
                ),
                false,
            ));
        } else {
            fields.push(("Requests", "No root request recorded yet.".into(), false));
        }
        fields.push((
            "Recent measured reuse",
            format!(
                "{}\n{} of {} requests report input and cache reads\nCache writes: {}",
                if total > 0 {
                    format!(
                        "**{:.1}%** · {} / {} input tokens",
                        cached as f64 / total as f64 * 100.,
                        crate::ui::number(cached),
                        crate::ui::number(total)
                    )
                } else {
                    "No measured hit rate yet".into()
                },
                known,
                rows.len(),
                if write_reports > 0 {
                    format!(
                        "{} tokens across {write_reports} reports",
                        crate::ui::number(writes)
                    )
                } else {
                    "not reported".into()
                }
            ),
            false,
        ));
        if let Some(t) = trace {
            fields.push(("Fresh-turn prefix",format!("{}\nMemory: **{:.1} / {:.1} KiB** unchanged at complete-line boundaries\nFixed system/tools/model/effort: {}",t["reason"].as_str().unwrap_or("Unknown"),t["shared_view_bytes"].as_u64().unwrap_or(0) as f64/1024.,t["previous_view_bytes"].as_u64().unwrap_or(0) as f64/1024.,if t["fixed_prefix_compatible"]==true{"unchanged"}else{"changed or no baseline"}),false));
        }
        if !rows.is_empty() {
            fields.push((
                "Recent requests",
                rows.iter()
                    .take(6)
                    .map(|(_, p, u, _)| format!("{} · {}", p.replace('_', " "), u.ratio()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                false,
            ));
        }
        fields.push(("How to read this","Provider token counters measure actual cache reuse. Prefix similarity is a local diagnostic, not a cache-hit guarantee. Unknown counters stay unknown. Last 100 root requests; workers and compactor calls are excluded. OptChat still starts every turn fresh.".into(),false));
        Ok(crate::ui::card(
            "Prompt cache",
            "Measured provider reuse and stable-prefix diagnostics for this channel.",
            fields,
            false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_distinguishes_reads_writes_and_missing_reports() {
        let a = Usage::parse(
            &json!({"input_tokens":100,"cache_read_input_tokens":200,"cache_creation_input_tokens":50}),
        );
        assert_eq!(a.input, Some(350));
        assert_eq!(a.cached, Some(200));
        assert_eq!(a.written, Some(50));
        let o = Usage::parse(
            &json!({"input_tokens":1000,"input_tokens_details":{"cached_tokens":800,"cache_write_tokens":100}}),
        );
        assert_eq!(o.input, Some(1000));
        assert_eq!(o.written, Some(100));
        assert!(Usage::parse(&json!({"input_tokens":100})).cached.is_none());
    }
    #[test]
    fn fingerprints_and_usage_survive_restart_without_storing_prompt_text() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("db");
        let s = Store::open(&path).unwrap();
        let mut memory = crate::memory::Memory::open(d.path().join("memory"), 128000).unwrap();
        memory
            .append(crate::memory::Kind::User, "private-message")
            .unwrap();
        let first = memory.render();
        s.cache_turn(
            1,
            "openai/test",
            "medium",
            "private-instructions",
            &[],
            &first,
        )
        .unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        memory.append(crate::memory::Kind::Talk, "new").unwrap();
        let appended = memory.render();
        s.cache_turn(
            1,
            "openai/test",
            "medium",
            "private-instructions",
            &[],
            &appended,
        )
        .unwrap();
        for i in 0..105 {
            s.observed_usage(
                1,
                "openai/test",
                &if i % 2 == 0 {
                    json!({"input_tokens":100,"input_tokens_details":{"cached_tokens":80}})
                } else {
                    json!({"input_tokens":100})
                },
                "fresh_turn",
            )
            .unwrap();
        }
        let card = s.cache_card(1).unwrap().to_string();
        assert!(card.contains("50 of 100"));
        assert!(card.contains("80.0%"));
        assert!(card.contains("grew by appending"));
        let trace: String =
            s.db.lock()
                .unwrap()
                .query_row("SELECT trace FROM cache_views WHERE channel='1'", [], |r| {
                    r.get(0)
                })
                .unwrap();
        let trace: Value = serde_json::from_str(&trace).unwrap();
        assert_eq!(trace["previous_view_bytes"], first.len());
        assert_eq!(trace["shared_view_bytes"], first.len() - 7);
        s.cache_turn(
            1,
            "openai/test",
            "medium",
            "private-instructions",
            &[],
            &appended,
        )
        .unwrap();
        assert!(
            s.cache_card(1)
                .unwrap()
                .to_string()
                .contains("Memory view unchanged")
        );
        assert!(!card.contains("private-message"));
        let raw: String =
            s.db.lock()
                .unwrap()
                .query_row(
                    "SELECT snapshot FROM cache_views WHERE channel='1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
        assert!(!raw.contains("private-message"));
        assert!(!raw.contains("private-instructions"));
        s.cache_turn(
            1,
            "openai/test",
            "medium",
            "private-instructions",
            &[],
            "<chat>\n0+2|summary\n</chat>",
        )
        .unwrap();
        assert!(
            s.cache_card(1)
                .unwrap()
                .to_string()
                .contains("coarsened or changed")
        );
    }
}
