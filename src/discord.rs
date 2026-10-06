//! Discord transport. The durable inbox/outbox and turn ownership live above this layer.
use std::{
    collections::{HashSet, VecDeque},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Method};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    sync::mpsc,
    task::JoinSet,
    time::{Instant, sleep},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

const API: &str = "https://discord.com/api/v10";
const MESSAGE_LIMIT: usize = 2000;

#[derive(Clone)]
pub enum Inbound {
    Prompt {
        id: String,
        channel: u64,
        user: u64,
        text: String,
    },
    Command {
        id: String,
        token: String,
        channel: u64,
        user: u64,
        name: String,
        options: Value,
    },
}

impl std::fmt::Debug for Inbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prompt {
                id, channel, user, ..
            } => f
                .debug_struct("Prompt")
                .field("id", id)
                .field("channel", channel)
                .field("user", user)
                .finish_non_exhaustive(),
            Self::Command {
                id,
                channel,
                user,
                name,
                ..
            } => f
                .debug_struct("Command")
                .field("id", id)
                .field("channel", channel)
                .field("user", user)
                .field("name", name)
                .finish_non_exhaustive(),
        }
    }
}

// Deliberately no Debug: tokens must never appear in logs.
pub struct Discord {
    token: String,
    application_id: u64,
    allowed_users: HashSet<u64>,
    bot_id: AtomicU64,
    client: Client,
    api: String,
    ingress: Option<Ingress>,
}

#[derive(Default, Serialize, Deserialize)]
struct Session {
    id: Option<String>,
    sequence: Option<u64>,
    resume_url: Option<String>,
    bot_id: u64,
}

/// Operational ingress only; canonical conversation history remains in the runtime memory log.
struct Ingress {
    db: Mutex<Connection>,
}
impl Ingress {
    fn open(path: &Path, application_id: u64) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS gateway_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1),data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS gateway_identity(singleton INTEGER PRIMARY KEY CHECK(singleton=1),application TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS gateway_pending(id TEXT PRIMARY KEY,channel TEXT NOT NULL,user TEXT NOT NULL,text TEXT NOT NULL);")?;
        db.execute(
            "INSERT OR IGNORE INTO gateway_identity(singleton,application) VALUES(1,?1)",
            [application_id.to_string()],
        )?;
        let application: String = db.query_row(
            "SELECT application FROM gateway_identity WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if application != application_id.to_string() {
            bail!("Discord ingress state belongs to another application");
        }
        Ok(Self { db: Mutex::new(db) })
    }
    fn load(&self) -> Result<(Session, VecDeque<Inbound>)> {
        let db = self
            .db
            .lock()
            .map_err(|_| anyhow!("Discord ingress lock poisoned"))?;
        let state: Option<String> = db
            .query_row(
                "SELECT data FROM gateway_state WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let session = state
            .map(|data| serde_json::from_str(&data))
            .transpose()?
            .unwrap_or_default();
        let mut statement =
            db.prepare("SELECT id,channel,user,text FROM gateway_pending ORDER BY rowid")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let pending = rows
            .map(|row| {
                let (id, channel, user, text) = row?;
                Ok(Inbound::Prompt {
                    id,
                    channel: channel.parse()?,
                    user: user.parse()?,
                    text,
                })
            })
            .collect::<Result<_>>()?;
        Ok((session, pending))
    }
    fn commit(&self, session: &Session, prompt: Option<&Inbound>) -> Result<()> {
        let mut db = self
            .db
            .lock()
            .map_err(|_| anyhow!("Discord ingress lock poisoned"))?;
        let transaction = db.transaction()?;
        if let Some(Inbound::Prompt {
            id,
            channel,
            user,
            text,
        }) = prompt
        {
            transaction.execute(
                "INSERT OR IGNORE INTO gateway_pending(id,channel,user,text) VALUES(?1,?2,?3,?4)",
                params![id, channel.to_string(), user.to_string(), text],
            )?;
        }
        transaction.execute("INSERT INTO gateway_state(singleton,data) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET data=excluded.data", [serde_json::to_string(session)?])?;
        transaction.commit()?;
        Ok(())
    }
    fn acknowledge(&self, id: &str) -> Result<()> {
        self.db
            .lock()
            .map_err(|_| anyhow!("Discord ingress lock poisoned"))?
            .execute("DELETE FROM gateway_pending WHERE id=?1", [id])?;
        Ok(())
    }
}

#[derive(Debug)]
struct IngressFailure;
impl std::fmt::Display for IngressFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Discord durable ingress failed; stopping before advancing the replay cursor")
    }
}
impl std::error::Error for IngressFailure {}

impl Discord {
    pub fn new(token: String, application_id: u64, allowed_users: Vec<u64>) -> Result<Self> {
        if token.trim().is_empty() || application_id == 0 || allowed_users.is_empty() {
            bail!("Discord requires a token, application ID, and at least one allowed user");
        }
        Ok(Self {
            token,
            application_id,
            allowed_users: allowed_users.into_iter().collect(),
            bot_id: AtomicU64::new(0),
            client: Client::builder()
                .timeout(Duration::from_secs(20))
                .connect_timeout(Duration::from_secs(10))
                .user_agent("Pantheon/0.1 (Discord bot)")
                .build()?,
            api: API.into(),
            ingress: None,
        })
    }

    /// Enable process-crash replay. The daemon always supplies its private state directory.
    pub fn with_state_dir(mut self, path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        self.ingress = Some(Ingress::open(
            &path.join("discord-ingress.sqlite"),
            self.application_id,
        )?);
        std::fs::File::open(path)?.sync_all()?;
        Ok(self)
    }

    /// Call only after the runtime's FULL-synchronous inbox admission succeeds, including duplicates.
    pub fn acknowledge(&self, id: &str) -> Result<()> {
        if let Some(ingress) = &self.ingress {
            ingress.acknowledge(id)?;
        }
        Ok(())
    }

    fn checkpoint(&self, session: &Session, prompt: Option<&Inbound>) -> Result<()> {
        if let Some(ingress) = &self.ingress {
            ingress
                .commit(session, prompt)
                .map_err(|_| IngressFailure)?;
        }
        Ok(())
    }

    /// Retries only Discord rate limits and transient failures. Never expose response bodies or URLs.
    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        for attempt in 0..6 {
            let mut request = self
                .client
                .request(method.clone(), format!("{}{path}", self.api))
                .header("Authorization", format!("Bot {}", self.token));
            if let Some(ref body) = body {
                request = request.json(body);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(_) if attempt < 5 => {
                    sleep(Duration::from_millis(250 * (1 << attempt))).await;
                    continue;
                }
                Err(_) => bail!("Discord transport failed after bounded retries"),
            };
            let status = response.status();
            if status.is_success() {
                if status.as_u16() == 204 {
                    return Ok(Value::Null);
                }
                return response
                    .json()
                    .await
                    .map_err(|_| anyhow!("Invalid Discord response JSON"));
            }
            if status.as_u16() == 429 && attempt < 5 {
                let body: Value = response.json().await.unwrap_or_default();
                let seconds = body["retry_after"].as_f64().unwrap_or(1.0);
                if !seconds.is_finite() || seconds > 60.0 {
                    bail!("Discord rate limit exceeds retry budget");
                }
                sleep(Duration::from_secs_f64(seconds.clamp(0.05, 60.0))).await;
                continue;
            }
            if status.is_server_error() && attempt < 5 {
                sleep(Duration::from_millis(250 * (1 << attempt))).await;
                continue;
            }
            bail!("Discord request failed (HTTP {})", status.as_u16());
        }
        bail!("Discord retry budget exhausted")
    }

    /// One outbox item per call. Content is already segmented; mention only grants ping permission.
    pub async fn send(
        &self,
        channel: u64,
        content: &str,
        mention: Option<u64>,
        nonce: &str,
    ) -> Result<String> {
        self.send_reply(channel, content, mention, nonce, None)
            .await
    }

    pub async fn send_reply(
        &self,
        channel: u64,
        content: &str,
        mention: Option<u64>,
        nonce: &str,
        reply_to: Option<u64>,
    ) -> Result<String> {
        if content.is_empty() || utf16_len(content) > MESSAGE_LIMIT {
            bail!("Discord message must contain 1–2000 UTF-16 units");
        }
        // Discord nonces have a 25-character limit; keep durable UUID/sequence keys deterministic.
        let nonce = hex::encode(Sha256::digest(nonce.as_bytes()))[..25].to_owned();
        let users: Vec<String> = mention.into_iter().map(|id| id.to_string()).collect();
        let mut payload = json!({"content":content,"nonce":nonce,"enforce_nonce":true,"allowed_mentions":{"parse":[],"users":users,"replied_user":reply_to.is_some() && mention.is_some()}});
        if let Some(message) = reply_to {
            payload["message_reference"] = json!({"message_id":message.to_string(),"channel_id":channel.to_string(),"fail_if_not_exists":false});
        }
        let body = self
            .request(
                Method::POST,
                &format!("/channels/{channel}/messages"),
                Some(payload),
            )
            .await?;
        body["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("Discord response omitted message ID"))
    }

    /// Update an existing tool row. Edits cannot ping users, roles, or everyone.
    pub async fn activity(
        &self,
        channel: u64,
        content: &str,
        nonce: &str,
        reply_to: Option<u64>,
        receipt: Option<&str>,
    ) -> Result<String> {
        if content.is_empty() || utf16_len(content) > MESSAGE_LIMIT {
            bail!("activity message exceeds Discord's limit");
        }
        let mut body = json!({"content":content,"allowed_mentions":{"parse":[],"users":[],"roles":[],"replied_user":false}});
        let (method, path) = if let Some(receipt) = receipt {
            let message = receipt.parse::<u64>().context("invalid activity receipt")?;
            (
                Method::PATCH,
                format!("/channels/{channel}/messages/{message}"),
            )
        } else {
            body["nonce"] = json!(hex::encode(Sha256::digest(nonce.as_bytes()))[..25].to_owned());
            body["enforce_nonce"] = json!(true);
            if let Some(message) = reply_to {
                body["message_reference"] = json!({"message_id":message.to_string(),"channel_id":channel.to_string(),"fail_if_not_exists":false});
            }
            (Method::POST, format!("/channels/{channel}/messages"))
        };
        // Progress is low priority. Durable outbox retries own failures rather
        // than holding the channel's completion reply behind a long HTTP retry.
        let response = self
            .client
            .request(method, format!("{}{path}", self.api))
            .header("Authorization", format!("Bot {}", self.token))
            .json(&body)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(|_| anyhow!("activity delivery deferred"))?;
        if !response.status().is_success() {
            bail!(
                "activity delivery deferred (HTTP {})",
                response.status().as_u16()
            );
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| anyhow!("invalid activity delivery receipt"))?;
        value["id"]
            .as_str()
            .map(str::to_owned)
            .context("missing activity delivery receipt")
    }

    pub async fn edit(&self, channel: u64, message_id: &str, content: &str) -> Result<String> {
        if content.is_empty() || utf16_len(content) > MESSAGE_LIMIT {
            bail!("Discord message must contain 1–2000 UTF-16 units");
        }
        let message_id = message_id
            .parse::<u64>()
            .map_err(|_| anyhow!("Invalid Discord message ID"))?;
        let response = self.request(Method::PATCH, &format!("/channels/{channel}/messages/{message_id}"), Some(json!({
            "content": content,
            "allowed_mentions": {"parse": [], "users": [], "roles": [], "replied_user": false}
        }))).await?;
        response["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("Discord response omitted message ID"))
    }

    pub async fn reply_interaction(&self, token: &str, text: &str) -> Result<()> {
        let content = truncate_utf16(text, MESSAGE_LIMIT);
        self.request(
            Method::PATCH,
            &format!(
                "/webhooks/{}/{token}/messages/@original",
                self.application_id
            ),
            Some(json!({"content": content, "allowed_mentions": {"parse": []}})),
        )
        .await?;
        Ok(())
    }

    pub async fn reply_card(&self, token: &str, payload: Value) -> Result<()> {
        self.request(
            Method::PATCH,
            &format!(
                "/webhooks/{}/{token}/messages/@original",
                self.application_id
            ),
            Some(payload),
        )
        .await?;
        Ok(())
    }

    pub async fn delivery_reaction(
        &self,
        channel: u64,
        message: u64,
        phase: i64,
        _previous: i64,
    ) -> Result<()> {
        let emoji = |phase| match phase {
            0 => Some("📥"),
            1 => Some("🧠"),
            2 => Some("✅"),
            _ => None,
        };
        let next = emoji(phase).context("invalid reaction phase")?;
        self.request(
            Method::PUT,
            &format!("/channels/{channel}/messages/{message}/reactions/{next}/@me"),
            None,
        )
        .await?;
        // Reconcile earlier phases too: a crash after PUT but before the
        // receipt commit must not leave a stale brain beside the final tick.
        for prior in 0..phase {
            self.request(
                Method::DELETE,
                &format!(
                    "/channels/{channel}/messages/{message}/reactions/{}/@me",
                    emoji(prior).unwrap()
                ),
                None,
            )
            .await?;
        }
        Ok(())
    }

    pub async fn typing(&self, channel: u64) -> Result<()> {
        let response = self
            .client
            .post(format!("{}/channels/{channel}/typing", self.api))
            .header("Authorization", format!("Bot {}", self.token))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(|_| anyhow!("typing indicator unavailable"))?;
        if !response.status().is_success() {
            bail!("typing indicator unavailable");
        }
        tracing::debug!(channel, "typing indicator renewed");
        Ok(())
    }

    /// Initial callback has a three-second deadline. Do not use slow durable delivery retries here.
    async fn acknowledge_interaction(&self, id: &str, token: &str, authorized: bool) -> Result<()> {
        let body = if authorized {
            json!({"type": 5, "data": {"flags": 64}})
        } else {
            json!({"type": 4, "data": {"flags": 64, "content": "You are not authorized to use this bot.", "allowed_mentions": {"parse": []}}})
        };
        let response = self
            .client
            .post(format!("{}/interactions/{id}/{token}/callback", self.api))
            .timeout(Duration::from_secs(2))
            .json(&body)
            .send()
            .await
            .map_err(|_| anyhow!("Discord interaction acknowledgment failed"))?;
        if !response.status().is_success() {
            bail!(
                "Discord interaction acknowledgment rejected (HTTP {})",
                response.status().as_u16()
            );
        }
        Ok(())
    }

    pub async fn run(
        self: Arc<Self>,
        tx: mpsc::Sender<Inbound>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        let (mut session, mut pending) = self
            .ingress
            .as_ref()
            .map(Ingress::load)
            .transpose()?
            .unwrap_or_default();
        self.bot_id.store(session.bot_id, Ordering::Relaxed);
        // Deliver committed ingress before depending on any network startup operation.
        while let Some(prompt) = pending.pop_front() {
            if let Inbound::Prompt { ref id, user, .. } = prompt
                && !self.allowed_users.contains(&user)
            {
                self.acknowledge(id)?;
                continue;
            }
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                delivery = tx.send(prompt) => delivery.map_err(|_| anyhow!("Discord inbox receiver closed"))?,
            }
        }
        // Global command registration happens once at startup, never on every reconnect.
        let commands_path = format!("/applications/{}/commands", self.application_id);
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            registration = self.request(
            Method::PUT,
            &commands_path,
            Some(command_definitions()),
        )
        => { registration?; }
        }
        let mut gateway = tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            gateway = self.request(Method::GET, "/gateway/bot", None) => gateway?,
        };
        let gateway_url = gateway["url"]
            .as_str()
            .ok_or_else(|| anyhow!("Discord omitted gateway URL"))?
            .to_owned();
        if gateway["shards"].as_u64().unwrap_or(1) > 1 {
            bail!("This deployment supports one Discord gateway shard");
        }
        let mut interactions = JoinSet::new();
        let mut reconnect = 0u32;
        let mut last_identify: Option<Instant> = None;
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            if session.id.is_none() {
                if let Some(last) = last_identify {
                    let delay = Duration::from_secs(5).saturating_sub(last.elapsed());
                    tokio::select! { _ = shutdown.cancelled() => break, _ = sleep(delay) => {} }
                }
                if gateway["session_start_limit"]["remaining"].as_u64() == Some(0) {
                    let reset = gateway["session_start_limit"]["reset_after"]
                        .as_u64()
                        .unwrap_or(60_000);
                    tokio::select! { _ = shutdown.cancelled() => break, _ = sleep(Duration::from_millis(reset)) => {} }
                    gateway = tokio::select! {
                        _ = shutdown.cancelled() => return Ok(()),
                        gateway = self.request(Method::GET, "/gateway/bot", None) => gateway?,
                    };
                    continue;
                }
                if let Some(remaining) = gateway["session_start_limit"]["remaining"].as_u64() {
                    gateway["session_start_limit"]["remaining"] =
                        json!(remaining.saturating_sub(1));
                }
                last_identify = Some(Instant::now());
            }
            let started = Instant::now();
            let result = self
                .connection(
                    &gateway_url,
                    &mut session,
                    &tx,
                    &shutdown,
                    &mut pending,
                    &mut interactions,
                )
                .await;
            if started.elapsed() > Duration::from_secs(60) {
                reconnect = 0;
            }
            match result {
                Ok(()) => break,
                Err(error)
                    if error.downcast_ref::<FatalGateway>().is_some()
                        || error.downcast_ref::<IngressFailure>().is_some() =>
                {
                    return Err(error);
                }
                Err(_) => tracing::warn!("Discord gateway disconnected; reconnecting"),
            }
            reconnect = reconnect.saturating_add(1);
            let delay = Duration::from_millis((1000 * (1u64 << reconnect.min(5))) + jitter(1000));
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = sleep(delay) => {},
            }
            while interactions.try_join_next().is_some() {}
        }
        interactions.abort_all();
        while interactions.join_next().await.is_some() {}
        Ok(())
    }

    async fn connection(
        self: &Arc<Self>,
        gateway_url: &str,
        session: &mut Session,
        tx: &mpsc::Sender<Inbound>,
        shutdown: &CancellationToken,
        pending: &mut VecDeque<Inbound>,
        interactions: &mut JoinSet<()>,
    ) -> Result<()> {
        let url = session.resume_url.as_deref().unwrap_or(gateway_url);
        if !url.starts_with("wss://") {
            bail!("Invalid Discord gateway URL");
        }
        let (socket, _) = tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            socket = tokio::time::timeout(Duration::from_secs(20), connect_async(format!("{}/?v=10&encoding=json", url.trim_end_matches('/')))) =>
                socket.map_err(|_| anyhow!("Discord gateway connect timed out"))?.map_err(|_| anyhow!("Discord gateway connect failed"))?,
        };
        let (mut write, mut read) = socket.split();
        let hello = tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            hello = tokio::time::timeout(Duration::from_secs(20), read.next()) => hello.map_err(|_| anyhow!("Discord gateway Hello timed out"))?,
        }.ok_or_else(|| anyhow!("Discord gateway closed before Hello"))?.map_err(|_| anyhow!("Discord gateway Hello failed"))?;
        let Message::Text(hello) = hello else {
            bail!("Expected Discord gateway Hello");
        };
        let hello: Value = serde_json::from_str(&hello)?;
        if hello["op"] != 10 {
            bail!("Expected Discord gateway Hello opcode");
        }
        let interval = hello["d"]["heartbeat_interval"]
            .as_u64()
            .filter(|v| *v >= 1000)
            .ok_or_else(|| anyhow!("Invalid Discord heartbeat interval"))?;
        let identify = match session.id.as_ref() {
            Some(id) => {
                json!({"op": 6, "d": {"token": self.token, "session_id": id, "seq": session.sequence}})
            }
            None => json!({"op": 2, "d": {"token": self.token,
                "intents": (1u64 << 0) | (1u64 << 9) | (1u64 << 12) | (1u64 << 15),
                "properties": {"os": "linux", "browser": "pantheon", "device": "pantheon"}}}),
        };
        write
            .send(Message::Text(identify.to_string().into()))
            .await
            .map_err(|_| anyhow!("Discord identify failed"))?;
        let mut heartbeat_at = Instant::now() + Duration::from_millis(jitter(interval));
        let mut acked = true;
        loop {
            tokio::select! {
                // Drop TCP without a normal WebSocket close, preserving Discord's resumable session.
                _ = shutdown.cancelled() => return Ok(()),
                permit = tx.reserve(), if !pending.is_empty() => {
                    permit.map_err(|_| anyhow!("Discord inbox receiver closed"))?.send(pending.pop_front().expect("nonempty queue"));
                }
                _ = tokio::time::sleep_until(heartbeat_at) => {
                    if !acked { bail!("Discord missed heartbeat acknowledgment"); }
                    write.send(Message::Text(json!({"op": 1, "d": session.sequence}).to_string().into())).await
                        .map_err(|_| anyhow!("Discord heartbeat failed"))?;
                    acked = false;
                    heartbeat_at = Instant::now() + Duration::from_millis(interval);
                }
                incoming = read.next(), if pending.len() < 1024 => {
                    let incoming = incoming.ok_or_else(|| anyhow!("Discord gateway closed"))?
                        .map_err(|_| anyhow!("Discord gateway read failed"))?;
                    let text = match incoming {
                        Message::Text(text) => text,
                        Message::Ping(bytes) => { write.send(Message::Pong(bytes)).await.map_err(|_| anyhow!("Discord gateway pong failed"))?; continue; }
                        Message::Close(frame) => {
                            let code = frame.map(|frame| u16::from(frame.code)).unwrap_or(1006);
                            if matches!(code, 4004 | 4010..=4014) { return Err(FatalGateway(code).into()); }
                            if matches!(code, 1000 | 1001 | 4007 | 4009) { *session = Session::default(); self.checkpoint(session, None)?; }
                            bail!("Discord gateway closed (code {code})");
                        }
                        _ => continue,
                    };
                    let event: Value = serde_json::from_str(&text).map_err(|_| anyhow!("Invalid Discord gateway payload"))?;
                    match event["op"].as_u64() {
                        Some(0) => {
                            if let Some(seq) = event["s"].as_u64() { session.sequence = Some(seq); }
                            let mut prompt = None;
                            match event["t"].as_str() {
                            Some("READY") => {
                                session.id = event["d"]["session_id"].as_str().map(str::to_owned);
                                session.resume_url = event["d"]["resume_gateway_url"].as_str().map(str::to_owned);
                                session.bot_id = snowflake(&event["d"]["user"]["id"]).unwrap_or(0);
                                self.bot_id.store(session.bot_id, Ordering::Relaxed);
                                tracing::info!("Discord gateway ready");
                            }
                            Some("MESSAGE_CREATE") => {
                                prompt = self.prompt(&event["d"]);
                            }
                            Some("INTERACTION_CREATE") if event["d"]["type"] == 2 => {
                                    let discord = self.clone(); let data = event["d"].clone(); let tx = tx.clone(); let cancel = shutdown.clone();
                                    interactions.spawn(async move {
                                        tokio::select! {
                                            _ = cancel.cancelled() => {},
                                            _ = discord.interaction(data, tx) => {},
                                        }
                                    });
                            }
                            _ => {},
                            }
                            // Cursor and authorized message are one FULL-synchronous transaction.
                            self.checkpoint(session, prompt.as_ref())?;
                            if let Some(prompt) = prompt { pending.push_back(prompt); }
                        },
                        Some(1) => {
                            write.send(Message::Text(json!({"op": 1, "d": session.sequence}).to_string().into())).await
                                .map_err(|_| anyhow!("Discord requested heartbeat failed"))?;
                            acked = false;
                        }
                        Some(7) => bail!("Discord requested reconnect"),
                        Some(9) => {
                            if event["d"] != true { *session = Session::default(); self.checkpoint(session, None)?; }
                            tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = sleep(Duration::from_millis(1000 + jitter(4000))) => {} }
                            bail!("Discord invalidated gateway session");
                        }
                        Some(11) => acked = true,
                        _ => {},
                    }
                    while interactions.try_join_next().is_some() {}
                }
            }
        }
    }

    fn prompt(&self, message: &Value) -> Option<Inbound> {
        if message["author"]["bot"] == true || !message["webhook_id"].is_null() {
            return None;
        }
        let user = snowflake(&message["author"]["id"])?;
        if !self.allowed_users.contains(&user) {
            return None;
        }
        let bot_id = self.bot_id.load(Ordering::Relaxed);
        if !message["guild_id"].is_null()
            && !message["mentions"]
                .as_array()?
                .iter()
                .any(|user| snowflake(&user["id"]) == Some(bot_id))
        {
            return None;
        }
        let text = message["content"]
            .as_str()?
            .replace(&format!("<@{bot_id}>"), "")
            .replace(&format!("<@!{bot_id}>"), "")
            .trim()
            .to_owned();
        if text.is_empty() {
            return None;
        }
        Some(Inbound::Prompt {
            id: message["id"].as_str()?.into(),
            channel: snowflake(&message["channel_id"])?,
            user,
            text,
        })
    }

    async fn interaction(&self, event: Value, tx: mpsc::Sender<Inbound>) {
        let Some(id) = event["id"].as_str() else {
            return;
        };
        let Some(token) = event["token"].as_str() else {
            return;
        };
        let user =
            snowflake(&event["member"]["user"]["id"]).or_else(|| snowflake(&event["user"]["id"]));
        let authorized = user.is_some_and(|user| self.allowed_users.contains(&user));
        if self
            .acknowledge_interaction(id, token, authorized)
            .await
            .is_err()
        {
            tracing::warn!("Discord slash command could not be acknowledged; command not executed");
            return;
        }
        if !authorized {
            return;
        }
        let (Some(channel), Some(name)) = (
            snowflake(&event["channel_id"]),
            event["data"]["name"].as_str(),
        ) else {
            return;
        };
        let command = Inbound::Command {
            id: id.into(),
            token: token.into(),
            channel,
            user: user.expect("authorized user"),
            name: name.into(),
            options: event["data"]["options"]
                .as_array()
                .map(|options| Value::Array(options.clone()))
                .unwrap_or_else(|| json!([])),
        };
        if tx.send(command).await.is_err() {
            tracing::warn!("Discord command inbox closed");
        }
    }
}

#[derive(Debug)]
struct FatalGateway(u16);
impl std::fmt::Display for FatalGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Discord rejected gateway configuration (code {})",
            self.0
        )
    }
}
impl std::error::Error for FatalGateway {}
fn snowflake(value: &Value) -> Option<u64> {
    value
        .as_str()
        .and_then(|value| value.parse().ok())
        .or_else(|| value.as_u64())
}
fn jitter(max: u64) -> u64 {
    (uuid::Uuid::new_v4().as_u128() as u64) % max.max(1)
}
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}
fn truncate_utf16(text: &str, limit: usize) -> &str {
    let mut units = 0;
    for (index, character) in text.char_indices() {
        units += character.len_utf16();
        if units > limit {
            return &text[..index];
        }
    }
    text
}

fn string_option(name: &str, description: &str, required: bool) -> Value {
    json!({"type": 3, "name": name, "description": description, "required": required})
}
pub fn command_definitions() -> Value {
    let command = |name: &str, description: &str, options: Vec<Value>| json!({"type": 1, "name": name, "description": description, "options": options, "integration_types": [0], "contexts": [0,1]});
    let action = |choices: &[&str]| {
        let mut option = string_option("action", "Operation", true);
        option["choices"] = json!(
            choices
                .iter()
                .map(|value| json!({"name":value,"value":value}))
                .collect::<Vec<_>>()
        );
        option
    };
    let mut browser_action = action(&["list", "open", "handoff", "resume", "close"]);
    browser_action["required"] = json!(false);
    json!([
        command(
            "context",
            "Show durable context and cache statistics",
            vec![]
        ),
        command(
            "model",
            "Show or change this channel's model",
            vec![string_option("id", "Model identifier", false)]
        ),
        command(
            "reasoning",
            "Show or change reasoning effort",
            vec![string_option("level", "Reasoning effort", false)]
        ),
        command(
            "stop",
            "Cancel the channel's active run and background agents",
            vec![]
        ),
        command("status", "Show active work and delivery status", vec![]),
        command(
            "skills",
            "Browse available task guides",
            vec![string_option("id", "Skill ID to inspect", false)]
        ),
        command(
            "mcp",
            "Show configured integrations available in this channel",
            vec![]
        ),
        command(
            "subagents",
            "List background agents and their state",
            vec![]
        ),
        command(
            "browser",
            "Manage browser desktops and human handoff",
            vec![
                browser_action,
                string_option("browser_id", "Browser identifier", false),
                string_option("resume_token", "Explicit handoff resume token", false),
                string_option("url", "URL to open in a new browser", false)
            ]
        ),
        command(
            "wakeup",
            "Manage durable scheduled prompts",
            vec![
                action(&["add", "list", "cancel"]),
                string_option(
                    "schedule",
                    "in 5m, every 1h, or once <RFC3339 timestamp>",
                    false
                ),
                string_option("prompt", "Prompt for the scheduled run", false),
                string_option("id", "Wakeup ID to cancel", false)
            ]
        ),
        command(
            "monitor",
            "Manage background command monitors",
            vec![
                action(&["add", "list", "cancel"]),
                string_option("command", "Shell command to check", false),
                json!({"type":4,"name":"interval_seconds","description":"Poll interval in seconds","min_value":5,"max_value":31536000}),
                string_option("prompt", "Prompt when output changes", false),
                string_option("id", "Monitor ID to cancel", false)
            ]
        )
    ])
}

#[derive(Debug, Clone, Copy)]
pub enum ToolStatus {
    Running,
    Done,
    Error,
}
/// Only a tool's name, outcome, and duration appear in Discord. Arguments/results are never rendered.
pub fn render_tool(name: &str, status: ToolStatus, elapsed: Duration) -> String {
    let safe_name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(64)
        .collect();
    let marker = match status {
        ToolStatus::Running => "◇",
        ToolStatus::Done => "✓",
        ToolStatus::Error => "✗",
    };
    format!("{marker} {safe_name} · {:.1}s", elapsed.as_secs_f64())
}

#[derive(Clone)]
struct Fence {
    marker: String,
    opener: String,
}
fn fence_after(line: &str, current: &Option<Fence>) -> Option<Fence> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return current.clone();
    }
    let Some(character @ ('`' | '~')) = trimmed.chars().next() else {
        return current.clone();
    };
    let count = trimmed.chars().take_while(|c| *c == character).count();
    if count < 3 {
        return current.clone();
    }
    let suffix = &trimmed[count..];
    match current {
        Some(fence)
            if fence.marker.starts_with(character)
                && count >= fence.marker.len()
                && suffix.trim().is_empty() =>
        {
            None
        }
        Some(_) => current.clone(),
        None if count <= 64 && utf16_len(trimmed.trim_end()) <= 256 => Some(Fence {
            marker: character.to_string().repeat(count),
            opener: trimmed.trim_end().to_owned(),
        }),
        None => None,
    }
}
fn close_suffix(fence: &Option<Fence>, text: &str) -> String {
    fence
        .as_ref()
        .map(|fence| {
            format!(
                "{}{}",
                if text.ends_with('\n') { "" } else { "\n" },
                fence.marker
            )
        })
        .unwrap_or_default()
}
fn flush_chunk(chunks: &mut Vec<String>, chunk: &mut String, fence: &Option<Fence>) {
    if chunk.is_empty() {
        return;
    }
    chunk.push_str(&close_suffix(fence, chunk));
    chunks.push(std::mem::take(chunk));
    if let Some(fence) = fence {
        chunk.push_str(&fence.opener);
        chunk.push('\n');
    }
}

/// UTF-16-safe Discord chunks, with standalone fences and a single final-response ping.
pub fn split_message(content: &str, mention: Option<u64>) -> Vec<String> {
    let mut chunks = Vec::new();
    let prefix = mention
        .map(|user| format!("<@{user}> "))
        .unwrap_or_default();
    let mut chunk = prefix.clone();
    let mut fence = None;
    for line in content.split_inclusive('\n') {
        let next_fence = fence_after(line, &fence);
        let suffix = close_suffix(&next_fence, line);
        if utf16_len(&chunk) + utf16_len(line) + utf16_len(&suffix) <= MESSAGE_LIMIT {
            chunk.push_str(line);
            fence = next_fence;
            continue;
        }
        // Prefer splitting at complete lines, including around an opening/closing fence.
        if !chunk.is_empty() && !(chunks.is_empty() && chunk == prefix) {
            flush_chunk(&mut chunks, &mut chunk, &fence);
        }
        let mut remaining = line;
        while !remaining.is_empty() {
            let reserve = fence
                .as_ref()
                .map(|fence| fence.marker.len() + 1)
                .unwrap_or(0);
            let budget = MESSAGE_LIMIT - utf16_len(&chunk) - reserve;
            let part = truncate_utf16(remaining, budget);
            chunk.push_str(part);
            remaining = &remaining[part.len()..];
            if !remaining.is_empty() {
                flush_chunk(&mut chunks, &mut chunk, &fence);
            }
        }
        fence = next_fence;
        // A long opening line may establish a fence only at its final piece; reserve the closer.
        if utf16_len(&chunk) + utf16_len(&close_suffix(&fence, &chunk)) > MESSAGE_LIMIT {
            let before = fence.take();
            flush_chunk(&mut chunks, &mut chunk, &fence);
            fence = before;
        }
    }
    if !chunk.is_empty() {
        let suffix = close_suffix(&fence, &chunk);
        chunk.push_str(&suffix);
        chunks.push(chunk);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durable_ingress_replays_until_runtime_acknowledges() {
        let directory = tempfile::tempdir().unwrap();
        let prompt = Inbound::Prompt {
            id: "message".into(),
            channel: 10,
            user: 42,
            text: "Keep this through a crash".into(),
        };
        {
            let discord = Discord::new("SECRET_TOKEN".into(), 1, vec![42])
                .unwrap()
                .with_state_dir(directory.path())
                .unwrap();
            let mut session = Session {
                id: Some("session".into()),
                sequence: Some(7),
                resume_url: Some("wss://gateway.discord.gg".into()),
                bot_id: 11,
            };
            discord.checkpoint(&session, Some(&prompt)).unwrap();
            // Ignored dispatches may advance the cursor while the earlier prompt remains durable.
            session.sequence = Some(8);
            discord.checkpoint(&session, None).unwrap();
        }
        let discord = Discord::new("SECRET_TOKEN".into(), 1, vec![42])
            .unwrap()
            .with_state_dir(directory.path())
            .unwrap();
        let (session, pending) = discord.ingress.as_ref().unwrap().load().unwrap();
        assert_eq!(session.sequence, Some(8));
        assert_eq!(session.bot_id, 11);
        assert_eq!(pending.len(), 1);
        assert!(
            matches!(&pending[0], Inbound::Prompt { id, text, .. } if id == "message" && text.contains("crash"))
        );
        // Discord can invalidate the remote session without invalidating locally admitted messages.
        discord.checkpoint(&Session::default(), None).unwrap();
        assert_eq!(discord.ingress.as_ref().unwrap().load().unwrap().1.len(), 1);
        discord.acknowledge("message").unwrap();
        discord.acknowledge("message").unwrap();
        assert!(
            discord
                .ingress
                .as_ref()
                .unwrap()
                .load()
                .unwrap()
                .1
                .is_empty()
        );
        let db = discord.ingress.as_ref().unwrap().db.lock().unwrap();
        let serialized: String = db
            .query_row("SELECT data FROM gateway_state", [], |row| row.get(0))
            .unwrap();
        assert!(!serialized.contains("SECRET_TOKEN"));
    }
    #[test]
    fn failed_checkpoint_rolls_back_message_and_cursor_together() {
        let directory = tempfile::tempdir().unwrap();
        let discord = Discord::new("token".into(), 1, vec![42])
            .unwrap()
            .with_state_dir(directory.path())
            .unwrap();
        let mut session = Session {
            sequence: Some(7),
            ..Default::default()
        };
        discord.checkpoint(&session, None).unwrap();
        discord.ingress.as_ref().unwrap().db.lock().unwrap().execute_batch("CREATE TRIGGER reject_cursor BEFORE UPDATE ON gateway_state BEGIN SELECT RAISE(FAIL,'simulated write failure'); END;").unwrap();
        session.sequence = Some(8);
        let prompt = Inbound::Prompt {
            id: "message".into(),
            channel: 10,
            user: 42,
            text: "atomic".into(),
        };
        let error = discord.checkpoint(&session, Some(&prompt)).unwrap_err();
        assert!(error.downcast_ref::<IngressFailure>().is_some());
        let (restored, pending) = discord.ingress.as_ref().unwrap().load().unwrap();
        assert_eq!(restored.sequence, Some(7));
        assert!(pending.is_empty());
    }
    #[test]
    fn ingress_state_cannot_be_reused_for_another_bot() {
        let directory = tempfile::tempdir().unwrap();
        Discord::new("token".into(), 1, vec![42])
            .unwrap()
            .with_state_dir(directory.path())
            .unwrap();
        assert!(
            Discord::new("token".into(), 2, vec![42])
                .unwrap()
                .with_state_dir(directory.path())
                .is_err()
        );
    }
    #[tokio::test]
    async fn startup_replays_ingress_even_when_discord_is_unavailable() {
        use axum::{Router, http::StatusCode, routing::put};
        let app = Router::new().route(
            "/applications/1/commands",
            put(|| async { StatusCode::UNAUTHORIZED }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let mut discord = Discord::new("token".into(), 1, vec![42])
            .unwrap()
            .with_state_dir(directory.path())
            .unwrap();
        discord.api = format!("http://{address}");
        let prompt = Inbound::Prompt {
            id: "message".into(),
            channel: 10,
            user: 42,
            text: "durable".into(),
        };
        discord
            .checkpoint(&Session::default(), Some(&prompt))
            .unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(
            Arc::new(discord)
                .run(tx, CancellationToken::new())
                .await
                .is_err()
        );
        assert!(matches!(rx.try_recv(), Ok(Inbound::Prompt { id, .. }) if id == "message"));
        // Policy changes take effect for durable ingress too.
        let mut revoked = Discord::new("token".into(), 1, vec![43])
            .unwrap()
            .with_state_dir(directory.path())
            .unwrap();
        revoked.api = format!("http://{address}");
        let revoked = Arc::new(revoked);
        let (tx, mut rx) = mpsc::channel(8);
        assert!(
            revoked
                .clone()
                .run(tx, CancellationToken::new())
                .await
                .is_err()
        );
        assert!(rx.try_recv().is_err());
        assert!(
            revoked
                .ingress
                .as_ref()
                .unwrap()
                .load()
                .unwrap()
                .1
                .is_empty()
        );
        server.abort();
    }
    #[test]
    fn emoji_limits_and_one_mention() {
        let chunks = split_message(&"🦀".repeat(5000), Some(123));
        assert!(chunks.iter().all(|chunk| utf16_len(chunk) <= MESSAGE_LIMIT));
        assert!(chunks[0].starts_with("<@123> "));
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.contains("<@123>"))
                .count(),
            1
        );
        assert_eq!(chunks.concat().replace("<@123> ", ""), "🦀".repeat(5000));
    }
    #[test]
    fn long_fenced_code_keeps_language() {
        let source = format!("```rust\n{}\n```\n", "let crab = 1;\n".repeat(1000));
        let chunks = split_message(&source, None);
        assert!(chunks.len() > 1);
        for chunk in chunks {
            assert!(utf16_len(&chunk) <= MESSAGE_LIMIT);
            assert!(chunk.starts_with("```rust\n"));
            assert!(chunk.trim_end().ends_with("```"));
        }
    }
    #[test]
    fn long_code_line_and_tilde_fences() {
        let source = format!("~~~~python\n{}\n~~~~", "🦀".repeat(5000));
        let chunks = split_message(&source, None);
        assert!(chunks.iter().all(|chunk| utf16_len(chunk) <= MESSAGE_LIMIT
            && chunk.starts_with("~~~~python\n")
            && chunk.trim_end().ends_with("~~~~")));
    }
    #[test]
    fn admission_requires_allowlist_and_guild_mention() {
        let bot = Discord::new("test".into(), 1, vec![42]).unwrap();
        bot.bot_id.store(7, Ordering::Relaxed);
        let mut message = json!({"id":"10","channel_id":"11","author":{"id":"42"},"content":"hello","guild_id":"9","mentions":[]});
        assert!(bot.prompt(&message).is_none());
        message["mentions"] = json!([{"id":"7"}]);
        message["content"] = json!("<@7> hello");
        assert!(
            matches!(bot.prompt(&message), Some(Inbound::Prompt { text, .. }) if text == "hello")
        );
        message["author"]["id"] = json!("666");
        assert!(bot.prompt(&message).is_none());
    }
    #[test]
    fn tool_rows_do_not_allow_mentions_or_multiline_names() {
        let row = render_tool(
            "shell\n<@123>secret",
            ToolStatus::Done,
            Duration::from_millis(150),
        );
        assert!(!row.contains('\n') && !row.contains('<'));
    }
    #[test]
    fn long_replies_keep_the_ping_with_response_text() {
        let chunks = split_message(&"x".repeat(5000), Some(1));
        assert!(chunks[0].starts_with("<@1> x"));
        let inbound = Inbound::Command {
            id: "1".into(),
            token: "SECRET_TOKEN".into(),
            channel: 1,
            user: 1,
            name: "status".into(),
            options: json!([]),
        };
        assert!(!format!("{inbound:?}").contains("SECRET_TOKEN"));
    }
    #[tokio::test]
    async fn acknowledgment_precedes_command_and_denial_is_private() {
        use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let app = Router::new()
            .route(
                "/interactions/123/token/callback",
                post(
                    |State(captured): State<Arc<tokio::sync::Mutex<Vec<Value>>>>,
                     Json(body): Json<Value>| async move {
                        captured.lock().await.push(body);
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut discord = Discord::new("test".into(), 1, vec![42]).unwrap();
        discord.api = format!("http://{address}");
        let (tx, mut rx) = mpsc::channel(8);
        let mut event = json!({"id":"123","token":"token","channel_id":"1","user":{"id":"42"},"data":{"name":"status"}});
        discord.interaction(event.clone(), tx.clone()).await;
        assert!(matches!(rx.try_recv(), Ok(Inbound::Command { name, .. }) if name == "status"));
        assert_eq!(
            captured.lock().await[0],
            json!({"type":5,"data":{"flags":64}})
        );
        event["user"]["id"] = json!("666");
        discord.interaction(event, tx).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(captured.lock().await[1]["type"], 4);
        assert_eq!(captured.lock().await[1]["data"]["flags"], 64);
        server.abort();
    }
    #[tokio::test]
    async fn rate_limits_retry_and_permanent_errors_hide_bodies() {
        use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
        let attempts = Arc::new(AtomicU64::new(0));
        let app = Router::new()
            .route(
                "/channels/1/messages",
                post(|State(attempts): State<Arc<AtomicU64>>| async move {
                    match attempts.fetch_add(1, Ordering::Relaxed) {
                        0 => (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(json!({"retry_after":0.001})),
                        ),
                        1 => (StatusCode::OK, Json(json!({"id":"123"}))),
                        _ => (
                            StatusCode::FORBIDDEN,
                            Json(json!({"message":"SECRET_TOKEN"})),
                        ),
                    }
                }),
            )
            .with_state(attempts.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut discord = Discord::new("test".into(), 1, vec![42]).unwrap();
        discord.api = format!("http://{address}");
        assert_eq!(discord.send(1, "done", None, "one").await.unwrap(), "123");
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        let error = discord
            .send(1, "done", None, "two")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("403") && !error.contains("SECRET_TOKEN"));
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
        server.abort();
    }
    #[tokio::test]
    async fn rest_nonce_and_mentions_are_explicit() {
        use axum::{Json, Router, extract::State, routing::post};
        let captured = Arc::new(tokio::sync::Mutex::new(None));
        let app = Router::new()
            .route(
                "/channels/1/messages",
                post(
                    |State(captured): State<Arc<tokio::sync::Mutex<Option<Value>>>>,
                     Json(body): Json<Value>| async move {
                        *captured.lock().await = Some(body);
                        Json(json!({"id":"123"}))
                    },
                ),
            )
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut discord = Discord::new("test".into(), 1, vec![1]).unwrap();
        discord.api = format!("http://{address}");
        assert_eq!(
            discord
                .send(1, "<@1> finished @everyone", Some(1), "durable-outbox-id")
                .await
                .unwrap(),
            "123"
        );
        let body = captured.lock().await.clone().unwrap();
        assert_eq!(body["enforce_nonce"], true);
        assert_eq!(body["nonce"].as_str().unwrap().len(), 25);
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        assert_eq!(body["allowed_mentions"]["users"], json!(["1"]));
        server.abort();
    }
    #[tokio::test]
    async fn tool_row_edits_disable_mentions_and_keep_the_message_receipt() {
        use axum::{Json, Router, extract::State, routing::patch};
        let captured = Arc::new(tokio::sync::Mutex::new(None));
        let app = Router::new()
            .route(
                "/channels/1/messages/123",
                patch(
                    |State(captured): State<Arc<tokio::sync::Mutex<Option<Value>>>>,
                     Json(body): Json<Value>| async move {
                        *captured.lock().await = Some(body);
                        Json(json!({"id":"123"}))
                    },
                ),
            )
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut discord = Discord::new("SECRET_TOKEN".into(), 1, vec![1]).unwrap();
        discord.api = format!("http://{address}");
        let text = "✓ shell · 0.1s @everyone <@1>";
        assert_eq!(discord.edit(1, "123", text).await.unwrap(), "123");
        let body = captured.lock().await.clone().unwrap();
        assert_eq!(body["content"], text);
        assert_eq!(
            body["allowed_mentions"],
            json!({"parse":[],"users":[],"roles":[],"replied_user":false})
        );
        assert!(!body.to_string().contains("SECRET_TOKEN"));
        assert!(
            discord
                .edit(1, "123/SECRET_TOKEN", text)
                .await
                .unwrap_err()
                .to_string()
                .contains("Invalid Discord message ID")
        );
        server.abort();
    }

    #[tokio::test]
    async fn native_replies_and_cards_preserve_references_and_control_notifications() {
        use axum::{
            Json, Router,
            routing::{patch, post},
        };
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let a = captured.clone();
        let b = captured.clone();
        let app = Router::new()
            .route(
                "/channels/1/messages",
                post(move |Json(body): Json<Value>| {
                    let captured = a.clone();
                    async move {
                        captured.lock().await.push(body);
                        Json(json!({"id":"123"}))
                    }
                }),
            )
            .route(
                "/webhooks/1/interaction/messages/@original",
                patch(move |Json(body): Json<Value>| {
                    let captured = b.clone();
                    async move {
                        captured.lock().await.push(body);
                        Json(json!({"id":"124"}))
                    }
                }),
            )
            .route(
                "/channels/1/typing",
                post(|| async { axum::http::StatusCode::NO_CONTENT }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut discord = Discord::new("private-token".into(), 1, vec![42]).unwrap();
        discord.api = format!("http://{address}");
        discord
            .send_reply(1, "Finished", Some(42), "final", Some(99))
            .await
            .unwrap();
        discord
            .activity(1, "```text\n✓ shell · 7.0s\n```", "activity", None, None)
            .await
            .unwrap();
        discord
            .reply_card(
                "interaction",
                crate::ui::card(
                    "Context",
                    "Ready",
                    vec![("Model", "codex/test".into(), false)],
                    false,
                ),
            )
            .await
            .unwrap();
        discord.typing(1).await.unwrap();
        let bodies = captured.lock().await;
        assert_eq!(bodies[0]["message_reference"]["message_id"], "99");
        assert_eq!(bodies[0]["message_reference"]["fail_if_not_exists"], false);
        assert_eq!(bodies[0]["allowed_mentions"]["replied_user"], true);
        assert_eq!(bodies[0]["content"], "Finished");
        assert!(bodies[1].get("message_reference").is_none());
        assert_eq!(bodies[1]["allowed_mentions"]["replied_user"], false);
        assert_eq!(bodies[2]["content"], "");
        assert_eq!(bodies[2]["embeds"][0]["title"], "Context");
        assert_eq!(bodies[2]["allowed_mentions"]["parse"], json!([]));
        server.abort();
    }
    #[tokio::test]
    async fn input_reactions_replace_only_our_prior_phases_and_reconcile_partial_delivery() {
        use axum::{
            Router,
            extract::{OriginalUri, State},
            http::{Method, StatusCode},
            routing::any,
        };
        type Captured = Arc<tokio::sync::Mutex<Vec<(Method, String)>>>;
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::<(Method, String)>::new()));
        let app = Router::new()
            .route(
                "/channels/1/messages/100/reactions/{emoji}/@me",
                any(
                    |State(captured): State<Captured>,
                     method: Method,
                     OriginalUri(uri): OriginalUri| async move {
                        captured.lock().await.push((method, uri.to_string()));
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut discord = Discord::new("private".into(), 1, vec![2]).unwrap();
        discord.api = format!("http://{address}");
        discord.delivery_reaction(1, 100, 0, -1).await.unwrap();
        discord.delivery_reaction(1, 100, 1, 0).await.unwrap();
        // Stored previous=0 simulates an unacknowledged brain PUT at a crash.
        discord.delivery_reaction(1, 100, 2, 0).await.unwrap();
        let requests = captured.lock().await;
        assert_eq!(requests.len(), 6);
        assert_eq!(requests[0].0, Method::PUT);
        assert!(requests[0].1.contains("%F0%9F%93%A5"));
        assert!(requests[1].1.contains("%F0%9F%A7%A0"));
        assert_eq!(requests[2].0, Method::DELETE);
        assert!(requests[3].1.contains("%E2%9C%85"));
        assert_eq!(requests[4].0, Method::DELETE);
        assert_eq!(requests[5].0, Method::DELETE);
        assert!(requests.iter().all(|r| r.1.ends_with("/@me")));
        server.abort();
    }
    #[tokio::test]
    async fn one_channel_rate_limit_does_not_lock_other_transport_requests() {
        use axum::{
            Json, Router,
            extract::{Path, State},
            http::StatusCode,
            routing::post,
        };
        let limited = Arc::new(tokio::sync::Notify::new());
        let app =
            Router::new()
                .route(
                    "/channels/{channel}/messages",
                    post(
                        |Path(channel): Path<u64>,
                         State(limited): State<Arc<tokio::sync::Notify>>| async move {
                            if channel == 1 {
                                limited.notify_one();
                                (
                                    StatusCode::TOO_MANY_REQUESTS,
                                    Json(json!({"retry_after":10.0})),
                                )
                            } else {
                                (StatusCode::OK, Json(json!({"id":"2"})))
                            }
                        },
                    ),
                )
                .with_state(limited.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut discord = Discord::new("token".into(), 1, vec![1]).unwrap();
        discord.api = format!("http://{address}");
        let discord = Arc::new(discord);
        let blocked = discord.clone();
        let blocked = tokio::spawn(async move { blocked.send(1, "waiting", None, "first").await });
        limited.notified().await;
        assert_eq!(
            discord
                .send(2, "independent", None, "second")
                .await
                .unwrap(),
            "2"
        );
        assert!(!blocked.is_finished());
        blocked.abort();
        server.abort();
    }
}
