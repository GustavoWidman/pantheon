//! Sandboxed-ish, opt-in Claude Code subprocess support.
//!
//! This is deliberately a task tool, not a provider adapter: Pantheon owns the
//! conversation, tools and durable history.  Claude receives one explicit task
//! over stdin and produces one untrusted work report.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

use crate::config::ClaudeConfig;

#[cfg(unix)]
struct ProcessGroup {
    id: u32,
    armed: bool,
}
#[cfg(unix)]
impl ProcessGroup {
    fn new(id: u32) -> Self {
        Self { id, armed: true }
    }
    fn kill(&mut self) {
        if self.armed {
            // The child was made leader of its own process group before spawn,
            // so this also cleans up a CLI descendant on timeout/cancellation.
            unsafe { libc::kill(-(self.id as i32), libc::SIGKILL) };
            self.armed = false;
        }
    }
}
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const OVERRIDES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "ANTHROPIC_MODEL",
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
    ensure!(
        config.executable == "claude",
        "claude executable must be `claude` resolved on PATH"
    );
    validate_cli(config).await
}

async fn validate_cli(config: &ClaudeConfig) -> Result<()> {
    let mut process = command(&config.executable);
    process
        .arg("auth")
        .arg("status")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process
        .spawn()
        .context("claude.enabled requires manually installed `claude` on PATH")?;
    #[cfg(unix)]
    let mut group = ProcessGroup::new(child.id().context("Claude auth missing process ID")?);
    let stdout = child.stdout.take().unwrap();
    let (status, stdout) = tokio::time::timeout(AUTH_TIMEOUT, async {
        tokio::try_join!(child.wait(), drain_limited(stdout, 65_536))
    })
    .await
    .context("claude auth status timed out")??;
    #[cfg(unix)]
    group.kill();
    ensure!(
        !stdout.truncated,
        "claude auth status exceeded output limit"
    );
    ensure!(
        status.success(),
        "claude.enabled requires successful `claude auth status`"
    );
    let status: Value = serde_json::from_slice(&stdout.bytes)
        .context("claude auth status returned malformed JSON")?;
    ensure!(
        status["loggedIn"] == true,
        "claude.enabled requires an authenticated Claude Code login"
    );
    ensure!(
        status["authMethod"] == "claude.ai",
        "claude.enabled requires Claude.ai authentication, not an API key or external backend"
    );
    ensure!(
        status["apiProvider"] == "firstParty",
        "claude.enabled requires the first-party Claude Code backend"
    );
    Ok(())
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

// Drain inline, not in detached tasks: dropping the enclosing timeout/cancel
// future closes the pipes immediately even if a descendant kept one open.
async fn drain_limited(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<CapturedOutput> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        truncated |= n > remaining;
        bytes.extend_from_slice(&buf[..n.min(remaining)]);
    }
    Ok(CapturedOutput { bytes, truncated })
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
    ensure!(!task.trim().is_empty(), "claude task must not be empty");
    crate::config::validate_claude_effort(effort)?;
    ensure!(
        !cancel.is_cancelled(),
        "Claude task cancelled before launch"
    );
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
        // CLI administrator-managed policy still applies.
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
        .arg("--allowedTools")
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
    let mut process_group = ProcessGroup::new(group);
    // JSON escaping can use six bytes per character, and metadata is not the
    // text report. Cap the envelope separately and never parse a cut-off JSON
    // document as though the CLI emitted malformed JSON.
    let envelope_limit = config
        .max_output_chars
        .saturating_mul(6)
        .saturating_add(65_536);
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let execution = async {
        let write = async {
            stdin
                .write_all(task.as_bytes())
                .await
                .context("write Claude task")?;
            stdin.shutdown().await.context("close Claude task input")?;
            drop(stdin); // EOF delimits print-mode input.
            Ok::<(), anyhow::Error>(())
        };
        let (_, status, out, err) = tokio::try_join!(
            write,
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
            async { Ok::<_, anyhow::Error>(drain_limited(stdout, envelope_limit).await?) },
            async { Ok::<_, anyhow::Error>(drain_limited(stderr, 4096).await?) },
        )?;
        Ok::<_, anyhow::Error>((status, out, err))
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow::anyhow!("Claude task cancelled; side effects may remain")),
        result = tokio::time::timeout(Duration::from_secs(config.timeout_seconds), execution) => {
            result.context("Claude task timed out; side effects may remain").and_then(|result| result)
        }
    };
    // Clean descendants even on successful direct-child exit. Do this before
    // parsing output, and disarm after the one kill to avoid a later recycled PGID.
    #[cfg(unix)]
    process_group.kill();
    if result.is_err() {
        child.kill().await.ok();
    }
    let (status, out, err) = result?;
    ensure!(
        status.success(),
        "Claude Code exited {}{}",
        status
            .code()
            .map_or("by signal".into(), |code| code.to_string()),
        if err.bytes.is_empty() {
            ""
        } else {
            " (stderr was produced)"
        }
    );
    ensure!(
        !out.truncated,
        "Claude output exceeded JSON envelope limit ({envelope_limit} bytes)"
    );
    let result: Value =
        serde_json::from_slice(&out.bytes).context("Claude Code returned malformed JSON")?;
    ensure!(result["is_error"] != true, "Claude Code reported an error");
    let report = result["result"]
        .as_str()
        .context("Claude Code result JSON has no text result")?;
    let mut report: String = report.chars().take(config.max_output_chars).collect();
    if result["result"].as_str().unwrap().chars().count() > config.max_output_chars {
        report.push_str("\n[Claude report truncated at configured max_output_chars]");
    }
    if let Some(models) = result["modelUsage"].as_object() {
        let models = models
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        report = format!("Model: {models}\n{report}");
    }
    Ok(crate::memory::cap_tool_result(&report))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn override_list_keeps_native_provider_credentials_scoped_out() {
        assert!(OVERRIDES.contains(&"ANTHROPIC_API_KEY"));
        assert!(OVERRIDES.contains(&"CLAUDE_CODE_USE_BEDROCK"));
        let cmd = command("claude");
        let overrides: std::collections::HashMap<_, _> = cmd.as_std().get_envs().collect();
        for key in OVERRIDES {
            assert_eq!(overrides.get(std::ffi::OsStr::new(key)), Some(&None));
        }
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
                r#"printf '%s' '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty"}'"#,
                true,
            ),
            (r#"printf '%s' '{"loggedIn":false}'"#, false),
            (
                r#"printf '%s' '{"loggedIn":true,"authMethod":"apiKey","apiProvider":"firstParty"}'"#,
                false,
            ),
            (
                r#"printf '%s' '{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"bedrock"}'"#,
                false,
            ),
            ("printf nope", false),
            ("exit 2", false),
        ] {
            let (_dir, path) = fake(script);
            assert_eq!(validate_cli(&test_config(&path)).await.is_ok(), expected);
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
    #[cfg(unix)]
    #[tokio::test]
    async fn permission_arguments_are_read_only_by_default_and_broader_tools_are_explicit() {
        let (dir, path) = fake(
            r#"printf '%s\n' "$@" > args; cat >/dev/null; printf '%s' '{"is_error":false,"result":"done"}'"#,
        );
        let mut config = test_config(&path);
        for tools in [
            config.allowed_tools.clone(),
            vec!["Read".into(), "Edit".into(), "Bash".into()],
            vec![],
        ] {
            config.allowed_tools = tools.clone();
            run(
                &config,
                dir.path(),
                "x",
                "opus",
                "low",
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
            assert!(args.contains("--safe-mode\n--restricted\n--strict-mcp-config\n--disable-slash-commands\n--setting-sources\n\n--no-session-persistence"));
            assert!(args.contains("--permission-mode\ndontAsk\n--permission-prompts\nnone"));
            assert!(args.contains(&format!(
                "--tools\n{}\n--allowedTools\n{}\n",
                tools.join(","),
                tools.join(",")
            )));
            assert!(!args.contains("--bare"));
            assert!(!args.contains("bypassPermissions"));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn report_limit_is_not_a_truncated_json_parse_error() {
        let (_dir, path) =
            fake(r#"cat >/dev/null; printf '%s' '{"is_error":false,"result":"abcdefghijk"}'"#);
        let mut config = test_config(&path);
        config.max_output_chars = 10;
        let report = run(
            &config,
            Path::new("."),
            "x",
            "opus",
            "low",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(report.starts_with("abcdefghij\n[Claude report truncated"));
        // A valid envelope exceeding the separate metadata budget is reported
        // explicitly as an output limit, never as malformed upstream JSON.
        let large = format!(
            "cat >/dev/null; printf '%s' '{{\"is_error\":false,\"result\":\"{}\"}}'",
            "a".repeat(66_000)
        );
        let (_dir, path) = fake(&large);
        config.executable = path.display().to_string();
        let error = run(
            &config,
            Path::new("."),
            "x",
            "opus",
            "low",
            &CancellationToken::new(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("exceeded JSON envelope limit"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_and_descendant_pipes_share_timeout_and_cancellation() {
        let cwd = Path::new(".");
        // More than a pipe buffer, and the child never reads it.
        let task = "x".repeat(100_000);
        let (_dir, path) = fake("sleep 30");
        let mut config = test_config(&path);
        config.max_task_chars = task.len();
        config.timeout_seconds = 1;
        let started = tokio::time::Instant::now();
        let error = run(
            &config,
            cwd,
            &task,
            "opus",
            "low",
            &CancellationToken::new(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
        config.timeout_seconds = 30;
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.cancel();
        });
        let error = run(&config, cwd, &task, "opus", "low", &cancel)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("cancelled"), "{error}");
        // Direct child exits, but a descendant holds its stdout: still bounded.
        let (_dir, path) = fake("cat >/dev/null; sleep 30 & exit 0");
        config.executable = path.display().to_string();
        config.timeout_seconds = 1;
        let error = run(&config, cwd, "x", "opus", "low", &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_descendants_and_never_launches_when_already_cancelled() {
        let (dir, path) = fake("cat >/dev/null; sleep 30 & echo $! > descendant.pid; wait");
        let config = test_config(&path);
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let pid_file = dir.path().join("descendant.pid");
        let watch_file = pid_file.clone();
        tokio::spawn(async move {
            while !watch_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            trigger.cancel();
        });
        assert!(
            run(&config, dir.path(), "x", "opus", "low", &cancel)
                .await
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // A killed adopted child can briefly remain a zombie until init reaps it.
        #[cfg(target_os = "linux")]
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let proc_state =
                    std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                if proc_state.is_empty() || proc_state.contains(") Z ") {
                    break;
                }
                // SIGKILL delivery and init reaping are asynchronous; an
                // immediate /proc snapshot can still show the runnable child.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        #[cfg(not(target_os = "linux"))]
        let _ = pid;
        std::fs::remove_file(&pid_file).unwrap();
        assert!(
            run(&config, dir.path(), "x", "opus", "low", &cancel)
                .await
                .is_err()
        );
        assert!(!pid_file.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_auth_output_is_bounded_and_disabled_is_a_noop() {
        let (_dir, path) = fake(&format!("printf '%s' '{}'", "x".repeat(70_000)));
        let mut config = test_config(&path);
        let error = validate_cli(&config).await.unwrap_err().to_string();
        assert!(error.contains("output limit"), "{error}");
        config.enabled = false;
        assert!(validate(&config).await.is_ok());
    }
}
