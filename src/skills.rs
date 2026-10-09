use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    pub bundled: bool,
    pub directories: Vec<PathBuf>,
}
impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            bundled: true,
            directories: vec![],
        }
    }
}
impl SkillsConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.directories.len() <= 64, "too many skill directories");
        Ok(())
    }
}
#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default, rename = "disable-model-invocation")]
    explicit_only: bool,
}
#[derive(Clone)]
pub(crate) struct Skill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) explicit_only: bool,
    text: String,
    pub(crate) invocations: i64,
    pub(crate) refinements: i64,
    pub(crate) root: Option<PathBuf>,
    pub(crate) resources: Option<BTreeMap<String, String>>,
    pub(crate) revision: i64,
    pub(crate) origin: String,
}
#[derive(Clone)]
pub struct Skills {
    pub(crate) entries: BTreeMap<String, Skill>,
}
impl Skills {
    pub fn load(config: &SkillsConfig) -> Result<Self> {
        config.validate()?;
        let mut library = Self {
            entries: BTreeMap::new(),
        };
        if config.bundled {
            for (id, text) in [
                ("research", include_str!("../skills/research/SKILL.md")),
                (
                    "browser-activities",
                    include_str!("../skills/browser-activities/SKILL.md"),
                ),
                ("learning", include_str!("../skills/learning/SKILL.md")),
                (
                    "engineering",
                    include_str!("../skills/engineering/SKILL.md"),
                ),
            ] {
                library.insert(id, text.to_owned(), None)?;
            }
        }
        for directory in &config.directories {
            let root = directory
                .canonicalize()
                .with_context(|| format!("open skill directory {}", directory.display()))?;
            if root.join("SKILL.md").is_file() {
                library.load_directory(&root)?;
            } else {
                let mut children =
                    std::fs::read_dir(&root)?.collect::<std::io::Result<Vec<_>>>()?;
                children.sort_by_key(|entry| entry.file_name());
                for child in children {
                    let path = child.path().canonicalize()?;
                    ensure!(
                        path.starts_with(&root),
                        "skill symlink escapes configured directory"
                    );
                    if path.join("SKILL.md").is_file() {
                        library.load_directory(&path)?;
                    }
                }
            }
        }
        ensure!(
            library.entries.len() <= 256,
            "skill library exceeds 256 entries"
        );
        Ok(library)
    }
    fn load_directory(&mut self, root: &Path) -> Result<()> {
        let file = root.join("SKILL.md").canonicalize()?;
        ensure!(
            file.starts_with(root),
            "SKILL.md escapes its skill directory"
        );
        let text = read_bounded(&file)?;
        let id = root
            .file_name()
            .and_then(|x| x.to_str())
            .context("invalid skill directory name")?;
        self.insert(id, text, Some(root.into()))
    }
    pub(crate) fn insert(&mut self, id: &str, text: String, root: Option<PathBuf>) -> Result<()> {
        ensure!(
            !id.is_empty()
                && id.len() <= 64
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "skill directory IDs must be lowercase words separated by hyphens"
        );
        let (header, _) = text
            .strip_prefix("---\n")
            .or_else(|| text.strip_prefix("---\r\n"))
            .and_then(|rest| rest.split_once("\n---"))
            .context("SKILL.md requires YAML frontmatter")?;
        let meta: Frontmatter =
            serde_yaml_ng::from_str(header).context("parse skill frontmatter")?;
        ensure!(
            !meta.name.trim().is_empty() && meta.name.chars().count() <= 128,
            "invalid skill name"
        );
        ensure!(
            !meta.description.trim().is_empty() && meta.description.chars().count() <= 1024,
            "invalid skill description"
        );
        ensure!(
            !self.entries.contains_key(id),
            "duplicate skill ID {id}; disable bundled skills or use a distinct directory name"
        );
        self.entries.insert(
            id.into(),
            Skill {
                name: meta.name,
                description: meta.description,
                explicit_only: meta.explicit_only,
                text,
                invocations: 0,
                refinements: 0,
                root,
                resources: None,
                revision: 0,
                origin: "seed".into(),
            },
        );
        Ok(())
    }
    pub(crate) fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
    pub(crate) fn main_text(&self, id: &str) -> Result<&str> {
        Ok(&self.entries.get(id).context("unknown skill")?.text)
    }
    pub fn catalogue(&self, description_chars: usize) -> Result<String> {
        ensure!(
            (1..=1024).contains(&description_chars),
            "description_chars must be 1–1024"
        );
        let entries: Vec<_> = self
            .entries
            .iter()
            .map(|(id, s)| {
                json!({
                    "id":id,"name":s.name,"revision":s.revision,"invocations":s.invocations,
                    "refinements":s.refinements,"explicit_only":s.explicit_only,
                    "description":elide(&s.description, description_chars)
                })
            })
            .collect();
        Ok(format!(
            "Complete active skill catalogue (description limit {description_chars} Unicode characters; invocation/refinement counts are historical snapshots, not success measures). Load a relevant guide before using it. Explicit-only guides require a user request.\n{}",
            json!(entries)
        ))
    }
    pub fn index(&self) -> String {
        self.catalogue(180).expect("valid description bound")
    }
    pub fn execute(&self, args: &Value) -> Result<Value> {
        match crate::tools::string(args, "action")? {
            "list" => {
                let offset = integer(args, "offset", 0, 256)?;
                let entries: Vec<_> = self.entries.iter().skip(offset).take(8).map(|(id,s)|json!({"id":id,"name":s.name,"description":s.description,"explicit_only":s.explicit_only,"revision":s.revision,"origin":s.origin,"invocations":s.invocations,"refinements":s.refinements})).collect();
                let next = offset + entries.len();
                Ok(
                    json!({"skills":entries,"next_offset":if next<self.entries.len(){Some(next)}else{None}}),
                )
            }
            "preview" => {
                let id = crate::tools::string(args, "id")?;
                let skill = self
                    .entries
                    .get(id)
                    .context("unknown skill ID; use skill list")?;
                let max = integer(args, "max_chars", 1200, 4000)?;
                ensure!(max > 0, "max_chars must be positive");
                let body = skill
                    .text
                    .strip_prefix("---\n")
                    .or_else(|| skill.text.strip_prefix("---\r\n"))
                    .and_then(|rest| rest.split_once("\n---"))
                    .map(|(_, body)| body.trim_start())
                    .unwrap_or(&skill.text);
                Ok(
                    json!({"id":id,"name":skill.name,"description":skill.description,
                    "text":elide(body,max),"total_chars":body.chars().count(),"revision":skill.revision,
                    "origin":skill.origin,"explicit_only":skill.explicit_only,
                    "invocations":skill.invocations,"refinements":skill.refinements}),
                )
            }
            "load" => {
                let id = crate::tools::string(args, "id")?;
                let skill = self
                    .entries
                    .get(id)
                    .context("unknown skill ID; use skill list")?;
                let file = args
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("SKILL.md");
                let text = if file == "SKILL.md" {
                    skill.text.clone()
                } else {
                    let relative = Path::new(file);
                    ensure!(
                        relative
                            .components()
                            .all(|c| matches!(c, Component::Normal(_))),
                        "skill file must be a relative path without traversal"
                    );
                    if let Some(resources) = &skill.resources {
                        resources
                            .get(file)
                            .context("unknown skill resource")?
                            .clone()
                    } else {
                        let root = skill
                            .root
                            .as_ref()
                            .context("bundled skill has no supporting file")?;
                        let path = root
                            .join(relative)
                            .canonicalize()
                            .context("find skill resource")?;
                        ensure!(
                            path.starts_with(root),
                            "skill resource escapes its directory"
                        );
                        read_bounded(&path)?
                    }
                };
                let offset = integer(args, "offset", 0, 512_000)?;
                let max = integer(args, "max_chars", 8000, 12_000)?;
                ensure!(max > 0, "max_chars must be positive");
                let total = text.chars().count();
                ensure!(offset <= total, "offset exceeds skill text length");
                let mut budget = max;
                loop {
                    let page: String = text.chars().skip(offset).take(budget).collect();
                    let next = offset + page.chars().count();
                    let result = json!({"id":id,"name":skill.name,"description":skill.description,"file":file,"text":page,"next_offset":if next<total{Some(next)}else{None},"total_chars":total,"directory":skill.root,"resources":skill.resources.as_ref().map(|files|files.keys().collect::<Vec<_>>()),"explicit_only":skill.explicit_only,"revision":skill.revision,"origin":skill.origin});
                    if result.to_string().chars().count() <= 24_000 {
                        return Ok(result);
                    }
                    ensure!(budget > 1, "skill metadata is too large to render");
                    budget /= 2;
                }
            }
            _ => bail!("unknown skill action"),
        }
    }
}
pub(crate) fn read_bounded(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(512_001)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 512_000, "skill file exceeds 512000 bytes");
    String::from_utf8(bytes).context("skill file is not UTF-8 text")
}
fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        text.chars()
            .take(max.saturating_sub(1))
            .chain(std::iter::once('…'))
            .collect()
    }
}
fn integer(args: &Value, key: &str, default: usize, max: usize) -> Result<usize> {
    let n = args
        .get(key)
        .map(|v| {
            v.as_u64()
                .context("paging values must be nonnegative integers")
        })
        .transpose()?
        .unwrap_or(default as u64);
    ensure!(n <= max as u64, "{key} exceeds {max}");
    Ok(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loads_standard_skills_and_reads_paged_references_without_escaping() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shopping");
        std::fs::create_dir_all(root.join("references")).unwrap();
        std::fs::write(root.join("SKILL.md"),"---\nname: Shopping Guide\ndescription: >\n  Compare products using the customer's criteria.\nmetadata:\n  owner: example\n---\nUse primary sources.\n").unwrap();
        std::fs::write(
            root.join("references/criteria.md"),
            "price\nquality\nsupport",
        )
        .unwrap();
        let library = Skills::load(&SkillsConfig {
            bundled: false,
            directories: vec![dir.path().into()],
        })
        .unwrap();
        assert_eq!(
            library.execute(&json!({"action":"list"})).unwrap()["skills"][0]["name"],
            "Shopping Guide"
        );
        let first=library.execute(&json!({"action":"load","id":"shopping","file":"references/criteria.md","max_chars":6})).unwrap();
        assert_eq!(first["text"], "price\n");
        assert_eq!(first["next_offset"], 6);
        assert_eq!(library.execute(&json!({"action":"load","id":"shopping","file":"references/criteria.md","offset":6})).unwrap()["text"],"quality\nsupport");
        for file in ["../outside", "/etc/passwd", "references/../SKILL.md"] {
            assert!(
                library
                    .execute(&json!({"action":"load","id":"shopping","file":file}))
                    .is_err()
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", root.join("references/outside")).unwrap();
            assert!(
                library
                    .execute(&json!({"action":"load","id":"shopping","file":"references/outside"}))
                    .is_err()
            );
        }
        std::fs::write(root.join("SKILL.md"), "changed after startup").unwrap();
        assert!(
            library
                .execute(&json!({"action":"load","id":"shopping"}))
                .unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("Use primary sources.")
        );
    }
    #[test]
    fn explicit_only_skills_are_catalogued_with_invocation_restriction() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("special");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("SKILL.md"),"---\nname: special\ndescription: Special workflow.\ndisable-model-invocation: true\n---\nDo the requested special task.").unwrap();
        let library = Skills::load(&SkillsConfig {
            bundled: false,
            directories: vec![dir.path().into()],
        })
        .unwrap();
        assert!(library.index().contains("Special workflow"));
        assert!(library.index().contains("explicit_only"));
        assert_eq!(
            library.execute(&json!({"action":"list"})).unwrap()["skills"][0]["explicit_only"],
            true
        );
        assert!(
            library
                .execute(&json!({"action":"load","id":"special"}))
                .unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("requested special task")
        );
    }
    #[test]
    fn catalogs_paginate_and_duplicate_ids_fail_instead_of_shadowing() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..10 {
            let root = dir.path().join(format!("guide-{n}"));
            std::fs::create_dir(&root).unwrap();
            std::fs::write(
                root.join("SKILL.md"),
                format!("---\nname: guide-{n}\ndescription: Task guide {n}.\n---\nWork."),
            )
            .unwrap();
        }
        let cfg = SkillsConfig {
            bundled: false,
            directories: vec![dir.path().into()],
        };
        let library = Skills::load(&cfg).unwrap();
        let first = library.execute(&json!({"action":"list"})).unwrap();
        assert_eq!(first["skills"].as_array().unwrap().len(), 8);
        assert_eq!(first["next_offset"], 8);
        let last = library
            .execute(&json!({"action":"list","offset":8}))
            .unwrap();
        assert_eq!(last["skills"].as_array().unwrap().len(), 2);
        assert!(last["next_offset"].is_null());
        assert!(
            Skills::load(&SkillsConfig {
                bundled: false,
                directories: vec![dir.path().into(), dir.path().into()]
            })
            .is_err()
        );
    }
    #[test]
    fn previews_and_catalogue_bounds_preserve_unicode_without_loading_full_body() {
        let mut skills = Skills::empty();
        skills
            .insert(
                "unicode",
                "---\nname: Guide\ndescription: αβγδεζη\n---\nαβγδεζη procedure".into(),
                None,
            )
            .unwrap();
        let catalogue = skills.catalogue(4).unwrap();
        assert!(catalogue.contains("αβγ…"));
        assert!(!catalogue.contains("αβγδε"));
        let preview = skills
            .execute(&json!({"action":"preview","id":"unicode","max_chars":4}))
            .unwrap();
        assert_eq!(preview["text"], "αβγ…");
        assert_eq!(preview["invocations"], 0);
        assert!(!preview["text"].as_str().unwrap().contains("---"));
        assert!(
            skills
                .execute(&json!({"action":"preview","id":"unicode","max_chars":0}))
                .is_err()
        );
        assert!(
            skills
                .execute(&json!({"action":"load","id":"unicode"}))
                .unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("procedure")
        );
    }
}
