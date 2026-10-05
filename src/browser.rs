//! One durable browser profile, separately owned windows, authenticated viewers and handoff.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
    pub port_start: u16,
    pub port_end: u16,
    pub display_start: u16,
    pub max_windows: usize,
    pub novnc_web: PathBuf,
    pub python: PathBuf,
    pub worker: PathBuf,
    pub startup_timeout_secs: u64,
    pub action_timeout_secs: u64,
}
impl Default for BrowserConfig {
    fn default() -> Self {
        let env = |key, fallback| {
            std::env::var_os(key)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(fallback))
        };
        Self {
            port_start: 6080,
            port_end: 6180,
            display_start: 100,
            max_windows: 16,
            novnc_web: env("PANTHEON_NOVNC_WEB", "/usr/share/novnc"),
            python: env("PANTHEON_BROWSER_PYTHON", "python3"),
            worker: env("PANTHEON_BROWSER_WORKER", "scripts/browser-worker.py"),
            startup_timeout_secs: 90,
            action_timeout_secs: 45,
        }
    }
}
#[derive(Clone)]
pub struct BrowserManager {
    root: PathBuf,
    config: BrowserConfig,
    sessions: Arc<Mutex<BTreeMap<String, Arc<Mutex<Session>>>>>,
    open_lock: Arc<Mutex<()>>,
    backend: Arc<Mutex<Option<SharedBackend>>>,
    stopping: Arc<AtomicBool>,
}
struct SharedBackend {
    process: Child,
    input: ChildStdin,
    process_group_id: i32,
}
impl Drop for SharedBackend {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            kill(-self.process_group_id, 9);
        }
    }
}
struct Session {
    id: String,
    owner: String,
    port: u16,
    token: String,
    lease: Option<String>,
    poisoned: bool,
    process_group_id: i32,
    group_terminated: bool,
    process: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
// A process group keeps Xvfb, VNC, Playwright and browser descendants from surviving the supervisor.
#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}
impl Drop for Session {
    fn drop(&mut self) {
        #[cfg(unix)]
        if !self.group_terminated {
            // The worker is started as its own process group, never the harness group.
            unsafe {
                kill(-self.process_group_id, 9);
            }
        }
    }
}
impl Session {
    fn authorize(&self, owner: &str, action: &str) -> Result<()> {
        if self.owner != owner {
            bail!("browser belongs to a different agent");
        }
        if self.poisoned && action != "close" {
            bail!("browser worker timed out; close and reopen its persistent profile");
        }
        if self.lease.is_some() && !matches!(action, "resume" | "handoff" | "close") {
            bail!(
                "browser automation is paused for human handoff; explicitly resume with the lease token"
            );
        }
        Ok(())
    }
    async fn request(&mut self, request: &Value, duration: Duration) -> Result<Value> {
        let result = tokio::time::timeout(duration, async {
            self.input
                .write_all(serde_json::to_string(request)?.as_bytes())
                .await?;
            self.input.write_all(b"\n").await?;
            self.input.flush().await?;
            let mut line = String::new();
            if self.output.read_line(&mut line).await? == 0 {
                bail!("browser worker exited");
            }
            let reply: Value =
                serde_json::from_str(&line).context("invalid browser worker response")?;
            if let Some(error) = reply.get("error").and_then(Value::as_str) {
                bail!("{error}");
            }
            Ok(reply)
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                // A timed-out frame must never be mistaken for the next action's reply.
                self.poisoned = true;
                bail!("browser action timed out; close and reopen its persistent profile")
            }
        }
    }
    fn info(&self) -> Value {
        json!({"browser_id": self.id, "state": if self.lease.is_some() { "human" } else { "agent" },
            "view_urls": candidate_urls(self.port, &self.token),
            "reachability": "candidate interface addresses; remote firewall/routing reachability is not verified",
            "profile_persistent": true, "profile": "pantheon-shared", "profile_shared": true,
            "view_only": self.lease.is_none()})
    }
}
impl BrowserManager {
    pub fn new(root: PathBuf, config: BrowserConfig) -> Self {
        Self {
            root,
            config,
            sessions: Arc::default(),
            open_lock: Arc::default(),
            backend: Arc::default(),
            stopping: Arc::default(),
        }
    }
    pub async fn claim_owner(&self, id: &str, from: &str, to: &str) -> Result<Value> {
        let session = self
            .sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .context("unknown browser_id")?;
        let mut session = session.lock().await;
        if session.owner != from {
            bail!("browser is not available for adoption");
        }
        let directory = self.root.join(id);
        let temporary = directory.join(format!(".owner-{}.tmp", Uuid::new_v4()));
        tokio::fs::write(&temporary, serde_json::to_vec(to)?).await?;
        std::fs::File::open(&temporary)?.sync_all()?;
        tokio::fs::rename(&temporary, directory.join("owner.json")).await?;
        std::fs::File::open(&directory)?.sync_all()?;
        session.owner = to.to_owned();
        Ok(session.info())
    }
    /// Adopt a completed child's live browsers without invalidating viewers or human leases.
    pub async fn transfer_owner(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let sessions: Vec<_> = self.sessions.lock().await.values().cloned().collect();
        for session in sessions {
            let mut session = session.lock().await;
            if session.owner != from {
                continue;
            }
            let directory = self.root.join(&session.id);
            let owner = to.to_owned();
            // File replacement and directory fsync make ownership atomic on disk.
            // Hold the session lock until both durable and live ownership agree.
            tokio::task::spawn_blocking(move || -> Result<()> {
                use std::io::Write;
                let destination = directory.join("owner.json");
                let temporary = directory.join(format!(".owner-{}.tmp", Uuid::new_v4()));
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let outcome = (|| -> Result<()> {
                    let mut file = options.open(&temporary)?;
                    file.write_all(&serde_json::to_vec(&owner)?)?;
                    file.sync_all()?;
                    std::fs::rename(&temporary, &destination)?;
                    std::fs::File::open(&directory)?.sync_all()?;
                    Ok(())
                })();
                if outcome.is_err() {
                    let _ = std::fs::remove_file(&temporary);
                }
                outcome
            })
            .await
            .context("browser ownership persistence task")??;
            session.owner = to.to_owned();
        }
        Ok(())
    }
    pub async fn execute(&self, owner: &str, request: Value) -> Result<Value> {
        if self.stopping.load(Ordering::Acquire) {
            bail!("browser service is stopping");
        }
        let action = request
            .get("action")
            .and_then(Value::as_str)
            .context("browser action is required")?;
        if action == "open" {
            return self.open(owner, request).await;
        }
        if action == "list" {
            let sessions: Vec<_> = self.sessions.lock().await.values().cloned().collect();
            let mut rows = Vec::new();
            for session in sessions {
                let session = session.lock().await;
                if session.owner == owner {
                    rows.push(session.info());
                }
            }
            return Ok(json!({"browsers": rows}));
        }
        let id = request
            .get("browser_id")
            .and_then(Value::as_str)
            .context("browser_id is required")?;
        let session = self
            .sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .context("unknown browser_id")?;
        let mut session = session.lock().await;
        session.authorize(owner, action)?;
        let deadline = Duration::from_secs(self.config.action_timeout_secs);
        match action {
            "handoff" => {
                // Establish the lease before any I/O: a partially applied handoff
                // must fail closed if the worker or its acknowledgement is lost.
                let lease = session
                    .lease
                    .get_or_insert_with(|| Uuid::new_v4().to_string())
                    .clone();
                session
                    .request(&json!({"action":"handoff"}), deadline)
                    .await?;
                let mut info = session.info();
                info["resume_token"] = json!(lease);
                info["instructions"] = json!(
                    "Automation is paused. Finish in noVNC, then explicitly resume using resume_token."
                );
                Ok(info)
            }
            "resume" => {
                let lease = session
                    .lease
                    .as_ref()
                    .context("browser has no active human handoff")?;
                let token = request
                    .get("resume_token")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                use subtle::ConstantTimeEq;
                if !bool::from(lease.as_bytes().ct_eq(token.as_bytes())) {
                    bail!("invalid resume token");
                }
                session
                    .request(&json!({"action":"resume"}), deadline)
                    .await?;
                session.lease = None;
                Ok(session.info())
            }
            "close" => {
                // Close this window only. The backend flushes the shared profile at shutdown.
                let outcome = session.request(&json!({"action":"close"}), deadline).await;
                #[cfg(unix)]
                {
                    unsafe {
                        kill(-session.process_group_id, 9);
                    }
                    session.group_terminated = true;
                }
                let _ = session.process.wait().await;
                drop(session);
                self.sessions.lock().await.remove(id);
                outcome.map(|_| json!({"closed":id,"profile_retained":true}))
            }
            "navigate" | "snapshot" | "click" | "type" | "screenshot" | "tabs" | "new_tab"
            | "select_tab" | "close_tab" => {
                let result = session.request(&request, deadline).await;
                if result.is_err() && session.process.try_wait()?.is_some() {
                    drop(session);
                    self.sessions.lock().await.remove(id);
                }
                result
            }
            _ => bail!("unknown browser action: {action}"),
        }
    }
    async fn open(&self, owner: &str, request: Value) -> Result<Value> {
        // Serialize reservations, not browser actions. OS bind is the authority for port availability.
        let _reservation = self.open_lock.lock().await;
        if self.stopping.load(Ordering::Acquire) {
            bail!("browser service is stopping");
        }
        if self.config.port_start == 0 || self.config.port_end < self.config.port_start {
            bail!("invalid browser port range");
        }
        if self.config.max_windows == 0 || self.config.max_windows > 256 {
            bail!("max_windows must be 1 to 256");
        }
        let sessions: Vec<_> = self.sessions.lock().await.values().cloned().collect();
        if sessions.len() >= self.config.max_windows {
            bail!("browser window capacity exhausted");
        }
        let mut used = BTreeSet::new();
        for session in sessions {
            used.insert(session.lock().await.port);
        }
        let port = (self.config.port_start..=self.config.port_end)
            .find(|p| !used.contains(p))
            .context("browser port range exhausted")?;
        let id = match request.get("browser_id").and_then(Value::as_str) {
            Some(id) => Uuid::parse_str(id)
                .context("invalid persistent browser_id")?
                .to_string(),
            None => Uuid::new_v4().to_string(),
        };
        if self.sessions.lock().await.contains_key(&id) {
            bail!("browser is already open");
        }
        let profile = self.root.join(&id);
        let ownership = profile.join("owner.json");
        if ownership.exists() {
            let previous: String = serde_json::from_slice(&tokio::fs::read(&ownership).await?)?;
            if previous != owner {
                bail!("persistent browser belongs to a different agent");
            }
        } else if request.get("browser_id").is_some() {
            bail!("unknown persistent browser_id");
        }
        tokio::fs::create_dir_all(&profile).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700)).await?;
        }
        if !ownership.exists() {
            tokio::fs::write(&ownership, serde_json::to_vec(owner)?).await?;
            std::fs::File::open(&ownership)?.sync_all()?;
            std::fs::File::open(&profile)?.sync_all()?;
        }
        self.ensure_backend().await?;
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let mut command = Command::new(&self.config.python);
        command
            .arg(&self.config.worker)
            .arg("--profile")
            .arg(&profile)
            .arg("--shared-root")
            .arg(self.root.join("pantheon-shared"))
            .arg("--port")
            .arg(port.to_string())
            .arg("--port-end")
            .arg(self.config.port_end.to_string())
            .arg("--reserved-ports")
            .arg(
                used.iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            )
            .arg("--web")
            .arg(&self.config.novnc_web)
            .env("PANTHEON_BROWSER_TOKEN", &token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut process = command
            .spawn()
            .context("launch packaged Camoufox worker (use nix run or nix develop)")?;
        let input = process.stdin.take().context("worker stdin")?;
        let output = BufReader::new(process.stdout.take().context("worker stdout")?);
        let process_group_id = process.id().context("worker process id")? as i32;
        let mut session = Session {
            id: id.clone(),
            owner: owner.into(),
            port,
            token,
            lease: None,
            poisoned: false,
            process_group_id,
            group_terminated: false,
            process,
            input,
            output,
        };
        // Startup is one JSON frame; process errors and port collisions fail closed.
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(self.config.startup_timeout_secs),
            session.output.read_line(&mut line),
        )
        .await
        .context("browser startup timeout")??;
        let ready: Value =
            serde_json::from_str(&line).context("browser worker did not become ready")?;
        if let Some(error) = ready.get("error").and_then(Value::as_str) {
            bail!("browser startup: {error}");
        }
        if ready.get("ready") != Some(&Value::Bool(true)) {
            bail!("browser worker did not become ready");
        }
        if let Some(actual) = ready.get("port").and_then(Value::as_u64) {
            if actual < u64::from(port) || actual > u64::from(self.config.port_end) {
                bail!("browser worker selected an out-of-range port");
            }
            session.port = actual as u16;
        }
        if let Some(url) = request.get("url") {
            session
                .request(
                    &json!({"action":"navigate","url":url}),
                    Duration::from_secs(self.config.action_timeout_secs),
                )
                .await?;
        }
        let mut info = session.info();
        info["display"] = ready["display"].clone();
        self.sessions
            .lock()
            .await
            .insert(id, Arc::new(Mutex::new(session)));
        Ok(info)
    }
    async fn ensure_backend(&self) -> Result<()> {
        let mut backend = self.backend.lock().await;
        if let Some(existing) = backend.as_mut()
            && existing.process.try_wait()?.is_none()
        {
            return Ok(());
        }
        *backend = None;
        let root = self.root.join("pantheon-shared");
        tokio::fs::create_dir_all(&root).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let mut command = Command::new(&self.config.python);
        command
            .arg(&self.config.worker)
            .arg("--backend")
            .arg("--shared-root")
            .arg(&root)
            .arg("--capacity")
            .arg(
                self.config
                    .max_windows
                    .min(usize::from(self.config.port_end - self.config.port_start) + 1)
                    .to_string(),
            )
            .arg("--web")
            .arg(&self.config.novnc_web)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut process = command.spawn().context("launch shared Camoufox profile")?;
        let input = process.stdin.take().context("shared browser stdin")?;
        let mut output = BufReader::new(process.stdout.take().context("shared browser stdout")?);
        let process_group_id = process.id().context("shared browser process ID")? as i32;
        let shared = SharedBackend {
            process,
            input,
            process_group_id,
        };
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(self.config.startup_timeout_secs),
            output.read_line(&mut line),
        )
        .await
        .context("shared browser startup timeout")??;
        let reply: Value =
            serde_json::from_str(&line).context("shared browser startup response")?;
        if reply["ready"] != true {
            bail!("shared browser startup: {}", reply["error"]);
        }
        *backend = Some(shared);
        Ok(())
    }
    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        let _reservation = self.open_lock.lock().await;
        // Flush the one profile first. Closing the backend aborts in-flight
        // page operations, so a busy window cannot delay every profile flush.
        if let Some(mut backend) = self.backend.lock().await.take() {
            let _ = backend.input.write_all(b"shutdown\n").await;
            let _ = backend.input.flush().await;
            let _ = tokio::time::timeout(Duration::from_secs(20), backend.process.wait()).await;
        }
        let sessions = std::mem::take(&mut *self.sessions.lock().await);
        for (_, session) in sessions {
            let mut session = session.lock().await;
            #[cfg(unix)]
            {
                unsafe {
                    kill(-session.process_group_id, 9);
                }
                session.group_terminated = true;
            }
            let _ = session.process.wait().await;
        }
    }
}
fn candidate_urls(port: u16, token: &str) -> Vec<String> {
    let mut addresses = BTreeSet::from(["127.0.0.1".to_owned()]);
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if let std::net::IpAddr::V4(ip) = interface.ip()
                && !ip.is_unspecified()
            {
                addresses.insert(ip.to_string());
            }
        }
    }
    addresses.into_iter().map(|ip| format!("http://{ip}:{port}/vnc.html?autoconnect=1&resize=scale&path=websockify%3Ftoken%3D{token}")).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn viewer_urls_include_localhost_and_secret_path() {
        let urls = candidate_urls(6080, "test-secret");
        assert!(
            urls.iter()
                .any(|url| url.starts_with("http://127.0.0.1:6080/"))
        );
        assert!(
            urls.iter()
                .all(|url| url.contains("path=websockify%3Ftoken%3Dtest-secret"))
        );
    }
    #[tokio::test]
    async fn unknown_browser_and_bad_range_fail_without_spawning() {
        let manager = BrowserManager::new(
            PathBuf::from("unused"),
            BrowserConfig {
                port_start: 0,
                ..Default::default()
            },
        );
        assert!(
            manager
                .execute("a", json!({"action":"open"}))
                .await
                .is_err()
        );
        assert!(
            manager
                .execute("a", json!({"action":"close","browser_id":"missing"}))
                .await
                .is_err()
        );
        assert_eq!(
            manager
                .execute("b", json!({"action":"list"}))
                .await
                .unwrap(),
            json!({"browsers":[]})
        );
    }
    #[tokio::test]
    async fn ownership_handoff_and_profile_reopen_are_enforced() {
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("mock.py");
        std::fs::write(
            &worker,
            r#"
import json, sys
print(json.dumps({'ready':True,'display':':123'}),flush=True)
for line in sys.stdin:
    if line.strip()=='shutdown':break
    request=json.loads(line)
    print(json.dumps({'done':True}),flush=True)
    if request['action']=='close':break
"#,
        )
        .unwrap();
        let manager = BrowserManager::new(
            directory.path().join("profiles"),
            BrowserConfig {
                worker,
                ..Default::default()
            },
        );
        let browser = manager
            .execute("root", json!({"action":"open"}))
            .await
            .unwrap();
        let id = browser["browser_id"].as_str().unwrap();
        assert!(
            manager
                .execute("subagent", json!({"action":"snapshot","browser_id":id}))
                .await
                .is_err()
        );
        assert_eq!(
            manager
                .execute("subagent", json!({"action":"list"}))
                .await
                .unwrap()["browsers"],
            json!([])
        );
        let handoff = manager
            .execute("root", json!({"action":"handoff","browser_id":id}))
            .await
            .unwrap();
        assert_eq!(handoff["state"], "human");
        assert!(
            manager
                .execute(
                    "root",
                    json!({"action":"navigate","browser_id":id,"url":"https://example.com"})
                )
                .await
                .is_err()
        );
        assert!(
            manager
                .execute(
                    "root",
                    json!({"action":"resume","browser_id":id,"resume_token":"wrong"})
                )
                .await
                .is_err()
        );
        let resumed = manager
            .execute(
                "root",
                json!({"action":"resume","browser_id":id,"resume_token":handoff["resume_token"]}),
            )
            .await
            .unwrap();
        assert_eq!(resumed["state"], "agent");
        assert_eq!(resumed["view_only"], true);
        manager
            .execute("root", json!({"action":"close","browser_id":id}))
            .await
            .unwrap();
        assert!(
            manager
                .execute("subagent", json!({"action":"open","browser_id":id}))
                .await
                .is_err()
        );
        let reopened = manager
            .execute("root", json!({"action":"open","browser_id":id}))
            .await
            .unwrap();
        assert_eq!(reopened["browser_id"], id);
        assert_ne!(reopened["view_urls"], browser["view_urls"]);
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn completed_child_browsers_are_adopted_without_interrupting_handoff() {
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("mock.py");
        std::fs::write(
            &worker,
            r#"
import json, sys
print(json.dumps({'ready':True,'display':':123'}),flush=True)
for line in sys.stdin:
    if line.strip()=='shutdown':break
    request=json.loads(line)
    print(json.dumps({'done':True}),flush=True)
    if request['action']=='close':break
"#,
        )
        .unwrap();
        let profiles = directory.path().join("profiles");
        let manager = BrowserManager::new(
            profiles.clone(),
            BrowserConfig {
                worker,
                ..Default::default()
            },
        );
        let opened = manager
            .execute("child", json!({"action":"open"}))
            .await
            .unwrap();
        let id = opened["browser_id"].as_str().unwrap();
        assert!(
            manager
                .execute("channel:123", json!({"action":"snapshot","browser_id":id}))
                .await
                .is_err()
        );
        let handoff = manager
            .execute("child", json!({"action":"handoff","browser_id":id}))
            .await
            .unwrap();
        manager
            .transfer_owner("child", "channel:123")
            .await
            .unwrap();
        assert!(
            manager
                .execute("child", json!({"action":"close","browser_id":id}))
                .await
                .is_err()
        );
        let listed = manager
            .execute("channel:123", json!({"action":"list"}))
            .await
            .unwrap();
        assert_eq!(listed["browsers"][0]["view_urls"], opened["view_urls"]);
        assert_eq!(listed["browsers"][0]["state"], "human");
        assert!(
            manager
                .execute("channel:123", json!({"action":"snapshot","browser_id":id}))
                .await
                .is_err()
        );
        let persisted: String =
            serde_json::from_slice(&std::fs::read(profiles.join(id).join("owner.json")).unwrap())
                .unwrap();
        assert_eq!(persisted, "channel:123");
        manager
            .execute(
                "channel:123",
                json!({"action":"resume","browser_id":id,"resume_token":handoff["resume_token"]}),
            )
            .await
            .unwrap();
        manager
            .execute("channel:123", json!({"action":"close","browser_id":id}))
            .await
            .unwrap();
        assert!(
            manager
                .execute("child", json!({"action":"open","browser_id":id}))
                .await
                .is_err()
        );
        manager
            .execute("channel:123", json!({"action":"open","browser_id":id}))
            .await
            .unwrap();
        manager.shutdown().await;
    }
}
