use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub state_dir: PathBuf,
    pub workspace: PathBuf,
    pub instructions_file: Option<PathBuf>,
    pub discord: DiscordConfig,
    pub agent: AgentConfig,
    pub browser: crate::browser::BrowserConfig,
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
    pub max_steps: usize,
    pub max_subagents: usize,
    pub request_timeout_seconds: u64,
    pub tool_timeout_seconds: u64,
    pub show_reasoning: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            state_dir: "state".into(),
            workspace: ".".into(),
            instructions_file: None,
            discord: DiscordConfig::default(),
            agent: AgentConfig::default(),
            browser: Default::default(),
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
            max_steps: 128,
            max_subagents: 8,
            request_timeout_seconds: 300,
            tool_timeout_seconds: 120,
            show_reasoning: false,
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
            config.agent.max_steps > 0 && config.agent.max_subagents > 0,
            "agent limits must be positive"
        );
        ensure!(
            config.agent.tool_timeout_seconds > 0 && config.agent.request_timeout_seconds > 0,
            "timeouts must be positive"
        );
        crate::provider::model_parts(&config.agent.model)?;
        crate::provider::model_parts(&config.agent.compactor_model)?;
        validate_reasoning(&config.agent.reasoning)?;
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
        ["none", "minimal", "low", "medium", "high", "xhigh"].contains(&s),
        "reasoning must be none, minimal, low, medium, high or xhigh"
    );
    Ok(())
}
