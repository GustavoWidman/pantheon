use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

pub fn definitions(child: bool) -> Vec<Value> {
    let mut tools = vec![
        tool(
            "zoom",
            "Open a memory node into its two children, or retrieve an original message when n=1.",
            json!({"id":{"type":"integer","minimum":0},"n":{"type":"integer","minimum":1}}),
            &["id", "n"],
        ),
        tool(
            "date",
            "Retrieve a memory message's original timestamp.",
            json!({"id":{"type":"integer","minimum":0}}),
            &["id"],
        ),
        tool(
            "read",
            "Read a UTF-8 file inside the workspace; large results keep head and tail.",
            json!({"path":{"type":"string"}}),
            &["path"],
        ),
        tool(
            "write",
            "Atomically replace a UTF-8 file inside the workspace.",
            json!({"path":{"type":"string"},"text":{"type":"string"}}),
            &["path", "text"],
        ),
        tool(
            "shell",
            "Execute a shell command in the workspace. Bounded by tool timeout. Use a subagent for long background work.",
            json!({"command":{"type":"string"}}),
            &["command"],
        ),
        tool(
            "browser",
            "Control an isolated Camoufox browser. Open creates a browser with a dedicated live noVNC display. Handoff pauses automation until explicit resume with the returned lease token.",
            json!({"action":{"type":"string","enum":["open","list","navigate","snapshot","click","type","screenshot","handoff","resume","close"]},"browser_id":{"type":"string"},"url":{"type":"string"},"selector":{"type":"string"},"role":{"type":"string"},"name":{"type":"string"},"text":{"type":"string"},"resume_token":{"type":"string"}}),
            &["action"],
        ),
    ];
    if !child {
        tools.extend([
            tool("spawn","Start one background subagent per task and return their IDs immediately. Reports arrive together between tool calls or start a fresh turn. Never wait or poll for them. Children cannot spawn.",json!({"tasks":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":8}}),&["tasks"]),
            tool("tell","Send a message to a running subagent; delivered between its tool calls.",json!({"id":{"type":"string"},"message":{"type":"string"}}),&["id","message"]),
            tool("wakeup","Manage durable wakeups. Schedules: in 10m, once ISO8601, every 1h. Wakes preserve this channel and user.",json!({"action":{"type":"string","enum":["add","list","cancel"]},"schedule":{"type":"string"},"prompt":{"type":"string"},"id":{"type":"string"}}),&["action"]),
            tool("monitor","Manage durable change monitors. A command runs at intervals; only changed status/output wakes the agent. Commands have timeout and bounded output.",json!({"action":{"type":"string","enum":["add","list","cancel"]},"command":{"type":"string"},"interval_seconds":{"type":"integer","minimum":5},"id":{"type":"string"}}),&["action"]),
        ]);
    }
    tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    tools
}
fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"name":name,"description":description,"input_schema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}
pub fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .with_context(|| format!("missing string {key}"))
}
pub fn number(args: &Value, key: &str) -> Result<u64> {
    args[key]
        .as_u64()
        .with_context(|| format!("missing nonnegative integer {key}"))
}

pub fn workspace_path(workspace: &Path, input: &str, write: bool) -> Result<PathBuf> {
    let root = workspace.canonicalize()?;
    let p = root.join(input);
    let resolved = if write {
        let parent = p.parent().context("path has no parent")?.canonicalize()?;
        let name = p.file_name().context("path has no name")?;
        let q = parent.join(name);
        if q.exists() { q.canonicalize()? } else { q }
    } else {
        p.canonicalize()?
    };
    ensure!(resolved.starts_with(&root), "path escapes workspace");
    Ok(resolved)
}
// Drain both pipes concurrently with bounded buffers, so verbose tools never exhaust memory.
async fn drain(mut pipe: impl AsyncRead + Unpin) -> Result<String> {
    let mut head = Vec::new();
    let mut tail = std::collections::VecDeque::new();
    let mut count = 0usize;
    let mut buf = [0u8; 8192];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        count += n;
        for b in &buf[..n] {
            if head.len() < 60_000 {
                head.push(*b);
            } else {
                if tail.len() >= 60_000 {
                    tail.pop_front();
                }
                tail.push_back(*b);
            }
        }
    }
    let mut result = String::from_utf8_lossy(&head).into_owned();
    if count > head.len() + tail.len() {
        result.push_str("\n… output omitted …\n");
    }
    result.push_str(&String::from_utf8_lossy(
        &tail.into_iter().collect::<Vec<_>>(),
    ));
    Ok(crate::memory::cap_tool_result(&result))
}
pub async fn read_file(path: &Path) -> Result<String> {
    drain(tokio::fs::File::open(path).await?).await
}
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
pub async fn shell(
    workspace: &Path,
    command: &str,
    timeout: u64,
    cancel: &CancellationToken,
) -> Result<String> {
    ensure!(!command.is_empty(), "empty command");
    let mut process = Command::new("bash");
    process
        .arg("-c")
        .arg(command)
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process.spawn().context("spawn shell")?;
    #[cfg(unix)]
    let _group = ProcessGroup(child.id().context("shell missing process ID")?);
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = tokio::spawn(drain(stdout));
    let err = tokio::spawn(drain(stderr));
    let wait = tokio::select! {
        result=child.wait()=>Some(result?),
        _=tokio::time::sleep(Duration::from_secs(timeout))=>None,
        _=cancel.cancelled()=>None,
    };
    if wait.is_none() {
        #[cfg(unix)]
        if let Some(id) = child.id() {
            unsafe {
                libc::kill(-(id as i32), libc::SIGKILL);
            }
        }
        child.kill().await.ok();
        out.abort();
        err.abort();
        bail!("shell cancelled or timed out; side effects may remain");
    }
    // Descendants can inherit pipes after the shell exits; bound drain time as well.
    let streams = tokio::time::timeout(Duration::from_secs(2), async {
        Ok::<_, anyhow::Error>((out.await??, err.await??))
    })
    .await;
    let (out, err) = streams.context("shell descendants still hold output pipes")??;
    Ok(format!(
        "exit: {}\nstdout:\n{}\nstderr:\n{}",
        wait.unwrap()
            .code()
            .map_or("signal".into(), |c| c.to_string()),
        out,
        err
    ))
}
pub fn schedule(text: &str) -> Result<(i64, Option<i64>)> {
    let (mode, value) = text
        .split_once(' ')
        .context("schedule: in 10m, every 1h, or once ISO8601")?;
    if mode == "once" {
        let date = chrono::DateTime::parse_from_rfc3339(value)?;
        ensure!(
            date.timestamp() > crate::store::now(),
            "schedule is in the past"
        );
        return Ok((date.timestamp(), None));
    }
    ensure!(["in", "every"].contains(&mode), "unknown schedule mode");
    let unit = value.chars().last().context("empty duration")?;
    let multiplier = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => bail!("duration must end in s, m, h or d"),
    };
    let amount: i64 = value[..value.len() - unit.len_utf8()].parse()?;
    ensure!(amount > 0, "duration must be positive");
    let seconds = amount
        .checked_mul(multiplier)
        .context("duration overflow")?;
    ensure!(
        mode != "every" || seconds >= 5,
        "minimum interval is 5 seconds"
    );
    Ok((
        crate::store::now()
            .checked_add(seconds)
            .context("date overflow")?,
        if mode == "every" { Some(seconds) } else { None },
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn symlink_escape_is_denied() {
        let d = tempfile::tempdir().unwrap();
        let x = tempfile::tempdir().unwrap();
        std::fs::write(x.path().join("secret"), "s").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(x.path(), d.path().join("outside")).unwrap();
            assert!(workspace_path(d.path(), "outside/secret", false).is_err());
            assert!(workspace_path(d.path(), "outside/new", true).is_err());
        }
    }
    #[test]
    fn schedules_validate() {
        assert!(schedule("every 0s").is_err());
        assert!(schedule("in -1h").is_err());
        assert_eq!(schedule("every 10m").unwrap().1, Some(600));
    }
    #[tokio::test]
    async fn shell_is_bounded_and_cancelled() {
        let d = tempfile::tempdir().unwrap();
        let c = CancellationToken::new();
        c.cancel();
        assert!(shell(d.path(), "sleep 30", 1, &c).await.is_err());
    }
}
