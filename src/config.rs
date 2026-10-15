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
    pub attachments: crate::attachments::AttachmentConfig,
    pub agent: AgentConfig,
    pub auth: crate::auth::AuthConfig,
    pub web: crate::web::WebConfig,
    pub browser: crate::browser::BrowserConfig,
    pub skills: crate::skills::SkillsConfig,
    pub curator: crate::curator::CuratorConfig,
    pub mcp: crate::mcp::McpConfig,
    pub claude: ClaudeConfig,
    pub zai: crate::provider::zai::Config,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClaudeConfig {
    /// Opt-in because this invokes the separately installed, logged-in CLI.
    pub enabled: bool,
    /// Resolved only through the daemon's PATH; do not point at a credential wrapper.
    pub executable: String,
    pub model: String,
    pub effort: String,
    pub max_jobs: usize,
    pub timeout_seconds: u64,
    pub max_task_chars: usize,
    pub max_output_chars: usize,
    pub allowed_tools: Vec<String>,
}
impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            executable: "claude".into(),
            model: "opus".into(),
            effort: "medium".into(),
            max_jobs: 2,
            timeout_seconds: 900,
            max_task_chars: 12_000,
            max_output_chars: 30_000,
            allowed_tools: vec!["Read".into(), "Glob".into(), "Grep".into()],
        }
    }
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
            attachments: Default::default(),
            agent: AgentConfig::default(),
            auth: Default::default(),
            web: Default::default(),
            browser: Default::default(),
            skills: Default::default(),
            curator: Default::default(),
            mcp: Default::default(),
            claude: Default::default(),
            zai: Default::default(),
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
        config.zai.validate()?;
        config.attachments.validate()?;
        config.web.validate()?;
        config.skills.validate()?;
        config.curator.validate()?;
        if config.curator.enabled {
            crate::curator::effective_model(
                &config.agent.model,
                &config.agent.model,
                config.curator.model.as_deref(),
                None,
            )?;
        }
        config.mcp.validate()?;
        ensure!(
            config.claude.max_jobs > 0
                && config.claude.timeout_seconds > 0
                && config.claude.max_task_chars > 0
                && config.claude.max_output_chars > 0,
            "claude limits must be positive"
        );
        ensure!(
            config.claude.max_jobs <= 32
                && config.claude.timeout_seconds <= 86400
                && config.claude.max_task_chars <= 100_000
                && config.claude.max_output_chars <= 1_000_000,
            "claude limits exceed hard bounds (32 jobs, 86400 seconds, 100000 task chars, 1000000 output chars)"
        );
        ensure!(
            config.claude.executable == "claude",
            "claude.executable must be `claude` resolved on the service PATH, not a path or wrapper"
        );
        ensure!(
            !config.claude.model.trim().is_empty(),
            "claude.model must not be empty"
        );
        validate_claude_effort(&config.claude.effort)?;
        ensure!(
            !config
                .claude
                .allowed_tools
                .iter()
                .any(|tool| tool.trim().is_empty()),
            "claude.allowed_tools must not contain empty names"
        );
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
/// Claude Code's CLI accepts a narrower effort vocabulary than Pantheon's
/// providers. Keep this separate so an otherwise valid agent value (such as
/// `ultra`) cannot create a durable Claude job that fails only after launch.
pub fn validate_claude_effort(s: &str) -> Result<()> {
    ensure!(
        ["low", "medium", "high", "xhigh", "max"].contains(&s),
        "claude effort must be low, medium, high, xhigh or max"
    );
    Ok(())
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
    fn claude_is_default_off_and_its_limits_are_validated_without_resolving_a_cli() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pantheon.toml");
        std::fs::write(&path, "[discord]\napplication_id=1\nallowed_users=[2]\n").unwrap();
        assert!(!Config::load(&path).unwrap().claude.enabled);
        std::fs::write(
            &path,
            "[discord]\napplication_id=1\nallowed_users=[2]\n[claude]\nmax_jobs=0\n",
        )
        .unwrap();
        assert!(Config::load(&path).is_err());
        std::fs::write(
            &path,
            "[discord]\napplication_id=1\nallowed_users=[2]\n[claude]\neffort='ultra'\n",
        )
        .unwrap();
        assert!(Config::load(&path).is_err());
        assert!(validate_claude_effort("high").is_ok());
        assert!(validate_claude_effort("none").is_err());
        for value in ["/home/agent/.local/bin/claude", "other-wrapper"] {
            std::fs::write(&path, format!("[discord]\napplication_id=1\nallowed_users=[2]\n[claude]\nexecutable='{value}'\n")).unwrap();
            assert!(Config::load(&path).is_err());
        }
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            assert!(validate_claude_effort(effort).is_ok());
        }
        for effort in ["none", "minimal", "ultra", ""] {
            assert!(validate_claude_effort(effort).is_err());
        }
    }
    #[test]
    fn startup_glm_requires_explicit_maintenance_route_or_disabled_curator() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let base = "[discord]\napplication_id=1\nallowed_users=[2]\n[agent]\nmodel='zai/glm-5.3'\n[zai]\nenabled=true\n";
        std::fs::write(&path, base).unwrap();
        assert!(
            Config::load(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("implicit GLM")
        );
        for extra in [
            "[curator]\nenabled=false\n",
            "[curator]\nmodel='openai/explicit'\n",
            "[curator]\nmodel='zai/glm-5.3'\n",
        ] {
            std::fs::write(&path, format!("{base}{extra}")).unwrap();
            assert!(Config::load(&path).is_ok());
        }
    }
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
