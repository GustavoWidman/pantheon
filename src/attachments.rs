//! Managed attachment bytes are disposable; immutable history keeps stable IDs and findings.
use crate::{
    discord::Discord,
    store::{Input, now},
};
use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use futures_util::StreamExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AttachmentConfig {
    /// Zero keeps original bytes indefinitely. Active consumers and explicit keep always win.
    pub retention_seconds: u64,
    pub cleanup_interval_seconds: u64,
    /// Opt-in only: the subscription endpoint does not document public API file support.
    pub codex_native_files: bool,
}
impl Default for AttachmentConfig {
    fn default() -> Self {
        Self {
            retention_seconds: 7 * 86400,
            cleanup_interval_seconds: 300,
            codex_native_files: false,
        }
    }
}
impl AttachmentConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.retention_seconds <= i64::MAX as u64 && self.cleanup_interval_seconds > 0,
            "invalid attachment retention/cleanup interval"
        );
        Ok(())
    }
}
// No Debug: signed CDN URLs are transport credentials, never conversation content.
#[derive(Clone, Serialize, Deserialize)]
pub struct IncomingFile {
    pub id: String,
    pub filename: String,
    #[serde(default)]
    pub content_type: Option<String>,
    pub size: u64,
    pub url: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: String,
    pub channel: u64,
    pub message: String,
    pub filename: String,
    pub mime: String,
    pub size: u64,
    pub hash: Option<String>,
    pub kept: bool,
    pub error: Option<String>,
    pub outgoing: bool,
}
pub struct PendingInput {
    pub input: Input,
    pub files: Vec<IncomingFile>,
}
pub struct Attachments {
    db: Mutex<Connection>,
    root: PathBuf,
    workspace: PathBuf,
    config: AttachmentConfig,
    http: reqwest::Client,
    // Serializes mutation/GC, not channel inference. Downloads in different channels run concurrently.
    pub(crate) lifecycle: tokio::sync::Mutex<()>,
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Attachments {
    pub fn open(state: &Path, workspace: &Path, config: AttachmentConfig) -> Result<Self> {
        config.validate()?;
        let root = state.join("attachments");
        std::fs::create_dir_all(root.join("tmp"))?;
        std::fs::create_dir_all(root.join("files"))?;
        let root = root.canonicalize()?;
        let workspace = workspace.canonicalize()?;
        let db = Connection::open(state.join("runtime.sqlite"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        initialize(&db)?;
        for directory in [root.join("files"), root.join("tmp")] {
            ensure!(
                !directory.is_symlink(),
                "attachment state directory was replaced by a symlink"
            );
        }
        // The daemon writer lock guarantees no live previous download at startup.
        for entry in std::fs::read_dir(root.join("tmp"))? {
            let p = entry?.path();
            if p.is_file() || p.is_symlink() {
                std::fs::remove_file(p)?;
            }
        }
        for entry in std::fs::read_dir(root.join("files"))? {
            let path = entry?.path();
            let Some(id) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let tracked: Option<Option<String>> = db
                .query_row("SELECT hash FROM attachments WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .optional()?;
            match tracked {
                None => {
                    std::fs::remove_file(path)?;
                }
                Some(None) if path.is_file() => {
                    let hash = hash_file_sync(&path)?;
                    db.execute(
                        "UPDATE attachments SET hash=?2,size=?3 WHERE id=?1",
                        params![id, hash, path.metadata()?.len().to_string()],
                    )?;
                }
                _ => {}
            }
        }
        let scratch = db
            .prepare("SELECT path FROM attachment_scratch")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for path in scratch {
            let path = PathBuf::from(path);
            if managed_path(&workspace, &path) && path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        db.execute("DELETE FROM attachment_scratch", [])?;
        db.execute("DELETE FROM attachment_curators", [])?;
        Ok(Self {
            db: Mutex::new(db),
            root,
            workspace,
            config,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            lifecycle: tokio::sync::Mutex::new(()),
        })
    }
    pub fn queue(&self, input: &Input, files: &[IncomingFile]) -> Result<()> {
        ensure!(
            files.is_empty() || input.id.parse::<u64>().is_ok(),
            "invalid attachment message ID"
        );
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute("INSERT OR IGNORE INTO attachment_ingress(id,channel,user,text,files) VALUES(?1,?2,?3,?4,?5)",params![input.id,input.channel.to_string(),input.user.to_string(),input.text,serde_json::to_string(files)?])?;
        for f in files {
            ensure!(f.id.parse::<u64>().is_ok(), "invalid Discord attachment ID");
            tx.execute("INSERT OR IGNORE INTO attachments(id,channel,message,filename,mime,size,last_used) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![format!("{}-{}",input.id,f.id),input.channel.to_string(),input.id,safe_name(&f.filename),mime(&f.filename,f.content_type.as_deref()),f.size.to_string(),now()])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn next(&self, excluded: &HashSet<u64>) -> Result<Option<PendingInput>> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT id,channel,user,text,files FROM attachment_ingress p WHERE state='pending' AND NOT EXISTS(SELECT 1 FROM attachment_ingress older WHERE older.channel=p.channel AND older.state='pending' AND older.seq<p.seq) ORDER BY seq")?;
        let rows = q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (id, channel, user, text, files) = row?;
            let channel = channel.parse()?;
            if !excluded.contains(&channel) {
                return Ok(Some(PendingInput {
                    input: Input {
                        id,
                        channel,
                        user: user.parse()?,
                        text,
                    },
                    files: serde_json::from_str(&files)?,
                }));
            }
        }
        Ok(None)
    }
    pub fn finish(&self, id: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE attachment_ingress SET state='done',files='[]' WHERE id=?1 AND state='pending'",
            [id],
        )?;
        Ok(())
    }
    pub fn cancel_channel(&self, channel: u64) -> Result<()> {
        self.db.lock().unwrap().execute("UPDATE attachment_ingress SET state='cancelled',files='[]' WHERE channel=?1 AND state='pending'",[channel.to_string()])?;
        Ok(())
    }
    pub fn pending(&self, id: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT state='pending' FROM attachment_ingress WHERE id=?1",
            [id],
            |r| r.get(0),
        )?)
    }
    pub fn has_pending(&self, channel: u64) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM attachment_ingress WHERE channel=?1 AND state='pending')",
            [channel.to_string()],
            |r| r.get(0),
        )?)
    }
    pub fn files(&self, channel: u64, message: Option<&str>) -> Result<Vec<FileRecord>> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT id,channel,message,filename,mime,size,hash,kept,error,outgoing FROM attachments WHERE channel=?1 AND (?2 IS NULL OR message=?2) ORDER BY rowid")?;
        Ok(q.query_map(params![channel.to_string(), message], record)?
            .collect::<std::result::Result<_, _>>()?)
    }
    pub fn list(&self, channel: u64, offset: u64) -> Result<Value> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT id,channel,message,filename,mime,size,hash,kept,error,outgoing FROM attachments WHERE channel=?1 ORDER BY rowid LIMIT 26 OFFSET ?2")?;
        let mut files = q
            .query_map(
                params![channel.to_string(), offset.min(i64::MAX as u64) as i64],
                record,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let next = if files.len() > 25 {
            files.pop();
            Some(offset.saturating_add(25))
        } else {
            None
        };
        Ok(json!({"files":files,"next_offset":next}))
    }
    pub fn get(&self, channel: u64, id: &str) -> Result<FileRecord> {
        self.db.lock().unwrap().query_row("SELECT id,channel,message,filename,mime,size,hash,kept,error,outgoing FROM attachments WHERE channel=?1 AND id=?2",params![channel.to_string(),id],record).optional()?.context("unknown attachment in this channel")
    }
    pub fn keep(&self, channel: u64, id: &str, keep: bool) -> Result<FileRecord> {
        ensure!(
            self.db.lock().unwrap().execute(
                "UPDATE attachments SET kept=?3,last_used=?4 WHERE channel=?1 AND id=?2",
                params![channel.to_string(), id, keep, now()]
            )? == 1,
            "unknown attachment in this channel"
        );
        self.get(channel, id)
    }
    fn blob(&self, f: &FileRecord) -> PathBuf {
        self.root.join("files").join(&f.id)
    }
    fn copy_path(&self, f: &FileRecord) -> PathBuf {
        self.workspace
            .join(".pantheon-attachments")
            .join(f.channel.to_string())
            .join(&f.id)
            .join(&f.filename)
    }
    pub fn relative_path(&self, f: &FileRecord) -> String {
        self.copy_path(f)
            .strip_prefix(&self.workspace)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
    fn touch(&self, id: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE attachments SET last_used=?2 WHERE id=?1",
            params![id, now()],
        )?;
        Ok(())
    }
    async fn commit_blob(&self, id: &str, temporary: &Path, hash: &str, size: u64) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        let path = self.root.join("files").join(id);
        tokio::fs::rename(temporary, &path).await?;
        std::fs::File::open(path.parent().unwrap())?.sync_all()?;
        self.db.lock().unwrap().execute(
            "UPDATE attachments SET hash=?2,size=?3,error=NULL,last_used=?4 WHERE id=?1",
            params![id, hash, size.to_string(), now()],
        )?;
        Ok(())
    }
    async fn download(
        &self,
        f: &IncomingFile,
        discord: &Discord,
        channel: u64,
        message: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let stored_id = format!("{message}-{}", f.id);
        let existing = self.get(channel, &stored_id)?;
        if let Some(hash) = &existing.hash
            && hash_file(&self.blob(&existing))
                .await
                .is_ok_and(|h| h == *hash)
        {
            return Ok(());
        }
        // Never expose a damaged original as usable evidence if its re-download fails.
        if self.blob(&existing).exists() {
            let _guard = self.lifecycle.lock().await;
            tokio::fs::remove_file(self.blob(&existing)).await?;
        }
        let mut url = f.url.clone();
        for attempt in 0..3 {
            validate_cdn(&url)?;
            let response =
                tokio::time::timeout(Duration::from_secs(120), self.http.get(&url).send()).await;
            let response = response.ok().and_then(Result::ok);
            let response = match response {
                Some(r) if r.status().is_success() => r,
                _ if attempt < 2 => {
                    // Re-fetch the authorized message to renew an expired signed CDN URL.
                    let fresh = discord.message(channel, message).await?;
                    url = fresh["attachments"]
                        .as_array()
                        .and_then(|files| files.iter().find(|v| v["id"] == f.id))
                        .and_then(|v| v["url"].as_str())
                        .context("attachment is no longer available on Discord")?
                        .into();
                    tokio::time::sleep(Duration::from_secs(attempt + 1)).await;
                    continue;
                }
                _ => bail!("attachment download failed after retries"),
            };
            let temp = Temporary(self.root.join("tmp").join(uuid::Uuid::new_v4().to_string()));
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp.0)
                .await?;
            let mut stream = response.bytes_stream();
            let mut hash = Sha256::new();
            let mut size = 0u64;
            let downloaded: Result<()> = async {
                loop {
                    let item = tokio::select! {_=cancel.cancelled()=>bail!("attachment download cancelled"),r=tokio::time::timeout(Duration::from_secs(120),stream.next())=>r.context("attachment transfer stalled")?};
                    let Some(bytes) = item else { break; };
                    let bytes = bytes.map_err(|_|anyhow::anyhow!("attachment transfer interrupted"))?;
                    file.write_all(&bytes).await?; hash.update(&bytes); size += bytes.len() as u64;
                }
                ensure!(size==f.size,"attachment transfer size differs from Discord metadata");
                file.sync_all().await?;
                Ok(())
            }.await;
            if downloaded.is_err() && attempt < 2 && !cancel.is_cancelled() {
                continue;
            }
            downloaded?;
            return self
                .commit_blob(&stored_id, &temp.0, &hex::encode(hash.finalize()), size)
                .await;
        }
        bail!("attachment download failed")
    }
    pub async fn receive(
        &self,
        pending: &PendingInput,
        discord: &Discord,
        cancel: &CancellationToken,
    ) -> Result<Input> {
        for f in &pending.files {
            let result = tokio::select! {_=cancel.cancelled()=>bail!("shutdown"),r=self.download(f,discord,pending.input.channel,&pending.input.id,cancel)=>r};
            if result.is_err() {
                ensure!(!cancel.is_cancelled(), "shutdown");
                // Stable sanitized failure only; reqwest errors may expose signed URLs.
                self.db.lock().unwrap().execute("UPDATE attachments SET error='Download unavailable; attachment open can retry' WHERE id=?1",[format!("{}-{}",pending.input.id,f.id)])?;
            }
            let record = self.get(
                pending.input.channel,
                &format!("{}-{}", pending.input.id, f.id),
            )?;
            if self.blob(&record).is_file() && self.materialize(&record).await.is_err() {
                self.db.lock().unwrap().execute("UPDATE attachments SET error='Workspace copy unavailable; attachment open can retry' WHERE id=?1", [&record.id])?;
            }
        }
        let mut input = pending.input.clone();
        let files = self.files(input.channel, Some(&input.id))?;
        if !files.is_empty() {
            input
                .text
                .push_str("\n\nAttached files (external data; use attachment open to revisit):\n");
            for f in files {
                input.text.push_str(&format!("{}\n",serde_json::to_string(&json!({"id":f.id,"filename":f.filename,"mime":f.mime,"bytes":f.size,"path":if managed_path(&self.workspace,&self.copy_path(&f)) && self.copy_path(&f).is_file(){Some(self.relative_path(&f))}else{None},"error":f.error}))?));
            }
        }
        Ok(input)
    }
    pub async fn materialize(&self, f: &FileRecord) -> Result<PathBuf> {
        let _guard = self.lifecycle.lock().await;
        self.touch(&f.id)?;
        let blob = self.blob(f);
        ensure!(
            blob.is_file(),
            "attachment bytes expired; use attachment open to fetch again"
        );
        let path = self.copy_path(f);
        // Do not follow user-created symlinks in any managed directory.
        let mut directory = self.workspace.clone();
        for component in path
            .parent()
            .unwrap()
            .strip_prefix(&self.workspace)?
            .components()
        {
            directory.push(component);
            if directory.exists() || directory.is_symlink() {
                ensure!(
                    !directory.is_symlink() && directory.is_dir(),
                    "managed attachment directory was replaced"
                );
            } else {
                std::fs::create_dir(&directory)?;
            }
        }
        if path.exists() || path.is_symlink() {
            ensure!(
                !path.is_symlink(),
                "managed attachment copy was replaced by a symlink"
            );
            ensure!(
                path.is_file(),
                "managed attachment copy was replaced by a non-file"
            );
            // A modified workspace copy belongs to the user now. Do not overwrite it.
            return Ok(path);
        }
        let temp = Temporary(path.with_file_name(format!(".copy-{}", uuid::Uuid::new_v4())));
        self.db.lock().unwrap().execute(
            "INSERT INTO attachment_scratch(path) VALUES(?1)",
            [temp.0.to_string_lossy().as_ref()],
        )?;
        tokio::fs::copy(&blob, &temp.0).await?;
        tokio::fs::File::open(&temp.0).await?.sync_all().await?;
        tokio::fs::rename(&temp.0, &path).await?;
        std::fs::File::open(path.parent().unwrap())?.sync_all()?;
        self.db.lock().unwrap().execute(
            "DELETE FROM attachment_scratch WHERE path=?1",
            [temp.0.to_string_lossy().as_ref()],
        )?;
        Ok(path)
    }
    pub async fn reopen(
        &self,
        channel: u64,
        id: &str,
        discord: &Discord,
        cancel: &CancellationToken,
    ) -> Result<FileRecord> {
        let f = self.get(channel, id)?;
        let valid = if let Some(hash) = &f.hash {
            hash_file(&self.blob(&f))
                .await
                .is_ok_and(|actual| actual == *hash)
        } else {
            false
        };
        if !valid {
            ensure!(
                !f.outgoing,
                "outgoing snapshot expired; use the source workspace file if it still exists"
            );
            let message = discord.message(channel, &f.message).await?;
            let fresh: IncomingFile = serde_json::from_value(
                message["attachments"]
                    .as_array()
                    .and_then(|files| {
                        files
                            .iter()
                            .find(|v| v["id"] == id.rsplit('-').next().unwrap_or(id))
                    })
                    .context("attachment no longer available on Discord")?
                    .clone(),
            )?;
            self.download(&fresh, discord, channel, &f.message, cancel)
                .await?;
        }
        let f = self.get(channel, id)?;
        self.materialize(&f).await?;
        Ok(f)
    }
    pub async fn snapshot(&self, channel: u64, id: &str, path: &Path) -> Result<FileRecord> {
        ensure!(
            id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid snapshot ID"
        );
        if let Ok(existing) = self.get(channel, id) {
            if existing.hash.is_some() {
                ensure!(
                    self.blob(&existing).is_file(),
                    "previous snapshot bytes unavailable"
                );
                return Ok(existing);
            }
            self.db
                .lock()
                .unwrap()
                .execute("DELETE FROM attachments WHERE id=?1 AND hash IS NULL", [id])?;
        }
        let filename = safe_name(
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("attachment"),
        );
        let temp = Temporary(self.root.join("tmp").join(uuid::Uuid::new_v4().to_string()));
        ensure!(
            tokio::fs::metadata(path).await?.is_file(),
            "send_file requires a regular file"
        );
        let mut input = tokio::fs::File::open(path).await?;
        ensure!(
            input.metadata().await?.is_file(),
            "send_file requires a regular file"
        );
        let source_meta = input.metadata().await?;
        let mut output = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp.0)
            .await?;
        let mut buf = [0u8; 65536];
        let mut size = 0u64;
        let mut hash = Sha256::new();
        loop {
            let n = input.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n]).await?;
            hash.update(&buf[..n]);
            size += n as u64;
        }
        let after = input.metadata().await?;
        ensure!(
            source_meta.len() == after.len()
                && source_meta.modified().ok() == after.modified().ok(),
            "source file changed while snapshotting; retry after writing finishes"
        );
        output.sync_all().await?;
        self.db.lock().unwrap().execute("INSERT INTO attachments(id,channel,message,filename,mime,size,outgoing,last_used) VALUES(?1,?2,'',?3,?4,?5,1,?6)",params![id,channel.to_string(),filename,mime(&filename,None),size.to_string(),now()])?;
        self.commit_blob(id, &temp.0, &hex::encode(hash.finalize()), size)
            .await?;
        self.get(channel, id)
    }
    pub fn outgoing_path(&self, f: &FileRecord) -> PathBuf {
        self.blob(f)
    }
    /// Durable per-channel reader protection, refreshed before a root goes idle.
    pub fn protect_curators(&self, channels: &HashSet<u64>) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute("DELETE FROM attachment_curators", [])?;
        for channel in channels {
            tx.execute(
                "INSERT INTO attachment_curators(channel) VALUES(?1)",
                [channel.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub async fn clean(self: &std::sync::Arc<Self>, extra_active: &HashSet<u64>) -> Result<()> {
        self.clean_with_cancel(extra_active, &CancellationToken::new())
            .await
    }
    pub async fn clean_with_cancel(
        self: &std::sync::Arc<Self>,
        extra_active: &HashSet<u64>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let this = self.clone();
        let extra = extra_active.clone();
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || this.clean_sync(&extra, &cancel))
            .await
            .context("attachment cleanup task failed")?
    }
    fn clean_sync(&self, extra_active: &HashSet<u64>, cancel: &CancellationToken) -> Result<()> {
        let files = {
            let db = self.db.lock().unwrap();
            let mut q=db.prepare("SELECT id,channel,message,filename,mime,size,hash,kept,error,outgoing FROM attachments")?;
            q.query_map([], record)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for f in files {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if extra_active.contains(&f.channel) {
                continue;
            }
            let path = self.copy_path(&f);
            // Expensive hashing happens without holding SQLite or a runtime thread.
            let original_metadata = std::fs::metadata(&path).ok();
            let unchanged = managed_path(&self.workspace, &path)
                && path.is_file()
                && f.hash.as_ref().is_some_and(|hash| {
                    hash_file_sync_cancel(&path, cancel).is_ok_and(|h| h == *hash)
                });
            if cancel.is_cancelled() {
                return Ok(());
            }
            // Hashing never holds the mutation lock; only the short deletion transaction does.
            let _guard = self.lifecycle.blocking_lock();
            let mut db = self.db.lock().unwrap();
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // Recheck after hashing, atomically with admission/publication. New work always wins.
            let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM inbox WHERE channel=?1 AND state IN ('queued','running')) OR EXISTS(SELECT 1 FROM attachment_ingress WHERE channel=?1 AND state='pending') OR EXISTS(SELECT 1 FROM agent_inbox WHERE channel=?1 AND state='queued') OR EXISTS(SELECT 1 FROM tasks t JOIN agent_runs r ON r.owner=t.id WHERE t.channel=?1 AND r.state='running') OR EXISTS(SELECT 1 FROM shell_runs WHERE channel=?1 AND state='running') OR EXISTS(SELECT 1 FROM ui_agents WHERE channel=?1 AND active=1) OR EXISTS(SELECT 1 FROM attachment_curators WHERE channel=?1) OR EXISTS(SELECT 1 FROM outbox WHERE attachment=?2 AND state='queued')",params![f.channel.to_string(),f.id],|r|r.get(0))?;
            let (kept, last): (bool, i64) = tx.query_row(
                "SELECT kept,last_used FROM attachments WHERE id=?1",
                [&f.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if active || kept {
                continue;
            }
            if unchanged
                && managed_path(&self.workspace, &path)
                && original_metadata
                    .as_ref()
                    .zip(std::fs::metadata(&path).ok().as_ref())
                    .is_some_and(|(a, b)| {
                        a.len() == b.len() && a.modified().ok() == b.modified().ok()
                    })
            {
                std::fs::remove_file(&path)?;
            }
            if managed_path(&self.workspace, &path) {
                let _ = std::fs::remove_dir(path.parent().unwrap());
                let _ = std::fs::remove_dir(path.parent().unwrap().parent().unwrap());
                let _ = std::fs::remove_dir(self.workspace.join(".pantheon-attachments"));
            }
            let complete:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM outbox WHERE attachment=?1 AND state IN ('sent','failed'))",[&f.id],|r|r.get(0))?;
            if complete
                || self.config.retention_seconds > 0
                    && now().saturating_sub(last) >= self.config.retention_seconds as i64
            {
                let blob = self.blob(&f);
                if blob.exists() {
                    std::fs::remove_file(blob)?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }
    /// New user items only: never rewrite an existing cached prefix or embed bytes in memory.
    pub async fn input_parts(
        &self,
        channel: u64,
        ids: &[String],
        model: &str,
        budget: InputBudget,
    ) -> Result<Vec<Value>> {
        let mut parts = vec![];
        let mut remaining = budget;
        for id in ids {
            for f in self.files(channel, Some(id))? {
                if f.hash.is_none() || !self.blob(&f).is_file() {
                    continue;
                }
                self.touch(&f.id)?;
                if let Some(part) = native_part(
                    &self.blob(&f),
                    &f.filename,
                    &f.mime,
                    model,
                    self.config.codex_native_files,
                    &mut remaining,
                )
                .await?
                {
                    parts.extend(labelled_native(
                        part,
                        &format!("{} · {}", f.id, f.filename),
                        model,
                    ));
                }
            }
        }
        Ok(parts)
    }
    pub async fn inspect(
        &self,
        path: &Path,
        model: &str,
        page: Option<u64>,
        budget: InputBudget,
        cancel: &CancellationToken,
    ) -> Result<(String, Vec<Value>)> {
        let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
        let mime = mime(filename, None);
        let mut remaining = budget;
        if mime.starts_with("text/") || mime == "application/json" {
            return Ok((crate::tools::read_file(path).await?, vec![]));
        }
        if page.is_none()
            && let Some(part) = native_part(
                path,
                filename,
                &mime,
                model,
                self.config.codex_native_files,
                &mut remaining,
            )
            .await?
        {
            return Ok((
                format!(
                    "Opened {filename} as native {} input. Original remains at {}. DOCX/PPTX inputs omit embedded visuals; spreadsheet inputs may summarize/truncate rows. Use shell for complete tabular analysis.",
                    if mime.starts_with("image/") {
                        "image"
                    } else {
                        "file"
                    },
                    path.display()
                ),
                labelled_native(part, filename, model),
            ));
        }
        if mime == "application/pdf" {
            let page = page.unwrap_or(1);
            ensure!(page > 0, "PDF pages start at 1");
            let binary = std::env::var("PANTHEON_PDFTOTEXT").unwrap_or_else(|_| "pdftotext".into());
            let text = crate::tools::command_readonly(
                &binary,
                &[
                    "-f".into(),
                    page.to_string(),
                    "-l".into(),
                    page.to_string(),
                    "-layout".into(),
                    path.to_string_lossy().into_owned(),
                    "-".into(),
                ],
                cancel,
            )
            .await?;
            let temp = Temporary(
                self.root
                    .join("tmp")
                    .join(format!("{}.png", uuid::Uuid::new_v4())),
            );
            let prefix = temp.0.with_extension("");
            let binary = std::env::var("PANTHEON_PDFTOPPM").unwrap_or_else(|_| "pdftoppm".into());
            crate::tools::command_readonly(
                &binary,
                &[
                    "-f".into(),
                    page.to_string(),
                    "-l".into(),
                    page.to_string(),
                    "-singlefile".into(),
                    "-scale-to".into(),
                    "1920".into(),
                    "-png".into(),
                    path.to_string_lossy().into_owned(),
                    prefix.to_string_lossy().into_owned(),
                ],
                cancel,
            )
            .await?;
            let image = native_part(
                &temp.0,
                "page.png",
                "image/png",
                model,
                false,
                &mut remaining,
            )
            .await?;
            return Ok((
                format!(
                    "PDF page {page} (only this page, request further pages explicitly):\n{text}"
                ),
                image
                    .into_iter()
                    .flat_map(|part| {
                        labelled_native(part, &format!("{filename} · page {page}"), model)
                    })
                    .collect(),
            ));
        }
        Ok((
            format!(
                "File available at {} ({} bytes, {mime}). Native input is unavailable for this backend/type or exceeds its request allowance. Inspect with workspace tools; use read(path, page) for PDF pages.",
                path.display(),
                tokio::fs::metadata(path).await?.len()
            ),
            vec![],
        ))
    }
}
fn record(r: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id: r.get(0)?,
        channel: r.get::<_, String>(1)?.parse().unwrap_or(0),
        message: r.get(2)?,
        filename: r.get(3)?,
        mime: r.get(4)?,
        size: r.get::<_, String>(5)?.parse().unwrap_or(0),
        hash: r.get(6)?,
        kept: r.get(7)?,
        error: r.get(8)?,
        outgoing: r.get(9)?,
    })
}
fn safe_name(name: &str) -> String {
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| {
            if matches!(c, '/' | '\\' | '"' | '<' | '>') {
                '_'
            } else {
                c
            }
        })
        .take(120)
        .collect();
    if name.is_empty() || name == "." || name == ".." {
        "attachment".into()
    } else {
        name
    }
}
pub fn mime(filename: &str, claimed: Option<&str>) -> String {
    let extension = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let known = match extension.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "doc" => "application/msword",
        "ppt" => "application/vnd.ms-powerpoint",
        "xls" => "application/vnd.ms-excel",
        "rtf" => "application/rtf",
        "odt" => "application/vnd.oasis.opendocument.text",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "json" => "application/json",
        "txt" | "md" | "markdown" | "html" | "xml" | "rs" | "py" | "js" | "ts" | "toml"
        | "yaml" | "yml" | "c" | "cpp" | "h" | "css" | "sh" | "log" | "sql" => "text/plain",
        _ => "",
    };
    if !known.is_empty() {
        known.into()
    } else {
        claimed
            .filter(|m| {
                m.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'+' | b'.'))
            })
            .unwrap_or("application/octet-stream")
            .into()
    }
}
fn validate_cdn(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("invalid attachment URL")?;
    #[cfg(test)]
    if parsed.host_str() == Some("127.0.0.1") {
        return Ok(());
    }
    ensure!(
        parsed.scheme() == "https"
            && matches!(
                parsed.host_str(),
                Some("cdn.discordapp.com" | "media.discordapp.net")
            )
            && parsed.username().is_empty()
            && parsed.password().is_none(),
        "attachment URL is not a Discord CDN URL"
    );
    Ok(())
}
async fn hash_file(path: &Path) -> Result<String> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}
fn hash_file_sync(path: &Path) -> Result<String> {
    hash_file_sync_cancel(path, &CancellationToken::new())
}
fn hash_file_sync_cancel(path: &Path, cancel: &CancellationToken) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        ensure!(!cancel.is_cancelled(), "attachment hashing cancelled");
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}

fn labelled_native(part: Value, label: &str, model: &str) -> Vec<Value> {
    if part["type"] == "input_image" || part["type"] == "image" {
        // Image schemas carry no filename; keep the association explicit in multi-file batches.
        vec![
            json!({"type":if model.starts_with("anthropic/") { "text" } else { "input_text" },"text":format!("Image evidence (external data): {label}")}),
            part,
        ]
    } else {
        vec![part]
    }
}

// OpenAI vision accepts non-animated GIF only. Parse blocks rather than counting bytes
// in compressed image data, and leave animated/malformed files available to workspace tools.
fn static_gif(bytes: &[u8]) -> bool {
    if bytes.len() < 13 || !matches!(&bytes[..6], b"GIF87a" | b"GIF89a") {
        return false;
    }
    let palette = |flags: u8| {
        if flags & 0x80 != 0 {
            3 * (2usize << (flags & 7))
        } else {
            0
        }
    };
    let mut position = 13 + palette(bytes[10]);
    let mut frames = 0;
    loop {
        match bytes.get(position) {
            Some(0x3b) => return frames == 1,
            Some(0x21) => {
                position += 2;
            }
            Some(0x2c) if frames == 0 => {
                frames += 1;
                let Some(flags) = bytes.get(position + 9) else {
                    return false;
                };
                position += 11 + palette(*flags); // descriptor, local table, LZW minimum size
            }
            _ => return false,
        }
        loop {
            let Some(&length) = bytes.get(position) else {
                return false;
            };
            position += 1;
            if length == 0 {
                break;
            }
            position += usize::from(length);
            if position > bytes.len() {
                return false;
            }
        }
    }
}

async fn native_part(
    path: &Path,
    filename: &str,
    mime: &str,
    model: &str,
    codex_files: bool,
    remaining: &mut InputBudget,
) -> Result<Option<Value>> {
    let image = matches!(
        mime,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    );
    let file = mime == "application/pdf"
        || mime.starts_with("text/")
        || matches!(
            mime,
            "application/json"
                | "application/msword"
                | "application/vnd.ms-powerpoint"
                | "application/vnd.ms-excel"
                | "application/rtf"
                | "application/vnd.oasis.opendocument.text"
        )
        || mime.starts_with("application/vnd.openxmlformats-officedocument.");
    let public = model.starts_with("openai/");
    if !image && !(file && (public || model.starts_with("codex/") && codex_files)) {
        return Ok(None);
    }
    let size = tokio::fs::metadata(path).await?.len();
    if !remaining.fits(size, image, model) {
        return Ok(None);
    }
    // Memory allocation is bounded by the provider's actual request allowance, not an ingress limit.
    let allowance = remaining
        .payload
        .min(if image { u64::MAX } else { remaining.files });
    let mut bytes = Vec::with_capacity(size.min(allowance) as usize);
    tokio::fs::File::open(path)
        .await?
        .take(allowance.saturating_add(1))
        .read_to_end(&mut bytes)
        .await?;
    if !remaining.fits(bytes.len() as u64, image, model) {
        return Ok(None);
    }
    if mime == "image/gif" && !static_gif(&bytes) {
        return Ok(None);
    }
    remaining.payload = remaining
        .payload
        .saturating_sub(bytes.len() as u64)
        .saturating_sub(1024);
    if image {
        remaining.images = remaining.images.saturating_sub(1);
    } else {
        remaining.files = remaining.files.saturating_sub(bytes.len() as u64);
    }
    let anthropic_image = image && model.starts_with("anthropic/");
    let mut data = if anthropic_image {
        String::new()
    } else {
        format!("data:{mime};base64,")
    };
    base64::engine::general_purpose::STANDARD.encode_string(bytes, &mut data);
    // Move transport bytes into JSON rather than cloning large base64 strings.
    let mut part = if image {
        if anthropic_image {
            json!({"type":"image","source":{"type":"base64","media_type":mime}})
        } else {
            json!({"type":"input_image","detail":"auto"})
        }
    } else {
        json!({"type":"input_file","filename":filename})
    };
    if anthropic_image {
        part["source"]["data"] = Value::String(data);
    } else {
        part[if image { "image_url" } else { "file_data" }] = Value::String(data);
    }
    Ok(Some(part))
}
#[cfg(test)]
mod tests;

/// Provider allowances govern model requests, never Discord attachment admission.
#[derive(Clone, Copy, Debug)]
pub struct InputBudget {
    pub files: u64,
    pub payload: u64,
    pub images: usize,
}
impl InputBudget {
    fn fits(&self, bytes: u64, image: bool, model: &str) -> bool {
        bytes.saturating_add(1024) < self.payload
            && if image {
                self.images > 0
                    && (!model.starts_with("anthropic/") || bytes.div_ceil(3) * 4 <= 10_000_000)
            } else {
                bytes < self.files
            }
    }
}
pub fn remaining_budget(model: &str, history: &[Value]) -> InputBudget {
    fn count(v: &Value) -> (u64, usize) {
        let own_file = if v["type"] == "input_file" {
            v["file_data"]
                .as_str()
                .and_then(|s| s.split_once(','))
                .map(|(_, s)| {
                    s.len() as u64 / 4 * 3
                        - s.bytes().rev().take_while(|b| *b == b'=').count() as u64
                })
                .unwrap_or(0)
        } else {
            0
        };
        let own_image = usize::from(v["type"] == "input_image" || v["type"] == "image");
        let children = match v {
            Value::Object(map) => map.values().map(count).collect::<Vec<_>>(),
            Value::Array(values) => values.iter().map(count).collect(),
            _ => vec![],
        };
        (
            own_file + children.iter().map(|c| c.0).sum::<u64>(),
            own_image + children.iter().map(|c| c.1).sum::<usize>(),
        )
    }
    // Base64 strings have no JSON escapes. Count their size without scanning/copying the bytes.
    fn size(v: &Value) -> u64 {
        match v {
            Value::String(s) => {
                if s.starts_with("data:") {
                    s.len() as u64 + 2
                } else {
                    s.len() as u64
                        + 2
                        + s.bytes()
                            .filter(|b| matches!(b, b'"' | b'\\' | 0..=31))
                            .map(|b| if b < 32 { 5 } else { 1 })
                            .sum::<u64>()
                }
            }
            Value::Object(map) => {
                2 + map
                    .iter()
                    .map(|(k, v)| {
                        k.len() as u64
                            + 4
                            + if k == "data" && v.is_string() {
                                v.as_str().unwrap().len() as u64 + 2
                            } else {
                                size(v)
                            }
                    })
                    .sum::<u64>()
            }
            Value::Array(values) => 2 + values.iter().map(|v| size(v) + 1).sum::<u64>(),
            _ => v.to_string().len() as u64,
        }
    }
    let (files, images) = history
        .iter()
        .map(count)
        .fold((0, 0), |(f, i), (ff, ii)| (f + ff, i + ii));
    let allowance = if model.starts_with("anthropic/") {
        32_000_000u64
    } else {
        512_000_000
    };
    InputBudget {
        files: 50_000_000u64.saturating_sub(files),
        payload: allowance
            .saturating_sub(history.iter().map(size).sum::<u64>())
            .saturating_mul(3)
            / 4,
        images: (if model.starts_with("anthropic/") {
            100usize
        } else {
            1500
        })
        .saturating_sub(images),
    }
}
/// Account for already queued tool media as well as the existing immutable transcript.
pub fn budget_with_media(model: &str, history: &[Value], media: &[Value]) -> InputBudget {
    let current = remaining_budget(model, history);
    let pending = remaining_budget(model, media);
    let full = remaining_budget(model, &[]);
    InputBudget {
        files: current.files.saturating_sub(full.files - pending.files),
        payload: current
            .payload
            .saturating_sub(full.payload - pending.payload),
        images: current.images.saturating_sub(full.images - pending.images),
    }
}

/// Existing curator text guards must not mistake base64 transport bytes for guide text.
pub fn text_chars(value: &Value) -> usize {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| {
                if matches!(key.as_str(), "file_data" | "image_url" | "data")
                    && value.as_str().is_some_and(|s| {
                        s.starts_with("data:")
                            || key == "data" && map.get("type").is_some_and(|v| v == "base64")
                    })
                {
                    key.len() + 32
                } else {
                    key.len() + text_chars(value)
                }
            })
            .sum(),
        Value::Array(values) => values.iter().map(text_chars).sum(),
        Value::String(s) => s.chars().count(),
        _ => 8,
    }
}

fn managed_path(workspace: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(workspace) else {
        return false;
    };
    let mut current = workspace.to_path_buf();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return false;
        }
        current.push(component);
        if current.is_symlink() {
            return false;
        }
    }
    true
}

pub(crate) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS attachment_ingress(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,channel TEXT NOT NULL,user TEXT NOT NULL,text TEXT NOT NULL,files TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'pending');
            CREATE INDEX IF NOT EXISTS attachment_ingress_pending ON attachment_ingress(channel,seq) WHERE state='pending';
            CREATE TABLE IF NOT EXISTS attachments(id TEXT PRIMARY KEY,channel TEXT NOT NULL,message TEXT NOT NULL,filename TEXT NOT NULL,mime TEXT NOT NULL,size TEXT NOT NULL,hash TEXT,kept INTEGER NOT NULL DEFAULT 0,error TEXT,outgoing INTEGER NOT NULL DEFAULT 0,last_used INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS attachments_message ON attachments(channel,message);
            CREATE TABLE IF NOT EXISTS attachment_scratch(path TEXT PRIMARY KEY);
            CREATE TABLE IF NOT EXISTS attachment_curators(channel TEXT PRIMARY KEY);")?;
    Ok(())
}
