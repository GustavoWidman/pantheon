//! Local autocomplete snapshots; discovery never runs on an interaction's deadline.
use crate::{auth::AuthConfig, config::Config, provider::Provider};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::RwLock};

pub const PROVIDERS: [&str; 3] = ["codex", "openai", "anthropic"];
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub adaptive_thinking: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub observed_at: Option<i64>,
}
impl Model {
    pub fn plain(id: &str) -> Self {
        Self {
            id: id.into(),
            name: id.into(),
            efforts: vec![],
            default_effort: None,
            adaptive_thinking: false,
            source: "configured fallback".into(),
            observed_at: None,
        }
    }
}
#[derive(Default)]
pub struct Catalog {
    entries: RwLock<BTreeMap<String, Vec<Model>>>,
    chats: RwLock<(String, BTreeMap<u64, String>)>,
}
impl Catalog {
    pub fn initialize(&self, config: &Config) {
        self.chats.write().unwrap().0 = config.agent.model.clone();
        let cached = std::fs::File::open(config.state_dir.join("model-catalog.json"))
            .ok()
            .and_then(|f| {
                use std::io::Read;
                serde_json::from_reader::<_, BTreeMap<String, Vec<Model>>>(f.take(4_194_304)).ok()
            })
            .unwrap_or_default();
        let mut entries = self.entries.write().unwrap();
        for provider in available(&config.auth) {
            let mut models = cached.get(provider).cloned().unwrap_or_default();
            if provider == "codex" && models.is_empty() {
                let local = codex_cache(&config.auth);
                if !local.is_empty() {
                    models = local;
                }
            }
            if models.is_empty() {
                for full in [&config.agent.model, &config.agent.compactor_model]
                    .into_iter()
                    .chain(config.agent.context_windows.keys())
                {
                    if let Some(id) = full.strip_prefix(&format!("{provider}/")) {
                        models.push(Model::plain(id));
                    }
                }
            }
            normalize(&mut models);
            entries.insert(provider.into(), models);
        }
    }
    pub async fn refresh(&self, config: &Config, provider: &Provider) {
        let available = available(&config.auth);
        self.entries
            .write()
            .unwrap()
            .retain(|p, _| available.contains(&p.as_str()));
        for name in available {
            // Make a newly authenticated provider discoverable even if its listing is down.
            self.entries
                .write()
                .unwrap()
                .entry(name.into())
                .or_default();
            match provider.list_models(name).await {
                Ok(models) => self.replace(name, models),
                Err(error) => {
                    tracing::warn!(provider=name,error=%error,"model discovery failed; retaining cached suggestions")
                }
            }
        }
        if let Err(error) = self.persist(&config.state_dir) {
            tracing::warn!(error=%error,"model catalog snapshot could not be saved");
        }
    }
    pub(crate) fn replace(&self, provider: &str, mut models: Vec<Model>) {
        normalize(&mut models);
        self.entries
            .write()
            .unwrap()
            .insert(provider.into(), models);
    }
    fn persist(&self, dir: &Path) -> Result<()> {
        use std::io::Write;
        let bytes = serde_json::to_vec(&*self.entries.read().unwrap())?;
        let temp = dir.join(format!(".model-catalog-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, dir.join("model-catalog.json"))?;
            std::fs::File::open(dir)?.sync_all()?;
            Ok(())
        })();
        let _ = std::fs::remove_file(temp);
        result
    }
    pub fn choices(&self, options: &Value) -> Vec<Value> {
        let options = options.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let Some(focused) = options.iter().find(|o| o["focused"] == true) else {
            return vec![];
        };
        let query = focused["value"].as_str().unwrap_or("").to_lowercase();
        let provider = options
            .iter()
            .find(|o| o["name"] == "provider")
            .and_then(|o| o["value"].as_str())
            .filter(|s| !s.is_empty());
        let entries = self.entries.read().unwrap();
        let mut choices: Vec<(String, String)> = match focused["name"].as_str() {
            Some("kind") => vec![
                ("chat".into(), "chat".into()),
                ("compact".into(), "compact".into()),
            ],
            Some("provider") => entries.keys().map(|p| (p.clone(), p.clone())).collect(),
            Some("model") => entries
                .iter()
                .filter(|(p, _)| provider.is_none_or(|selected| selected == p.as_str()))
                .flat_map(|(p, models)| {
                    models.iter().map(move |m| {
                        let full = format!("{p}/{}", m.id);
                        (format!("{} · {p}", m.name), full)
                    })
                })
                .collect(),
            _ => vec![],
        };
        if focused["name"] == "model" {
            choices.push((
                "Config default · clear this chat's override".into(),
                "default".into(),
            ));
        }
        choices.retain(|(name, value)| {
            value.chars().count() <= 100
                && (value.to_lowercase().contains(&query) || name.to_lowercase().contains(&query))
        });
        choices.sort_by_key(|(_, v)| (!v.to_lowercase().starts_with(&query), v.clone()));
        choices
            .into_iter()
            .take(25)
            .map(|(name, value)| json!({"name":crate::ui::clean(&name,100),"value":value}))
            .collect()
    }
    pub fn resolve(&self, provider: Option<&str>, model: &str) -> Result<String> {
        let model = model.trim();
        ensure!(
            !model.is_empty() && model.len() <= 200 && !model.chars().any(char::is_whitespace),
            "Invalid model identifier"
        );
        let entries = self.entries.read().unwrap();
        if let Some(p) = provider {
            ensure!(entries.contains_key(p), "Provider is unavailable: {p}");
            if let Some((explicit, _)) = model.split_once('/') {
                ensure!(
                    explicit == p,
                    "Selected provider does not match the model identifier"
                );
            }
            let full = if model.contains('/') {
                model.into()
            } else {
                format!("{p}/{model}")
            };
            crate::provider::model_parts(&full)?;
            return Ok(full);
        }
        if let Some((p, _)) = model.split_once('/') {
            crate::provider::model_parts(model)?;
            ensure!(entries.contains_key(p), "Provider is unavailable: {p}");
            return Ok(model.into());
        }
        let matching = entries
            .iter()
            .filter(|(_, ms)| ms.iter().any(|m| m.id == model))
            .map(|(p, _)| p)
            .collect::<Vec<_>>();
        match matching.as_slice() {
            [p] => Ok(format!("{p}/{model}")),
            [] => bail!("Choose a provider or enter provider/model-id"),
            _ => bail!("That model exists under multiple providers; select a provider"),
        }
    }
    pub fn reasoning(&self, model: &str, preferred: &str) -> String {
        let entries = self.entries.read().unwrap();
        let metadata = model
            .split_once('/')
            .and_then(|(p, id)| entries.get(p)?.iter().find(|m| m.id == id));
        if let Some(m) = metadata
            && !m.efforts.is_empty()
            && !m.efforts.iter().any(|e| e == preferred)
        {
            return m
                .default_effort
                .iter()
                .chain(m.efforts.iter())
                .find(|e| crate::config::validate_reasoning(e).is_ok())
                .cloned()
                .unwrap_or_else(|| preferred.into());
        }
        preferred.into()
    }
    pub fn set_chat_model(&self, channel: u64, model: &str) {
        self.chats.write().unwrap().1.insert(channel, model.into());
    }
    pub fn effort_choices(&self, channel: u64, options: &Value) -> Vec<Value> {
        let query = options
            .as_array()
            .into_iter()
            .flatten()
            .find(|o| o["focused"] == true)
            .and_then(|o| o["value"].as_str())
            .unwrap_or("")
            .to_lowercase();
        let model = {
            let chats = self.chats.read().unwrap();
            chats.1.get(&channel).unwrap_or(&chats.0).clone()
        };
        self.metadata(&model)
            .map(|m| {
                m.efforts
                    .into_iter()
                    .filter(|e| crate::config::validate_reasoning(e).is_ok() && e.contains(&query))
                    .take(25)
                    .map(|e| json!({"name":e,"value":e}))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn validate_effort(&self, model: &str, effort: &str) -> Result<()> {
        crate::config::validate_reasoning(effort)?;
        if let Some(m) = self.metadata(model)
            && !m.efforts.is_empty()
        {
            ensure!(
                m.efforts.iter().any(|e| e == effort),
                "Unsupported reasoning for {model}; available: {}",
                m.efforts.join(", ")
            );
        }
        Ok(())
    }
    pub(crate) fn metadata(&self, model: &str) -> Option<Model> {
        let (provider, id) = model.split_once('/')?;
        self.entries
            .read()
            .unwrap()
            .get(provider)?
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }
    pub fn advertised_effort(&self, model: &str, requested: &str) -> Result<Option<String>> {
        if let Some(m) = self.metadata(model)
            && !m.efforts.is_empty()
        {
            let effort = if requested == "minimal"
                && !m.efforts.iter().any(|e| e == "minimal")
                && m.efforts.iter().any(|e| e == "low")
            {
                "low"
            } else {
                requested
            };
            self.validate_effort(model, effort)?;
            return Ok(Some(effort.into()));
        }
        Ok(None)
    }
    pub fn report(
        &self,
        provider: Option<&str>,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> Value {
        let entries = self.entries.read().unwrap();
        let query = query.to_lowercase();
        let rows = entries
            .iter()
            .filter(|(p, _)| provider.is_none_or(|v| v == p.as_str()))
            .flat_map(|(p, ms)| ms.iter().map(move |m| (p, m)))
            .filter(|(p, m)| {
                format!("{p}/{} {}", m.id, m.name)
                    .to_lowercase()
                    .contains(&query)
            })
            .collect::<Vec<_>>();
        let limit = limit.clamp(1, 50);
        json!({"providers":entries.iter().map(|(p,ms)|json!({"id":p,"model_count":ms.len()})).collect::<Vec<_>>(),
            "models":rows.iter().skip(offset).take(limit).map(|(p,m)|json!({"id":format!("{p}/{}",m.id),"name":m.name,"reasoning_levels":if m.efforts.is_empty(){Value::Null}else{json!(m.efforts)},"default_reasoning":m.default_effort,"source":m.source,"observed_at":m.observed_at,"pricing":null,"billing":if p.as_str()=="codex"{"ChatGPT subscription; token pricing is not exposed by the catalog"}else{"API; current token pricing is not exposed by the catalog"}})).collect::<Vec<_>>(),
            "total":rows.len(),"next_offset":if offset.saturating_add(limit)<rows.len(){Some(offset+limit)}else{None},
            "note":"Live provider discovery with last-known local snapshots during outages. Unknown reasoning or prices are null. Catalog visibility does not guarantee quota or endpoint access. Use exact provider/model IDs and advertised effort levels when spawning workers."})
    }
}
fn normalize(models: &mut Vec<Model>) {
    models.retain(|m| {
        !m.id.is_empty()
            && m.id.len() <= 200
            && !m
                .id
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '/')
    });
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
}
fn available(auth: &AuthConfig) -> Vec<&'static str> {
    PROVIDERS
        .into_iter()
        .filter(|p| match *p {
            "codex" => auth.inspect().is_ok(),
            "openai" => std::env::var("OPENAI_API_KEY").is_ok_and(|s| !s.trim().is_empty()),
            "anthropic" => std::env::var("ANTHROPIC_API_KEY").is_ok_and(|s| !s.trim().is_empty()),
            _ => false,
        })
        .collect()
}
fn codex_cache(auth: &AuthConfig) -> Vec<Model> {
    use std::io::Read;
    let value = (|| -> Option<Value> {
        let file = std::fs::File::open(auth.home().ok()?.join("models_cache.json")).ok()?;
        serde_json::from_reader(file.take(4_194_304)).ok()
    })()
    .unwrap_or(Value::Null);
    value["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| {
            Some(Model {
                id: m["slug"].as_str()?.into(),
                name: m["display_name"]
                    .as_str()
                    .unwrap_or(m["slug"].as_str()?)
                    .into(),
                efforts: m["supported_reasoning_levels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e["effort"].as_str().map(str::to_owned))
                    .collect(),
                default_effort: m["default_reasoning_level"].as_str().map(str::to_owned),
                adaptive_thinking: false,
                source: "Codex CLI cache".into(),
                observed_at: value["fetched_at"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.timestamp()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_filtered_choices_are_bounded_and_unambiguous() {
        let c = Catalog::default();
        c.replace(
            "codex",
            (0..40)
                .map(|i| Model::plain(&format!("gpt-{i:02}")))
                .collect(),
        );
        c.replace("openai", vec![Model::plain("gpt-00")]);
        let all = c.choices(&json!([{"name":"model","value":"gpt","focused":true}]));
        assert_eq!(all.len(), 25);
        assert!(
            all.iter()
                .all(|v| v["value"].as_str().unwrap().contains('/'))
        );
        let filtered=c.choices(&json!([{"name":"provider","value":"openai"},{"name":"model","value":"GPT","focused":true}]));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["value"], "openai/gpt-00");
        assert!(
            c.resolve(None, "gpt-00")
                .unwrap_err()
                .to_string()
                .contains("multiple")
        );
        assert_eq!(c.resolve(Some("codex"), "gpt-00").unwrap(), "codex/gpt-00");
        assert!(c.resolve(Some("openai"), "codex/gpt-00").is_err());
        assert!(c.resolve(Some("unconfigured"), "gpt-00").is_err());
        assert_eq!(
            c.choices(&json!([{"name":"kind","value":"comp","focused":true}]))[0]["value"],
            "compact"
        );
    }
    #[test]
    fn reasoning_choices_follow_each_chat_and_unknown_metadata_stays_unknown() {
        let c = Catalog::default();
        let mut a = Model::plain("a");
        a.efforts = vec!["low".into(), "high".into()];
        a.default_effort = Some("low".into());
        let mut b = Model::plain("b");
        b.efforts = vec!["medium".into(), "max".into(), "ultra".into()];
        c.replace("codex", vec![a, b, Model::plain("unknown")]);
        c.set_chat_model(1, "codex/a");
        c.set_chat_model(2, "codex/b");
        c.set_chat_model(3, "codex/unknown");
        let options = json!([{"name":"level","value":"","focused":true}]);
        assert_eq!(
            c.effort_choices(1, &options),
            vec![
                json!({"name":"low","value":"low"}),
                json!({"name":"high","value":"high"})
            ]
        );
        assert_eq!(c.effort_choices(2, &options).len(), 3);
        assert!(c.effort_choices(3, &options).is_empty());
        assert!(c.validate_effort("codex/a", "minimal").is_err());
        assert_eq!(c.reasoning("codex/a", "medium"), "low");
        let report = c.report(None, "unknown", 0, 25);
        assert!(report["models"][0]["reasoning_levels"].is_null());
        assert!(report["models"][0]["pricing"].is_null());
    }
    #[test]
    fn catalogs_survive_restart_and_cli_cache_never_advertises_hidden_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.json"),
            json!({"tokens":{"access_token":"private-test-token","account_id":"test"}}).to_string(),
        )
        .unwrap();
        let mut config = Config {
            state_dir: dir.path().into(),
            ..Default::default()
        };
        config.auth.codex_home = Some(dir.path().into());
        let c = Catalog::default();
        c.replace("codex", vec![Model::plain("saved")]);
        c.persist(dir.path()).unwrap();
        let restored = Catalog::default();
        restored.initialize(&config);
        assert_eq!(
            restored.report(Some("codex"), "", 0, 25)["models"][0]["id"],
            "codex/saved"
        );
        std::fs::write(dir.path().join("models_cache.json"),json!({"models":[{"slug":"visible","visibility":"list","supported_reasoning_levels":[{"effort":"low"}]},{"slug":"hidden","visibility":"hide"}]}).to_string()).unwrap();
        restored.initialize(&config);
        assert_eq!(
            restored.report(Some("codex"), "", 0, 25)["models"][0]["id"],
            "codex/saved"
        );
        std::fs::remove_file(dir.path().join("model-catalog.json")).unwrap();
        restored.initialize(&config);
        assert_eq!(restored.report(Some("codex"), "", 0, 25)["total"], 1);
        assert_eq!(
            restored.report(Some("codex"), "", 0, 25)["models"][0]["id"],
            "codex/visible"
        );
        restored.persist(dir.path()).unwrap();
        assert!(
            !std::fs::read_to_string(dir.path().join("model-catalog.json"))
                .unwrap()
                .contains("private-test-token")
        );
    }
}
