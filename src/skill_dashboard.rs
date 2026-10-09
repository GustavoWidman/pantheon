//! Private Discord skill browser. Component identifiers are bound to their channel and owner.
use crate::skill_library::{Files, SkillLibrary};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PAGE_SIZE: usize = 20;
const TEXT_UNITS: usize = 2600;

fn key(id: &str) -> String {
    hex::encode(Sha256::digest(id.as_bytes()))[..16].into()
}
fn component(channel: u64, user: u64, action: &str, skill: &str, page: usize) -> String {
    format!("sk1:{channel}:{user}:{action}:{skill}:{page}")
}
/// Checked before acknowledging a component; unauthorized users cannot edit a private dashboard.
pub fn validate_id(custom: &str, channel: u64, user: u64) -> Result<()> {
    ensure!(
        custom.len() <= 100 && channel > 0 && user > 0,
        "invalid dashboard identifier"
    );
    let parts: Vec<_> = custom.split(':').collect();
    ensure!(
        parts.len() == 6 && parts[0] == "sk1",
        "unknown dashboard component"
    );
    ensure!(
        parts[1] == channel.to_string() && parts[2] == user.to_string(),
        "dashboard belongs to another channel or user"
    );
    let action = parts[3];
    ensure!(
        matches!(action, "h" | "s" | "b" | "f" | "t" | "pick" | "file")
            || action
                .strip_prefix('r')
                .is_some_and(|r| r.parse::<i64>().is_ok_and(|r| r > 0))
            || action
                .strip_prefix('d')
                .is_some_and(|r| r.parse::<i64>().is_ok_and(|r| r > 0))
            || action
                .strip_prefix('x')
                .is_some_and(|r| r.parse::<usize>().is_ok_and(|r| r < 64)),
        "invalid dashboard action"
    );
    ensure!(
        parts[4] == "-"
            || (parts[4].len() == 16
                && parts[4]
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
        "invalid skill identifier"
    );
    ensure!(
        parts[5].parse::<usize>().is_ok_and(|n| n <= 100_000),
        "invalid dashboard page"
    );
    Ok(())
}
fn bound(s: &str, units: usize) -> String {
    let mut used = 0;
    s.chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= units
        })
        .collect()
}
fn text_pages(text: &str) -> Vec<String> {
    let mut pages = vec![];
    let mut page = String::new();
    let mut units = 0;
    for c in text.chars() {
        if units + c.len_utf16() > TEXT_UNITS {
            pages.push(std::mem::take(&mut page));
            units = 0;
        }
        page.push(c);
        units += c.len_utf16();
    }
    if !page.is_empty() || pages.is_empty() {
        pages.push(page);
    }
    pages
}
fn button(channel: u64, user: u64, action: &str, skill: &str, page: usize, label: &str) -> Value {
    json!({"type":2,"style":2,"custom_id":component(channel,user,action,skill,page),"label":label})
}
fn row(items: Vec<Value>) -> Value {
    json!({"type":1,"components":items})
}
fn card(
    title: &str,
    description: &str,
    fields: Vec<Value>,
    components: Vec<Value>,
    footer: &str,
) -> Value {
    json!({"content":"","allowed_mentions":{"parse":[]},"embeds":[{
        "title":bound(title,256),"description":bound(description,3200),"color":0x8b7cf8,
        "fields":fields,"footer":{"text":bound(footer,200)}
    }],"components":components})
}
fn field(name: &str, value: &str) -> Value {
    json!({"name":bound(name,256),"value":bound(value,1024),"inline":false})
}
fn nav(channel: u64, user: u64, skill: &str) -> Value {
    row(vec![
        button(channel, user, "s", skill, 0, "Overview"),
        button(channel, user, "b", skill, 0, "Guide"),
        button(channel, user, "f", skill, 0, "Files"),
        button(channel, user, "t", skill, 0, "History"),
        button(channel, user, "h", "-", 0, "Home"),
    ])
}
fn pager(
    channel: u64,
    user: u64,
    action: &str,
    skill: &str,
    page: usize,
    count: usize,
) -> Option<Value> {
    if count <= 1 {
        return None;
    }
    let mut previous = button(
        channel,
        user,
        action,
        skill,
        page.saturating_sub(1),
        "Previous",
    );
    previous["disabled"] = json!(page == 0);
    let mut next = button(
        channel,
        user,
        action,
        skill,
        (page + 1).min(count - 1),
        "Next",
    );
    next["disabled"] = json!(page + 1 >= count);
    Some(row(vec![previous, next]))
}
fn guide_body(text: &str) -> &str {
    text.strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .and_then(|rest| rest.split_once("\n---"))
        .map(|(_, body)| body.trim_start())
        .unwrap_or(text)
}
fn header(files: &Files) -> (String, String) {
    let text = files.get("SKILL.md").map(String::as_str).unwrap_or("");
    let metadata = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .and_then(|rest| rest.split_once("\n---"))
        .and_then(|(yaml, _)| serde_yaml_ng::from_str::<Value>(yaml).ok())
        .unwrap_or(Value::Null);
    (
        metadata["name"].as_str().unwrap_or("Skill").into(),
        metadata["description"]
            .as_str()
            .unwrap_or("No description")
            .into(),
    )
}

pub fn home(library: &SkillLibrary, channel: u64, user: u64) -> Result<Value> {
    render(
        library,
        channel,
        user,
        &component(channel, user, "h", "-", 0),
        &[],
    )
}

pub fn curator_status(status: &Value, model: &str, effort: &str) -> Value {
    let latest = &status["latest"];
    let phase = if status["enabled"] == false {
        "disabled"
    } else {
        latest["phase"].as_str().unwrap_or("waiting")
    };
    let queued = status["queued_count"].as_i64().unwrap_or(0);
    let mut fields = vec![
        field(
            "Channel queue",
            &format!(
                "Phase: **{phase}**\nQueued jobs: **{queued}**\nManual request: **{}**\nOne curator job at a time in this channel.",
                status["requested"].as_bool().unwrap_or(false)
            ),
        ),
        field(
            "Reviewers",
            &format!(
                "Spawned: **{}** · Running: **{}** · Finished: **{}**",
                latest["reviewers_spawned"].as_i64().unwrap_or(0),
                latest["reviewers_running"].as_i64().unwrap_or(0),
                latest["reviewers_finished"].as_i64().unwrap_or(0)
            ),
        ),
        field(
            "Model",
            &format!(
                "Next job: {} · {}\nActive job: {} · {}",
                bound(model, 400),
                bound(effort, 100),
                bound(latest["model"].as_str().unwrap_or("—"), 400),
                bound(latest["reasoning"].as_str().unwrap_or("—"), 100)
            ),
        ),
    ];
    if let Some(report) = latest["report"].as_str().filter(|s| !s.is_empty()) {
        fields.push(field("Latest verdict", report));
    }
    card(
        "✦ Skill curator",
        "Curation and review run independently of the conversation. Research stays private; approved changes appear in the activity timeline.",
        fields,
        vec![],
        latest["id"]
            .as_str()
            .unwrap_or("No curation job has started in this channel"),
    )
}
pub fn open_skill(library: &SkillLibrary, channel: u64, user: u64, id: &str) -> Result<Value> {
    render(
        library,
        channel,
        user,
        &component(channel, user, "s", &key(id), 0),
        &[],
    )
}
pub fn render(
    library: &SkillLibrary,
    channel: u64,
    user: u64,
    custom: &str,
    values: &[String],
) -> Result<Value> {
    validate_id(custom, channel, user)?;
    ensure!(
        values.len() <= 1 && values.iter().all(|v| v.len() <= 32),
        "invalid dashboard selection"
    );
    let parts: Vec<_> = custom.split(':').collect();
    let action = parts[3];
    let mut skill_key = parts[4].to_owned();
    let page: usize = parts[5].parse()?;
    let heads = library.heads()?;
    let heads = heads.as_array().context("invalid skill catalogue")?;
    if action == "h" {
        let catalogue = library.snapshot()?;
        ensure!(values.is_empty(), "unexpected home selection");
        let count = heads.len().div_ceil(PAGE_SIZE).max(1);
        ensure!(
            page < count,
            "skill catalogue page no longer exists; reopen /skills"
        );
        let active = heads.iter().filter(|h| h["retired"] != true).count();
        let mut components = vec![];
        let mut lines = vec![];
        let mut options = vec![];
        for h in heads.iter().skip(page * PAGE_SIZE).take(PAGE_SIZE) {
            let id = h["id"].as_str().context("invalid skill id")?;
            let retired = h["retired"] == true;
            lines.push(format!(
                "{} `{id}` · r{} · {} uses · {} refinements",
                if retired { "⊖" } else { "✦" },
                h["revision"],
                h["invocations"].as_i64().unwrap_or(0),
                h["refinements"].as_i64().unwrap_or(0)
            ));
            let guide = catalogue.entries.get(id);
            options.push(json!({"label":bound(guide.map(|s|s.name.as_str()).unwrap_or(id),100),"value":key(id),"description":bound(&format!("{id} · r{} · {}",h["revision"],if retired {"Retired"}else{guide.map(|s|s.description.as_str()).unwrap_or("Active")}),100)}));
        }
        if !options.is_empty() {
            components.push(row(vec![json!({"type":3,"custom_id":component(channel,user,"pick","-",page),"placeholder":"Choose a skill","min_values":1,"max_values":1,"options":options})]));
        }
        if let Some(p) = pager(channel, user, "h", "-", page, count) {
            components.push(p);
        }
        return Ok(card(
            "✦ Skill library",
            &format!(
                "**{active} active · {} retired**\n\n{}",
                heads.len() - active,
                if lines.is_empty() {
                    "No skills available.".into()
                } else {
                    lines.join("\n")
                }
            ),
            vec![],
            components,
            &format!(
                "Page {} / {count} · Usage and refinements are history, not success measures",
                page + 1
            ),
        ));
    }
    let action = if action == "pick" {
        skill_key = values.first().context("choose one skill")?.clone();
        "s"
    } else {
        action
    };
    let matching: Vec<_> = heads
        .iter()
        .filter(|h| h["id"].as_str().is_some_and(|id| key(id) == skill_key))
        .collect();
    ensure!(
        matching.len() == 1,
        "skill no longer exists; reopen /skills"
    );
    let head = matching[0];
    let id = head["id"].as_str().unwrap();
    let revision = head["revision"]
        .as_i64()
        .context("invalid skill revision")?;
    let files = library.revision_files(id, revision)?;
    let (name, description) = header(&files);
    let mut components = vec![];
    let footer = format!(
        "{id} · revision {revision} · {}",
        if head["retired"] == true {
            "retired"
        } else {
            "active"
        }
    );
    let mut fields = vec![];
    let (title, text) = match action {
        "s" => {
            ensure!(
                values.is_empty() || parts[3] == "pick",
                "unexpected skill selection"
            );
            fields.push(field(
                "Library record",
                &format!(
                    "Origin: {}\nInvocations: {}\nCurator refinements: {}\nSupporting files: {}",
                    head["origin"].as_str().unwrap_or("unknown"),
                    head["invocations"].as_i64().unwrap_or(0),
                    head["refinements"].as_i64().unwrap_or(0),
                    files.len().saturating_sub(1)
                ),
            ));
            (format!("✦ {name}"), description)
        }
        "b" => {
            let body = guide_body(files.get("SKILL.md").context("missing guide")?);
            let pages = text_pages(body);
            ensure!(page < pages.len(), "guide page no longer exists");
            if let Some(p) = pager(channel, user, action, &skill_key, page, pages.len()) {
                components.push(p);
            }
            (
                format!("{name} · guide {}/{}", page + 1, pages.len()),
                pages[page].clone(),
            )
        }
        "f" => {
            let names: Vec<_> = files.keys().filter(|k| k.as_str() != "SKILL.md").collect();
            let count = names.len().div_ceil(PAGE_SIZE).max(1);
            ensure!(page < count, "file page no longer exists");
            if !names.is_empty() {
                let options: Vec<_> = names.iter().enumerate().skip(page*PAGE_SIZE).take(PAGE_SIZE).map(|(i,name)|json!({"label":bound(name,100),"value":i.to_string(),"description":format!("{} characters",files[*name].chars().count())})).collect();
                components.push(row(vec![json!({"type":3,"custom_id":component(channel,user,"file",&skill_key,0),"placeholder":"Read a supporting file","options":options})]));
            }
            if let Some(p) = pager(channel, user, action, &skill_key, page, count) {
                components.push(p);
            }
            (
                format!("{name} · supporting files"),
                if names.is_empty() {
                    "This guide has no supporting files.".into()
                } else {
                    names
                        .iter()
                        .skip(page * PAGE_SIZE)
                        .take(PAGE_SIZE)
                        .map(|n| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join("\n")
                },
            )
        }
        "t" => {
            let history = library.history(id, page * 8)?;
            let revisions = history["revisions"].as_array().context("invalid history")?;
            ensure!(
                page == 0 || !revisions.is_empty(),
                "history page no longer exists"
            );
            for revision in revisions {
                fields.push(field(
                    &format!(
                        "Revision {}{}",
                        revision["revision"],
                        if revision["retired"] == true {
                            " · retired"
                        } else {
                            ""
                        }
                    ),
                    &bound(revision["reason"].as_str().unwrap_or("No reason"), 512),
                ));
            }
            let options:Vec<_> = revisions.iter().map(|r|json!({"label":format!("Revision {}{}",r["revision"],if r["retired"]==true{" · retired"}else{""}),"value":r["revision"].to_string(),"description":bound(r["reason"].as_str().unwrap_or(""),100)})).collect();
            if !options.is_empty() {
                components.push(row(vec![json!({"type":3,"custom_id":component(channel,user,"r1",&skill_key,0),"placeholder":"Inspect a revision and its changes","options":options})]));
            }
            let count = page + 1 + usize::from(history["next_offset"].is_number());
            if let Some(p) = pager(channel, user, action, &skill_key, page, count) {
                components.push(p);
            }
            (format!("{name} · revision history"),"Select a revision to read its content or compare it with the preceding revision. Additional reasons appear in each revision's record.".into())
        }
        _ => {
            let action = if action == "file" {
                let index = values.first().context("select a file")?.parse::<usize>()?;
                ensure!(index < 64, "invalid file index");
                format!("x{index}")
            } else if action.starts_with('r') && !values.is_empty() {
                let selected = values[0].parse::<i64>()?;
                ensure!(selected > 0 && selected <= revision, "invalid revision");
                format!("r{selected}")
            } else {
                ensure!(values.is_empty(), "unexpected selection");
                action.into()
            };
            let (label, raw) = if let Some(index) = action.strip_prefix('x') {
                let index: usize = index.parse()?;
                let filename = files
                    .keys()
                    .filter(|k| k.as_str() != "SKILL.md")
                    .nth(index)
                    .context("file no longer exists")?;
                (filename.clone(), files[filename].clone())
            } else {
                let selected: i64 = action[1..].parse()?;
                let selected_files = library.revision_files(id, selected)?;
                if action.starts_with('d') {
                    let previous = if selected > 1 {
                        library.revision_files(id, selected - 1)?
                    } else {
                        Files::new()
                    };
                    (
                        format!("revision {selected} changes"),
                        diff(&previous, &selected_files),
                    )
                } else {
                    components.push(row(vec![button(
                        channel,
                        user,
                        &format!("d{selected}"),
                        &skill_key,
                        0,
                        "Compare previous revision",
                    )]));
                    (
                        format!("revision {selected}"),
                        selected_files.get("SKILL.md").cloned().unwrap_or_default(),
                    )
                }
            };
            let pages = text_pages(&raw.replace("```", "ˋˋˋ"));
            ensure!(page < pages.len(), "content page no longer exists");
            if let Some(p) = pager(channel, user, &action, &skill_key, page, pages.len()) {
                components.push(p);
            }
            (
                format!("{name} · {label} · {}/{}", page + 1, pages.len()),
                format!("```text\n{}\n```", pages[page]),
            )
        }
    };
    components.push(nav(channel, user, &skill_key));
    Ok(card(&title, &text, fields, components, &footer))
}
/// A deterministic changed-block comparison, including supporting-file additions/removals.
fn diff(previous: &Files, current: &Files) -> String {
    let mut names: std::collections::BTreeSet<_> = previous.keys().collect();
    names.extend(current.keys());
    let mut output = String::new();
    for name in names {
        let old = previous.get(name).map(String::as_str).unwrap_or("");
        let new = current.get(name).map(String::as_str).unwrap_or("");
        if old == new {
            continue;
        }
        output.push_str(&format!("--- {name} (previous)\n+++ {name} (selected)\n"));
        let old: Vec<_> = old.lines().collect();
        let new: Vec<_> = new.lines().collect();
        let common = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let suffix = old[common..]
            .iter()
            .rev()
            .zip(new[common..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        for line in &old[common..old.len() - suffix] {
            output.push_str(&format!("- {line}\n"));
        }
        for line in &new[common..new.len() - suffix] {
            output.push_str(&format!("+ {line}\n"));
        }
    }
    if output.is_empty() {
        "Content unchanged (for example, retirement or restoration).".into()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillsConfig;

    fn library(root: &std::path::Path, count: usize) -> SkillLibrary {
        let seeds = root.join("seeds");
        std::fs::create_dir_all(&seeds).unwrap();
        for i in 0..count {
            let guide = seeds.join(format!("guide-{i:03}"));
            std::fs::create_dir(&guide).unwrap();
            std::fs::write(
                guide.join("SKILL.md"),
                format!(
                    "---\nname: Guide {i}\ndescription: Concrete verification workflow\n---\n{}",
                    "🦀 bounded procedure\n".repeat(1000)
                ),
            )
            .unwrap();
            std::fs::write(guide.join("support.txt"), "Read-only supporting text").unwrap();
        }
        SkillLibrary::open(
            &SkillsConfig {
                bundled: false,
                directories: vec![seeds],
            },
            &root.join("state"),
        )
        .unwrap()
    }
    fn limits(payload: &Value) {
        fn length(s: &Value) -> usize {
            s.as_str().unwrap_or("").encode_utf16().count()
        }
        let embeds = payload["embeds"].as_array().unwrap();
        let mut total = 0;
        for embed in embeds {
            assert!(length(&embed["title"]) <= 256);
            assert!(length(&embed["description"]) <= 4096);
            total += length(&embed["title"])
                + length(&embed["description"])
                + length(&embed["footer"]["text"]);
            for field in embed["fields"].as_array().unwrap() {
                assert!(length(&field["value"]) <= 1024);
                total += length(&field["name"]) + length(&field["value"]);
            }
        }
        assert!(total <= 6000);
        let rows = payload["components"].as_array().unwrap();
        assert!(rows.len() <= 5);
        for row in rows {
            for item in row["components"].as_array().unwrap() {
                assert!(item["custom_id"].as_str().unwrap().len() <= 100);
                if let Some(options) = item["options"].as_array() {
                    assert!(options.len() <= 25);
                    for option in options {
                        assert!(length(&option["label"]) <= 100);
                        assert!(length(&option["description"]) <= 100);
                    }
                }
            }
        }
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
    }
    #[test]
    fn catalogue_pagination_reaches_every_skill_and_large_guides_stay_bounded() {
        let root = tempfile::tempdir().unwrap();
        let library = library(root.path(), 43);
        let mut selected = std::collections::BTreeSet::new();
        for page in 0..3 {
            let payload = render(&library, 1, 42, &component(1, 42, "h", "-", page), &[]).unwrap();
            limits(&payload);
            for option in payload["components"][0]["components"][0]["options"]
                .as_array()
                .unwrap()
            {
                selected.insert(option["value"].as_str().unwrap().to_owned());
            }
        }
        assert_eq!(selected.len(), 43);
        let id = key("guide-000");
        let payload = render(
            &library,
            1,
            42,
            &component(1, 42, "pick", "-", 0),
            std::slice::from_ref(&id),
        )
        .unwrap();
        limits(&payload);
        for action in ["s", "b", "f", "t", "r1", "d1", "x0"] {
            let payload = render(&library, 1, 42, &component(1, 42, action, &id, 0), &[]).unwrap();
            limits(&payload);
        }
        let page = render(&library, 1, 42, &component(1, 42, "b", &id, 1), &[]).unwrap();
        limits(&page);
        assert!(page["embeds"][0]["title"].as_str().unwrap().contains("2/"));
    }
    #[test]
    fn invalid_components_cannot_cross_channels_or_users() {
        let id = component(1, 42, "h", "-", 0);
        assert!(validate_id(&id, 1, 42).is_ok());
        assert!(validate_id(&id, 2, 42).is_err());
        assert!(validate_id(&id, 1, 43).is_err());
        for invalid in [
            "sk1:1:42:h:-:999999999",
            "sk1:1:42:delete:-:0",
            "sk1:1:42:h:../../secret:0",
            "sk1:1:42:d0:-:0",
            "sk1:1:42:x64:-:0",
        ] {
            assert!(validate_id(invalid, 1, 42).is_err());
        }
    }
    #[test]
    fn changed_block_diff_covers_support_files_and_text_paging_preserves_unicode() {
        let before = Files::from([
            ("SKILL.md".into(), "one\ntwo\nthree".into()),
            ("removed.txt".into(), "old".into()),
        ]);
        let after = Files::from([
            ("SKILL.md".into(), "one\nchanged\nthree".into()),
            ("added.txt".into(), "new".into()),
        ]);
        let changes = diff(&before, &after);
        assert!(changes.contains("- two\n+ changed"));
        assert!(changes.contains("removed.txt"));
        assert!(changes.contains("added.txt"));
        let text = "🦀a".repeat(10000);
        let pages = text_pages(&text);
        assert_eq!(pages.concat(), text);
        assert!(pages.iter().all(|p| p.encode_utf16().count() <= TEXT_UNITS));
    }
}
