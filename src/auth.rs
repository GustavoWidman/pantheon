//! Read-through Codex credentials. The official CLI owns token rotation/storage.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io::Read, path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::Mutex,
};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub codex_home: Option<PathBuf>,
    pub codex_cli: Option<PathBuf>,
}
impl AuthConfig {
    pub fn home(&self) -> Result<PathBuf> {
        self.codex_home
            .clone()
            .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".codex")))
            .context("set auth.codex_home or CODEX_HOME for a Codex login")
    }
    pub fn reasoning_for_model(&self, model: &str, requested: &str) -> Result<String> {
        crate::config::validate_reasoning(requested)?;
        let Some(slug) = model.strip_prefix("codex/") else {
            return Ok(requested.into());
        };
        let metadata = (|| -> Option<Value> {
            let file = std::fs::File::open(self.home().ok()?.join("models_cache.json")).ok()?;
            let mut bytes = Vec::new();
            file.take(4_194_305).read_to_end(&mut bytes).ok()?;
            if bytes.len() > 4_194_304 {
                return None;
            }
            let value: Value = serde_json::from_slice(&bytes).ok()?;
            value["models"]
                .as_array()?
                .iter()
                .find(|m| m["slug"] == slug)
                .cloned()
        })();
        let levels = metadata
            .as_ref()
            .and_then(|m| m["supported_reasoning_levels"].as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v["effort"].as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if levels.is_empty() {
            return Ok(if requested == "minimal" {
                "low"
            } else {
                requested
            }
            .into());
        }
        if levels.contains(&requested) {
            return Ok(requested.into());
        }
        if requested == "minimal" {
            return Ok(levels
                .iter()
                .find(|v| **v == "low")
                .copied()
                .unwrap_or(levels[0])
                .into());
        }
        bail!(
            "unsupported reasoning effort for {model}; supported: {}",
            levels.join(", ")
        )
    }
    pub fn cli(&self) -> PathBuf {
        self.codex_cli
            .clone()
            .or_else(|| std::env::var_os("PANTHEON_CODEX_CLI").map(PathBuf::from))
            .unwrap_or_else(|| "codex".into())
    }
    pub fn validate(&self) -> Result<()> {
        for path in [&self.codex_home, &self.codex_cli].into_iter().flatten() {
            ensure!(
                !path.as_os_str().is_empty(),
                "Codex paths must not be empty"
            );
        }
        Ok(())
    }
    pub fn inspect(&self) -> Result<()> {
        Credentials::read(&self.home()?).map(|_| ())
    }
    pub async fn login(&self, device_auth: bool) -> Result<()> {
        let home = self.home()?;
        std::fs::create_dir_all(&home)?;
        let mut command = Command::new(self.cli());
        command
            .args(["-c", "cli_auth_credentials_store=\"file\"", "login"])
            .env("CODEX_HOME", home)
            .env_remove("OPENAI_API_KEY")
            .kill_on_drop(true);
        if device_auth {
            command.arg("--device-auth");
        }
        ensure!(
            command
                .status()
                .await
                .context("start official Codex login")?
                .success(),
            "Codex login did not complete"
        );
        self.inspect()
    }
}

// Deliberately no Debug/Serialize: tokens must never enter traces or tool output.
pub(crate) struct Credentials {
    pub access: String,
    pub account: String,
    pub residency: Option<String>,
    expiry: Option<i64>,
}
impl Credentials {
    fn read(home: &std::path::Path) -> Result<Self> {
        let file = std::fs::File::open(home.join("auth.json"))
            .context("Codex file login missing; run codex login with this CODEX_HOME and file credential storage")?;
        let mut bytes = Vec::new();
        file.take(1_048_577)
            .read_to_end(&mut bytes)
            .context("read Codex login cache")?;
        ensure!(bytes.len() <= 1_048_576, "Codex login cache is too large");
        let value: Value =
            serde_json::from_slice(&bytes).context("invalid Codex login cache JSON")?;
        let tokens = &value["tokens"];
        let access = tokens["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context(
                "Codex login cache has no ChatGPT access token; API-key logins use openai/model",
            )?
            .to_owned();
        let claims = jwt_claims(&access);
        let identity = &claims["https://api.openai.com/auth"];
        let account = tokens["account_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| identity["chatgpt_account_id"].as_str())
            .context("Codex login cache has no ChatGPT account ID")?
            .to_owned();
        let residency = identity["chatgpt_data_residency"]
            .as_str()
            .or_else(|| identity["chatgpt_compute_residency"].as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        for header in [&access, &account].into_iter().chain(residency.iter()) {
            ensure!(
                reqwest::header::HeaderValue::from_str(header).is_ok(),
                "invalid Codex credential header"
            );
        }
        Ok(Self {
            access,
            account,
            residency,
            expiry: claims["exp"].as_i64(),
        })
    }
    fn expiring(&self) -> bool {
        self.expiry
            .is_some_and(|expiry| expiry <= crate::store::now() + 120)
    }
}
fn jwt_claims(token: &str) -> Value {
    token
        .split('.')
        .nth(1)
        .and_then(|s| URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

#[derive(Clone)]
pub(crate) struct CodexAuth {
    config: AuthConfig,
    refresh: Arc<Mutex<()>>,
}
impl CodexAuth {
    pub fn reasoning_for_model(&self, model: &str, requested: &str) -> Result<String> {
        self.config.reasoning_for_model(model, requested)
    }
    pub fn new(config: AuthConfig) -> Self {
        Self {
            config,
            refresh: Arc::default(),
        }
    }
    pub async fn credentials(&self, rejected: Option<&str>) -> Result<Credentials> {
        let _guard = self.refresh.lock().await;
        let home = self.config.home()?;
        let before = Credentials::read(&home)?;
        // Re-read every request. A CLI refresh or relogin can replace the canonical
        // file while Pantheon is running; never keep an independently rotating copy.
        if before.expiring() || rejected.is_some_and(|token| token == before.access) {
            refresh_with_cli(&self.config, &home).await?;
            let after = Credentials::read(&home)?;
            ensure!(
                !after.expiring(),
                "Codex credentials remain expired; sign in again in this CODEX_HOME"
            );
            Ok(after)
        } else {
            Ok(before)
        }
    }
}

async fn refresh_with_cli(config: &AuthConfig, home: &std::path::Path) -> Result<()> {
    let mut child = Command::new(config.cli())
        .args(["-c", "cli_auth_credentials_store=\"file\"", "app-server"])
        .env("CODEX_HOME", home)
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start official Codex auth helper; install codex or set auth.codex_cli")?;
    let mut input = child.stdin.take().context("Codex auth helper stdin")?;
    let mut output = BufReader::new(child.stdout.take().context("Codex auth helper stdout")?);
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        input.write_all(format!("{}\n", json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"pantheon","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":false}}})).as_bytes()).await?;
        rpc_result(&mut output, 1).await?;
        input.write_all(b"{\"method\":\"initialized\"}\n{\"id\":2,\"method\":\"account/read\",\"params\":{\"refreshToken\":true}}\n").await?;
        let value = rpc_result(&mut output, 2).await?;
        ensure!(value["account"]["type"] == "chatgpt", "Codex auth helper has no ChatGPT login");
        Ok::<_, anyhow::Error>(())
    }).await.context("Codex auth refresh timed out; no request replayed")?;
    let _ = child.kill().await;
    let _ = child.wait().await;
    result
}
async fn rpc_result(output: &mut BufReader<tokio::process::ChildStdout>, id: u64) -> Result<Value> {
    use tokio::io::AsyncReadExt;
    loop {
        let mut bytes = Vec::new();
        // Bound individual notifications as well as the entire exchange timeout.
        output.take(1_048_577).read_until(b'\n', &mut bytes).await?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= 1_048_576 && bytes.last() == Some(&b'\n'),
            "Codex auth helper closed or sent an oversized response"
        );
        let value: Value = serde_json::from_slice(&bytes).context("invalid Codex auth RPC JSON")?;
        if value["id"] == id {
            if value.get("error").is_some() {
                // RPC errors can contain authentication diagnostics: never log their bodies.
                bail!("Codex auth refresh failed; run codex login in the configured CODEX_HOME");
            }
            return value
                .get("result")
                .cloned()
                .context("missing Codex auth RPC result");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_reasoning_against_local_model_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let auth = AuthConfig {
            codex_home: Some(dir.path().into()),
            codex_cli: None,
        };
        // Missing metadata still avoids the unsupported Codex minimal effort.
        assert_eq!(
            auth.reasoning_for_model("codex/test", "minimal").unwrap(),
            "low"
        );
        std::fs::write(dir.path().join("models_cache.json"),json!({"models":[{"slug":"test","supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}]}]}).to_string()).unwrap();
        assert_eq!(
            auth.reasoning_for_model("codex/test", "minimal").unwrap(),
            "low"
        );
        assert_eq!(
            auth.reasoning_for_model("codex/test", "high").unwrap(),
            "high"
        );
        assert!(
            auth.reasoning_for_model("codex/test", "ultra")
                .unwrap_err()
                .to_string()
                .contains("low, medium, high")
        );
        assert_eq!(
            auth.reasoning_for_model("anthropic/test", "minimal")
                .unwrap(),
            "minimal"
        );
        assert!(auth.reasoning_for_model("codex/test", "invented").is_err());
    }
    #[test]
    fn reads_canonical_cache_and_jwt_account_without_exposing_refresh_token() {
        let directory = tempfile::tempdir().unwrap();
        let claims = json!({"exp":crate::store::now()+600,"https://api.openai.com/auth":{"chatgpt_account_id":"workspace","chatgpt_data_residency":"eu"}});
        let token = format!("x.{}.signature", URL_SAFE_NO_PAD.encode(claims.to_string()));
        std::fs::write(
            directory.path().join("auth.json"),
            json!({"tokens":{"access_token":token,"refresh_token":"private"}}).to_string(),
        )
        .unwrap();
        let credential = Credentials::read(directory.path()).unwrap();
        assert_eq!(credential.account, "workspace");
        assert_eq!(credential.residency.as_deref(), Some("eu"));
        assert!(!credential.expiring());
    }
    #[tokio::test]
    async fn sees_relogin_and_does_not_refresh_a_new_token_after_old_token_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        let save = |token| {
            std::fs::write(
                &path,
                json!({"tokens":{"access_token":token,"account_id":"a"}}).to_string(),
            )
            .unwrap()
        };
        save("old");
        let auth = CodexAuth::new(AuthConfig {
            codex_home: Some(directory.path().into()),
            codex_cli: Some("does-not-exist".into()),
        });
        assert_eq!(auth.credentials(None).await.unwrap().access, "old");
        save("new");
        assert_eq!(auth.credentials(Some("old")).await.unwrap().access, "new");
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn official_helper_protocol_serializes_refresh_and_rereads_canonical_file() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let python = std::env::var_os("PATH")
            .unwrap()
            .to_string_lossy()
            .split(':')
            .map(|p| PathBuf::from(p).join("python3"))
            .find(|p| p.is_file())
            .unwrap();
        let script = directory.path().join("codex-mock");
        std::fs::write(
            &script,
            format!(
                "#!{}\n{}",
                python.display(),
                r#"import json, os, sys
from pathlib import Path
home = Path(os.environ['CODEX_HOME'])
for line in sys.stdin:
    request = json.loads(line)
    if request.get('method') == 'initialize':
        assert request['params']['clientInfo']['name'] == 'pantheon'
        print(json.dumps({'id':request['id'],'result':{}}), flush=True)
    elif request.get('method') == 'account/read':
        assert request['params']['refreshToken'] is True
        path = home/'auth.json'
        value = json.loads(path.read_text())
        value['tokens']['access_token'] = 'fresh-token'
        path.write_text(json.dumps(value))
        count = home/'refresh-count'
        count.write_text(str(int(count.read_text())+1 if count.exists() else 1))
        print(json.dumps({'method':'account/updated','params':{'authMode':'chatgpt'}}), flush=True)
        print(json.dumps({'id':request['id'],'result':{'account':{'type':'chatgpt'}}}), flush=True)
"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let token = format!(
            "x.{}.signature",
            URL_SAFE_NO_PAD.encode(json!({"exp":0}).to_string())
        );
        std::fs::write(
            directory.path().join("auth.json"),
            json!({"tokens":{"access_token":token,"account_id":"a","refresh_token":"private"}})
                .to_string(),
        )
        .unwrap();
        let auth = CodexAuth::new(AuthConfig {
            codex_home: Some(directory.path().into()),
            codex_cli: Some(script),
        });
        let values = futures_util::future::join_all((0..6).map(|_| auth.credentials(None))).await;
        assert!(
            values
                .into_iter()
                .all(|v| v.unwrap().access == "fresh-token")
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("refresh-count")).unwrap(),
            "1"
        );
        assert_eq!(
            Credentials::read(directory.path()).unwrap().access,
            "fresh-token"
        );
    }
}
