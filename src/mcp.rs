use anyhow::{Context, Result, bail, ensure};
use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientConfig, GetPromptRequestParams,
        Implementation, PaginatedRequestParams, ReadResourceRequestParams,
    },
    service::{RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub servers: BTreeMap<String, ServerConfig>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub description: String,
    pub command: Option<PathBuf>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub bearer_env: Option<String>,
    /// Server environment variable -> service environment variable, never a literal secret.
    pub env: BTreeMap<String, String>,
    pub inherit_env: Vec<String>,
    pub allowed_channels: Vec<u64>,
    pub workers: bool,
    pub allowed_tools: Vec<String>,
    pub timeout_seconds: u64,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            description: String::new(),
            command: None,
            args: vec![],
            url: None,
            bearer_env: None,
            env: BTreeMap::new(),
            inherit_env: vec!["PATH".into(), "HOME".into(), "LANG".into(), "TMPDIR".into()],
            allowed_channels: vec![],
            workers: true,
            allowed_tools: vec![],
            timeout_seconds: 120,
        }
    }
}
impl McpConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.servers.len() <= 32,
            "at most 32 MCP servers may be configured"
        );
        for (id, s) in &self.servers {
            ensure!(
                valid_id(id),
                "MCP server IDs must be 1-48 ASCII letters, digits, underscores or hyphens"
            );
            ensure!(
                s.description.len() <= 1024,
                "MCP description exceeds 1024 bytes"
            );
            ensure!(
                s.command.is_some() != s.url.is_some(),
                "MCP {id} needs exactly one command or URL"
            );
            ensure!(
                (1..=3600).contains(&s.timeout_seconds),
                "MCP timeout must be 1-3600 seconds"
            );
            if let Some(url) = &s.url {
                let parsed = reqwest::Url::parse(url).context("invalid MCP URL")?;
                ensure!(
                    matches!(parsed.scheme(), "http" | "https")
                        && parsed.host_str().is_some()
                        && parsed.username().is_empty()
                        && parsed.password().is_none()
                        && parsed.fragment().is_none(),
                    "MCP URL must be HTTP(S) without embedded credentials or fragments"
                );
                ensure!(
                    s.env.is_empty() && s.args.is_empty(),
                    "HTTP MCP server cannot use process args or env"
                );
            } else {
                ensure!(
                    s.bearer_env.is_none(),
                    "bearer_env is only supported by HTTP servers"
                );
                ensure!(
                    s.command
                        .as_ref()
                        .is_some_and(|p| !p.as_os_str().is_empty()),
                    "MCP command must not be empty"
                );
            }
            for name in s
                .env
                .keys()
                .chain(s.env.values())
                .chain(s.inherit_env.iter())
                .chain(s.bearer_env.iter())
            {
                ensure!(valid_env(name), "invalid MCP environment variable name");
            }
        }
        Ok(())
    }
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
fn valid_env(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
type Client = RunningService<RoleClient, ClientConfig>;
struct Connection {
    client: Mutex<Option<Client>>,
    used: AtomicU64,
}
struct Entry {
    config: ServerConfig,
    channels: Mutex<HashMap<u64, Arc<Connection>>>,
}
pub struct Mcp {
    servers: BTreeMap<String, Entry>,
    workspace: PathBuf,
    results: PathBuf,
    started: Instant,
}
impl Mcp {
    pub fn new(config: &McpConfig, workspace: &Path, state: &Path) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            servers: config
                .servers
                .iter()
                .map(|(id, cfg)| {
                    (
                        id.clone(),
                        Entry {
                            config: cfg.clone(),
                            channels: Mutex::new(HashMap::new()),
                        },
                    )
                })
                .collect(),
            workspace: workspace.to_owned(),
            results: state.join("mcp/results"),
            started: Instant::now(),
        })
    }
    pub fn servers(&self, channel: u64, child: bool) -> Value {
        json!({"servers":self.servers.iter().filter(|(_,s)|authorized(&s.config,channel,child)).map(|(id,s)|json!({"id":id,"description":s.config.description,"transport":if s.config.url.is_some(){"streamable_http"}else{"stdio"},"workers":s.config.workers})).collect::<Vec<_>>()})
    }
    pub async fn execute(
        &self,
        channel: u64,
        child: bool,
        coordinator: bool,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let action = crate::tools::string(args, "action")?;
        if action == "servers" {
            return Ok(self.servers(channel, child));
        }
        let id = crate::tools::string(args, "server")?;
        let entry = self.servers.get(id).context("unknown MCP server")?;
        ensure!(
            authorized(&entry.config, channel, child),
            "MCP server is unavailable to this agent/channel"
        );
        ensure!(
            !coordinator
                || child
                || matches!(
                    action,
                    "list_tools" | "list_resources" | "list_prompts" | "list_resource_templates"
                ),
            "strict coordinators delegate MCP execution and content retrieval to workers"
        );
        if action == "read_result" {
            return self.read_result(channel, id, args);
        }
        if action == "call" {
            let tool = crate::tools::string(args, "tool")?;
            ensure!(
                entry.config.allowed_tools.is_empty()
                    || entry.config.allowed_tools.iter().any(|t| t == tool),
                "MCP tool is outside this server's configured allowlist"
            );
        }
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(entry.config.timeout_seconds);
        let now = self.started.elapsed().as_secs();
        let connection = {
            let mut channels = entry.channels.lock().await;
            channels.retain(|_, c| {
                Arc::strong_count(c) > 1
                    || now.saturating_sub(c.used.load(Ordering::Relaxed)) < 3600
            });
            if !channels.contains_key(&channel) {
                ensure!(
                    channels.len() < 32,
                    "MCP server has too many active channel sessions"
                );
            }
            channels
                .entry(channel)
                .or_insert_with(|| {
                    Arc::new(Connection {
                        client: Mutex::new(None),
                        used: AtomicU64::new(now),
                    })
                })
                .clone()
        };
        connection.used.store(now, Ordering::Relaxed);
        let mut client = tokio::select! {
            c=connection.client.lock()=>c,
            _=cancel.cancelled()=>bail!("MCP request cancelled before execution"),
            _=tokio::time::sleep_until(deadline)=>bail!("MCP server busy; request not issued"),
        };
        if client.is_none() {
            let connected = tokio::select! {
                c=self.connect(&entry.config)=>c,
                _=cancel.cancelled()=>bail!("MCP connection cancelled; no tool call issued"),
                _=tokio::time::sleep_until(deadline)=>bail!("MCP connection timed out; no tool call issued"),
            }?;
            *client = Some(connected);
        }
        let result = tokio::select! {
            r=invoke(client.as_ref().unwrap(),action,args)=>r,
            _=cancel.cancelled()=>Err(anyhow::anyhow!("MCP request cancelled; inspect effects before retrying")),
            _=tokio::time::sleep_until(deadline)=>Err(anyhow::anyhow!("MCP request timed out; inspect effects before retrying")),
        };
        connection
            .used
            .store(self.started.elapsed().as_secs(), Ordering::Relaxed);
        if result.is_err()
            && let Some(c) = client.take()
        {
            c.cancellation_token().cancel();
        }
        let mut value = result?;
        if action == "list_tools"
            && !entry.config.allowed_tools.is_empty()
            && let Some(tools) = value["tools"].as_array_mut()
        {
            tools.retain(|t| {
                t["name"]
                    .as_str()
                    .is_some_and(|name| entry.config.allowed_tools.iter().any(|t| t == name))
            });
        }
        self.render_result(channel, id, value)
    }
    async fn connect(&self, config: &ServerConfig) -> Result<Client> {
        let handler = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("pantheon", env!("CARGO_PKG_VERSION")),
        );
        if let Some(command) = &config.command {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(&config.args)
                .current_dir(&self.workspace)
                .env_clear()
                .kill_on_drop(true);
            for key in &config.inherit_env {
                if let Some(value) = std::env::var_os(key) {
                    cmd.env(key, value);
                }
            }
            for (key, source) in &config.env {
                cmd.env(
                    key,
                    std::env::var_os(source)
                        .with_context(|| format!("missing MCP environment variable {source}"))?,
                );
            }
            let mut wrapped = process_wrap::tokio::CommandWrap::from(cmd);
            wrapped.wrap(process_wrap::tokio::KillOnDrop);
            #[cfg(unix)]
            wrapped.wrap(process_wrap::tokio::ProcessGroup::leader());
            let (transport, _) = TokioChildProcess::builder(wrapped)
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("start MCP command")?;
            handler.serve(transport).await.map_err(|_| {
                anyhow::anyhow!(
                    "MCP initialization failed; check server command, protocol and credentials"
                )
            })
        } else {
            let mut transport_config =
                StreamableHttpClientTransportConfig::with_uri(config.url.clone().unwrap());
            transport_config.max_sse_event_size = 2_097_152;
            transport_config.reinit_on_expired_session = false;
            if let Some(key) = &config.bearer_env {
                transport_config = transport_config.auth_header(
                    std::env::var(key)
                        .with_context(|| format!("missing MCP environment variable {key}"))?,
                );
            }
            let _ = rustls::crypto::ring::default_provider().install_default();
            let http = reqwest_mcp::Client::builder()
                .redirect(reqwest_mcp::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(20))
                .build()?;
            let transport = StreamableHttpClientTransport::with_client(http, transport_config);
            handler.serve(transport).await.map_err(|_| {
                anyhow::anyhow!(
                    "MCP initialization failed; check endpoint, protocol and bearer credentials"
                )
            })
        }
    }
    fn render_result(&self, channel: u64, server: &str, value: Value) -> Result<Value> {
        let text = serde_json::to_string(&value)?;
        ensure!(
            text.len() <= 2_097_152,
            "MCP result exceeds 2 MiB; operation may have completed, inspect effects before retrying"
        );
        if text.chars().count() <= 24_000 {
            return Ok(value);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.results.join(channel.to_string()).join(server);
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(dir.join(format!("{id}.json")))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::File::open(&dir)?.sync_all()?;
        Ok(
            json!({"result_id":id,"isError":value["isError"],"truncated":true,"preview":text.chars().take(8000).collect::<String>(),"next_offset":8000,"total_chars":text.chars().count(),"retrieve":"Use mcp read_result with this server/result_id and next_offset. Full result is retained privately."}),
        )
    }
    fn read_result(&self, channel: u64, server: &str, args: &Value) -> Result<Value> {
        let id = crate::tools::string(args, "result_id")?;
        uuid::Uuid::parse_str(id).context("invalid MCP result ID")?;
        let text = std::fs::read_to_string(
            self.results
                .join(channel.to_string())
                .join(server)
                .join(format!("{id}.json")),
        )
        .context("MCP result is unavailable in this channel")?;
        let offset = args
            .get("offset")
            .map(|v| v.as_u64().context("offset must be nonnegative"))
            .transpose()?
            .unwrap_or(0) as usize;
        let total = text.chars().count();
        ensure!(offset <= total, "offset exceeds result length");
        let chunk: String = text.chars().skip(offset).take(8000).collect();
        let next = offset + chunk.chars().count();
        Ok(
            json!({"result_id":id,"text":chunk,"next_offset":if next<total{Some(next)}else{None},"total_chars":total}),
        )
    }
}
fn authorized(s: &ServerConfig, channel: u64, child: bool) -> bool {
    (!child || s.workers)
        && (s.allowed_channels.is_empty() || s.allowed_channels.contains(&channel))
}
async fn invoke(client: &Client, action: &str, args: &Value) -> Result<Value> {
    let cursor = args
        .get("cursor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let page = cursor.map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
    macro_rules! response {($call:expr)=>{match $call.await {Ok(value)=>Ok(serde_json::to_value(value)?),Err(ServiceError::McpError(error))=>Ok(json!({"isError":true,"error":error})),Err(_)=>bail!("MCP transport/request failed; inspect effects before retrying")}};}
    match action {
        "list_tools" => response!(client.list_tools(page)),
        "list_resources" => response!(client.list_resources(page)),
        "list_resource_templates" => response!(client.list_resource_templates(page)),
        "list_prompts" => response!(client.list_prompts(page)),
        "call" => {
            let arguments = args
                .get("arguments")
                .map(|v| {
                    v.as_object()
                        .cloned()
                        .context("MCP arguments must be an object")
                })
                .transpose()?
                .unwrap_or_default();
            response!(
                client.call_tool(
                    CallToolRequestParams::new(crate::tools::string(args, "tool")?.to_owned())
                        .with_arguments(arguments)
                )
            )
        }
        "read_resource" => response!(client.read_resource(ReadResourceRequestParams::new(
            crate::tools::string(args, "uri")?.to_owned()
        ))),
        "get_prompt" => {
            let mut request =
                GetPromptRequestParams::new(crate::tools::string(args, "prompt")?.to_owned());
            if let Some(arguments) = args.get("arguments") {
                request = request.with_arguments(
                    arguments
                        .as_object()
                        .cloned()
                        .context("prompt arguments must be an object")?,
                );
            }
            response!(client.get_prompt(request))
        }
        _ => bail!("unknown MCP action"),
    }
}
