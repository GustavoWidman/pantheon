use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub state_dir: PathBuf,
    pub workspace: PathBuf,
    pub instructions_file: Option<PathBuf>,
    pub discord: DiscordConfig,
    pub agent: AgentConfig,
    pub auth: crate::auth::AuthConfig,
    pub web: crate::web::WebConfig,
    pub browser: crate::browser::BrowserConfig,
    pub skills: crate::skills::SkillsConfig,
    pub curator: crate::curator::CuratorConfig,
    pub mcp: crate::mcp::McpConfig,
}
#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct DiscordConfig {
    pub application_id: u64,
    pub allowed_users: Vec<u64>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub model: String,
    pub compactor_model: String,
    pub reasoning: String,
    pub view_bytes: usize,
    pub context_windows: BTreeMap<String, u64>,
    pub max_steps: usize,
    pub max_subagents: usize,
    pub agent_idle_seconds: u64,
    pub request_timeout_seconds: u64,
    pub request_max_attempts: usize,
    pub request_backoff_seconds: u64,
    pub tool_timeout_seconds: u64,
    pub show_reasoning: bool,
    pub coordinator_root: bool,
    pub shell_background_after_seconds: u64,
    pub max_shell_jobs: usize,
    pub shell_timeout_seconds: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            state_dir: "state".into(),
            workspace: ".".into(),
            instructions_file: None,
            discord: DiscordConfig::default(),
            agent: AgentConfig::default(),
            auth: Default::default(),
            web: Default::default(),
            browser: Default::default(),
            skills: Default::default(),
            curator: Default::default(),
            mcp: Default::default(),
        }
    }
}
impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "openai/gpt-5.6".into(),
            compactor_model: "openai/gpt-5-mini".into(),
            reasoning: "medium".into(),
            view_bytes: 128_000,
            context_windows: BTreeMap::new(),
            max_steps: 128,
            max_subagents: 8,
            agent_idle_seconds: 3600,
            request_timeout_seconds: 300,
            request_max_attempts: 4,
            request_backoff_seconds: 2,
            tool_timeout_seconds: 120,
            show_reasoning: false,
            coordinator_root: false,
            shell_background_after_seconds: 5,
            max_shell_jobs: 16,
            shell_timeout_seconds: 3600,
        }
    }
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(&std::fs::read_to_string(path).context("read config")?)
            .context("parse config")?;
        ensure!(
            config.discord.application_id != 0,
            "discord.application_id must be set"
        );
        ensure!(
            !config.discord.allowed_users.is_empty(),
            "discord.allowed_users must explicitly authorize users"
        );
        ensure!(
            config.agent.view_bytes >= 1024,
            "view_bytes must be >= 1024"
        );
        ensure!(
            config.agent.max_steps > 0
                && config.agent.max_subagents > 0
                && config.agent.max_shell_jobs > 0,
            "agent limits must be positive"
        );
        ensure!(
            config.agent.tool_timeout_seconds > 0
                && config.agent.request_timeout_seconds > 0
                && config.agent.shell_timeout_seconds > 0
                && config.agent.agent_idle_seconds > 0
                && config.agent.agent_idle_seconds <= i64::MAX as u64,
            "timeouts must be positive"
        );
        crate::provider::model_parts(&config.agent.model)?;
        crate::provider::validate_request_retries(
            config.agent.request_max_attempts,
            config.agent.request_backoff_seconds,
        )?;
        crate::provider::model_parts(&config.agent.compactor_model)?;
        validate_reasoning(&config.agent.reasoning)?;
        for (model, tokens) in &config.agent.context_windows {
            crate::provider::model_parts(model)?;
            ensure!(*tokens > 0, "context window limits must be positive");
        }
        config.auth.validate()?;
        config.web.validate()?;
        config.skills.validate()?;
        config.curator.validate()?;
        config.mcp.validate()?;
        Ok(config)
    }
    pub fn instructions(&self) -> Result<String> {
        self.instructions_file
            .as_ref()
            .map(|p| std::fs::read_to_string(p).context("read user instructions"))
            .transpose()
            .map(|x| x.unwrap_or_default())
    }
}
pub fn validate_reasoning(s: &str) -> Result<()> {
    ensure!(
        [
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"
        ]
        .contains(&s),
        "reasoning must be none, minimal, low, medium, high, xhigh, max or ultra"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_configs_inherit_bounded_retries_and_operator_values_are_validated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pantheon.toml");
        let old = "[discord]\napplication_id = 1\nallowed_users = [2]\n";
        std::fs::write(&path, old).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.agent.request_max_attempts, 4);
        assert_eq!(config.agent.request_backoff_seconds, 2);
        for (attempts, backoff, valid) in [
            (1, 1, true),
            (5, 10, true),
            (10, 60, true),
            (0, 2, false),
            (11, 2, false),
            (4, 0, false),
            (4, 61, false),
        ] {
            std::fs::write(&path, format!("{old}\n[agent]\nrequest_max_attempts={attempts}\nrequest_backoff_seconds={backoff}\n")).unwrap();
            let config = Config::load(&path);
            assert_eq!(
                config.is_ok(),
                valid,
                "{attempts} attempts, {backoff}s delay"
            );
            if let Ok(config) = config {
                assert_eq!(config.agent.request_max_attempts, attempts);
                assert_eq!(config.agent.request_backoff_seconds, backoff);
            }
        }
    }
}
