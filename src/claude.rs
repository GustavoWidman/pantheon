//! Sandboxed-ish, opt-in Claude Code subprocess support.
//!
//! This is deliberately a task tool, not a provider adapter: Pantheon owns the
//! conversation, tools and durable history.  Claude receives one explicit task
//! over stdin and produces one untrusted work report.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

use crate::config::ClaudeConfig;

#[cfg(unix)]
struct ProcessGroup(u32);
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const OVERRIDES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_LITELLM",
];

fn command(executable: &str) -> Command {
    let mut command = Command::new(executable);
    // A service may legitimately have an API key for Pantheon's native provider.
    // Do not let that silently turn this plan-authenticated invocation into API
    // billing or another backend.  This affects only the child process.
    for key in OVERRIDES {
        command.env_remove(key);
    }
    command
}

/// Verify that the executable resolved on the daemon's actual PATH is logged in
/// with Claude.ai. This intentionally does not inspect or persist credentials.
pub async fn validate(config: &ClaudeConfig) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    let child = command(&config.executable)
        .arg("auth")
        .arg("status")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("claude.enabled requires `{}` on PATH", config.executable))?;
    let output = tokio::time::timeout(AUTH_TIMEOUT, child.wait_with_output())
        .await
        .context("claude auth status timed out")??;
    ensure!(
        output.status.success(),
        "claude.enabled requires successful `claude auth status`"
    );
    let status: Value = serde_json::from_slice(&output.stdout)
        .context("claude auth status returned malformed JSON")?;
    ensure!(
        status["loggedIn"] == true,
        "claude.enabled requires an authenticated Claude Code login"
    );
    ensure!(
        status["authMethod"] == "claude.ai",
        "claude.enabled requires Claude.ai authentication, not an API key or external backend"
    );
    Ok(())
}

fn drain_limited(
    mut pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    limit: usize,
) -> tokio::task::JoinHandle<Result<String>> {
    tokio::spawn(async move {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = pipe.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            if bytes.len() < limit {
                let remaining = limit - bytes.len();
                bytes.extend_from_slice(&buf[..n.min(remaining)]);
            }
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })
}

/// Run one non-interactive Claude Code task.  `task` is written to stdin rather
/// than included in argv, so user text is never parsed as an option or shell.
pub async fn run(
    config: &ClaudeConfig,
    cwd: &Path,
    task: &str,
    model: &str,
    effort: &str,
    cancel: &CancellationToken,
) -> Result<String> {
    ensure!(
        task.chars().count() <= config.max_task_chars,
        "claude task exceeds configured max_task_chars"
    );
    let mut process = command(&config.executable);
    process
        .arg("-p")
        .arg("--output-format")
        .arg("json")
        .arg("--input-format")
        .arg("text")
        .arg("--model")
        .arg(model)
        .arg("--effort")
        .arg(effort)
        // Blank config is intentional: no project/user CLAUDE.md, hooks,
        // plugins, skills, custom agents or MCP servers are inherited.
        .arg("--safe-mode")
        .arg("--restricted")
        .arg("--strict-mcp-config")
        .arg("--disable-slash-commands")
        .arg("--setting-sources")
        .arg("")
        .arg("--no-session-persistence")
        .arg("--permission-mode")
        .arg("dontAsk")
        .arg("--permission-prompts")
        .arg("none")
        .arg("--tools")
        .arg(config.allowed_tools.join(","))
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process.spawn().context("spawn Claude Code")?;
    #[cfg(unix)]
    let group = child.id().context("Claude Code missing process ID")?;
    #[cfg(unix)]
    let _process_group = ProcessGroup(group);
    let stdout = drain_limited(child.stdout.take().unwrap(), config.max_output_chars);
    // stderr is never returned (it can contain third-party detail); bound it
    // only to determine whether the exit had diagnostic output.
    let stderr = drain_limited(
        child.stderr.take().unwrap(),
        config.max_output_chars.min(4096),
    );
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(task.as_bytes())
        .await
        .context("write Claude task")?;
    stdin.shutdown().await.context("close Claude task input")?;
    // Closing the pipe is the EOF delimiter for `claude -p` stdin mode.
    drop(stdin);
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = tokio::time::sleep(Duration::from_secs(config.timeout_seconds)) => {
            #[cfg(unix)] unsafe { libc::kill(-(group as i32), libc::SIGKILL); }
            child.kill().await.ok();
            bail!("Claude task timed out; side effects may remain")
        }
        _ = cancel.cancelled() => {
            #[cfg(unix)] unsafe { libc::kill(-(group as i32), libc::SIGKILL); }
            child.kill().await.ok();
            bail!("Claude task cancelled; side effects may remain")
        }
    };
    let out = tokio::time::timeout(Duration::from_secs(2), stdout)
        .await
        .context("Claude descendants still hold stdout")???;
    let err = tokio::time::timeout(Duration::from_secs(2), stderr)
        .await
        .context("Claude descendants still hold stderr")???;
    ensure!(
        status.success(),
        "Claude Code exited {}{}",
        status
            .code()
            .map_or("by signal".into(), |code| code.to_string()),
        if err.is_empty() {
            ""
        } else {
            " (stderr was produced)"
        }
    );
    let result: Value =
        serde_json::from_str(&out).context("Claude Code returned malformed JSON")?;
    ensure!(result["is_error"] != true, "Claude Code reported an error");
    let report = result["result"]
        .as_str()
        .context("Claude Code result JSON has no text result")?;
    Ok(crate::memory::cap_tool_result(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn override_list_keeps_native_provider_credentials_scoped_out() {
        assert!(OVERRIDES.contains(&"ANTHROPIC_API_KEY"));
        assert!(OVERRIDES.contains(&"CLAUDE_CODE_USE_BEDROCK"));
    }

    #[cfg(unix)]
    fn fake(script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, path)
    }
    #[cfg(unix)]
    fn test_config(executable: &std::path::Path) -> ClaudeConfig {
        ClaudeConfig {
            enabled: true,
            executable: executable.display().to_string(),
            timeout_seconds: 2,
            ..Default::default()
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn startup_auth_requires_successful_well_formed_claude_ai_login() {
        for (script, expected) in [
            (
                r#"printf '%s' '{"loggedIn":true,"authMethod":"claude.ai"}'"#,
                true,
            ),
            (r#"printf '%s' '{"loggedIn":false}'"#, false),
            ("printf nope", false),
            ("exit 2", false),
        ] {
            let (_dir, path) = fake(script);
            assert_eq!(validate(&test_config(&path)).await.is_ok(), expected);
        }
        let config = test_config(std::path::Path::new("definitely-missing-claude"));
        assert!(validate(&config).await.is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_json_result_is_bounded_and_not_shell_interpolated() {
        let (_dir, path) = fake(
            r#"input=$(cat); test "$input" = 'hello; $(touch nope)' || exit 9; printf '%s' '{"is_error":false,"result":"completed"}'"#,
        );
        let config = test_config(&path);
        let output = run(
            &config,
            std::env::current_dir().unwrap().as_path(),
            "hello; $(touch nope)",
            "opus",
            "medium",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(output, "completed");
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_nonzero_and_timeout_are_failures() {
        let cwd = std::env::current_dir().unwrap();
        for (script, needle) in [
            ("printf nope", "malformed JSON"),
            ("printf err >&2; exit 7", "exited 7"),
            ("sleep 10", "timed out"),
        ] {
            let (_dir, path) = fake(script);
            let mut config = test_config(&path);
            config.timeout_seconds = 1;
            let error = run(
                &config,
                &cwd,
                "x",
                "opus",
                "medium",
                &CancellationToken::new(),
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(error.contains(needle), "{error}");
        }
    }
}
