use crate::{
    browser::BrowserManager,
    config::Config,
    discord::{Discord, Inbound, split_message},
    memory::{COMPACT, Kind, Memory, NodeKey},
    provider::{Provider, ToolCall, model_parts},
    store::{Input, Job, Store},
    tools,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub struct Harness {
    pub config: Config,
    pub store: Store,
    pub discord: Arc<Discord>,
    attachments: Arc<crate::attachments::Attachments>,
    attachment_jobs: Mutex<HashMap<u64, CancellationToken>>,
    provider: Provider,
    web: crate::web::Web,
    browser: BrowserManager,
    skills: crate::skill_library::SkillLibrary,
    curators: Mutex<HashMap<u64, CancellationToken>>,
    curator_changed: Notify,
    curator_instructions: String,
    mcp: crate::mcp::Mcp,
    channels: Mutex<HashMap<u64, Arc<Channel>>>,
    shared: Arc<SharedMemory>,
    coordinator_turn: Mutex<()>,
    run_started: std::sync::atomic::AtomicBool,
    compactor_settings: Mutex<()>,
    children: Mutex<HashMap<String, Child>>,
    known_workers: Mutex<HashSet<String>>,
    capacity: Arc<Semaphore>,
    shell_capacity: Arc<Semaphore>,
    claude_capacity: Arc<Semaphore>,
    shell_jobs: Mutex<HashMap<String, (u64, CancellationToken)>>,
    shutdown: CancellationToken,
    #[cfg(test)]
    native_before_use: Option<(usize, Arc<Notify>, Arc<Notify>)>,
    #[cfg(test)]
    native_batch_paused: Option<(Arc<Notify>, Arc<Notify>)>,
    master_system: String,
    child_system: String,
}
// Memory belongs to the bot installation, never to a reply channel.
// One mutex linearizes journal IDs/publication; one scheduler owns compactions.
struct SharedMemory {
    memory: Mutex<Memory>,
    changed: Notify,
    #[cfg(test)]
    compaction_selected: Notify,
}
struct Channel {
    shared: Arc<SharedMemory>,
    settings_lock: Mutex<()>,
    incoming: Notify,
    cancel: Mutex<Option<CancellationToken>>,
}
impl std::ops::Deref for Channel {
    type Target = SharedMemory;
    fn deref(&self) -> &SharedMemory {
        &self.shared
    }
}
// Discord snowflakes are nonzero. Operational compactor settings use this
// reserved instance-global scope; it is never a message delivery destination.
const GLOBAL_COMPACTOR: u64 = 0;
fn attributed_input(input: &Input) -> String {
    format!(
        "[channel:{} user:{}]\n{}",
        input.channel, input.user, input.text
    )
}
struct Child {
    channel: u64,
    cancel: CancellationToken,
}
struct ChildStart {
    id: String,
    channel: u64,
    user: u64,
    task: String,
    previous: String,
    settings: (String, String),
    memory: Arc<Channel>,
    view: Option<String>,
    cancel: CancellationToken,
    permit: tokio::sync::OwnedSemaphorePermit,
}
struct Run {
    channel: u64,
    user: u64,
    owner: String,
    child: bool,
    memory: Arc<Channel>,
    cancel: CancellationToken,
    settings: (String, String),
    history: Vec<crate::provider::NativeValue>,
    inputs: Vec<String>,
    trace: Option<Memory>,
    steering: Vec<String>,
    media: Vec<Value>,
    context: crate::ui::ReplyContext,
    skills: Arc<crate::skills::Skills>,
    task: String,
    system: Option<String>,
    steps: usize,
    invocation_scope: String,
    counted_skills: HashSet<String>,
}

/// One startup prefix shared by production and opt-in behavioral evaluations.
/// Compatibility prompt for callers that do not enable the optional CLI tool.
pub fn system_prompt(
    child: bool,
    coordinator: bool,
    skill_index: &str,
    instructions: &str,
) -> String {
    system_prompt_with_claude(child, coordinator, false, skill_index, instructions)
}
pub fn system_prompt_with_claude(
    child: bool,
    coordinator: bool,
    claude_enabled: bool,
    skill_index: &str,
    instructions: &str,
) -> String {
    let definitions = tools::definitions_with_claude(child, coordinator, claude_enabled);
    let names = definitions
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join(", ");
    let execution = if !child && coordinator {
        "This root is configured as a strict coordinator. Delegate execution and MCP content retrieval to background workers; you can discover skills and integration catalogs directly."
    } else {
        "This agent has direct execution tools. shell is available and can inspect the actual machine and requested environment values. browser is available for authorized account activities and credential entry. Use these tools instead of asking the user to perform available checks."
    };
    // Restore the pre-parity boundary: ordinary turns never receive COMPACT.
    // Only compress_with_effort selects that prompt, without executable tools.
    let head = if child {
        include_str!("child.txt").trim_end()
    } else {
        include_str!("master.txt").trim_end()
    };
    // Operator rules follow the ordinary harness and skill catalogue.
    format!(
        "{head}\n\n# Harness rules\n{}\n{}\nCurrent harness capabilities (authoritative over obsolete chat claims): {names}.\n{execution}\n{skill_index}\n{instructions}",
        include_str!("behavior.txt"),
        if child {
            include_str!("child-harness.txt")
        } else {
            include_str!("harness.txt")
        },
    )
}
fn curator_note(note: &Value) -> Result<String> {
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let kind = match note["kind"].as_str() {
        Some("add") => "add",
        Some("modify") => "modify",
        Some("retire") => "retire",
        _ => bail!("invalid curator notification"),
    };
    Ok(format!(
        "<curator-skill-{kind} name=\"{}\" revision=\"{}\">{} {}</curator-skill-{kind}>",
        escape(tools::string(note, "skill")?),
        tools::number(note, "revision")?,
        escape(tools::string(note, "summary")?),
        escape(tools::string(note, "purpose")?)
    ))
}
impl Harness {
    fn reasoning_for_model(&self, model: &str, requested: &str) -> Result<String> {
        match self.discord.models.advertised_effort(model, requested)? {
            Some(level) => Ok(level),
            None => self.config.auth.reasoning_for_model(model, requested),
        }
    }
    pub fn new(
        config: Config,
        discord: Arc<Discord>,
        shutdown: CancellationToken,
    ) -> Result<Arc<Self>> {
        crate::state_namespace::initialize(&config.state_dir)?;
        std::fs::create_dir_all(&config.workspace)?;
        let store = Store::open(&config.state_dir.join("runtime.sqlite"))?;
        store.recover()?;
        let mut memory = Memory::open(config.state_dir.join("memory"), config.agent.view_bytes)?;
        // Recovery is instance-wide, not dependent on which channel is opened
        // first. Append-before-ack with source IDs makes this crash-idempotent.
        for input in store.unlogged_global()? {
            if store.input_is_work(&input.id)? {
                memory.append_work_with_id(&attributed_input(&input), &input.id)?;
            } else {
                memory.append_with_id(Kind::User, &attributed_input(&input), &input.id)?;
            }
            store.mark_logged(&input.id)?;
        }
        let shared = Arc::new(SharedMemory {
            memory: Mutex::new(memory),
            changed: Notify::new(),
            #[cfg(test)]
            compaction_selected: Notify::new(),
        });
        discord.models.initialize(&config);
        for (channel, model) in store.chat_models()? {
            discord.models.set_chat_model(channel, &model);
        }
        discord.models.set_kind_model(
            GLOBAL_COMPACTOR,
            "compact",
            Some(&store.compactor_model(GLOBAL_COMPACTOR, &config.agent.compactor_model)?),
        )?;
        for channel in store.model_override_channels()? {
            discord.models.set_kind_model(
                channel,
                "curator",
                store.curator_model(channel)?.as_deref(),
            )?;
        }
        let instructions = config.instructions()?;
        let skills = crate::skill_library::SkillLibrary::open(&config.skills, &config.state_dir)?;
        let skill_index = format!(
            "{}\n{}",
            crate::skill_library::INDEX,
            skills.catalogue(config.curator.description_chars)?
        );
        let attachments = Arc::new(crate::attachments::Attachments::open(
            &config.state_dir,
            &config.workspace,
            config.attachments.clone(),
        )?);
        attachments.protect_curators(&if config.curator.enabled {
            skills
                .queued_channels()?
                .into_iter()
                .filter_map(|c| c.parse::<u64>().ok())
                .collect()
        } else {
            HashSet::new()
        })?;
        let h = Self {
            attachments,
            attachment_jobs: Mutex::new(HashMap::new()),
            provider: Provider::new(config.agent.request_timeout_seconds)?
                .with_error_capture(config.agent.provider_error_capture, &config.state_dir)
                .with_request_retries(
                    config.agent.request_max_attempts,
                    config.agent.request_backoff_seconds,
                )?
                .with_auth(config.auth.clone())
                .with_zai(config.zai.clone())?
                .with_catalog(discord.models.clone()),
            web: crate::web::Web::new(config.state_dir.join("web/cache"), config.web.clone())?,
            browser: BrowserManager::new(
                config.state_dir.join("browsers"),
                config.browser.clone(),
                config.workspace.clone(),
            ),
            mcp: crate::mcp::Mcp::new(&config.mcp, &config.workspace, &config.state_dir)?,
            capacity: Arc::new(Semaphore::new(config.agent.max_subagents)),
            shell_capacity: Arc::new(Semaphore::new(config.agent.max_shell_jobs)),
            claude_capacity: Arc::new(Semaphore::new(config.claude.max_jobs)),
            shell_jobs: Mutex::new(HashMap::new()),
            #[cfg(test)]
            native_before_use: None,
            #[cfg(test)]
            native_batch_paused: None,
            master_system: system_prompt_with_claude(
                false,
                config.agent.coordinator_root,
                config.claude.enabled,
                &skill_index,
                &instructions,
            ),
            child_system: system_prompt_with_claude(
                true,
                false,
                config.claude.enabled,
                &skill_index,
                &instructions,
            ),
            skills,
            curators: Mutex::new(HashMap::new()),
            curator_changed: Notify::new(),
            curator_instructions: instructions,
            config,
            store,
            discord,
            channels: Mutex::new(HashMap::new()),
            shared,
            coordinator_turn: Mutex::new(()),
            run_started: std::sync::atomic::AtomicBool::new(false),
            compactor_settings: Mutex::new(()),
            children: Mutex::new(HashMap::new()),
            known_workers: Mutex::new(HashSet::new()),
            shutdown,
        };
        Ok(Arc::new(h))
    }
    async fn channel(self: &Arc<Self>, id: u64) -> Result<Arc<Channel>> {
        let mut channels = self.channels.lock().await;
        if let Some(c) = channels.get(&id) {
            return Ok(c.clone());
        }
        let c = Arc::new(Channel {
            shared: self.shared.clone(),
            settings_lock: Mutex::new(()),
            incoming: Notify::new(),
            cancel: Mutex::new(None),
        });
        self.discord.models.set_kind_model(
            id,
            "curator",
            self.store.curator_model(id)?.as_deref(),
        )?;
        channels.insert(id, c.clone());
        let h = self.clone();
        let cc = c.clone();
        tokio::spawn(async move {
            if let Err(e) = h.clone().channel_worker(id, cc).await {
                tracing::error!(channel=id,error=%e,"channel worker stopped");
                h.shutdown.cancel();
            }
        });
        Ok(c)
    }
    pub async fn run(self: Arc<Self>, mut inbound: mpsc::Receiver<Inbound>) -> Result<()> {
        ensure!(
            !self
                .run_started
                .swap(true, std::sync::atomic::Ordering::AcqRel),
            "Harness runtime already started; do not create a second global scheduler"
        );
        // Start even with no queued channel: an idle reopen may have unfinished
        // native summaries. Never hold the coordinator permit in this scheduler.
        let h = self.clone();
        let compact = tokio::spawn(async move {
            if let Err(e) = h
                .clone()
                .compactor(GLOBAL_COMPACTOR, h.shared.clone())
                .await
            {
                tracing::error!(error=%e,"global compactor stopped");
                h.shutdown.cancel();
            }
        });
        let h = self.clone();
        let discovery = tokio::spawn(async move {
            loop {
                tokio::select! {_=h.shutdown.cancelled()=>break,_=h.discord.models.refresh(&h.config,&h.provider)=>{}}
                tokio::select! {_=h.shutdown.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(300))=>{}}
            }
        });
        let h = self.clone();
        let out = tokio::spawn(async move {
            let result = h.outbox_worker().await;
            if result.is_err() {
                h.shutdown.cancel();
            }
            result
        });
        let h = self.clone();
        let jobs = tokio::spawn(async move {
            let result = h.clone().job_worker().await;
            if result.is_err() {
                h.shutdown.cancel();
            }
            result
        });
        let h = self.clone();
        let progress = tokio::spawn(async move { h.progress_worker().await });
        let h = self.clone();
        let reactions = tokio::spawn(async move { h.reaction_worker().await });
        let h = self.clone();
        let curation = tokio::spawn(async move { h.curator_worker().await });
        let h = self.clone();
        let attachments = tokio::spawn(async move {
            if let Err(e) = h.clone().attachment_worker().await {
                tracing::error!(error=%e,"attachment worker stopped");
                h.shutdown.cancel();
            }
        });
        let mut commands = tokio::task::JoinSet::new();
        let command_capacity = Arc::new(Semaphore::new(16));
        for id in self.store.queued_channels()? {
            self.channel(id).await?.incoming.notify_one();
        }
        loop {
            tokio::select! {
                _=self.shutdown.cancelled()=>break,
                Some(result)=commands.join_next(),if !commands.is_empty()=>{if let Err(e)=result{tracing::error!(error=%e,"command task failed");}},
                input=inbound.recv()=>{
                    let Some(input)=input else{break;};
                    match input {
                        Inbound::Prompt{id,channel,user,text,attachments}=>{
                            self.attachments.queue(&Input{id:id.clone(),channel,user,text},&attachments)?;
                            self.discord.acknowledge(&id)?;
                        }
                        Inbound::Command{id:_,token,channel,user,name,options}=>{
                            let h=self.clone();let permit=command_capacity.clone().try_acquire_owned();
                            commands.spawn(async move {
                                let card=match permit {
                                    Ok(_permit)=>match h.command(channel,user,&name,&options).await {Ok(card)=>card,Err(e)=>crate::ui::card("Command couldn't complete",&e.to_string(),vec![],true)},
                                    Err(_)=>crate::ui::card("Please try again shortly","This channel has too many active commands.",vec![],true),
                                };
                                if h.discord.reply_card(&token,card).await.is_err(){tracing::warn!(channel,"interaction reply failed");}
                            });
                        }
                        Inbound::Component{id:_,token,channel,user,custom_id,values}=>{
                            let h=self.clone();commands.spawn(async move {
                                let card=crate::skill_dashboard::render(&h.skills,channel,user,&custom_id,&values).unwrap_or_else(|e|json!({"content":e.to_string(),"embeds":[],"components":[],"allowed_mentions":{"parse":[]}}));
                                if let Err(e)=h.discord.reply_card(&token,card).await {tracing::warn!(channel,error=%e,"dashboard interaction failed");}
                            });
                        }
                    }
                }
            }
        }
        self.shutdown.cancel();
        discovery.abort();
        let channels = self.channels.lock().await;
        for c in channels.values() {
            if let Some(cancel) = c.cancel.lock().await.as_ref() {
                cancel.cancel();
            }
        }
        drop(channels);
        for child in self.children.lock().await.values() {
            child.cancel.cancel();
        }
        self.browser.shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            let _ = compact.await;
            let _ = out.await;
            let _ = jobs.await;
            let _ = progress.await;
            let _ = reactions.await;
            let _ = curation.await;
            let _ = attachments.await;
        })
        .await;
        Ok(())
    }
    async fn append(c: &Channel, kind: Kind, text: &str) -> Result<u64> {
        let id = c.memory.lock().await.append(kind, text)?;
        c.changed.notify_waiters();
        Ok(id)
    }
    async fn append_input(c: &Channel, input: &Input) -> Result<()> {
        c.memory
            .lock()
            .await
            .append_with_id(Kind::User, &attributed_input(input), &input.id)?;
        c.changed.notify_waiters();
        Ok(())
    }
    async fn append_admitted_input(&self, c: &Channel, input: &Input) -> Result<()> {
        if self.store.input_is_work(&input.id)? {
            c.memory
                .lock()
                .await
                .append_work_with_id(&attributed_input(input), &input.id)?;
            c.changed.notify_waiters();
            Ok(())
        } else {
            Self::append_input(c, input).await
        }
    }
    async fn settle(&self, c: &Channel, cancel: &CancellationToken) -> Result<String> {
        loop {
            let changed = c.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let memory = c.memory.lock().await;
                if memory.is_settled() {
                    return Ok(memory.render());
                }
            }
            tokio::select! {_=changed=>{},_=cancel.cancelled()=>bail!("turn cancelled while waiting for summaries"),_=self.shutdown.cancelled()=>bail!("shutdown")}
        }
    }
    /// Fresh-turn admission mirrors Chat.from/Memory.whole: originals are
    /// journaled before selecting the raw suffix; wait only for summaries before
    /// that suffix, and recompute it when more same-activity inputs arrive.
    async fn prepare_native_turn(
        &self,
        channel: u64,
        c: &Channel,
        cancel: &CancellationToken,
        context: &crate::ui::ReplyContext,
        queued: &mut Vec<Input>,
    ) -> Result<(String, Vec<String>)> {
        let mut logged = HashSet::new();
        loop {
            ensure!(
                !cancel.is_cancelled() && !self.shutdown.is_cancelled(),
                "turn cancelled while preparing context"
            );
            let changed = c.changed.notified();
            let incoming = c.incoming.notified();
            tokio::pin!(changed, incoming);
            changed.as_mut().enable();
            incoming.as_mut().enable();
            for input in self.store.queued(channel)? {
                if !queued.iter().any(|q| q.id == input.id)
                    && self.store.reply_context(&input.id)?.activity == context.activity
                {
                    queued.push(input);
                }
            }
            for input in queued.iter() {
                if logged.insert(input.id.clone()) {
                    self.append_admitted_input(c, input).await?;
                    self.store.mark_logged(&input.id)?;
                }
            }
            {
                let memory = c.memory.lock().await;
                let first = memory
                    .lookup_source(&queued[0].id)
                    .context("missing admitted native source")?;
                let from = memory.whole(first)?;
                if memory.settled_before(from)? {
                    let mut texts = Vec::new();
                    for input in queued.iter() {
                        let (start, end) = memory.source_span(&input.id)?;
                        if end > from {
                            texts.push(memory.say(start.max(from), end)?);
                        }
                    }
                    return Ok((memory.render_before(from)?, texts));
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("turn cancelled while waiting for summaries"),
                _ = self.shutdown.cancelled() => bail!("shutdown"),
                _ = changed => {},
                _ = incoming => {},
            }
        }
    }
    fn admit_prompt(&self, input: &Input) -> Result<bool> {
        self.store.admit(input)
    }
    async fn curator_settings(&self, channel: u64) -> Result<(String, String)> {
        let main = self.store.settings(
            channel,
            &self.config.agent.model,
            &self.config.agent.reasoning,
        )?;
        let selected = self.store.curator_model(channel)?;
        let model = crate::curator::effective_model(
            &main.0,
            &self.config.agent.model,
            self.config.curator.model.as_deref(),
            selected.as_deref(),
        )?;
        let selected = self.store.reasoning_override(channel, "curator")?;
        let inherited = self.discord.models.reasoning(&model, &main.1);
        let requested = match selected.as_deref() {
            Some("inherit") => inherited.clone(),
            Some(e) => e.to_owned(),
            None => self.config.curator.reasoning.clone().unwrap_or(inherited),
        };
        let effort = self.reasoning_for_model(&model, &requested)?;
        self.discord.models.validate_effort(&model, &effort)?;
        Ok((model, effort))
    }
    // Only short map/SQLite operations run under this lock. GC hashes files independently.
    async fn refresh_attachment_curators(&self) -> Result<()> {
        let curators = self.curators.lock().await;
        let mut protected: HashSet<u64> = curators.keys().copied().collect();
        if self.config.curator.enabled {
            protected.extend(
                self.skills
                    .queued_channels()?
                    .into_iter()
                    .filter_map(|c| c.parse::<u64>().ok()),
            );
        }
        self.attachments.protect_curators(&protected)
    }
    async fn cancel_curator(&self, channel: u64) -> Result<()> {
        if let Some(cancel) = self.curators.lock().await.get(&channel) {
            cancel.cancel();
        }
        self.skills.cancel_channel_forks(&channel.to_string())?;
        self.refresh_attachment_curators().await?;
        Ok(())
    }
    async fn curator_worker(self: Arc<Self>) {
        loop {
            tokio::select! {_=self.shutdown.cancelled()=>break,_=self.curator_changed.notified()=>{},_=tokio::time::sleep(Duration::from_secs(10))=>{}}
            let result: Result<()> = async {
                // Bridge publication receipts idempotently; they never enter the root inbox.
                for channel in self.skills.notification_channels()? {
                    for event in self.skills.pending_skill_events(&channel)? {
                        let verb = match event["kind"].as_str() {
                            Some("add") => ("✦", "created"),
                            Some("retire") => ("⊖", "retired"),
                            _ => ("↻", "modified"),
                        };
                        let id = event["id"].as_str().context("notification id")?;
                        self.store.curator_activity(
                            channel.parse()?,
                            id,
                            &format!(
                                "{} curator · {} skill {}",
                                verb.0,
                                verb.1,
                                event["skill"].as_str().unwrap_or("unknown")
                            ),
                        )?;
                        self.skills
                            .mark_skill_events_presented(&channel, &[id.into()])?;
                    }
                }
                if !self.config.curator.enabled {
                    return Ok(());
                }
                for channel in self.skills.queued_channels()? {
                    let id = channel.parse::<u64>()?;
                    if self.curators.lock().await.contains_key(&id)
                        || self.store.has_channel_work(id)?
                    {
                        continue;
                    }
                    let requested = self.skills.fork_requested(&channel)?;
                    let last = self
                        .skills
                        .last_settled(&channel)?
                        .unwrap_or(crate::store::now());
                    if !requested
                        && crate::store::now() - last < self.config.curator.idle_seconds as i64
                    {
                        continue;
                    }
                    let c = self.channel(id).await?;
                    let memory = {
                        let m = c.memory.lock().await;
                        if !m.is_settled() {
                            continue;
                        }
                        Arc::new(m.snapshot()?)
                    };
                    // Claim under channel-specific state. Other channels continue independently.
                    let cancel = self.shutdown.child_token();
                    let job = {
                        // Keep the queued -> active reader-protection transition atomic.
                        let mut curators = self.curators.lock().await;
                        let Some(job) = self.skills.take_fork(&channel)? else {
                            continue;
                        };
                        curators.insert(id, cancel.clone());
                        job
                    };
                    let h = self.clone();
                    tokio::spawn(async move {
                        let result: Result<()> = async {
                            let settings = h.curator_settings(id).await?;
                            h.skills
                                .pin_fork_settings(&job.id, &settings.0, &settings.1)?;
                            let provider = h.provider.clone().with_cache_affinity(
                                h.store.cache_affinity(&format!("curator:{id}"))?,
                            );
                            let reviewers = (0..h.config.curator.reviewers)
                                .map(|slot| {
                                    Ok(h.provider.clone().with_cache_affinity(
                                        h.store.cache_affinity(&format!(
                                            "curator-reviewer:{id}:{slot}"
                                        ))?,
                                    ))
                                })
                                .collect::<Result<Vec<_>>>()?;
                            crate::curator::run(
                                crate::curator::Environment {
                                    provider,
                                    reviewer_providers: reviewers,
                                    library: &h.skills,
                                    web: &h.web,
                                    workspace: &h.config.workspace,
                                    config: &h.config.curator,
                                    instructions: &h.curator_instructions,
                                    model: &settings.0,
                                    reasoning: &settings.1,
                                    cancel: &cancel,
                                    attachments: Some((&h.attachments, &h.discord, id)),
                                },
                                &job,
                                memory,
                            )
                            .await
                        }
                        .await;
                        if let Err(e) = result {
                            let _ = h.skills.finish_fork(
                                &job.id,
                                "failed",
                                &json!({"reason":e.to_string()}),
                                &[],
                            );
                            tracing::warn!(channel=id,error=%e,"curator fork failed");
                        }
                        h.curators.lock().await.remove(&id);
                        if let Err(e) = h.refresh_attachment_curators().await {
                            tracing::warn!(error=%e,"attachment curator protection refresh failed");
                        }
                        h.curator_changed.notify_one();
                    });
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                tracing::warn!(error=%e,"curator maintenance failed");
            }
        }
    }
    async fn skill_notifications(&self, channel: u64, memory: &Channel) -> Result<Option<String>> {
        let notes = self.skills.notifications(&channel.to_string())?;
        if notes.is_empty() {
            return Ok(None);
        }
        let mut parts = vec![];
        let mut ids = vec![];
        for note in notes {
            let id = note["id"].as_str().context("notification id")?.to_owned();
            let text = curator_note(&note)?;
            let mut m = memory.memory.lock().await;
            let source = format!("curator-note:{id}");
            let already = m.has_source(&source);
            m.append_with_id(Kind::User, &text, &source)?;
            if !already {
                parts.push(text);
            }
            ids.push(id);
        }
        self.skills
            .acknowledge_notifications(&channel.to_string(), &ids)?;
        memory.changed.notify_waiters();
        if parts.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "<system-notification>\n{}\n</system-notification>",
            parts.join("\n")
        )))
    }
    async fn channel_worker(self: Arc<Self>, channel: u64, c: Arc<Channel>) -> Result<()> {
        loop {
            let notified = c.incoming.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.shutdown.is_cancelled() {
                break;
            }
            let queued = self.store.queued(channel)?;
            if queued.is_empty() {
                tokio::select! {_=notified=>continue,_=self.shutdown.cancelled()=>break};
            }
            let cancel = self.shutdown.child_token();
            *c.cancel.lock().await = Some(cancel.clone());
            let _turn = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    if !self.shutdown.is_cancelled() {
                        // A stopped channel waiting behind another coordinator
                        // must not answer its batch on the next loop. Preserve
                        // accepted inputs in the global journal before ack.
                        for input in self.store.queued(channel)? {
                            self.store.input_state(&input.id, "cancelled")?;
                            self.append_admitted_input(&c, &input).await?;
                            self.store.mark_logged(&input.id)?;
                        }
                    }
                    *c.cancel.lock().await = None; continue;
                },
                guard = self.coordinator_turn.lock() => guard,
            };
            // Another channel may have held the permit for a full turn.
            let queued = self.store.queued(channel)?;
            if queued.is_empty() {
                *c.cancel.lock().await = None;
                continue;
            }
            let context = self.store.reply_context(&queued[0].id)?;
            let mut queued = queued
                .into_iter()
                .filter(|input| {
                    self.store
                        .reply_context(&input.id)
                        .is_ok_and(|ctx| ctx.activity == context.activity)
                })
                .collect::<Vec<_>>();
            let owner = format!("channel:{channel}");
            let settings_guard = c.settings_lock.lock().await;
            let mut settings = self.store.settings(
                channel,
                &self.config.agent.model,
                &self.config.agent.reasoning,
            )?;
            settings.1 = self
                .reasoning_for_model(&settings.0, &settings.1)
                // Invalid saved settings become a visible turn error in Provider,
                // rather than killing the channel actor before it admits inputs.
                .unwrap_or(settings.1);
            self.store.set_settings(channel, &settings.0, &settings.1)?;
            drop(settings_guard);
            self.store
                .present_agent(&owner, channel, &context, "Coordinator", &settings.0)?;
            self.store.agent_phase(&owner, true, "Preparing context")?;
            let prepared = self
                .prepare_native_turn(channel, &c, &cancel, &context, &mut queued)
                .await;
            if let Err(e) = prepared {
                // Preserve cancelled admission in the immutable log without answering it.
                for input in &queued {
                    self.store.input_state(&input.id, "cancelled")?;
                    self.append_admitted_input(&c, input).await?;
                    self.store.mark_logged(&input.id)?;
                }
                self.notice(
                    &format!("cancel:{}", queued[0].id),
                    channel,
                    Some(queued[0].user),
                    &e.to_string(),
                )?;
                self.store.agent_phase(&owner, false, "Stopped")?;
                *c.cancel.lock().await = None;
                continue;
            }
            let (view, texts) = prepared.unwrap();
            let refresh = self.skills.catalogue_refresh_due(
                &channel.to_string(),
                self.config.curator.idle_seconds,
                crate::store::now(),
            )?;
            let catalogue = self.skills.cache_catalogue(
                &channel.to_string(),
                &self
                    .skills
                    .catalogue(self.config.curator.description_chars)?,
                refresh,
            )?;
            let system = system_prompt_with_claude(
                false,
                self.config.agent.coordinator_root,
                self.config.claude.enabled,
                &format!("{}\n{catalogue}", crate::skill_library::INDEX),
                &self.curator_instructions,
            );
            let note = self.skill_notifications(channel, &c).await?;
            let mut ids = vec![];
            for input in &queued {
                self.store.input_state(&input.id, "running")?;
                ids.push(input.id.clone());
            }
            let settings = self.store.settings(
                channel,
                &self.config.agent.model,
                &self.config.agent.reasoning,
            )?;
            self.store.cache_turn(
                channel,
                &settings.0,
                &settings.1,
                &system,
                &tools::definitions_with_claude(
                    false,
                    self.config.agent.coordinator_root,
                    self.config.claude.enabled,
                ),
                &view,
            )?;
            let (vendor, _) = model_parts(&settings.0)?;
            let working = self
                .children
                .lock()
                .await
                .iter()
                .filter(|(_, child)| child.channel == channel)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            let working = self.store.working_names(channel, &working)?;
            let mut run = Run {
                channel,
                user: queued[0].user,
                owner: format!("channel:{channel}"),
                child: false,
                memory: c.clone(),
                cancel: cancel.clone(),
                history: Provider::start_turn_with_agents(
                    vendor,
                    &view,
                    &texts.join("\n\n"),
                    note.as_deref(),
                    &working,
                ),
                settings,
                inputs: ids,
                trace: None,
                steering: vec![],
                media: vec![],
                context,
                skills: self.skills.snapshot()?,
                task: texts.join("\n\n").chars().take(8000).collect(),
                system: Some(system),
                steps: 0,
                invocation_scope: uuid::Uuid::new_v4().to_string(),
                counted_skills: HashSet::new(),
            };
            let parts = self
                .attachments
                .input_parts(
                    channel,
                    &run.inputs,
                    &run.settings.0,
                    crate::attachments::remaining_budget_native(&run.settings.0, &run.history),
                )
                .await?;
            Provider::attach_user_parts(run.history.last_mut().unwrap(), parts)?;
            let run_id = queued[0].id.clone();
            let result = self.clone().run_agent(run).await;
            if let Err(e) = result {
                let text = format!("Turn stopped: {e}. Tool effects already completed may remain.");
                Self::append(&c, Kind::Talk, &text).await?;
                self.notice(
                    &format!("error:{run_id}"),
                    channel,
                    Some(queued[0].user),
                    &text,
                )?;
                // Steering still queued is left for the next fresh turn.
            }
            self.store.agent_phase(&owner, false, "Idle")?;
            *c.cancel.lock().await = None;
        }
        Ok(())
    }
    fn run_agent(
        self: Arc<Self>,
        mut run: Run,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> {
        Box::pin(async move {
            let (vendor, _) = model_parts(&run.settings.0)?;
            let vendor = vendor.to_string();
            let defs = tools::definitions_with_claude(
                run.child,
                self.config.agent.coordinator_root,
                self.config.claude.enabled,
            );
            let system = run.system.clone().unwrap_or_else(|| {
                if run.child {
                    self.child_system.clone()
                } else {
                    self.master_system.clone()
                }
            });
            let name = self.store.agent_label(&run.owner)?;
            self.store.present_agent(
                &run.owner,
                run.channel,
                &run.context,
                &name,
                &run.settings.0,
            )?;
            self.store.agent_phase(&run.owner, true, "Thinking")?;
            let run_id = uuid::Uuid::new_v4().to_string();
            run.invocation_scope = run_id.clone();
            let outcome = self
                .run_steps(&mut run, &vendor, &defs, &system, &run_id)
                .await;
            if !run.child {
                self.skills
                    .note_channel_settled(&run.channel.to_string(), crate::store::now())?;
                if self.config.curator.enabled && run.steps >= self.config.curator.minimum_steps {
                    self.skills.enqueue_fork(&run.channel.to_string(),&run_id,&json!({"user":run.user,"activity":run.context.activity,"task":run.task,"model_iterations":run.steps,"turn_completed":outcome.is_ok()}))?;
                    self.refresh_attachment_curators().await?;
                    self.curator_changed.notify_one();
                }
            }
            self.store
                .refresh_activities(run.channel, run.memory.memory.lock().await.is_settled())?;
            self.store.agent_phase(
                &run.owner,
                false,
                if outcome.is_ok() { "Idle" } else { "Stopped" },
            )?;
            if !run.child {
                for id in &run.inputs {
                    self.store
                        .input_state(id, if outcome.is_ok() { "done" } else { "failed" })?;
                }
            }
            outcome
        })
    }
    // Completed calls are observed before execution, but Chat.on publishes them
    // with the matching echo; Chat.unanswered flushes those left at turn exit.
    // Owned cancellation runs this wrapper. Hard task abort/process death cannot
    // execute asynchronous cleanup; it does not replay tools on restart.
    async fn run_steps(
        self: &Arc<Self>,
        run: &mut Run,
        vendor: &str,
        defs: &[Value],
        system: &str,
        run_id: &str,
    ) -> Result<String> {
        let mut pending = Vec::new();
        let outcome = self
            .run_steps_inner(run, vendor, defs, system, run_id, &mut pending)
            .await;
        let flushed = self.flush_native_calls(run, &mut pending).await;
        match outcome {
            Err(error) => {
                // Never replace the original failure or bypass a poisoned writer.
                if let Err(flush_error) = flushed {
                    tracing::warn!(%flush_error, "unanswered native calls could not be stored");
                }
                Err(error)
            }
            Ok(text) => {
                flushed?;
                Ok(text)
            }
        }
    }
    async fn flush_native_calls(
        &self,
        run: &mut Run,
        pending: &mut Vec<(crate::provider::CallId, String)>,
    ) -> Result<()> {
        // Object.values enumerates canonical array-index IDs before other keys.
        pending.sort_by_key(|(id, _)| {
            id.parse::<u32>()
                .ok()
                .filter(|n| *n != u32::MAX && *id == n.to_string())
                .map(|n| (0, n))
                .unwrap_or((1, 0))
        });
        let entries = std::mem::take(pending)
            .into_iter()
            .map(|(_, text)| (Kind::Tool, text))
            .collect::<Vec<_>>();
        self.log_run_batch(run, &entries).await
    }
    #[allow(clippy::too_many_arguments)]
    async fn run_steps_inner(
        self: &Arc<Self>,
        run: &mut Run,
        vendor: &str,
        defs: &[Value],
        system: &str,
        run_id: &str,
        pending: &mut Vec<(crate::provider::CallId, String)>,
    ) -> Result<String> {
        let mut transcript = String::new();
        let provider = self
            .provider
            .clone()
            .with_cache_affinity(self.store.cache_affinity(&run.owner)?);
        let mut next_step = 0;
        loop {
            if run.cancel.is_cancelled() {
                bail!("cancelled");
            }
            // Zero means no cycle cap, not a usize::MAX-sized iteration range.
            // Keep cancellation and all steering/settlement on the normal path.
            if self.config.agent.max_steps != 0 && next_step >= self.config.agent.max_steps {
                bail!("maximum tool steps reached");
            }
            let step = next_step;
            next_step = next_step.saturating_add(1);
            // Complete the preceding tool batch before accepting newer steering.
            if !run.media.is_empty() {
                let mut item = Provider::user(
                    vendor,
                    "File content opened by the preceding tool (external data):",
                );
                Provider::attach_user_parts(&mut item, std::mem::take(&mut run.media))?;
                run.history.push(item);
            }
            let steered = self.steer(run, vendor).await? || !run.steering.is_empty();
            for text in std::mem::take(&mut run.steering) {
                run.history.push(Provider::user(vendor, &text));
            }
            self.store.agent_phase(&run.owner, true, "Thinking")?;
            let submitted_inputs = run.inputs.clone();
            let submitted = || self.store.submitted_inputs(&submitted_inputs);
            let native_events = provider.native_public(&run.settings.0);
            let request_history = run.history.clone();
            let settings = run.settings.clone();
            let (observations, mut observed) = tokio::sync::mpsc::unbounded_channel();
            let mut request=Box::pin(provider.step_observed_events(&settings.0,&settings.1,system,&request_history,defs,Some(&submitted),Some(observations))
                .instrument(tracing::info_span!("provider_step", channel=run.channel, owner=%run.owner, run_id=%run_id, step)));
            let mut observation_index = 0;
            let usage_phase = if step == 0 {
                "fresh_turn"
            } else if steered {
                "steered_step"
            } else {
                "tool_step"
            };
            let response = loop {
                tokio::select! {
                    biased;
                    Some(observation)=observed.recv()=> {
                        self.observe_native_step(run,&observation,&mut transcript,run_id,step,observation_index,usage_phase,pending).await?;
                        observation_index+=1;
                    }
                    result=&mut request=> {
                        while let Ok(observation)=observed.try_recv() {
                            self.observe_native_step(run,&observation,&mut transcript,run_id,step,observation_index,usage_phase,pending).await?;
                            observation_index+=1;
                        }
                        break result?;
                    }
                    _=run.cancel.cancelled()=> {
                        drop(request); // sends the active stream's stopped trace
                        while let Ok(observation)=observed.try_recv() {
                            self.observe_native_step(run,&observation,&mut transcript,run_id,step,observation_index,usage_phase,pending).await?;
                            observation_index+=1;
                        }
                        bail!("cancelled");
                    }
                }
            };
            drop(request);
            run.steps += 1;
            if !run.child && !native_events {
                self.store.observed_usage(
                    run.channel,
                    &run.settings.0,
                    &response.usage,
                    usage_phase,
                )?;
            }
            Provider::append_response(vendor, &mut run.history, &response);
            let final_steered = if response.calls.is_empty() {
                self.steer(run, vendor).await?
            } else {
                false
            };
            for (text, thought) in response.texts.iter().filter(|_| !native_events) {
                if text.trim().is_empty() {
                    continue;
                }
                if *thought {
                    if self.config.agent.show_reasoning && !run.child {
                        self.store.break_activity(run.channel)?;
                        for chunk in split_message(text, None) {
                            let _ = self
                                .discord
                                .send(run.channel, &chunk, None, &uuid::Uuid::new_v4().to_string())
                                .await;
                        }
                    }
                    continue;
                }
                self.log_run(run, Kind::Talk, text).await?;
                transcript.push_str(text);
                transcript.push('\n');
            }
            if !run.child {
                let text = response
                    .texts
                    .iter()
                    .filter(|(text, thought)| {
                        !*thought
                            && if native_events {
                                !crate::memory::summary_trim(text).is_empty()
                            } else {
                                !text.trim().is_empty()
                            }
                    })
                    .map(|(t, _)| t.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if response.calls.is_empty() && !final_steered {
                    // An intentionally silent completion still settles inputs and
                    // checks the atomic steering barrier, but queues no message.
                    // Do not split empty text: a mention prefix is itself a chunk.
                    let chunks = if text.is_empty() {
                        vec![]
                    } else {
                        split_message(
                            &text,
                            if run.context.reply_to.is_some()
                                || run.context.activity.starts_with("peer:")
                            {
                                None
                            } else {
                                Some(run.user)
                            },
                        )
                    };
                    let completed = self.store.complete_turn(
                        &run.inputs,
                        &format!("{run_id}:{step}:final"),
                        run.channel,
                        run.user,
                        &chunks,
                    )?;
                    if !completed {
                        if !text.is_empty() {
                            self.notice(
                                &format!("{run_id}:{step}:intermediate"),
                                run.channel,
                                None,
                                &text,
                            )?;
                        }
                        self.steer(run, vendor).await?;
                        continue;
                    }
                } else if !text.is_empty() {
                    self.notice(
                        &format!("{run_id}:{step}:intermediate"),
                        run.channel,
                        None,
                        &text,
                    )?;
                }
            }
            if response.calls.is_empty() {
                // A prompt arriving during the final provider call must still be consumed.
                if final_steered {
                    continue;
                }
                return Ok(if run.child {
                    response
                        .texts
                        .iter()
                        .filter(|(text, thought)| {
                            !*thought
                                && if native_events {
                                    !crate::memory::summary_trim(text).is_empty()
                                } else {
                                    !text.trim().is_empty()
                                }
                        })
                        .map(|(text, _)| text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                } else {
                    transcript
                });
            }
            for (index, call) in response.calls.iter().enumerate() {
                // Finish the model's requested batch before delivering queued steering.
                // Provider transcripts require a result for every requested call.
                #[cfg(test)]
                if native_events
                    && let Some((at, arrived, release)) = &self.native_before_use
                    && index == *at
                {
                    arrived.notify_one();
                    release.notified().await;
                }
                if run.cancel.is_cancelled() {
                    bail!("cancelled");
                }
                if !native_events {
                    self.log_run(run, Kind::Tool, &response.tool_entry(call, false))
                        .await?;
                }
                let tool_id = format!("{run_id}:{step}:tool:{index}");
                self.store.tool_start(&tool_id, run.channel, &call.name)?;
                let start = Instant::now();
                self.store
                    .agent_phase(&run.owner, true, &format!("Running {}", call.name))?;
                let control = matches!(call.name.as_str(), "spawn" | "tell");
                if !control {
                    self.store.start_tool_activity(
                        &run.owner,
                        &run.context,
                        run.channel,
                        &tool_id,
                        call,
                    )?;
                }
                let result = if !tools::definitions_with_claude(
                    run.child,
                    self.config.agent.coordinator_root,
                    self.config.claude.enabled,
                )
                .iter()
                .any(|tool| tool["name"] == call.name)
                {
                    Err(anyhow::anyhow!(
                        "tool {} is unavailable to this agent",
                        call.name
                    ))
                } else if !call.arguments.is_object() {
                    Err(anyhow::anyhow!("The input is not a JSON object."))
                } else {
                    self.execute_tool_output(run, call, Some(&tool_id)).await
                };
                let mut error = result.is_err();
                let (output, units) = match result {
                    Ok(v) => v,
                    Err(e) => (format!("Error: {e}"), None),
                };
                if call.name == "mcp"
                    && serde_json::from_str::<Value>(&output).is_ok_and(|v| v["isError"] == true)
                {
                    error = true;
                }
                // Native zoom already pages by UTF-16 units. A full page plus
                // its header can exceed the generic tool cap; clipping here
                // would silently destroy exact-history content without a cursor.
                let output = if call.name == "zoom" && !error {
                    output
                } else {
                    crate::memory::cap_tool_result(&output)
                };
                let mut entries = Vec::new();
                if native_events && let Some(at) = pending.iter().position(|(id, _)| id == &call.id)
                {
                    let (_, text) = pending.remove(at);
                    entries.push((Kind::Tool, text));
                }
                entries.push((Kind::Echo, output.clone()));
                self.log_run_batch(run, &entries).await?;
                let image =
                    if call.name == "browser" && call.arguments["action"] == "screenshot" && !error
                    {
                        let meta: Value = serde_json::from_str(&output)?;
                        let path = tools::string(&meta, "path")?;
                        if vendor == "zai" {
                            let (notice, parts) = self
                                .attachments
                                .inspect(
                                    std::path::Path::new(path),
                                    &run.settings.0,
                                    None,
                                    crate::attachments::budget_with_native_media(
                                        &run.settings.0,
                                        &run.history,
                                        &run.media,
                                    ),
                                    &run.cancel,
                                )
                                .await?;
                            run.media.extend(parts);
                            run.media
                                .push(Provider::user(vendor, &notice)["content"][0].clone());
                            None
                        } else {
                            let bytes = tokio::fs::read(path).await?;
                            ensure!(
                                bytes.len() <= 12_000_000,
                                "screenshot too large for provider input"
                            );
                            use base64::Engine;
                            Some(base64::engine::general_purpose::STANDARD.encode(bytes))
                        }
                    } else {
                        None
                    };
                if let Some(units) = units {
                    Provider::append_result_units(vendor, &mut run.history, call, units);
                } else {
                    Provider::append_result_with_image(
                        vendor,
                        &mut run.history,
                        call,
                        &output,
                        error,
                        image.as_deref(),
                    );
                }
                if native_events
                    && vendor == "openai"
                    && let Some(last) = run.history.last_mut()
                    && let Some(text) = last["output"].as_str()
                {
                    last.set("output", json!([{"type":"input_text","text":text}]));
                }
                self.store.tool_done(&tool_id)?;
                let background = !error
                    && serde_json::from_str::<Value>(&output)
                        .is_ok_and(|v| v["state"] == "background");
                if !control || error {
                    self.store.finish_tool_activity(
                        &run.owner,
                        &run.context,
                        run.channel,
                        &tool_id,
                        call,
                        &output,
                        if error {
                            "error"
                        } else if background {
                            "background"
                        } else {
                            "done"
                        },
                        start.elapsed(),
                    )?;
                }
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    async fn observe_native_step(
        &self,
        run: &mut Run,
        observation: &crate::provider::Observation,
        transcript: &mut String,
        run_id: &str,
        step: usize,
        index: usize,
        usage_phase: &str,
        pending: &mut Vec<(crate::provider::CallId, String)>,
    ) -> Result<()> {
        if !run.child {
            self.store.observed_usage(
                run.channel,
                &run.settings.0,
                &observation.response.usage,
                usage_phase,
            )?;
        }
        let completed = observation.completed.clone();
        let entries = observation.entries.clone().unwrap_or_else(|| {
            observation
                .response
                .texts
                .iter()
                .map(|(text, thought)| {
                    (
                        if *thought {
                            crate::provider::ObservationKind::Thought
                        } else {
                            crate::provider::ObservationKind::Reply
                        },
                        text.clone(),
                    )
                })
                .collect()
        });
        let entries = completed.unwrap_or_else(|| {
            entries
                .into_iter()
                .map(|(kind, text)| crate::provider::CompletedEntry {
                    kind,
                    text,
                    of: None,
                })
                .collect()
        });
        for entry in &entries {
            let (kind, text) = (&entry.kind, &entry.text);
            if matches!(kind, crate::provider::ObservationKind::Tool)
                && let Some(id) = &entry.of
            {
                if let Some((_, prior)) = pending.iter_mut().find(|(key, _)| key == id) {
                    *prior = text.clone();
                } else {
                    pending.push((id.clone(), text.clone()));
                }
                continue;
            }
            let mut paired = Vec::new();
            if let Some(id) = &entry.of
                && let Some(at) = pending.iter().position(|(key, _)| key == id)
            {
                let (_, tool) = pending.remove(at);
                paired.push((Kind::Tool, tool));
            }
            if matches!(kind, crate::provider::ObservationKind::Echo) {
                paired.push((Kind::Echo, text.clone()));
                self.log_run_batch(run, &paired).await?;
                continue;
            }
            // Non-Echo matching Entries are unusual, but the upstream callback
            // publishes their buffered call in the same synchronous hold too.
            if !paired.is_empty() {
                let kind = match kind {
                    crate::provider::ObservationKind::Tool => Kind::Tool,
                    _ => Kind::Talk,
                };
                paired.push((kind, text.clone()));
                self.log_run_batch(run, &paired).await?;
                if kind == Kind::Talk {
                    transcript.push_str(text);
                    transcript.push('\n');
                }
                continue;
            }

            // Completed public Entries already use ECMAScript trimming.
            // Rust whitespace additionally removes NEL, which is real reply text.
            if crate::memory::summary_trim(text).is_empty() {
                continue;
            }
            if matches!(kind, crate::provider::ObservationKind::Tool) {
                self.log_run(run, Kind::Tool, text).await?;
                continue;
            }
            if matches!(kind, crate::provider::ObservationKind::Thought) {
                if self.config.agent.show_reasoning && !run.child {
                    self.store.break_activity(run.channel)?;
                    for chunk in split_message(text, None) {
                        let _ = self
                            .discord
                            .send(run.channel, &chunk, None, &uuid::Uuid::new_v4().to_string())
                            .await;
                    }
                }
            } else {
                self.log_run(run, Kind::Talk, text).await?;
                transcript.push_str(text);
                transcript.push('\n');
            }
        }
        if observation.intermediate && !run.child {
            let text = observation
                .response
                .texts
                .iter()
                .filter(|(_, thought)| !*thought)
                .map(|(text, _)| text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            if !text.is_empty() {
                self.notice(
                    &format!("{run_id}:{step}:native:{index}"),
                    run.channel,
                    None,
                    &text,
                )?;
            }
        }
        Ok(())
    }
    async fn log_run(&self, run: &mut Run, kind: Kind, text: &str) -> Result<()> {
        self.log_run_batch(run, &[(kind, text.to_owned())]).await
    }
    // Match Chat.hold's synchronous publication boundary: no root arrival or
    // compactor may observe the middle of a completed call/result or unanswered
    // batch. Each append still fsyncs and fails closed. This is mutex isolation,
    // not a multi-record crash transaction: a disk failure can retain a prefix.
    async fn log_run_batch(&self, run: &mut Run, entries: &[(Kind, String)]) -> Result<()> {
        if let Some(trace) = run.trace.as_mut() {
            for (kind, text) in entries {
                trace.append(*kind, text)?;
            }
        } else {
            let mut memory = run.memory.memory.lock().await;
            // The position is used only by the deterministic test-only contention seam.
            #[allow(clippy::unused_enumerate_index)]
            for (_index, (kind, text)) in entries.iter().enumerate() {
                memory.append(*kind, text)?;
                #[cfg(test)]
                if _index == 0
                    && entries.len() > 1
                    && let Some((arrived, release)) = &self.native_batch_paused
                {
                    arrived.notify_one();
                    release.notified().await;
                }
            }
            drop(memory);
            run.memory.changed.notify_waiters();
        }
        Ok(())
    }
    async fn steer(&self, run: &mut Run, _vendor: &str) -> Result<bool> {
        let mut received = false;
        if !run.child {
            loop {
                let ready = run.memory.incoming.notified();
                tokio::pin!(ready);
                ready.as_mut().enable();
                if !self.attachments.has_pending(run.channel)? {
                    break;
                }
                tokio::select! {_=ready=>{},_=run.cancel.cancelled()=>bail!("cancelled while receiving files"),_=self.shutdown.cancelled()=>bail!("shutdown")};
            }
        }
        if run.child {
            for input in self.store.agent_events(&run.owner)? {
                if let Some(trace) = run.trace.as_mut() {
                    trace.append_with_id(Kind::User, &input.text, &input.id)?;
                }
                self.store.agent_event_done(&input.id)?;
                run.steering.push(input.text);
                received = true;
            }
        } else {
            if let Some(note) = self.skill_notifications(run.channel, &run.memory).await? {
                run.skills = self.skills.snapshot()?;
                run.steering.push(note);
                received = true;
            }
            for input in self.store.queued(run.channel)? {
                let context = self.store.reply_context(&input.id)?;
                let prompt = self.store.is_prompt(&input.id)?;
                if context.activity != run.context.activity && !prompt {
                    continue;
                }
                self.store.bind_context(&input.id, &run.context)?;
                self.store.input_state(&input.id, "running")?;
                self.append_admitted_input(&run.memory, &input).await?;
                self.store.mark_logged(&input.id)?;
                let parts = self
                    .attachments
                    .input_parts(
                        run.channel,
                        std::slice::from_ref(&input.id),
                        &run.settings.0,
                        crate::attachments::budget_with_native_media(
                            &run.settings.0,
                            &run.history,
                            &run.media,
                        ),
                    )
                    .await?;
                if parts.is_empty() {
                    run.steering.push(input.text);
                } else {
                    for text in std::mem::take(&mut run.steering) {
                        run.history.push(Provider::user(_vendor, &text));
                    }
                    let mut item = Provider::user(_vendor, &input.text);
                    Provider::attach_user_parts(&mut item, parts)?;
                    run.history.push(item);
                }
                run.inputs.push(input.id);
                received = true;
            }
        }
        Ok(received)
    }
    #[cfg(test)]
    async fn execute_tool(self: &Arc<Self>, run: &mut Run, call: &ToolCall) -> Result<String> {
        self.execute_tool_tracked(run, call, None).await
    }
    fn validate_tool(&self, run: &Run, call: &ToolCall) -> Result<()> {
        ensure!(
            tools::definitions_with_claude(
                run.child,
                self.config.agent.coordinator_root,
                self.config.claude.enabled
            )
            .iter()
            .any(|tool| tool["name"] == call.name),
            "tool {} is unavailable to this agent",
            call.name
        );
        let schema = tools::definitions_with_claude(
            run.child,
            self.config.agent.coordinator_root,
            self.config.claude.enabled,
        )
        .into_iter()
        .find(|tool| tool["name"] == call.name)
        .expect("tool validated");
        call.arguments
            .validate_builtin(&schema["input_schema"], call.name == "mcp")?;
        Ok(())
    }
    async fn execute_zoom_units(&self, run: &Run, a: &Value) -> Result<Vec<u16>> {
        if let Some(name) = a["id"].as_str().filter(|name| name.parse::<u64>().is_err()) {
            let Some(id) = self.store.agent_id(run.channel, name)? else {
                return Ok(crate::memory::clip_page_units(
                    &format!("No agent {name}."),
                    a["at"].as_u64().unwrap_or(0) as usize,
                ));
            };
            ensure!(
                id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
                "invalid worker identifier"
            );
            let directory = self.config.state_dir.join("subagents").join(id);
            if !directory.join("view.json").is_file() {
                return Ok(crate::memory::clip_page_units(
                    &format!("No agent {name}."),
                    a["at"].as_u64().unwrap_or(0) as usize,
                ));
            }
            return Memory::agent_log_units(&directory, a["at"].as_u64().unwrap_or(0) as usize);
        }
        return run.memory.memory.lock().await.zoom_units_at(
            a["id"]
                .as_str()
                .and_then(|id| id.parse().ok())
                .map(Ok)
                .unwrap_or_else(|| tools::number(a, "id"))?,
            a["n"].as_u64().unwrap_or(1),
            a["at"].as_u64().unwrap_or(0) as usize,
        );
    }
    async fn execute_tool_output(
        self: &Arc<Self>,
        run: &mut Run,
        call: &ToolCall,
        tool_id: Option<&str>,
    ) -> Result<(String, Option<Vec<u16>>)> {
        if call.name == "zoom" {
            self.validate_tool(run, call)?;
            let units = self.execute_zoom_units(run, &call.arguments).await?;
            Ok((String::from_utf16_lossy(&units), Some(units)))
        } else {
            Ok((self.execute_tool_tracked(run, call, tool_id).await?, None))
        }
    }
    async fn execute_tool_tracked(
        self: &Arc<Self>,
        run: &mut Run,
        call: &ToolCall,
        tool_id: Option<&str>,
    ) -> Result<String> {
        self.validate_tool(run, call)?;
        let a: &Value = &call.arguments;
        let value = match call.name.as_str() {
            "models" => self.discord.models.report(
                a["provider"].as_str(),
                a["query"].as_str().unwrap_or(""),
                a["offset"].as_u64().unwrap_or(0) as usize,
                a["limit"].as_u64().unwrap_or(25) as usize,
            ),
            "skill" => {
                let value = if a["action"] == "history" {
                    self.skills.history(
                        tools::string(a, "id")?,
                        a["offset"].as_u64().unwrap_or(0) as usize,
                    )?
                } else {
                    run.skills.execute(a)?
                };
                if a["action"] == "load" && a["file"].as_str().is_none_or(|file| file == "SKILL.md")
                {
                    let id = tools::string(a, "id")?;
                    if run.counted_skills.insert(id.into()) {
                        self.skills.record_invocation(id, &run.invocation_scope)?;
                    }
                }
                value
            }
            "mcp" => {
                let mut value = self
                    .mcp
                    .execute_input(
                        run.channel,
                        run.child,
                        self.config.agent.coordinator_root,
                        &call.arguments,
                        &run.cancel,
                    )
                    .await?;
                if run.settings.0.starts_with("zai/") {
                    crate::provider::zai::annotate_mcp(&mut value);
                }
                value
            }
            "zoom" => {
                return Ok(String::from_utf16_lossy(
                    &self.execute_zoom_units(run, a).await?,
                ));
            }
            "date" => return run.memory.memory.lock().await.date(tools::number(a, "id")?),
            "read" => {
                let p = tools::workspace_path(
                    &self.config.workspace,
                    tools::string(a, "path")?,
                    false,
                )?;
                let (text, parts) = self
                    .attachments
                    .inspect(
                        &p,
                        &run.settings.0,
                        a["page"].as_u64(),
                        crate::attachments::budget_with_native_media(
                            &run.settings.0,
                            &run.history,
                            &run.media,
                        ),
                        &run.cancel,
                    )
                    .await?;
                run.media.extend(parts);
                let lines = text.lines().count() as u64;
                if let Some(id) = tool_id {
                    self.store.tool_output_lines(id, lines)?;
                }
                return Ok(text);
            }
            "attachment" => {
                let action = tools::string(a, "action")?;
                match action {
                    "list" => self
                        .attachments
                        .list(run.channel, a["offset"].as_u64().unwrap_or(0))?,
                    "keep" | "release" => serde_json::to_value(self.attachments.keep(
                        run.channel,
                        tools::string(a, "id")?,
                        action == "keep",
                    )?)?,
                    "open" => {
                        let file = self
                            .attachments
                            .reopen(
                                run.channel,
                                tools::string(a, "id")?,
                                &self.discord,
                                &run.cancel,
                            )
                            .await?;
                        let path = self.attachments.materialize(&file).await?;
                        let (text, parts) = self
                            .attachments
                            .inspect(
                                &path,
                                &run.settings.0,
                                a["page"].as_u64(),
                                crate::attachments::budget_with_native_media(
                                    &run.settings.0,
                                    &run.history,
                                    &run.media,
                                ),
                                &run.cancel,
                            )
                            .await?;
                        run.media.extend(parts);
                        json!({"file":file,"path":self.attachments.relative_path(&file),"inspection":text})
                    }
                    _ => bail!("unknown attachment action"),
                }
            }
            "send_file" => {
                ensure!(!run.child, "only the orchestrator publishes files");
                let path = tools::workspace_path(
                    &self.config.workspace,
                    tools::string(a, "path")?,
                    false,
                )?;
                ensure!(
                    path.is_file(),
                    "send_file requires a regular workspace file"
                );
                let caption = a["caption"].as_str().unwrap_or("");
                ensure!(
                    caption.encode_utf16().count() <= 2000,
                    "caption exceeds Discord’s limit"
                );
                let id = format!(
                    "file-{}",
                    hex::encode(sha2::Sha256::digest(
                        call.id.scoped_preimage(&run.invocation_scope)
                    ))
                );
                ensure!(!run.cancel.is_cancelled(), "file snapshot cancelled");
                let file = tokio::select! {
                    _ = run.cancel.cancelled() => bail!("file snapshot cancelled"),
                    file = self.attachments.snapshot(run.channel, &id, &path) => file?,
                };
                ensure!(
                    !run.cancel.is_cancelled(),
                    "file delivery cancelled before queuing"
                );
                self.store.enqueue_file(
                    &id,
                    run.channel,
                    run.user,
                    caption,
                    &file.id,
                    run.context.reply_to,
                )?;
                json!({"delivery_id":id,"state":"queued","filename":file.filename,"bytes":file.size})
            }
            "write" => {
                let p =
                    tools::workspace_path(&self.config.workspace, tools::string(a, "path")?, true)?;
                let text = tools::string(a, "text")?;
                let tmp = p.with_file_name(format!(".pantheon-{}", uuid::Uuid::new_v4()));
                {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&tmp)?;
                    f.write_all(text.as_bytes())?;
                    f.sync_all()?;
                }
                std::fs::rename(&tmp, &p)?;
                std::fs::File::open(p.parent().unwrap())?.sync_all()?;
                json!({"written":text.len()})
            }
            "shell" => {
                return self
                    .shell_tool_tracked(run, tools::string(a, "command")?, tool_id)
                    .await;
            }
            "claude" => {
                ensure!(self.config.claude.enabled, "Claude tool is disabled");
                ensure!(
                    run.child || !self.config.agent.coordinator_root,
                    "strict coordinators must delegate Claude tasks to workers"
                );
                return self.claude_tool_tracked(run, a, tool_id).await;
            }
            "web_fetch" => {
                let max_chars = a
                    .get("max_chars")
                    .map(|_| tools::number(a, "max_chars"))
                    .transpose()?
                    .unwrap_or(20_000) as usize;
                let refresh = a
                    .get("refresh")
                    .map(|v| v.as_bool().context("refresh must be boolean"))
                    .transpose()?
                    .unwrap_or(false);
                tokio::select! {
                    value = self.web.fetch(tools::string(a, "url")?, max_chars, refresh) => value?,
                    _ = run.cancel.cancelled() => bail!("web fetch cancelled"),
                }
            }
            "web_search" => {
                let model = self.web.search_model(&run.settings.0);
                let limit = a
                    .get("max_results")
                    .map(|_| tools::number(a, "max_results"))
                    .transpose()?
                    .unwrap_or(5) as usize;
                let domains = a
                    .get("domains")
                    .map(|v| {
                        v.as_array()
                            .context("domains must be an array")?
                            .iter()
                            .map(|v| {
                                v.as_str()
                                    .map(str::to_owned)
                                    .context("domain must be a string")
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                let provider = self.provider.clone().with_cache_affinity(
                    self.store
                        .cache_affinity(&format!("search:{}", run.owner))?,
                );
                let value = tokio::select! {
                    value = tokio::time::timeout(Duration::from_secs(self.config.web.search_timeout_seconds), provider.search(model, tools::string(a, "query")?, limit, &domains)) => value.context("web search timed out")??,
                    _ = run.cancel.cancelled() => bail!("web search cancelled"),
                };
                value
            }
            "browser" => {
                let parent = format!("channel:{}", run.channel);
                if a["action"] == "claim" {
                    self.browser
                        .claim_owner(tools::string(a, "browser_id")?, &parent, &run.owner)
                        .await?
                } else if run.child && a["action"] == "list" {
                    let mut own = self.browser.execute(&run.owner, a.clone()).await?;
                    let adopted = self.browser.execute(&parent, a.clone()).await?;
                    for browser in adopted["browsers"].as_array().into_iter().flatten() {
                        let mut browser = browser.clone();
                        browser["needs_claim"] = json!(true);
                        own["browsers"].as_array_mut().unwrap().push(browser);
                    }
                    own
                } else {
                    self.browser.execute(&run.owner, a.clone()).await?
                }
            }

            "spawn" => {
                ensure!(!run.child, "subagents cannot spawn");
                let tasks = a["tasks"].as_array().context("tasks must be an array")?;
                ensure!(
                    !tasks.is_empty() && tasks.len() <= self.config.agent.max_subagents,
                    "invalid task count"
                );
                let mut names = HashSet::new();
                let tasks=tasks.iter().enumerate().map(|(index,value)| {
                    let task=value.as_str().or_else(||value["task"].as_str()).context("task must contain text")?.to_owned();
                    ensure!(!task.trim().is_empty() && task.len()<=64_000,"task must contain 1–64000 bytes");
                    let name=value["name"].as_str().map(str::to_owned).unwrap_or_else(||format!("Worker {}",index+1));
                    ensure!(!name.trim().is_empty() && name.encode_utf16().count()<=48 && name.chars().all(|c|c.is_alphanumeric() || matches!(c,' '|'-'|'_')),"agent names must be short words without formatting or control characters");
                    ensure!(names.insert(name.to_lowercase()),"agent names must be unique within a spawn batch");
                    let model=value["model"].as_str().unwrap_or(&run.settings.0).to_owned();
                    let preferred=self.discord.models.reasoning(&model,&run.settings.1);
                    let requested=value["reasoning"].as_str().unwrap_or(&preferred);
                    let reasoning=self.reasoning_for_model(&model,requested)?;
                    self.discord.models.validate_effort(&model,&reasoning)?;
                    model_parts(&model)?; crate::config::validate_reasoning(&reasoning)?;
                    Ok((task,name,(model,reasoning)))
                }).collect::<Result<Vec<_>>>()?;
                let mut permits = vec![];
                for _ in &tasks {
                    permits.push(
                        self.capacity
                            .clone()
                            .try_acquire_owned()
                            .context("subagent capacity full")?,
                    );
                }
                let view = self.settle(&run.memory, &run.cancel).await?;
                let batch = uuid::Uuid::new_v4().to_string();
                let ids: Vec<String> = tasks
                    .iter()
                    .map(|_| uuid::Uuid::new_v4().to_string())
                    .collect();
                let mut agents = vec![];
                for (id, (task, name, settings)) in ids.iter().zip(&tasks) {
                    self.store
                        .add_task(id, &batch, run.channel, run.user, task)?;
                    self.store.register_agent(id, &settings.0, &settings.1)?;
                    self.store
                        .present_agent(id, run.channel, &run.context, name, &settings.0)?;
                    self.store.activity_event(
                        &run.context,
                        run.channel,
                        &format!("spawn:{id}"),
                        &format!(
                            "↗ spawned {name} [{}] · {} · {}",
                            crate::ui::short_id(id),
                            settings.0,
                            settings.1
                        ),
                        "event",
                        Duration::ZERO,
                    )?;
                    agents.push(
                        json!({"id":id,"name":name,"model":settings.0,"reasoning":settings.1}),
                    );
                }
                for ((id, (task, _name, settings)), permit) in ids.iter().zip(tasks).zip(permits) {
                    self.start_child(ChildStart {
                        id: id.clone(),
                        channel: run.channel,
                        user: run.user,
                        task,
                        previous: String::new(),
                        settings,
                        memory: run.memory.clone(),
                        view: Some(view.clone()),
                        cancel: run.cancel.child_token(),
                        permit,
                    })
                    .await?;
                }

                json!({"batch":batch,"ids":ids,"agents":agents,"mode":"background"})
            }
            "list_agents" => {
                self.store
                    .archive_idle_agents(self.config.agent.agent_idle_seconds)?;
                let limit = a
                    .get("limit")
                    .map(|v| v.as_u64().context("limit must be an integer"))
                    .transpose()?
                    .unwrap_or(20);
                ensure!((1..=25).contains(&limit), "limit must be between 1 and 25");
                let before = a
                    .get("before")
                    .map(|v| v.as_i64().context("before must be an integer"))
                    .transpose()?;
                let archived = a
                    .get("include_archived")
                    .map(|v| v.as_bool().context("include_archived must be a boolean"))
                    .transpose()?
                    .unwrap_or(false);
                match a
                    .get("kind")
                    .map(|v| v.as_str().context("kind must be a string"))
                    .transpose()?
                    .unwrap_or("workers")
                {
                    "workers" => {
                        self.store
                            .list_agents(run.channel, archived, limit as usize, before)?
                    }
                    "coordinators" => {
                        ensure!(!run.child, "workers cannot discover coordinators");
                        self.store
                            .list_coordinators(archived, limit as usize, before)?
                    }
                    _ => bail!("unknown agent kind"),
                }
            }
            "tell" => {
                let id = tools::string(a, "id")?;
                let message_id = self.store.send_agent_message(
                    run.channel,
                    run.user,
                    &run.owner,
                    id,
                    tools::string(a, "message")?,
                )?;
                self.store.agent_activity_event(
                    &run.owner,
                    &run.context,
                    run.channel,
                    &format!("tell:{message_id}"),
                    &format!(
                        "↗ message sent to {} [{}]",
                        self.store.agent_label(id)?,
                        crate::ui::short_id(id)
                    ),
                    "event",
                    Duration::ZERO,
                )?;
                if let Some(channel) = id.strip_prefix("channel:") {
                    self.channel(channel.parse()?).await?.incoming.notify_one();
                } else {
                    if !self.children.lock().await.contains_key(id) {
                        let memory = self.channel(run.channel).await?;
                        let view = self.settle(&memory, &run.cancel).await?;
                        self.ensure_agent_with_view(id, Some(view)).await?;
                    }
                }
                json!({"delivered":id,"name":self.store.agent_label(id)?})
            }
            "wakeup" | "monitor" => {
                self.manage_jobs(run.channel, run.user, &run.owner, &call.name, a)?
            }
            _ => bail!("unknown tool {}", call.name),
        };
        Ok(value.to_string())
    }

    async fn start_child(self: &Arc<Self>, start: ChildStart) -> Result<()> {
        let mut children = self.children.lock().await;
        if children.contains_key(&start.id) {
            return Ok(());
        }
        let delivery_id = format!("agent:{}:{}", start.id, uuid::Uuid::new_v4());
        self.store.agent_run_start(&delivery_id, &start.id)?;
        children.insert(
            start.id.clone(),
            Child {
                channel: start.channel,
                cancel: start.cancel.clone(),
            },
        );
        drop(children);
        let h = self.clone();
        tokio::spawn(async move {
            let ChildStart {
                id,
                channel,
                user,
                task,
                previous: _previous,
                settings,
                memory,
                view,
                cancel,
                permit,
            } = start;
            let _permit = permit;
            let context = match h.store.reply_context(&id) {
                Ok(context) => context,
                Err(error) => {
                    tracing::error!(%error,"restore worker presentation failed");
                    h.shutdown.cancel();
                    return;
                }
            };
            let _ = h.store.agent_phase(&id, true, "Preparing context");
            let outcome = async {
                let mut trace = Memory::open(
                    h.config.state_dir.join("subagents").join(&id),
                    h.config.agent.view_bytes,
                )?;
                if trace.stats()["messages"] == 0 {
                    trace.append(Kind::Note, "subagent")?;
                    trace.append(Kind::User, &task)?;
                }
                let view = if let Some(view) = view {
                    h.store.set_worker_view(&id, &view)?;
                    h.known_workers.lock().await.insert(id.clone());
                    Some(view)
                } else if h.known_workers.lock().await.contains(&id) {
                    h.store.worker_view(&id)?
                } else {
                    // Tools.agent restores the original log with no captured
                    // root view after a cold start; zoom still reads live memory.
                    None
                };
                for input in h.store.agent_events(&id)? {
                    trace.append_with_id(Kind::User, &input.text, &input.id)?;
                    h.store.agent_event_done(&input.id)?;
                }
                let prompt = trace.original_log()?;
                let (vendor, _) = model_parts(&settings.0)?;
                let catalogue = h.skills.cache_catalogue(
                    &channel.to_string(),
                    &h.skills.catalogue(h.config.curator.description_chars)?,
                    false,
                )?;
                let worker_system = system_prompt_with_claude(
                    true,
                    false,
                    h.config.claude.enabled,
                    &format!("{}\n{catalogue}", crate::skill_library::INDEX),
                    &h.curator_instructions,
                );
                let run = Run {
                    channel,
                    user,
                    owner: id.clone(),
                    child: true,
                    memory: memory.clone(),
                    cancel,
                    settings: settings.clone(),
                    history: Provider::start_worker(vendor, view.as_deref(), &prompt),
                    inputs: vec![],
                    trace: Some(trace),
                    steering: vec![],
                    media: vec![],
                    context: context.clone(),
                    skills: h.skills.snapshot()?,
                    task: task.chars().take(8000).collect(),
                    system: Some(worker_system),
                    steps: 0,
                    invocation_scope: uuid::Uuid::new_v4().to_string(),
                    counted_skills: HashSet::new(),
                };
                h.clone().run_agent(run).await
            }
            .await;
            let successful = outcome.is_ok();
            let mut report = match outcome {
                Ok(text) => text,
                Err(error) => format!("Task stopped: {error}"),
            };
            if let Err(error) = h
                .browser
                .transfer_owner(&id, &format!("channel:{channel}"))
                .await
            {
                report.push_str(&format!("\nBrowser ownership transfer failed: {error}"));
                h.shutdown.cancel();
            }
            if let Err(error) = h
                .store
                .finish_agent_turn(&id, &report, &delivery_id, successful)
            {
                tracing::error!(error=%error,"persist agent report failed");
                h.shutdown.cancel();
            }
            h.children.lock().await.remove(&id);
            memory.incoming.notify_one();
            // Events arriving at the final boundary remain durable. The job pump
            // resumes this identity after releasing its concurrency permit.
        });
        Ok(())
    }
    async fn ensure_agent(self: &Arc<Self>, id: &str) -> Result<()> {
        self.ensure_agent_with_view(id, None).await
    }
    async fn ensure_agent_with_view(
        self: &Arc<Self>,
        id: &str,
        view: Option<String>,
    ) -> Result<()> {
        if self.shutdown.is_cancelled() || self.children.lock().await.contains_key(id) {
            return Ok(());
        }
        if self.store.agent_events(id)?.is_empty() {
            return Ok(());
        }
        let Some(crate::store::AgentRecord {
            channel,
            user,
            task,
            report: previous,
            model,
            reasoning,
        }) = self.store.agent(id)?
        else {
            bail!("notification targets unknown agent {id}");
        };
        let Ok(permit) = self.capacity.clone().try_acquire_owned() else {
            return Ok(());
        };
        let memory = self.channel(channel).await?;
        self.start_child(ChildStart {
            id: id.to_owned(),
            channel,
            user,
            task,
            previous,
            settings: (model, reasoning),
            memory,
            view,
            cancel: self.shutdown.child_token(),
            permit,
        })
        .await?;
        Ok(())
    }
    async fn claude_tool_tracked(
        self: &Arc<Self>,
        run: &Run,
        args: &Value,
        tool_id: Option<&str>,
    ) -> Result<String> {
        let task = tools::string(args, "task")?;
        ensure!(!task.trim().is_empty(), "claude task must not be empty");
        ensure!(
            task.chars().count() <= self.config.claude.max_task_chars,
            "claude task exceeds configured max_task_chars"
        );
        let name = args["name"].as_str().unwrap_or("task");
        ensure!(
            name.chars().count() <= 80 && !name.trim().is_empty(),
            "claude name must be 1–80 characters"
        );
        let cwd = match args["cwd"].as_str() {
            Some(path) => tools::workspace_path(&self.config.workspace, path, false)?,
            None => self
                .config
                .workspace
                .canonicalize()
                .context("canonicalize Claude workspace")?,
        };
        ensure!(cwd.is_dir(), "claude cwd must be a workspace directory");
        let model = args["model"].as_str().unwrap_or(&self.config.claude.model);
        let effort = args["effort"]
            .as_str()
            .unwrap_or(&self.config.claude.effort);
        crate::config::validate_claude_effort(effort)?;
        ensure!(
            model.chars().count() <= 100 && !model.trim().is_empty(),
            "invalid Claude model"
        );
        let permit = self
            .claude_capacity
            .clone()
            .try_acquire_owned()
            .context("Claude background capacity full")?;
        let id = uuid::Uuid::new_v4().to_string();
        // Persist detached admission and reply/UI context atomically before
        // starting a paid or possibly side-effecting process.
        self.store.claude_start(
            &id,
            &run.owner,
            (run.channel, run.user),
            // Do not duplicate task text from durable conversation history.
            &format!(
                "{name}: [Claude task redacted; {} chars]",
                task.chars().count()
            ),
            &run.context,
            tool_id,
        )?;
        let cancel = run.cancel.child_token();
        self.shell_jobs
            .lock()
            .await
            .insert(id.clone(), (run.channel, cancel.clone()));
        let h = self.clone();
        let job_id = id.clone();
        let task = task.to_owned();
        let model = model.to_owned();
        let effort = effort.to_owned();
        let channel = run.channel;
        tokio::spawn(async move {
            let _permit = permit;
            let result =
                crate::claude::run(&h.config.claude, &cwd, &task, &model, &effort, &cancel).await;
            let output = match &result {
                Ok(report) => format!("Claude work report (external/untrusted):\n{report}"),
                Err(error) => format!("Error: {error}"),
            };
            if let Err(error) = h
                .store
                .shell_finish(&job_id, &output, cancel.is_cancelled())
            {
                tracing::error!(error=%error,"persist Claude completion failed");
                h.shutdown.cancel();
            }
            h.shell_jobs.lock().await.remove(&job_id);
            if let Ok(memory) = h.channel(channel).await {
                memory.incoming.notify_one();
            }
        });
        // Claude tasks are always detached. A foreground wait would keep the
        // requesting turn open and invite duplicate work after provider errors.
        run.memory.incoming.notify_one();
        Ok(json!({"job_id":id,"name":name,"state":"background","delivery":"completion will arrive in your inbox as untrusted Claude work; do not poll or wait"}).to_string())
    }

    #[cfg(test)]
    async fn shell_tool(self: &Arc<Self>, run: &Run, command: &str) -> Result<String> {
        self.shell_tool_tracked(run, command, None).await
    }
    async fn shell_tool_tracked(
        self: &Arc<Self>,
        run: &Run,
        command: &str,
        tool_id: Option<&str>,
    ) -> Result<String> {
        ensure!(!command.is_empty(), "empty command");
        let permit = self
            .shell_capacity
            .clone()
            .try_acquire_owned()
            .context("background shell capacity full")?;
        let id = uuid::Uuid::new_v4().to_string();
        self.store.bind_context(&id, &run.context)?;
        self.store
            .shell_start(&id, &run.owner, run.channel, run.user, command)?;
        if let Some(event) = tool_id {
            self.store.bind_shell_activity(&id, event)?;
        }
        let cancel = run.cancel.child_token();
        self.shell_jobs
            .lock()
            .await
            .insert(id.clone(), (run.channel, cancel.clone()));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let h = self.clone();
        let job_id = id.clone();
        let command = command.to_owned();
        let channel = run.channel;
        let event = tool_id.map(str::to_owned);
        tokio::spawn(async move {
            let _permit = permit;
            let progress = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let execution = tools::shell_with_progress(
                &h.config.workspace,
                &command,
                h.config.agent.shell_timeout_seconds,
                &cancel,
                Some(progress.clone()),
            );
            tokio::pin!(execution);
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(1),
                Duration::from_secs(1),
            );
            let result = loop {
                tokio::select! {
                    result=&mut execution=>break result,
                    _=tick.tick()=>if let Some(id)=&event
                        && progress.load(std::sync::atomic::Ordering::Relaxed)>0
                        && let Err(error)=h.store.tool_output_lines(id,progress.load(std::sync::atomic::Ordering::Relaxed)) {
                            h.shutdown.cancel();cancel.cancel();break Err(error.context("persist shell progress"));
                        },
                }
            };
            let output = match &result {
                Ok(output) => output.clone(),
                Err(error) => format!("Error: {error}"),
            };
            if let Err(error) = h
                .store
                .shell_finish(&job_id, &output, cancel.is_cancelled())
            {
                tracing::error!(error=%error,"persist shell completion failed");
                h.shutdown.cancel();
            }
            let _ = sender.send(result);
            h.shell_jobs.lock().await.remove(&job_id);
            // Wake root inboxes immediately; child inboxes are also checked by
            // the durable job pump, including completions after a child ended.
            if let Ok(memory) = h.channel(channel).await {
                memory.incoming.notify_one();
            }
        });
        match tokio::time::timeout(
            Duration::from_secs(self.config.agent.shell_background_after_seconds),
            receiver,
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => bail!("shell worker stopped; inspect saved effects"),
            Err(_) => {
                self.store.shell_detach(&id)?;
                run.memory.incoming.notify_one();
                Ok(json!({"job_id":id,"state":"background","delivery":"completion will arrive in your inbox; do not poll or wait"}).to_string())
            }
        }
    }

    fn manage_jobs(
        &self,
        channel: u64,
        user: u64,
        owner: &str,
        kind: &str,
        args: &Value,
    ) -> Result<Value> {
        let administrative = owner == "*";
        let owner = if administrative {
            format!("channel:{channel}")
        } else {
            owner.to_owned()
        };
        let owned = |job: &Job| {
            job.kind == kind
                && (administrative
                    || job.payload["_owner"]
                        .as_str()
                        .unwrap_or(&format!("channel:{channel}"))
                        == owner)
        };
        match tools::string(args,"action")? {
            "list"=>Ok(json!(self.store.jobs(Some(channel),false)?.into_iter().filter(owned).map(|job|json!({"id":job.id,"owner":job.payload["_owner"].as_str().unwrap_or(&format!("channel:{channel}")),"due":job.due,"interval_seconds":job.interval,"payload":job.payload})).collect::<Vec<_>>())),
            "cancel"=>{
                let id=tools::string(args,"id")?;
                let authorized=self.store.jobs(Some(channel),false)?.iter().any(|job|job.id==id && owned(job));
                Ok(json!({"cancelled":authorized && self.store.cancel_job(channel,id)?}))
            }
            "add"=>{
                let (due,interval,mut payload)=if kind=="wakeup" {
                    let (due,interval)=tools::schedule(tools::string(args,"schedule")?)?;
                    (due,interval,json!({"prompt":args["prompt"].as_str().unwrap_or("Scheduled wakeup")}))
                } else {
                    let seconds=tools::number(args,"interval_seconds")?;
                    ensure!((5..=31_536_000).contains(&seconds),"interval must be 5 seconds to 1 year");
                    (crate::store::now()+seconds as i64,Some(seconds as i64),json!({"command":tools::string(args,"command")?,"prompt":args["prompt"].as_str().unwrap_or("Monitor output changed")}))
                };
                payload["_owner"]=json!(owner);
                let id=uuid::Uuid::new_v4().to_string();
                let context=if administrative {crate::ui::ReplyContext{reply_to:None,activity:format!("schedule:{id}")}}else{self.store.reply_context(&owner)?};
                self.store.bind_context(&id,&context)?;
                self.store.add_job(&Job{id:id.clone(),channel,user,kind:kind.into(),payload,due,interval})?;
                self.store.agent_activity_event(&owner,&context,channel,&format!("job:{id}:queued"),&format!("◷ {kind} saved [{}]",crate::ui::short_id(&id)),"event",Duration::ZERO)?;
                Ok(json!({"id":id,"due":due,"interval_seconds":interval}))
            }
            _=>bail!("unknown job action"),
        }
    }
    async fn command(
        self: &Arc<Self>,
        channel: u64,
        user: u64,
        name: &str,
        options: &Value,
    ) -> Result<Value> {
        let mut args = serde_json::Map::new();
        for option in options.as_array().into_iter().flatten() {
            if let Some(n) = option["name"].as_str() {
                args.insert(n.into(), option["value"].clone());
            }
        }
        if let Some(v) = args.remove("job_id") {
            args.insert("id".into(), v);
        }
        let args = Value::Object(args);
        let c = self.channel(channel).await?;
        let _settings_guard = if matches!(name, "model" | "reasoning") {
            Some(if args["kind"] == "compact" {
                self.compactor_settings.lock().await
            } else {
                c.settings_lock.lock().await
            })
        } else {
            None
        };
        let (mut model, mut reasoning) = self.store.settings(
            channel,
            &self.config.agent.model,
            &self.config.agent.reasoning,
        )?;
        let text = match name {
            "curator" => {
                match args["action"].as_str().unwrap_or("status") {
                    "run" => {
                        ensure!(self.config.curator.enabled, "Curator is disabled");
                        ensure!(
                            self.skills.request_channel_fork(&channel.to_string())?,
                            "No eligible queued work: curation requires a settled task with enough model iterations"
                        );
                        self.curator_changed.notify_one();
                    }
                    "cancel" => self.cancel_curator(channel).await?,
                    "status" => {}
                    _ => bail!("Unknown curator action"),
                }
                let settings = self.curator_settings(channel).await?;
                let mut status = self.skills.channel_status(&channel.to_string())?;
                status["enabled"] = json!(self.config.curator.enabled);
                status["requested"] = json!(self.skills.fork_requested(&channel.to_string())?);
                return Ok(crate::skill_dashboard::curator_status(
                    &status,
                    &settings.0,
                    &settings.1,
                ));
            }
            "skills" => {
                let action = args["action"].as_str().unwrap_or("list");
                match action {
                    "curator" => {
                        let settings = self.curator_settings(channel).await?;
                        let mut status = self.skills.channel_status(&channel.to_string())?;
                        status["enabled"] = json!(self.config.curator.enabled);
                        status["requested"] =
                            json!(self.skills.fork_requested(&channel.to_string())?);
                        return Ok(crate::skill_dashboard::curator_status(
                            &status,
                            &settings.0,
                            &settings.1,
                        ));
                    }
                    "curate" => {
                        ensure!(self.config.curator.enabled, "Curator is disabled");
                        ensure!(
                            self.skills.request_channel_fork(&channel.to_string())?,
                            "No eligible queued work"
                        );
                        self.curator_changed.notify_one();
                        return Ok(crate::ui::card(
                            "Curation requested",
                            "This channel's eligible work will run once memory settles; the idle debounce is bypassed.",
                            vec![],
                            false,
                        ));
                    }
                    "proposals" => {
                        let proposals = self.skills.proposals(&channel.to_string())?;
                        return Ok(crate::ui::card(
                            "Skill proposals",
                            "Recent proposals from this channel. Use action:proposal with an attempt ID to inspect one.",
                            proposals
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|p| {
                                    (
                                        "Proposal",
                                        format!(
                                            "`{}` · {}\n{}\n{}",
                                            p["id"].as_str().unwrap(),
                                            p["status"].as_str().unwrap(),
                                            p["task_family"].as_str().unwrap(),
                                            p["reason"].as_str().unwrap()
                                        ),
                                        false,
                                    )
                                })
                                .collect(),
                            false,
                        ));
                    }
                    "proposal" => {
                        let id = tools::string(&args, "id")?;
                        let p = self.skills.proposal(&channel.to_string(), id)?;
                        let changes = p
                            .changes
                            .iter()
                            .map(|c| {
                                format!(
                                    "{} · {} from revision {}",
                                    c.id,
                                    if c.retire { "retire" } else { "create/revise" },
                                    c.expected_revision
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        return Ok(crate::ui::card(
                            "Skill proposal",
                            &p.reason,
                            vec![
                                ("Changes", changes, false),
                                ("Task family", p.task_family, false),
                                ("Triggers", p.triggers, false),
                                ("Procedure", p.procedure, false),
                                ("Variable inputs", p.variables, false),
                                ("Verification", p.verification, false),
                                ("Limits", p.limits, false),
                            ],
                            false,
                        ));
                    }
                    "history" => {
                        let id = tools::string(&args, "id")?;
                        let history = self.skills.history(id, 0)?;
                        return Ok(crate::ui::card(
                            "Skill revisions",
                            &format!("History for `{id}`"),
                            history["revisions"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|r| {
                                    (
                                        "Revision",
                                        format!(
                                            "{}{} — {}",
                                            r["revision"],
                                            if r["retired"] == true {
                                                " (retired)"
                                            } else {
                                                ""
                                            },
                                            r["reason"].as_str().unwrap()
                                        ),
                                        false,
                                    )
                                })
                                .collect(),
                            false,
                        ));
                    }
                    "rollback" => {
                        self.cancel_curator(channel).await?;
                        let id = tools::string(&args, "id")?;
                        self.skills.rollback(
                            id,
                            i64::try_from(tools::number(&args, "revision")?)
                                .context("revision too large")?,
                        )?;
                        return Ok(crate::ui::card(
                            "Skill restored",
                            &format!(
                                "Restored `{id}` as a new revision. Active turns keep their existing snapshot."
                            ),
                            vec![],
                            false,
                        ));
                    }
                    "list" => {}
                    _ => bail!("Unknown skills action"),
                }
                return match args["id"].as_str() {
                    Some(id) => crate::skill_dashboard::open_skill(&self.skills, channel, user, id),
                    None => crate::skill_dashboard::home(&self.skills, channel, user),
                };
            }
            "mcp" => {
                let servers = self.mcp.servers(channel, false);
                let mut fields = servers["servers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| {
                        (
                            s["id"].as_str().unwrap(),
                            format!(
                                "{}\n{}",
                                s["transport"].as_str().unwrap(),
                                s["description"].as_str().unwrap()
                            ),
                            false,
                        )
                    })
                    .collect::<Vec<_>>();
                if fields.is_empty() {
                    fields.push((
                        "Available servers",
                        "No MCP servers configured. Add servers to the service configuration."
                            .into(),
                        false,
                    ));
                }
                return Ok(crate::ui::card(
                    "Integrations",
                    "Configured Model Context Protocol servers. Discovery connects on demand.",
                    fields,
                    false,
                ));
            }
            "model" => {
                let kind = args["kind"].as_str().unwrap_or("chat");
                ensure!(
                    ["chat", "compact", "curator"].contains(&kind),
                    "Model kind must be chat, compact or curator"
                );
                let provider = args["provider"].as_str();
                let selected = args["model"].as_str().or_else(|| args["id"].as_str());
                if kind == "curator" {
                    if let Some(id) = selected {
                        let target = match id {
                            "none" => Some("none".to_owned()),
                            "default" => None,
                            _ => Some(self.discord.models.resolve(provider, id)?),
                        };
                        let effective = crate::curator::effective_model(
                            &model,
                            &self.config.agent.model,
                            self.config.curator.model.as_deref(),
                            target.as_deref(),
                        )?;
                        let preferred = self.discord.models.reasoning(&effective, &reasoning);
                        self.reasoning_for_model(&effective, &preferred)?;
                        self.store.set_curator_model(channel, target.as_deref())?;
                        self.discord.models.set_kind_model(
                            channel,
                            "curator",
                            target.as_deref(),
                        )?;
                    } else {
                        ensure!(provider.is_none(), "Select a model to change providers");
                    }
                    let settings = self.curator_settings(channel).await?;
                    return Ok(crate::ui::card(
                        "Curator model",
                        "Applies to newly started jobs and all their reviewers. model:none inherits the main model; default restores configuration.",
                        vec![
                            ("Model", settings.0, false),
                            ("Reasoning", settings.1, true),
                        ],
                        false,
                    ));
                }
                if let Some(id) = selected {
                    let target = if id == "default" {
                        if kind == "chat" {
                            self.config.agent.model.clone()
                        } else {
                            self.config.agent.compactor_model.clone()
                        }
                    } else {
                        self.discord.models.resolve(provider, id)?
                    };
                    let effort = self
                        .discord
                        .models
                        .reasoning(&target, if kind == "chat" { &reasoning } else { "medium" });
                    let effort = self.reasoning_for_model(&target, &effort)?;
                    if kind == "compact" {
                        self.store.set_compactor_model(
                            GLOBAL_COMPACTOR,
                            if id == "default" { None } else { Some(&target) },
                        )?;
                        self.discord.models.set_kind_model(
                            GLOBAL_COMPACTOR,
                            "compact",
                            if id == "default" { None } else { Some(&target) },
                        )?;
                        c.changed.notify_one();
                    } else {
                        model = target;
                        reasoning = effort;
                        self.store.set_settings(channel, &model, &reasoning)?;
                        self.discord.models.set_chat_model(channel, &model);
                    }
                } else {
                    ensure!(provider.is_none(), "Select a model to change providers");
                }
                let compact = self
                    .store
                    .compactor_model(GLOBAL_COMPACTOR, &self.config.agent.compactor_model)?;
                return Ok(crate::ui::card(
                    "Model",
                    "Chat overrides apply only to this channel; compaction overrides apply instance-wide. Both survive restart. Changes affect newly started turns/jobs. Use model:default to return to config defaults.",
                    vec![
                        ("Selected kind", format!("`{kind}`"), true),
                        ("Chat model", format!("`{model}`"), false),
                        ("Reasoning", format!("`{reasoning}`"), true),
                        ("Compaction model", format!("`{compact}`"), false),
                    ],
                    false,
                ));
            }
            "reasoning" => {
                let kind = args["kind"].as_str().unwrap_or("chat");
                ensure!(
                    ["chat", "compact", "curator"].contains(&kind),
                    "Reasoning kind must be chat, compact or curator"
                );
                let scope = if kind == "compact" {
                    GLOBAL_COMPACTOR
                } else {
                    channel
                };
                let target = match kind {
                    "compact" => self
                        .store
                        .compactor_model(GLOBAL_COMPACTOR, &self.config.agent.compactor_model)?,
                    "curator" => crate::curator::effective_model(
                        &model,
                        &self.config.agent.model,
                        self.config.curator.model.as_deref(),
                        self.store.curator_model(channel)?.as_deref(),
                    )?,
                    _ => model.clone(),
                };
                if let Some(level) = args["level"].as_str() {
                    if level == "inherit" {
                        ensure!(kind == "curator", "inherit is available for the curator");
                        self.store
                            .set_reasoning_override(scope, kind, Some("inherit"))?;
                    } else if level == "default" {
                        if kind == "chat" {
                            reasoning = self
                                .discord
                                .models
                                .reasoning(&model, &self.config.agent.reasoning);
                            self.store.set_settings(channel, &model, &reasoning)?;
                        } else {
                            self.store.set_reasoning_override(scope, kind, None)?;
                        }
                    } else {
                        self.discord.models.validate_effort(&target, level)?;
                        let level = self.reasoning_for_model(&target, level)?;
                        if kind == "chat" {
                            reasoning = level;
                            self.store.set_settings(channel, &model, &reasoning)?;
                        } else {
                            self.store
                                .set_reasoning_override(scope, kind, Some(&level))?;
                        }
                    }
                }
                let current = match kind {
                    "curator" => self.curator_settings(channel).await?.1,
                    "compact" => self
                        .store
                        .reasoning_override(GLOBAL_COMPACTOR, "compact")?
                        .unwrap_or_else(|| self.discord.models.reasoning(&target, "medium")),
                    _ => reasoning,
                };
                return Ok(crate::ui::card(
                    "Reasoning effort",
                    "Compaction effort is instance-global; chat/curator effort is channel-owned. Changes apply to new turns/jobs; running jobs keep their settings.",
                    vec![
                        ("Kind", kind.into(), true),
                        ("Model", target, false),
                        ("Effort", current, true),
                    ],
                    false,
                ));
            }
            "cache" => return self.store.cache_card(channel),
            "context" => {
                let mut stats = c.memory.lock().await.stats();
                stats["scope"] = json!("instance-global");
                let operational = self.store.stats(channel)?;
                return Ok(crate::ui::context_card(
                    &self.config,
                    &model,
                    &reasoning,
                    &stats,
                    &operational["last_request_usage"],
                ));
            }
            "status" => {
                let stats = c.memory.lock().await.stats();
                let operational = self.store.stats(channel)?;
                let agents = operational["active_agents"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let state = if !agents.is_empty() {
                    "Work in progress"
                } else if operational["running_shells"].as_u64().unwrap_or(0) > 0 {
                    "Background shell work in progress"
                } else if operational["queued_worker_messages"].as_u64().unwrap_or(0) > 0
                    || operational["queued_prompts"].as_u64().unwrap_or(0) > 0
                {
                    "Processing queued messages"
                } else if operational["pending_delivery"].as_u64().unwrap_or(0) > 0 {
                    "Delivering responses"
                } else if stats["settled"] != true {
                    "Updating memory"
                } else {
                    "Ready for your next message"
                };
                let phases = if agents.is_empty() {
                    "No agents currently executing".into()
                } else {
                    agents
                        .iter()
                        .take(8)
                        .map(|a| {
                            format!(
                                "**{}** · {}",
                                crate::ui::clean(a["name"].as_str().unwrap_or("Agent"), 32),
                                crate::ui::clean(a["phase"].as_str().unwrap_or("Working"), 60)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let jobs = self.store.jobs(Some(channel), false)?;
                let mut card = crate::ui::card(
                    "Work status",
                    state,
                    vec![
                        ("Version", format!("`{}`", env!("CARGO_PKG_VERSION")), true),
                        ("Active work", phases, false),
                        (
                            "Shell jobs",
                            operational["running_shells"].to_string(),
                            true,
                        ),
                        (
                            "Queued messages",
                            format!(
                                "{} prompts · {} worker messages",
                                operational["queued_prompts"],
                                operational["queued_worker_messages"]
                            ),
                            true,
                        ),
                        (
                            "Delivery",
                            format!("{} messages pending", operational["pending_delivery"]),
                            true,
                        ),
                        (
                            "Schedules",
                            format!(
                                "{} wakeups · {} monitors",
                                jobs.iter().filter(|j| j.kind == "wakeup").count(),
                                jobs.iter().filter(|j| j.kind == "monitor").count()
                            ),
                            true,
                        ),
                        (
                            "Memory",
                            if stats["settled"] == true {
                                "Settled".into()
                            } else {
                                format!("Compacting · {} jobs ready", stats["ready_jobs"])
                            },
                            true,
                        ),
                        (
                            "Channel settings",
                            format!("`{model}` · {reasoning}"),
                            false,
                        ),
                    ],
                    false,
                );
                card["embeds"][0]["color"] = json!(if state == "Ready for your next message" {
                    0x57F287
                } else {
                    0xFEE75C
                });
                return Ok(card);
            }

            "stop" => {
                self.attachments.cancel_channel(channel)?;
                if let Some(cancel) = self.attachment_jobs.lock().await.get(&channel) {
                    cancel.cancel();
                }
                for (job_channel, cancel) in self.shell_jobs.lock().await.values() {
                    if *job_channel == channel {
                        cancel.cancel();
                    }
                }
                if let Some(cancel) = c.cancel.lock().await.as_ref() {
                    cancel.cancel();
                }
                for child in self.children.lock().await.values() {
                    if child.channel == channel {
                        child.cancel.cancel();
                    }
                }
                "Cancellation requested for this channel and its background subagents.".into()
            }
            "subagents" => {
                self.store
                    .archive_idle_agents(self.config.agent.agent_idle_seconds)?;
                let tasks = self.store.tasks(channel)?;
                let entries = tasks
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(12)
                    .map(|task| {
                        let name = task["name"].as_str().unwrap_or("Worker");
                        let value = format!(
                            "**{}** · `{}` · {}\n{}\nID: `{}`",
                            task["state"].as_str().unwrap_or(""),
                            task["model"].as_str().unwrap_or(""),
                            task["reasoning"].as_str().unwrap_or(""),
                            crate::ui::clean(task["task"].as_str().unwrap_or(""), 180),
                            task["id"].as_str().unwrap_or("")
                        );
                        (name, value, false)
                    })
                    .collect();
                return Ok(crate::ui::card(
                    "Background agents",
                    if tasks.as_array().is_none_or(Vec::is_empty) {
                        "No visible workers. Idle workers are archived after an hour by default; their identities and history remain available."
                    } else {
                        "Idle workers leave this list after an hour by default. Messages, wakeups and monitors revive the same identity."
                    },
                    entries,
                    false,
                ));
            }

            "wakeup" | "monitor" => {
                let result = self.manage_jobs(channel, user, "*", name, &args)?;
                let mut records = vec![];
                if let Some(rows) = result.as_array() {
                    for job in rows.iter().take(12) {
                        let owner = self
                            .store
                            .agent_label(job["owner"].as_str().unwrap_or("Coordinator"))?;
                        let id = job["id"].as_str().unwrap_or("");
                        let repeat = job["interval_seconds"]
                            .as_i64()
                            .map(|seconds| format!("\nRepeats every **{seconds}s**"))
                            .unwrap_or_default();
                        records.push((
                            format!("{owner} · {}", crate::ui::short_id(id)),
                            format!("Next: <t:{}:R>{repeat}\nID: `{id}`", job["due"]),
                        ));
                    }
                } else if let Some(id) = result["id"].as_str() {
                    records.push((
                        "Schedule saved".into(),
                        format!("Next: <t:{}:R>\nID: `{id}`", result["due"]),
                    ));
                } else {
                    records.push((
                        "Cancellation".into(),
                        if result["cancelled"] == true {
                            "Schedule cancelled".into()
                        } else {
                            "No matching active schedule".into()
                        },
                    ));
                }
                let fields = records
                    .iter()
                    .map(|(label, value)| (label.as_str(), value.clone(), false))
                    .collect();
                return Ok(crate::ui::card(
                    if name == "wakeup" {
                        "Wakeups"
                    } else {
                        "Monitors"
                    },
                    if records.is_empty() {
                        "No active schedules in this channel."
                    } else {
                        "Notifications return to the agent that created each schedule."
                    },
                    fields,
                    false,
                ));
            }
            "browser" => {
                let mut args = args;
                if args["action"].is_null() {
                    args["action"] = json!("list");
                }
                let result = self
                    .browser
                    .execute(&format!("channel:{channel}"), args)
                    .await?;
                let browsers: Vec<&Value> = if let Some(rows) = result["browsers"].as_array() {
                    rows.iter().take(4).collect()
                } else {
                    vec![&result]
                };
                if browsers.is_empty() {
                    "No live browsers. Use `/browser action:open` to create one.".into()
                } else {
                    browsers
                        .into_iter()
                        .map(crate::browser::describe_browser)
                        .collect::<Vec<_>>()
                        .join("\n\n")
                }
            }
            _ => bail!("unknown command"),
        };
        Ok(crate::ui::card(
            match name {
                "stop" => "Work stopped",
                "browser" => "Browser windows",
                "wakeup" => "Wakeups",
                "monitor" => "Monitors",
                _ => "Pantheon",
            },
            &text,
            vec![],
            false,
        ))
    }
    fn notice(&self, id: &str, channel: u64, mention: Option<u64>, text: &str) -> Result<()> {
        let context = self.store.reply_context(&format!("channel:{channel}"))?;
        self.store
            .enqueue_notice(&context, id, channel, mention, text)
    }
    async fn progress_worker(self: Arc<Self>) -> Result<()> {
        let mut typing = tokio::task::JoinSet::new();
        let mut last_typing = Instant::now() - Duration::from_secs(4);
        loop {
            tokio::select! {_=self.shutdown.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(1))=>{}};
            let send_typing = last_typing.elapsed() >= Duration::from_secs(4);
            if send_typing {
                last_typing = Instant::now();
            }
            while typing.try_join_next().is_some() {}
            let channels = self
                .channels
                .lock()
                .await
                .iter()
                .map(|(id, c)| (*id, c.clone()))
                .collect::<Vec<_>>();
            for (id, c) in channels {
                let settled = c.memory.lock().await.is_settled();
                if self.store.refresh_activities(id, settled)? && send_typing && typing.len() < 16 {
                    let discord = self.discord.clone();
                    let stop = self.shutdown.clone();
                    typing.spawn(async move {
                        tokio::select! {_=stop.cancelled()=>{},_=discord.typing(id)=>{}};
                    });
                }
            }
        }
        typing.abort_all();
        Ok(())
    }
    async fn reaction_worker(&self) -> Result<()> {
        let mut workers = tokio::task::JoinSet::new();
        let mut active = HashSet::new();
        loop {
            if self.shutdown.is_cancelled() {
                break;
            }
            while workers.len() < 8 {
                let Some((message, channel, phase, previous)) = self
                    .store
                    .next_reaction(&active.iter().cloned().collect::<Vec<_>>())?
                else {
                    break;
                };
                active.insert(message.clone());
                let discord = self.discord.clone();
                workers.spawn(async move {
                    let result = match message.parse::<u64>() {
                        Ok(id) => {
                            discord
                                .delivery_reaction(channel, id, phase, previous)
                                .await
                        }
                        Err(_) => Err(anyhow::anyhow!("invalid reaction message")),
                    };
                    (message, phase, result)
                });
            }
            tokio::select! {
                Some(done)=workers.join_next(),if !workers.is_empty()=>{
                    let (message,phase,result)=done.context("reaction worker failed")?;
                    active.remove(&message);
                    match result {Ok(())=>self.store.reaction_delivered(&message,phase)?,Err(_)=>self.store.retry_reaction(&message)?,}
                },
                _=self.shutdown.cancelled()=>break,
                _=tokio::time::sleep(Duration::from_millis(100))=>{},
            }
        }
        workers.abort_all();
        Ok(())
    }
    async fn attachment_worker(self: Arc<Self>) -> Result<()> {
        let mut workers = tokio::task::JoinSet::new();
        let mut active = HashSet::new();
        let mut last_cleanup = Instant::now();
        let mut cleanup = tokio::task::JoinSet::new();
        loop {
            if self.shutdown.is_cancelled() {
                break;
            }
            while workers.len() < 8 {
                let Some(pending) = self.attachments.next(&active)? else {
                    break;
                };
                let channel = pending.input.channel;
                active.insert(channel);
                let cancel = self.shutdown.child_token();
                {
                    let mut jobs = self.attachment_jobs.lock().await;
                    if !self.attachments.pending(&pending.input.id)? {
                        active.remove(&channel);
                        continue;
                    }
                    jobs.insert(channel, cancel.clone());
                }
                let h = self.clone();
                workers.spawn(async move {
                    let result = h.attachments.receive(&pending, &h.discord, &cancel).await;
                    (pending, result, cancel)
                });
            }
            while let Some(done) = cleanup.try_join_next() {
                match done {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!(error=%e,"attachment cleanup failed"),
                    Err(e) => tracing::warn!(error=%e,"attachment cleanup task failed"),
                }
            }
            if cleanup.is_empty()
                && last_cleanup.elapsed()
                    >= Duration::from_secs(self.config.attachments.cleanup_interval_seconds)
            {
                let attachments = self.attachments.clone();
                let protected = active.clone();
                let cancel = self.shutdown.clone();
                cleanup
                    .spawn(async move { attachments.clean_with_cancel(&protected, &cancel).await });
                last_cleanup = Instant::now();
            }
            tokio::select! {
                Some(done)=workers.join_next(),if !workers.is_empty()=>{
                    let (pending,result,cancel)=done.context("attachment task failed")?;
                    let channel=pending.input.channel;
                    active.remove(&channel);
                    self.attachment_jobs.lock().await.remove(&channel);
                    if !cancel.is_cancelled() && self.attachments.pending(&pending.input.id)? {
                        let input=result?;
                        self.admit_prompt(&input)?;
                        self.attachments.finish(&input.id)?;
                        self.channel(channel).await?.incoming.notify_one();
                    }
                },
                _=self.shutdown.cancelled()=>break,
                _=tokio::time::sleep(Duration::from_millis(100))=>{},
            }
        }
        workers.abort_all();
        while workers.join_next().await.is_some() {}
        while cleanup.join_next().await.is_some() {}
        Ok(())
    }

    async fn outbox_worker(self: &Arc<Self>) -> Result<()> {
        let mut workers = tokio::task::JoinSet::new();
        let mut active = HashSet::new();
        let mut ui_sent = HashMap::<u64, Instant>::new();
        loop {
            if self.shutdown.is_cancelled() {
                break;
            }
            while workers.len() < 8 {
                let Some(out) = self.store.next_outbound_with_ui_budget(
                    &active.iter().copied().collect::<Vec<_>>(),
                    &ui_sent
                        .iter()
                        .filter(|(_, at)| at.elapsed() < Duration::from_secs(2))
                        .map(|(id, _)| *id)
                        .collect::<Vec<_>>(),
                )?
                else {
                    break;
                };
                active.insert(out.channel);
                let discord = self.discord.clone();
                let file = out
                    .attachment
                    .as_deref()
                    .map(|id| self.attachments.get(out.channel, id))
                    .transpose()?;
                let path = file.as_ref().map(|f| self.attachments.outgoing_path(f));
                if file.is_some() {
                    self.store.attachment_attempt(&out.id)?;
                }
                workers.spawn(async move {
                    let result = if let Some(file) = file {
                        if path.as_ref().unwrap().is_file() {
                            discord.send_file(&out, &file, path.as_ref().unwrap()).await
                        } else {
                            Err(crate::discord::PermanentDelivery(410).into())
                        }
                    } else if out.id.contains(":activity:") {
                        discord
                            .activity(
                                out.channel,
                                &out.text,
                                &out.nonce,
                                out.reply_to,
                                out.receipt.as_deref(),
                            )
                            .await
                    } else if let Some(receipt) = &out.receipt {
                        discord.edit(out.channel, receipt, &out.text).await
                    } else {
                        discord
                            .send_reply(out.channel, &out.text, out.user, &out.nonce, out.reply_to)
                            .await
                    };
                    (out, result)
                });
            }
            tokio::select! {
                Some(done)=workers.join_next(),if !workers.is_empty()=>{
                    let (out,result)=done.context("delivery worker failed")?;active.remove(&out.channel);
                    if out.id.contains(":activity:"){ui_sent.insert(out.channel,Instant::now());}
                    match result {Ok(receipt)=>self.store.delivered(&out,&receipt)?,Err(e)=>{if let Some(rate)=e.downcast_ref::<crate::discord::FileRateLimit>() {self.store.rate_limited_file(&out.id,rate.0)?;}else if out.attachment.is_some() && let Some(permanent)=e.downcast_ref::<crate::discord::PermanentDelivery>() {let reason=if permanent.0==409 {"Delivery outcome is ambiguous beyond Discord’s nonce window; recent-message reconciliation found no receipt.".into()}else{permanent.to_string()};self.store.fail_file(&out,&reason)?;self.channel(out.channel).await?.incoming.notify_one();self.notice(&format!("file-error:{}",out.id),out.channel,out.user,&reason)?;}else{self.store.retry_outbound(&out.id)?;tracing::warn!(channel=out.channel,"Discord delivery pending retry");}}}
                },
                _=tokio::time::sleep(Duration::from_millis(200))=>{},_=self.shutdown.cancelled()=>break,
            }
        }
        workers.abort_all();
        Ok(())
    }
    async fn job_worker(self: Arc<Self>) -> Result<()> {
        let mut active = HashSet::new();
        let mut workers = tokio::task::JoinSet::new();
        let mut last_archive = Instant::now();
        self.store
            .archive_idle_agents(self.config.agent.agent_idle_seconds)?;
        loop {
            if self.shutdown.is_cancelled() {
                break;
            }
            if last_archive.elapsed() >= Duration::from_secs(60) {
                self.store
                    .archive_idle_agents(self.config.agent.agent_idle_seconds)?;
                last_archive = Instant::now();
            }
            for owner in self.store.pending_agents()? {
                self.ensure_agent(&owner).await?;
            }
            for job in self.store.jobs(None, true)? {
                if workers.len() >= 8 {
                    break;
                }
                if active.contains(&job.id) {
                    continue;
                }
                active.insert(job.id.clone());
                let h = self.clone();
                workers.spawn(async move {
                    let result = async {
                        let notified = if job.kind == "wakeup" {
                            h.store.fire_wakeup(
                                &job,
                                &format!(
                                    "[wakeup {}] {}",
                                    job.id,
                                    job.payload["prompt"].as_str().unwrap_or("Scheduled wakeup")
                                ),
                            )?
                        } else {
                            let result = tools::shell(
                                &h.config.workspace,
                                tools::string(&job.payload, "command")?,
                                h.config.agent.tool_timeout_seconds,
                                &h.shutdown,
                            )
                            .await;
                            let output = match result {
                                Ok(s) => s,
                                Err(e) => format!("Monitor failed: {e}"),
                            };
                            h.store
                                .monitor_result(&job, &crate::memory::cap_tool_result(&output))?
                        };
                        if !notified {
                            return Ok(());
                        }
                        let context = h.store.reply_context(&job.id)?;
                        h.store.agent_activity_event(
                            job.payload["_owner"]
                                .as_str()
                                .unwrap_or(&format!("channel:{}", job.channel)),
                            &context,
                            job.channel,
                            &format!("job:{}:{}:fired", job.id, job.due),
                            &format!("↙ {} notification", job.kind),
                            "event",
                            Duration::ZERO,
                        )?;
                        if let Some(owner) = job.payload["_owner"]
                            .as_str()
                            .filter(|owner| !owner.starts_with("channel:"))
                        {
                            h.ensure_agent(owner).await?;
                        } else {
                            h.channel(job.channel).await?.incoming.notify_one();
                        }
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    (job.id, result)
                });
            }
            tokio::select! {
                Some(done)=workers.join_next(),if !workers.is_empty()=>{match done{Ok((id,result))=>{active.remove(&id);if let Err(e)=result{tracing::error!(error=%e,"scheduled job failed");}},Err(e)=>tracing::error!(error=%e,"job worker panic")}},
                _=tokio::time::sleep(Duration::from_secs(1))=>{},_=self.shutdown.cancelled()=>break,
            }
        }
        workers.abort_all();
        Ok(())
    }
    async fn compactor(self: Arc<Self>, channel: u64, c: Arc<SharedMemory>) -> Result<()> {
        let mut workers = tokio::task::JoinSet::new();
        let mut busy = HashSet::new();
        let mut retry: HashMap<NodeKey, Instant> = HashMap::new();
        let mut reported = HashSet::new();
        loop {
            let notified = c.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.shutdown.is_cancelled() {
                break;
            }
            let candidates = c.memory.lock().await.ready_jobs(usize::MAX);
            for key in candidates {
                if workers.len() >= crate::memory::COMPACTOR_JOBS {
                    break;
                }
                if busy.contains(&key) || retry.get(&key).is_some_and(|t| *t > Instant::now()) {
                    continue;
                }
                let (context, source) = {
                    let memory = c.memory.lock().await;
                    (memory.compactor_context(key)?, memory.source(key)?)
                };
                busy.insert(key);
                let h = self.clone();
                let model = self
                    .store
                    .compactor_model(channel, &self.config.agent.compactor_model)?;
                let effort = self
                    .store
                    .reasoning_override(channel, "compact")?
                    .unwrap_or_else(|| self.discord.models.reasoning(&model, "medium"));
                workers.spawn(async move {
                    (
                        key,
                        h.compress_with_effort(channel, &model, &effort, key, &context, &source)
                            .instrument(tracing::info_span!(
                                "compaction",
                                channel,
                                level = key.level,
                                index = key.index
                            ))
                            .await,
                    )
                });
            }
            tokio::select! {
                Some(done)=workers.join_next(),if !workers.is_empty()=>{
                    match done {
                        Ok((key,Ok(text)))=>{
                            busy.remove(&key);retry.remove(&key);
                            #[cfg(test)]
                            c.compaction_selected.notify_one();
                            let mut memory=c.memory.lock().await;
                            // Publication admission linearizes at this check under
                            // the memory lock, with no await before synchronous finish.
                            // An already-started journal/fsync may finish during cancel.
                            if self.shutdown.is_cancelled() { break; }
                            memory.finish(key,&text)?;
                            drop(memory);
                            c.changed.notify_waiters();
                        }
                        Ok((key,Err(e)))=>{busy.remove(&key);retry.insert(key,Instant::now()+Duration::from_secs(10));if reported.insert(key){tracing::warn!(error=%e,level=key.level,index=key.index,"summary failed; retrying every 10s");}}
                        Err(e)=>bail!("compactor worker panic: {e}"),
                    }
                }
                _=notified=>{},_=tokio::time::sleep(Duration::from_secs(1)),if !retry.is_empty()=>{},_=self.shutdown.cancelled()=>break,
            }
        }
        workers.abort_all();
        Ok(())
    }
    #[cfg(test)]
    async fn compress(
        &self,
        channel: u64,
        model: &str,
        key: NodeKey,
        context: &str,
        source: &str,
    ) -> Result<String> {
        let reasoning = self.discord.models.reasoning(model, "medium");
        self.compress_with_effort(channel, model, &reasoning, key, context, source)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn compress_with_effort(
        &self,
        channel: u64,
        model: &str,
        reasoning: &str,
        key: NodeKey,
        context: &str,
        source: &str,
    ) -> Result<String> {
        let (vendor, _) = model_parts(model)?;
        self.discord.models.validate_effort(model, reasoning)?;
        // Restore the pre-parity compaction-only system prompt. Do not include
        // chat/worker instructions, the tool catalogue or operator task rules.
        // Storage, task format, retries and durable affinity are unchanged.
        let system = COMPACT;
        let step = crate::memory::compaction_task(key, source);
        let original = Provider::start_compaction(vendor, context, &step);
        let mut note: Option<String> = None;
        let mut best: Option<String> = None;
        let provider = self
            .provider
            .clone()
            .with_cache_affinity(self.store.cache_affinity(&format!("compactor:{channel}"))?);
        // A retry is the original context/task + at most one latest cutoff
        // note. Never feed prior answers or reasoning back into compaction.
        for _ in 0..5 {
            let mut history = original.clone();
            if let Some(note) = &note {
                // Unii appends one correction BLOCK to the original User item,
                // not another message/turn. No previous model output is replayed.
                let block = Provider::user(vendor, note)["content"][0].clone();
                history
                    .last_mut()
                    .unwrap()
                    .edit_host(|v| v["content"].as_array_mut().unwrap().push(block));
            }
            // AskAI's patient owner retries setup/down failures on the same
            // frozen correction after 60s. Outages are not answered length tries,
            // never release the native reservation, and never publish best early.
            let response = match crate::provider::patient_compaction(&self.shutdown, || {
                provider.step_compaction(model, reasoning, system, &history)
            })
            .await
            {
                Ok(response) => response,
                Err(error) => return best.ok_or(error),
            };
            let line = match response {
                crate::provider::CompactionReply::Answer(line) => line,
                crate::provider::CompactionReply::ModelFailure => break,
            };
            let line = crate::memory::summary_trim(&line).to_owned();
            if line.is_empty() {
                break;
            }
            if best.as_ref().is_none_or(|s| line.len() < s.len()) {
                best = Some(line.clone());
            }
            if line.len() <= 512 {
                break;
            }
            note = Some(crate::memory::compaction_over(&line));
        }
        // Explicit model failure/refusal/incomplete and completed-empty are
        // semantic ends, never refusal prose as a summary. Keep prior shortest,
        // otherwise upstream flat-source fallback. Generic errors are retried
        // by the patient owner above; cancellation never fabricates a summary.
        Ok(best.unwrap_or_else(|| crate::memory::compaction_fallback(source)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::post};
    use std::collections::VecDeque;
    struct Mock {
        responses: Mutex<VecDeque<Value>>,
        requests: Mutex<Vec<Value>>,
        started: Notify,
        release: Notify,
    }
    async fn respond(
        State(state): State<Arc<Mock>>,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let streaming = body["stream"] == true;
        let anthropic = body.get("messages").is_some() && body.get("input").is_none();
        let zai = body["model"]
            .as_str()
            .is_some_and(|id| id.starts_with("glm-"));
        let first = {
            let mut requests = state.requests.lock().await;
            let first = requests.is_empty();
            requests.push(body);
            first
        };
        if first {
            state.started.notify_one();
            state.release.notified().await;
        }
        let response = state
            .responses
            .lock()
            .await
            .pop_front()
            .expect("unexpected provider request");
        if streaming && zai {
            let choice = &response["choices"][0];
            let mut delta = choice["message"].clone();
            if let Some(calls) = delta["tool_calls"].as_array_mut() {
                for (index, call) in calls.iter_mut().enumerate() {
                    call["index"] = json!(index);
                }
            }
            ([(axum::http::header::CONTENT_TYPE,"text/event-stream")],format!("data: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":delta,"finish_reason":choice["finish_reason"]}],"usage":response["usage"]}))).into_response()
        } else if streaming && anthropic {
            let mut events = vec![
                json!({"type":"message_start","message":{"role":"assistant","content":[],"usage":response["usage"]}}),
            ];
            for (index, block) in response["content"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                events.push(
                    json!({"type":"content_block_start","index":index,"content_block":block}),
                );
                events.push(json!({"type":"content_block_stop","index":index}));
            }
            events.push(
                json!({"type":"message_delta","delta":{"stop_reason":response["stop_reason"]}}),
            );
            events.push(json!({"type":"message_stop"}));
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                events
                    .into_iter()
                    .map(|e| format!("data: {e}\n\n"))
                    .collect::<String>(),
            )
                .into_response()
        } else if streaming {
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                format!(
                    "data: {}\n\n",
                    json!({"type":"response.completed","response":response})
                ),
            )
                .into_response()
        } else {
            Json(response).into_response()
        }
    }
    async fn fixture(
        vendor: &str,
        responses: Vec<Value>,
    ) -> (
        tempfile::TempDir,
        Arc<Harness>,
        Run,
        Arc<Mock>,
        tokio::task::JoinHandle<()>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let mock = Arc::new(Mock {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(vec![]),
            started: Notify::new(),
            release: Notify::new(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = mock.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/", post(respond)).with_state(state),
            )
            .await
            .unwrap();
        });
        let mut config = Config {
            state_dir: directory.path().join("state"),
            workspace: directory.path().into(),
            ..Default::default()
        };
        let model = if vendor == "zai" {
            "zai/glm-5.3".to_owned()
        } else {
            format!("{vendor}/test")
        };
        config.agent.model = model.clone();
        let discord = Arc::new(Discord::new("mock-token".into(), 1, vec![2]).unwrap());
        let mut h = Harness::new(config, discord, CancellationToken::new()).unwrap();
        Arc::get_mut(&mut h).unwrap().provider = Provider::mock(format!("http://{address}/"));
        let c = Arc::new(Channel {
            shared: h.shared.clone(),
            settings_lock: Mutex::new(()),
            incoming: Notify::new(),
            cancel: Mutex::new(None),
        });
        h.channels.lock().await.insert(1, c.clone());
        let view = c.memory.lock().await.render();
        let input = Input {
            id: "first".into(),
            channel: 1,
            user: 2,
            text: "do work".into(),
        };
        h.store.admit(&input).unwrap();
        h.store.input_state(&input.id, "running").unwrap();
        Harness::append_input(&c, &input).await.unwrap();
        h.store.mark_logged(&input.id).unwrap();
        let run = Run {
            channel: 1,
            user: 2,
            owner: "channel:1".into(),
            child: false,
            memory: c,
            cancel: h.shutdown.child_token(),
            settings: (
                model,
                if vendor == "zai" {
                    "low".into()
                } else {
                    "medium".into()
                },
            ),
            history: Provider::start(vendor, &view, &input.text),
            context: crate::ui::ReplyContext::request(&input.id),
            inputs: vec![input.id],
            trace: None,
            steering: vec![],
            media: vec![],
            skills: h.skills.snapshot().unwrap(),
            task: input.text.chars().take(8000).collect(),
            system: None,
            steps: 0,
            invocation_scope: uuid::Uuid::new_v4().to_string(),
            counted_skills: HashSet::new(),
        };
        (directory, h, run, mock, server)
    }
    fn final_response(vendor: &str, text: &str) -> Value {
        if vendor == "zai" {
            return json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":text}}],"usage":{}});
        }
        if vendor == "openai" {
            json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],"usage":{"input_tokens_details":{"cached_tokens":123}}})
        } else {
            json!({"stop_reason":"end_turn","content":[{"type":"text","text":text}],"usage":{"cache_read_input_tokens":123}})
        }
    }
    fn step_response(index: usize) -> Value {
        // A deliberately invalid, side-effect-free tool still produces a real
        // tool result and forces the actual runtime/provider loop to continue.
        json!({"status":"completed","output":[{"type":"function_call","call_id":format!("step-{index}"),"name":"unknown_test_tool","arguments":"{}"}]})
    }

    #[tokio::test]
    async fn unlimited_agent_steps_pass_128_cycles_and_settle_normally() {
        let mut responses: Vec<_> = (0..129).map(step_response).collect();
        responses.push(final_response("openai", "finished after 129 tool batches"));
        let (_dir, h, mut run, mock, server) = fixture("openai", responses).await;
        assert_eq!(h.config.agent.max_steps, 0);
        mock.release.notify_one();
        let result = tokio::time::timeout(
            Duration::from_secs(120),
            h.run_steps(&mut run, "openai", &[], "test", "unlimited"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(result.contains("finished after 129 tool batches"));
        assert_eq!(run.steps, 130);
        assert_eq!(mock.requests.lock().await.len(), 130);
        let messages = drain(&h);
        assert!(
            messages
                .iter()
                .any(|m| m.text.contains("finished after 129 tool batches"))
        );
        assert!(h.store.queued(1).unwrap().is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn explicit_agent_step_cap_stops_at_exact_model_cycle_count() {
        for cap in [1, 128] {
            let responses: Vec<_> = (0..=cap).map(step_response).collect();
            let (_dir, mut h, mut run, mock, server) = fixture("openai", responses).await;
            Arc::get_mut(&mut h).unwrap().config.agent.max_steps = cap;
            mock.release.notify_one();
            let error = tokio::time::timeout(
                Duration::from_secs(120),
                h.run_steps(&mut run, "openai", &[], "test", "capped"),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert_eq!(error.to_string(), "maximum tool steps reached");
            assert_eq!(run.steps, cap);
            assert_eq!(mock.requests.lock().await.len(), cap);
            assert_eq!(mock.responses.lock().await.len(), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn unlimited_agent_steps_cancel_in_flight_on_stop_or_shutdown() {
        for shutdown in [false, true] {
            let (_dir, h, mut run, mock, server) =
                fixture("openai", vec![final_response("openai", "must not finish")]).await;
            let cancel = run.cancel.clone();
            let runner = h.clone();
            let task = tokio::spawn(async move {
                runner
                    .run_steps(&mut run, "openai", &[], "test", "cancel")
                    .await
            });
            mock.started.notified().await;
            if shutdown {
                h.shutdown.cancel();
            } else {
                cancel.cancel();
            }
            let error = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(error.to_string(), "cancelled");
            assert_eq!(mock.requests.lock().await.len(), 1);
            assert_eq!(mock.responses.lock().await.len(), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn codex_runtime_routes_root_worker_and_compactor_to_durable_scopes() {
        let (dir, mut h, mut run, mock, server) =
            fixture("openai", vec![final_response("openai", "done"); 3]).await;
        std::fs::write(
            dir.path().join("auth.json"),
            json!({"tokens":{"access_token":"test-token","account_id":"test-account"}}).to_string(),
        )
        .unwrap();
        let provider = h.provider.clone().with_auth(crate::auth::AuthConfig {
            codex_home: Some(dir.path().into()),
            codex_cli: None,
        });
        Arc::get_mut(&mut h).unwrap().provider = provider;
        run.settings = ("codex/gpt-6.1-sol".into(), "medium".into());
        mock.release.notify_one();
        h.run_steps(
            &mut run,
            "openai",
            &tools::definitions(false, false),
            &h.master_system,
            "root-run",
        )
        .await
        .unwrap();
        run.owner = "worker-id".into();
        run.child = true;
        run.history = Provider::start("openai", "<chat></chat>", "fresh worker task");
        run.trace = Some(Memory::open(dir.path().join("worker-trace"), 128000).unwrap());
        h.run_steps(
            &mut run,
            "openai",
            &tools::definitions(true, false),
            &h.child_system,
            "worker-run",
        )
        .await
        .unwrap();
        h.compress(
            1,
            "codex/gpt-6.1-sol",
            NodeKey { level: 0, index: 0 },
            "<chat></chat>",
            "source",
        )
        .await
        .unwrap();
        let requests = mock.requests.lock().await;
        for (request, scope) in requests
            .iter()
            .zip(["channel:1", "worker-id", "compactor:1"])
        {
            assert_eq!(
                request["prompt_cache_key"],
                h.store.cache_affinity(scope).unwrap().to_string()
            );
        }
        for request in &requests[..2] {
            let wire = request.to_string();
            assert!(!wire.contains("Call no tools"));
            assert!(!wire.contains("# Compactions"));
            assert!(request["tools"].as_array().is_some_and(|t| !t.is_empty()));
        }
        assert_eq!(requests[2]["input"][0]["role"], "developer");
        assert_eq!(requests[2]["input"][0]["content"][0]["text"], COMPACT);
        assert!(requests[2]["tools"].as_array().is_none_or(Vec::is_empty));
        assert_ne!(
            requests[0]["prompt_cache_key"],
            requests[1]["prompt_cache_key"]
        );
        assert_ne!(
            requests[0]["prompt_cache_key"],
            requests[2]["prompt_cache_key"]
        );
        server.abort();
    }
    fn drain(h: &Harness) -> Vec<crate::store::Outbound> {
        let mut out = vec![];
        while let Some(item) = h.store.next_outbound().unwrap() {
            h.store.sent(&item.id, "test").unwrap();
            out.push(item);
        }
        out
    }

    #[tokio::test]
    async fn silent_completions_settle_without_outbound_or_synthetic_history() {
        for vendor in ["openai", "anthropic", "zai"] {
            let no_blocks = match vendor {
                "openai" => json!({"status":"completed","output":[]}),
                "anthropic" => json!({"stop_reason":"end_turn","content":[]}),
                _ => {
                    json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":null}}]})
                }
            };
            let thought_only = match vendor {
                "openai" => {
                    json!({"status":"completed","output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"private thought"}]}]})
                }
                "anthropic" => {
                    json!({"stop_reason":"end_turn","content":[{"type":"thinking","thinking":"private thought","signature":"test"}]})
                }
                _ => {
                    json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":null,"reasoning_content":"private thought"}}]})
                }
            };
            for response in [
                no_blocks,
                final_response(vendor, ""),
                final_response(vendor, " \n\t "),
                thought_only,
            ] {
                let (_dir, h, run, mock, server) = fixture(vendor, vec![response]).await;
                let memory = run.memory.clone();
                let before = memory.memory.lock().await.export_html();
                mock.release.notify_one();
                let result = h.clone().run_agent(run).await.unwrap();
                assert_eq!(result, "", "{vendor}");
                assert_eq!(memory.memory.lock().await.export_html(), before, "{vendor}");
                assert!(
                    drain(&h).is_empty(),
                    "{vendor}: no placeholder or mention-only message"
                );
                {
                    let db = h.store.db.lock().unwrap();
                    let state: String = db
                        .query_row("SELECT state FROM inbox WHERE id='first'", [], |r| r.get(0))
                        .unwrap();
                    assert_eq!(state, "done");
                    let closed: bool = db
                        .query_row(
                            "SELECT closed FROM ui_sessions WHERE activity='request:first'",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                    assert!(closed);
                    let active: bool = db
                        .query_row(
                            "SELECT active FROM ui_agents WHERE owner='channel:1'",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                    assert!(!active);
                }
                h.store.recover().unwrap();
                assert!(h.store.queued(1).unwrap().is_empty());
                assert!(
                    drain(&h).is_empty(),
                    "completed silent input is not recovered/replayed"
                );
                assert_eq!(mock.requests.lock().await.len(), 1);
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn silent_final_keeps_visible_progress_and_tool_effects_once() {
        let call = json!({"status":"completed","output":[
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Checking the file"}]},
            {"type":"function_call","call_id":"write-once","name":"write","arguments":"{\"path\":\"result.txt\",\"text\":\"verified\"}"}
        ]});
        let (dir, h, run, mock, server) =
            fixture("openai", vec![call, final_response("openai", "")]).await;
        mock.release.notify_one();
        let transcript = h.clone().run_agent(run).await.unwrap();
        assert_eq!(transcript, "Checking the file\n");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
            "verified"
        );
        let out = drain(&h);
        let replies: Vec<_> = out
            .iter()
            .filter(|out| !out.id.contains(":activity:"))
            .collect();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].text, "Checking the file");
        {
            let db = h.store.db.lock().unwrap();
            let tools: i64 = db
                .query_row(
                    "SELECT count(*) FROM tool_runs WHERE state='done'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(tools, 1);
        }
        h.store.recover().unwrap();
        assert!(drain(&h).is_empty());
        assert_eq!(mock.requests.lock().await.len(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn silent_final_consumes_steering_and_only_sends_revised_answer() {
        let (_dir, h, run, mock, server) = fixture(
            "openai",
            vec![
                final_response("openai", ""),
                final_response("openai", "revised answer"),
            ],
        )
        .await;
        let task = tokio::spawn(h.clone().run_agent(run));
        mock.started.notified().await;
        h.store
            .admit(&Input {
                id: "200".into(),
                channel: 1,
                user: 2,
                text: "change the plan".into(),
            })
            .unwrap();
        mock.release.notify_one();
        assert_eq!(task.await.unwrap().unwrap(), "revised answer\n");
        let out = drain(&h);
        let replies: Vec<_> = out
            .iter()
            .filter(|out| !out.id.contains(":activity:"))
            .collect();
        assert_eq!(replies.len(), 1);
        assert!(replies[0].text.contains("revised answer"));
        assert_eq!(replies[0].user, Some(2));
        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[1].to_string().contains("change the plan"));
        server.abort();
    }

    #[tokio::test]
    async fn visible_reply_text_and_delivery_are_unchanged() {
        for vendor in ["openai", "anthropic", "zai"] {
            let text = "  A real reply.\nKeep this formatting.  ";
            let (_dir, h, run, mock, server) =
                fixture(vendor, vec![final_response(vendor, text)]).await;
            mock.release.notify_one();
            assert_eq!(h.clone().run_agent(run).await.unwrap(), format!("{text}\n"));
            let out = drain(&h);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].text, format!("<@2> {text}"));
            assert_eq!(out[0].user, Some(2));
            server.abort();
        }
    }

    #[tokio::test]
    async fn incomplete_provider_response_remains_a_turn_error() {
        let (_dir, h, run, mock, server) =
            fixture("openai", vec![json!({"status":"incomplete","output":[]})]).await;
        mock.release.notify_one();
        let error = h.clone().run_agent(run).await.unwrap_err();
        assert!(error.to_string().contains("provider response incomplete"));
        let db = h.store.db.lock().unwrap();
        let state: String = db
            .query_row("SELECT state FROM inbox WHERE id='first'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "failed");
        let phase: String = db
            .query_row(
                "SELECT phase FROM ui_agents WHERE owner='channel:1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "Stopped");
        drop(db);
        server.abort();
    }

    #[tokio::test]
    async fn silent_worker_still_delivers_durable_completion_to_coordinator() {
        let (_dir, h, mut run, mock, server) =
            fixture("openai", vec![final_response("openai", "")]).await;
        let output: Value = serde_json::from_str(&h.execute_tool(&mut run, &ToolCall {
            id: "spawn-silent".into(), name: "spawn".into(),
            arguments: json!({"tasks":[{"name":"Silent worker","task":"Finish without a visible reply"}]}).into(),
        }).await.unwrap()).unwrap();
        let id = output["ids"][0].as_str().unwrap();
        mock.started.notified().await;
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.children.lock().await.contains_key(id) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(h.store.agent(id).unwrap().unwrap().report, "");
        let inbox = h.store.queued(1).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].text, format!("[{id}] "));
        let out = drain(&h);
        assert!(
            out.iter().all(|out| out.id.contains(":activity:")),
            "only normal lifecycle activity, no worker text reply"
        );
        let db = h.store.db.lock().unwrap();
        let done: bool = db
            .query_row("SELECT state='done' FROM tasks WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(done);
        let runs: i64 = db
            .query_row(
                "SELECT count(*) FROM agent_runs WHERE owner=?1 AND state='done'",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(runs, 1);
        drop(db);
        h.shutdown.cancel();
        server.abort();
    }

    #[tokio::test]
    async fn default_root_reads_directly_with_a_stable_tool_prefix() {
        let read = json!({"status":"completed","output":[{"type":"function_call","call_id":"read-source","name":"read","arguments":"{\"path\":\"source.txt\"}"}],"usage":{}});
        let (directory, h, run, mock, server) = fixture(
            "openai",
            vec![
                read,
                final_response("openai", "Verified the source directly"),
            ],
        )
        .await;
        std::fs::write(
            directory.path().join("source.txt"),
            "verified-source-evidence",
        )
        .unwrap();
        let memory = run.memory.clone();
        let task = tokio::spawn(h.clone().run_agent(run));
        mock.started.notified().await;
        mock.release.notify_one();
        task.await.unwrap().unwrap();

        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        assert_eq!(requests[0]["input"][0], requests[1]["input"][0]);
        for name in [
            "read",
            "write",
            "shell",
            "browser",
            "web_search",
            "web_fetch",
            "spawn",
        ] {
            assert!(
                requests[0]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == name)
            );
        }
        assert!(requests[1]["input"].as_array().unwrap().iter().any(|item| {
            item["type"] == "function_call_output"
                && item["call_id"] == "read-source"
                && item["output"] == "verified-source-evidence"
        }));
        assert!(
            memory
                .memory
                .lock()
                .await
                .export_html()
                .contains("verified-source-evidence")
        );
        assert!(h.children.lock().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn named_workers_inherit_reply_context_and_use_explicit_model_settings() {
        let (_dir, h, mut run, mock, server) =
            fixture("openai", vec![final_response("anthropic", "Scout report")]).await;
        run.context = crate::ui::ReplyContext::request("123456789012345678");
        let call = ToolCall {
            id: "named".into(),
            name: "spawn".into(),
            arguments: json!({"tasks":[{"name":"Docs Scout","task":"Research documentation","model":"anthropic/test","reasoning":"low"}]}).into(),
        };
        let result =
            tokio::time::timeout(Duration::from_millis(500), h.execute_tool(&mut run, &call))
                .await
                .unwrap()
                .unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        let id = result["ids"][0].as_str().unwrap();
        assert_eq!(result["agents"][0]["name"], "Docs Scout");
        assert_eq!(result["agents"][0]["model"], "anthropic/test");
        assert_eq!(h.store.agent_label(id).unwrap(), "Docs Scout");
        assert_eq!(
            h.store.reply_context(id).unwrap().reply_to,
            run.context.reply_to
        );
        let record = h.store.agent(id).unwrap().unwrap();
        assert_eq!(record.model, "anthropic/test");
        assert_eq!(record.reasoning, "low");
        mock.started.notified().await;
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.children.lock().await.contains_key(id) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let activity = drain(&h);
        assert!(activity.iter().any(|m|m.text.contains("spawned Docs Scout")&&m.text.contains("anthropic/test")));
        assert!(
            activity
                .iter()
                .any(|m| m.text.contains("incoming agent message from Docs Scout"))
        );
        server.abort();
    }
    #[tokio::test]
    async fn model_overrides_keep_chat_local_and_compactor_global_across_restart() {
        let (dir, h, _run, _mock, server) = fixture("openai", vec![]).await;
        let mut alpha = crate::models::Model::plain("alpha");
        alpha.efforts = vec!["low".into(), "high".into()];
        alpha.default_effort = Some("low".into());
        h.discord
            .models
            .replace("openai", vec![alpha, crate::models::Model::plain("beta")]);
        h.store
            .usage(1, "openai/test", &json!({"input_tokens":42}))
            .unwrap();
        h.command(1,2,"model",&json!([{"name":"kind","value":"chat"},{"name":"provider","value":"openai"},{"name":"model","value":"alpha"}])).await.unwrap();
        assert_eq!(
            h.store.settings(1, "ignored", "ignored").unwrap(),
            ("openai/alpha".into(), "low".into())
        );
        assert_eq!(
            h.store
                .settings(2, &h.config.agent.model, &h.config.agent.reasoning)
                .unwrap()
                .0,
            h.config.agent.model
        );
        h.command(
            1,
            2,
            "model",
            &json!([{"name":"kind","value":"compact"},{"name":"model","value":"openai/beta"}]),
        )
        .await
        .unwrap();
        assert_eq!(
            h.store
                .compactor_model(GLOBAL_COMPACTOR, "fallback")
                .unwrap(),
            "openai/beta"
        );
        assert_eq!(
            h.store
                .compactor_model(2, &h.config.agent.compactor_model)
                .unwrap(),
            h.config.agent.compactor_model
        );
        assert_eq!(
            h.store.stats(1).unwrap()["last_request_usage"]["input_tokens"],
            42
        );
        let reopened = Store::open(&dir.path().join("state/runtime.sqlite")).unwrap();
        assert_eq!(
            reopened
                .compactor_model(GLOBAL_COMPACTOR, "fallback")
                .unwrap(),
            "openai/beta"
        );
        assert_eq!(
            reopened.settings(1, "ignored", "ignored").unwrap().0,
            "openai/alpha"
        );
        assert!(
            h.command(
                1,
                2,
                "reasoning",
                &json!([{"name":"level","value":"minimal"}])
            )
            .await
            .is_err()
        );
        h.command(1, 2, "reasoning", &json!([{"name":"level","value":"high"}]))
            .await
            .unwrap();
        assert!(
            h.command(
                1,
                2,
                "model",
                &json!([{"name":"kind","value":"wrong"},{"name":"model","value":"openai/beta"}])
            )
            .await
            .is_err()
        );
        assert!(
            h.command(
                1,
                2,
                "model",
                &json!([{"name":"provider","value":"openai"},{"name":"model","value":"codex/beta"}])
            )
            .await
            .is_err()
        );
        assert_eq!(
            h.store.settings(1, "ignored", "ignored").unwrap(),
            ("openai/alpha".into(), "high".into())
        );
        h.command(
            1,
            2,
            "model",
            &json!([{"name":"kind","value":"compact"},{"name":"model","value":"default"}]),
        )
        .await
        .unwrap();
        assert_eq!(
            h.store
                .compactor_model(GLOBAL_COMPACTOR, "new-config-default")
                .unwrap(),
            "new-config-default"
        );
        assert_eq!(h.config.agent.model, "openai/test");
        h.shutdown.cancel();
        server.abort();
    }
    #[tokio::test]
    async fn compactor_pins_running_jobs_and_picks_up_global_overrides_for_new_jobs() {
        let (_dir, h, run, mock, server) = fixture(
            "openai",
            vec![
                final_response("openai", &"x".repeat(512)),
                final_response("openai", "merged"),
            ],
        )
        .await;
        h.store
            .set_compactor_model(GLOBAL_COMPACTOR, Some("openai/first"))
            .unwrap();
        run.memory
            .memory
            .lock()
            .await
            .append(Kind::Talk, &"long source ".repeat(100))
            .unwrap();
        let worker = tokio::spawn(
            h.clone()
                .compactor(GLOBAL_COMPACTOR, run.memory.shared.clone()),
        );
        mock.started.notified().await;
        assert_eq!(mock.requests.lock().await[0]["model"], "first");
        h.store
            .set_compactor_model(GLOBAL_COMPACTOR, Some("openai/second"))
            .unwrap();
        run.memory.changed.notify_one();
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while mock.requests.lock().await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(mock.requests.lock().await[1]["model"], "second");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !run.memory.memory.lock().await.is_settled() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        h.shutdown.cancel();
        worker.await.unwrap().unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn command_cards_have_structured_context_and_configuration_fields() {
        let (_dir, h, _run, _mock, server) = fixture("openai", vec![]).await;
        for command in ["context", "status", "model", "reasoning", "subagents"] {
            let result = h.command(1, 2, command, &json!([])).await.unwrap();
            assert_eq!(result["content"], "");
            assert!(result["embeds"][0]["title"].as_str().is_some());
            assert!(result["embeds"][0]["color"].as_u64().is_some());
            assert_eq!(result["allowed_mentions"]["parse"], json!([]));
            if command == "context" {
                assert!(
                    result["embeds"][0]["fields"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|f| f["name"] == "Coordinator memory view")
                );
            }
        }
        server.abort();
    }

    #[tokio::test]
    async fn status_reports_compiled_harness_version_when_idle_and_busy() {
        let (_dir, h, run, mock, server) = fixture("openai", vec![]).await;
        h.store
            .present_agent("channel:1", 1, &run.context, "Pantheon", "openai/test")
            .unwrap();
        for busy in [false, true] {
            h.store.agent_phase("channel:1", busy, "Thinking").unwrap();
            let result = h.command(1, 2, "status", &json!([])).await.unwrap();
            let fields = result["embeds"][0]["fields"].as_array().unwrap();
            let versions = fields
                .iter()
                .filter(|field| field["name"] == "Version")
                .collect::<Vec<_>>();
            assert_eq!(versions.len(), 1);
            assert_eq!(
                versions[0]["value"],
                format!("`{}`", env!("CARGO_PKG_VERSION"))
            );
            assert_eq!(versions[0]["inline"], true);
            assert_eq!(
                result["embeds"][0]["color"],
                if busy { 0xFEE75C } else { 0x57F287 }
            );
        }
        assert!(mock.requests.lock().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn status_and_context_show_distinct_data_and_worker_search_keeps_root_usage() {
        let response = json!({"status":"completed","output":[{"type":"web_search_call","id":"search","status":"completed"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Result","annotations":[]}]}],"usage":{"input_tokens":999,"output_tokens":20}});
        let (_dir, h, mut run, mock, server) = fixture("openai", vec![response]).await;
        h.store.set_settings(1, "openai/test", "low").unwrap();
        h.store
            .usage(
                1,
                "openai/test",
                &json!({"input_tokens":42,"output_tokens":5}),
            )
            .unwrap();
        run.child = true;
        mock.release.notify_one();
        h.execute_tool(
            &mut run,
            &ToolCall {
                id: "s".into(),
                name: "web_search".into(),
                arguments: json!({"query":"official docs"}).into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            h.store.stats(1).unwrap()["last_request_usage"]["input_tokens"],
            42
        );
        let context = h.command(1, 2, "context", &json!([])).await.unwrap();
        let status = h.command(1, 2, "status", &json!([])).await.unwrap();
        let names = |card: &Value| {
            card["embeds"][0]["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert!(names(&context).contains(&"Model window".into()));
        assert!(!names(&context).contains(&"Delivery".into()));
        assert!(names(&status).contains(&"Active work".into()));
        assert!(names(&status).contains(&"Schedules".into()));
        assert!(!names(&status).contains(&"Model window".into()));
        server.abort();
    }

    #[tokio::test]
    async fn scheduled_notifications_use_incoming_markers() {
        let (_directory, h, run, _mock, server) = fixture("openai", vec![]).await;
        for kind in ["wakeup", "monitor"] {
            h.store.bind_context(kind, &run.context).unwrap();
            h.store.add_job(&Job {
                id: kind.into(), channel: 1, user: 2, kind: kind.into(),
                payload: json!({"_owner":"channel:1","prompt":"check","command":"printf ready"}),
                due: crate::store::now(), interval: None,
            }).unwrap();
        }
        let worker = tokio::spawn(h.clone().job_worker());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let timeline = h
                    .store
                    .db
                    .lock()
                    .unwrap()
                    .query_row(
                        "SELECT count(*) FROM ui_events WHERE label LIKE '↙ % notification%'",
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap();
                if timeline == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let timeline = drain(&h)
            .into_iter()
            .map(|m| m.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(timeline.contains("↙ wakeup notification"));
        assert!(timeline.contains("↙ monitor notification"));
        assert!(!timeline.contains("->"));
        assert!(!timeline.contains("for Coordinator"));
        h.shutdown.cancel();
        worker.await.unwrap().unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn scheduler_tools_are_owned_by_each_agent() {
        let (_directory, h, mut run, _mock, server) = fixture("openai", vec![]).await;
        run.child = true;
        run.owner = "child".into();
        let add = ToolCall {
            id: "job".into(),
            name: "wakeup".into(),
            arguments: json!({"action":"add","schedule":"in 5s","prompt":"check"}).into(),
        };
        let reply: Value =
            serde_json::from_str(&h.execute_tool(&mut run, &add).await.unwrap()).unwrap();
        assert!(drain(&h).is_empty());
        let cancel = ToolCall {
            id: "cancel".into(),
            name: "wakeup".into(),
            arguments: json!({"action":"cancel","id":reply["id"]}).into(),
        };
        run.child = false;
        run.owner = "channel:1".into();
        assert_eq!(
            serde_json::from_str::<Value>(&h.execute_tool(&mut run, &cancel).await.unwrap())
                .unwrap()["cancelled"],
            false
        );
        run.child = true;
        run.owner = "child".into();
        assert_eq!(
            serde_json::from_str::<Value>(&h.execute_tool(&mut run, &cancel).await.unwrap())
                .unwrap()["cancelled"],
            true
        );
        h.shutdown.cancel();
        server.abort();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn claude_task_detaches_and_delivers_an_untrusted_report_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let (directory, mut h, mut run, _mock, server) = fixture("openai", vec![]).await;
        let cli = directory.path().join("claude");
        std::fs::write(&cli, "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"is_error\":false,\"result\":\"finished review\"}'\n").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let harness = Arc::get_mut(&mut h).unwrap();
        harness.config.claude.enabled = true;
        harness.config.claude.executable = cli.display().to_string();
        run.child = true;
        run.owner = "child".into();
        let reply: Value = serde_json::from_str(
            &h.execute_tool(
                &mut run,
                &ToolCall {
                    id: "claude".into(),
                    name: "claude".into(),
                    arguments: json!({"name":"review","task":"inspect this"}).into(),
                },
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(reply["state"], "background");
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.store.agent_events("child").unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let event = &h.store.agent_events("child").unwrap()[0].text;
        assert!(event.contains("[claude "));
        assert!(event.contains("external/untrusted"));
        assert!(event.contains("finished review"));
        let command: String = h
            .store
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT command FROM shell_runs WHERE kind='claude'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!command.contains("inspect this"));
        assert!(command.contains("redacted"));
        h.shutdown.cancel();
        server.abort();
    }
    #[tokio::test]
    async fn claude_invalid_requests_disabled_and_strict_root_create_no_jobs() {
        let (_directory, mut h, mut run, _mock, server) = fixture("openai", vec![]).await;
        let call = |arguments: Value| ToolCall {
            id: "claude-invalid".into(),
            name: "claude".into(),
            arguments: arguments.into(),
        };
        assert!(
            h.execute_tool(&mut run, &call(json!({"task":"x"})))
                .await
                .is_err()
        );
        let harness = Arc::get_mut(&mut h).unwrap();
        harness.config.claude.enabled = true;
        harness.config.agent.coordinator_root = true;
        assert!(
            h.execute_tool(&mut run, &call(json!({"task":"x"})))
                .await
                .is_err()
        );
        run.child = true;
        run.owner = "child".into();
        for args in [
            json!({"task":" "}),
            json!({"task":"x".repeat(12001)}),
            json!({"task":"x","effort":"ultra"}),
            json!({"task":"x","cwd":"/tmp"}),
        ] {
            assert!(h.execute_tool(&mut run, &call(args)).await.is_err());
        }
        let count: i64 = h
            .store
            .db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM shell_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        h.shutdown.cancel();
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn claude_capacity_stop_shutdown_and_error_delivery_are_durable() {
        use std::os::unix::fs::PermissionsExt;
        for stop in [true, false] {
            let (directory, mut h, mut run, _mock, server) = fixture("openai", vec![]).await;
            let cli = directory.path().join("claude");
            std::fs::write(&cli, "#!/bin/sh\ncat >/dev/null\nsleep 30\n").unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            let harness = Arc::get_mut(&mut h).unwrap();
            harness.config.claude.enabled = true;
            harness.config.claude.executable = cli.display().to_string();
            harness.claude_capacity = Arc::new(Semaphore::new(1));
            run.child = true;
            run.owner = "child".into();
            let call = ToolCall {
                id: "bounded-job".into(),
                name: "claude".into(),
                arguments: json!({"task":"x"}).into(),
            };
            let result: Value =
                serde_json::from_str(&h.execute_tool(&mut run, &call).await.unwrap()).unwrap();
            assert!(
                h.execute_tool(&mut run, &call)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("capacity")
            );
            if stop {
                h.command(1, 2, "stop", &json!({})).await.unwrap();
            } else {
                h.shutdown.cancel();
            }
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let state: String = h
                        .store
                        .db
                        .lock()
                        .unwrap()
                        .query_row(
                            "SELECT state FROM shell_runs WHERE id=?1",
                            [result["job_id"].as_str().unwrap()],
                            |r| r.get(0),
                        )
                        .unwrap();
                    if state == "cancelled" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(
                h.store.agent_events("child").unwrap().is_empty(),
                "a cancelled caller must not be reawakened"
            );
            let count: i64 = h
                .store
                .db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM shell_runs", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1);
            h.shutdown.cancel();
            server.abort();
        }
        // A subprocess failure, unlike explicit cancellation, wakes the owner
        // exactly once with its persisted error and never retries the CLI.
        let (_directory, mut h, mut run, _mock, server) = fixture("openai", vec![]).await;
        let harness = Arc::get_mut(&mut h).unwrap();
        harness.config.claude.enabled = true;
        harness.config.claude.executable = "missing-fake-claude".into();
        run.child = true;
        run.owner = "child".into();
        h.execute_tool(
            &mut run,
            &ToolCall {
                id: "failed-job".into(),
                name: "claude".into(),
                arguments: json!({"task":"x"}).into(),
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while h.store.agent_events("child").unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let events = h.store.agent_events("child").unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].text.contains("Error: spawn Claude Code"));
        h.shutdown.cancel();
        server.abort();
    }

    /// Deliberately ignored: this spends one logged-in Claude plan request. It
    /// proves the public tool path (detachment plus owner inbox delivery), not
    /// just the subprocess parser. Enable only with explicit operator approval.
    #[ignore = "requires PANTHEON_CLAUDE_LIVE_TEST=1 and a manually logged-in Claude CLI on PATH"]
    #[tokio::test]
    async fn live_claude_task_detaches_and_delivers_to_owner() -> anyhow::Result<()> {
        anyhow::ensure!(
            std::env::var("PANTHEON_CLAUDE_LIVE_TEST").as_deref() == Ok("1"),
            "set PANTHEON_CLAUDE_LIVE_TEST=1 to authorize this paid Claude CLI test"
        );
        let (_directory, mut h, mut run, _mock, server) = fixture("openai", vec![]).await;
        let harness = Arc::get_mut(&mut h).unwrap();
        harness.config.claude.enabled = true;
        harness.config.claude.executable = "claude".into();
        harness.config.claude.timeout_seconds = 120;
        harness.config.claude.allowed_tools.clear();
        crate::claude::validate(&h.config.claude).await?;
        run.child = true;
        run.owner = "child".into();
        let reply: Value = serde_json::from_str(
            &h.execute_tool(
                &mut run,
                &ToolCall {
                    id: "claude-live".into(),
                    name: "claude".into(),
                    arguments: json!({
                        "name":"live smoke",
                        "task":"Reply with exactly CLAUDE_RUNTIME_SMOKE_OK. Do not use tools.",
                        "model":"opus",
                        "effort":"low"
                    })
                    .into(),
                },
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(reply["state"], "background");
        tokio::time::timeout(Duration::from_secs(130), async {
            loop {
                let events = h.store.agent_events("child").unwrap();
                if !events.is_empty() {
                    assert!(
                        events[0].text.contains("CLAUDE_RUNTIME_SMOKE_OK"),
                        "{}",
                        events[0].text
                    );
                    assert!(
                        events[0].text.contains("claude-opus-5-5"),
                        "{}",
                        events[0].text
                    );
                    eprintln!(
                        "CLAUDE LIVE EVIDENCE job={} durable_owner=child report={}",
                        reply["job_id"], events[0].text
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        h.shutdown.cancel();
        server.abort();
        Ok(())
    }
    #[tokio::test]
    async fn detached_shell_returns_then_delivers_completion_to_its_owner() {
        let (_directory, mut h, mut run, _mock, server) = fixture("openai", vec![]).await;
        Arc::get_mut(&mut h)
            .unwrap()
            .config
            .agent
            .shell_background_after_seconds = 0;
        run.child = true;
        run.owner = "child".into();
        let reply: Value = serde_json::from_str(
            &h.shell_tool(&run, "sleep 0.1; printf completed")
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(reply["state"], "background");
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.store.agent_events("child").unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let events = h.store.agent_events("child").unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].text.contains("completed"));
        assert!(h.store.queued(1).unwrap().is_empty());
        h.shutdown.cancel();
        server.abort();
    }
    #[tokio::test]
    async fn archived_child_resumes_same_identity_when_told() {
        let (_directory, h, mut run, mock, server) = fixture(
            "openai",
            vec![
                final_response("openai", "initial report"),
                final_response("openai", "completion report"),
            ],
        )
        .await;
        let spawn = ToolCall {
            id: "spawn".into(),
            name: "spawn".into(),
            arguments: json!({"tasks":["handle background work"]}).into(),
        };
        let response: Value =
            serde_json::from_str(&h.execute_tool(&mut run, &spawn).await.unwrap()).unwrap();
        let id = response["ids"][0].as_str().unwrap();
        mock.started.notified().await;
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !h.children.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        h.store
            .db
            .lock()
            .unwrap()
            .execute("UPDATE agent_lifecycle SET last_active=0 WHERE id=?1", [id])
            .unwrap();
        h.store.archive_idle_agents(3600).unwrap();
        assert!(h.store.tasks(1).unwrap().as_array().unwrap().is_empty());
        h.execute_tool(
            &mut run,
            &ToolCall {
                id: "tell".into(),
                name: "tell".into(),
                arguments: json!({"id":id,"message":"[shell job] completed successfully"}).into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(h.store.tasks(1).unwrap()[0]["id"], id);
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.store.queued(1).unwrap().len() != 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(h.store.agent_events(id).unwrap().is_empty());
        assert!(h.store.queued(1).unwrap()[1].text.contains(id));
        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[1].to_string().contains("completed successfully"));
        assert!(requests[1].to_string().contains("initial report"));
        h.shutdown.cancel();
        server.abort();
    }
    #[tokio::test]
    async fn steering_during_final_response_continues_without_premature_ping() {
        let (_d, h, run, mock, server) = fixture(
            "openai",
            vec![
                final_response("openai", "obsolete answer"),
                final_response("openai", "revised answer"),
            ],
        )
        .await;
        let task = tokio::spawn(h.clone().run_agent(run));
        mock.started.notified().await;
        h.store
            .admit(&Input {
                id: "200".into(),
                channel: 1,
                user: 2,
                text: "change the plan".into(),
            })
            .unwrap();
        assert_eq!(h.store.next_reaction(&[]).unwrap().unwrap().2, 0);
        mock.release.notify_one();
        task.await.unwrap().unwrap();
        assert_eq!(h.store.next_reaction(&[]).unwrap().unwrap().2, 1);
        let activity = drain(&h);
        assert!(
            activity
                .iter()
                .all(|item| !item.text.contains("steering message received"))
        );
        let out = activity
            .into_iter()
            .filter(|item| !item.id.contains(":activity:"))
            .collect::<Vec<_>>();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].user, None);
        assert_eq!(out[1].user, Some(2));
        assert!(out[1].text.contains("revised answer"));
        let requests = mock.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1]["input"].as_array().unwrap().last().unwrap()["content"][0]["text"],
            "change the plan"
        );
        server.abort();
    }
    #[tokio::test]
    async fn anthropic_steering_queues_until_all_requested_tool_effects_finish() {
        let calls = json!({"stop_reason":"tool_use","content":[{"type":"thinking","thinking":"private-thought","signature":"native-signature"},{"type":"tool_use","id":"c1","name":"write","input":{"path":"should-not-exist","text":"bad"}},{"type":"tool_use","id":"c2","name":"write","input":{"path":"also-missing","text":"bad"}}],"usage":{}});
        let (d, h, run, mock, server) = fixture(
            "anthropic",
            vec![calls.clone(), final_response("anthropic", "Changed plan")],
        )
        .await;
        let memory = run.memory.clone();
        let task = tokio::spawn(h.clone().run_agent(run));
        mock.started.notified().await;
        h.store
            .admit(&Input {
                id: "steer".into(),
                channel: 1,
                user: 2,
                text: "Do not write those files".into(),
            })
            .unwrap();
        mock.release.notify_one();
        task.await.unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("should-not-exist")).unwrap(),
            "bad"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("also-missing")).unwrap(),
            "bad"
        );
        let requests = mock.requests.lock().await;
        let messages = requests[1]["messages"].as_array().unwrap();
        assert_eq!(messages[1]["content"], calls["content"]);
        assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(
            messages[3]["content"][0]["text"],
            "Do not write those files"
        );
        let exported = memory.memory.lock().await.export_html();
        assert!(!exported.contains("private-thought"));
        assert!(!exported.contains("native-signature"));
        server.abort();
    }
    #[tokio::test]
    async fn steering_accumulates_during_shell_and_follows_the_completed_batch() {
        let calls = json!({"status":"completed","output":[{"type":"function_call","call_id":"slow","name":"shell","arguments":"{\"command\":\"printf started > marker; sleep 0.2; printf done\"}"},{"type":"function_call","call_id":"write","name":"write","arguments":"{\"path\":\"finished.txt\",\"text\":\"finished\"}"}]});
        let (d, h, run, mock, server) = fixture(
            "openai",
            vec![
                calls,
                final_response("openai", "Finished; received both steers"),
            ],
        )
        .await;
        let task = tokio::spawn(h.clone().run_agent(run));
        mock.started.notified().await;
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !d.path().join("marker").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        for (id, text) in [
            ("steer-a", "first correction"),
            ("steer-b", "second correction"),
        ] {
            h.store
                .admit(&Input {
                    id: id.into(),
                    channel: 1,
                    user: 2,
                    text: text.into(),
                })
                .unwrap();
        }
        task.await.unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("finished.txt")).unwrap(),
            "finished"
        );
        let requests = mock.requests.lock().await;
        let input = requests[1]["input"].as_array().unwrap();
        let outputs = input
            .iter()
            .filter(|i| i["type"] == "function_call_output")
            .collect::<Vec<_>>();
        assert_eq!(outputs.len(), 2);
        assert!(
            outputs[0]["output"]
                .as_str()
                .unwrap()
                .contains("stdout:\ndone")
        );
        assert!(!outputs.iter().any(|o| o.to_string().contains("Skipped")));
        let tail = &input[input.len() - 2..];
        assert_eq!(tail[0]["content"][0]["text"], "first correction");
        assert_eq!(tail[1]["content"][0]["text"], "second correction");
        assert!(
            h.store
                .cache_card(1)
                .unwrap()
                .to_string()
                .contains("steered step")
        );
        server.abort();
    }
    #[tokio::test]
    async fn root_detached_shell_updates_original_activity_without_completion_marker() {
        let (_d, mut h, mut run, _mock, server) = fixture("openai", vec![]).await;
        Arc::get_mut(&mut h)
            .unwrap()
            .config
            .agent
            .shell_background_after_seconds = 0;
        let call = ToolCall {
            id: "shell-call".into(),
            name: "shell".into(),
            arguments: json!({"command":"sleep 0.1; printf done"}).into(),
        };
        h.store
            .start_tool_activity(&run.owner, &run.context, 1, "public-call", &call)
            .unwrap();
        let reply = h
            .execute_tool_tracked(&mut run, &call, Some("public-call"))
            .await
            .unwrap();
        h.store
            .finish_tool_activity(
                &run.owner,
                &run.context,
                1,
                "public-call",
                &call,
                &reply,
                "background",
                Duration::ZERO,
            )
            .unwrap();
        h.store
            .enqueue_notice(
                &run.context,
                "progress",
                1,
                None,
                "Continuing independent work",
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while h
                .store
                .queued(1)
                .unwrap()
                .iter()
                .all(|i| !i.id.starts_with("shell:"))
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        h.steer(&mut run, "openai").await.unwrap();
        let timeline = drain(&h);
        assert_eq!(timeline.len(), 2);
        assert!(timeline[0].text.contains("↙ shell"));
        assert!(timeline[0].text.contains("↓ 1 lines"));
        assert!(!timeline.iter().any(|t| t.text.contains("shell completion")));
        assert!(run.steering[0].contains("stdout:\ndone"));
        h.shutdown.cancel();
        server.abort();
    }
    #[tokio::test]
    async fn fresh_turn_starts_from_view_and_not_previous_native_transcript() {
        let (_d, h, run, mock, server) =
            fixture("openai", vec![final_response("openai", "first answer")]).await;
        mock.release.notify_one();
        h.clone().run_agent(run).await.unwrap();
        let requests = mock.requests.lock().await;
        assert_eq!(requests[0]["input"].as_array().unwrap().len(), 2);
        assert_eq!(
            requests[0]["input"][1]["content"].as_array().unwrap().len(),
            3
        );
        assert_eq!(requests[0]["input"][1]["content"][2]["text"], "do work");
        server.abort();
    }
    #[tokio::test]
    async fn spawn_returns_before_children_finish_and_delivers_individual_reports() {
        let (_d, h, mut run, mock, server) = fixture(
            "openai",
            vec![
                final_response("openai", "first child report"),
                final_response("openai", "second child report"),
            ],
        )
        .await;
        let call = ToolCall {
            id: "spawn-call".into(),
            name: "spawn".into(),
            arguments: json!({"tasks":["independent task one","independent task two"]}).into(),
        };
        let response =
            tokio::time::timeout(Duration::from_secs(1), h.execute_tool(&mut run, &call))
                .await
                .unwrap()
                .unwrap();
        let value: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["mode"], "background");
        assert_eq!(value["ids"].as_array().unwrap().len(), 2);
        mock.started.notified().await;
        // One child remains blocked while the other finishes. Its report must
        // arrive immediately instead of waiting for the rest of the batch.
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.store.queued(1).unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(h.store.queued(1).unwrap().len(), 1);
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.store.queued(1).unwrap().len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let reports = h.store.queued(1).unwrap();
        assert_eq!(reports.len(), 2);
        assert!(
            reports
                .iter()
                .any(|r| r.text.contains("first child report"))
        );
        assert!(
            reports
                .iter()
                .any(|r| r.text.contains("second child report"))
        );
        assert_ne!(reports[0].id, reports[1].id);
        assert!(
            !run.memory
                .memory
                .lock()
                .await
                .export_html()
                .contains("child report")
        );
        for request in mock.requests.lock().await.iter() {
            assert!(
                request["input"][0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("subagent")
            );
            assert!(
                request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|t| t["name"] != "spawn")
            );
        }
        h.shutdown.cancel();
        server.abort();
    }
    #[tokio::test]
    async fn peer_message_wakes_archived_coordinator_with_shared_memory_and_channel_owned_reply() {
        let (_directory, h, mut run, mock, server) = fixture(
            "openai",
            vec![final_response("openai", "neighbor response")],
        )
        .await;
        run.memory
            .memory
            .lock()
            .await
            .append(Kind::User, "private source channel note")
            .unwrap();
        h.store
            .admit(&Input {
                id: "old-neighbor".into(),
                channel: 3,
                user: 2,
                text: "past discussion".into(),
            })
            .unwrap();
        h.store.input_state("old-neighbor", "done").unwrap();
        h.store
            .db
            .lock()
            .unwrap()
            .execute("UPDATE ui_sessions SET closed=1 WHERE channel='3'", [])
            .unwrap();
        h.store
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE coordinator_lifecycle SET last_active=0 WHERE channel='3'",
                [],
            )
            .unwrap();
        h.store.archive_idle_agents(3600).unwrap();
        assert!(
            h.store.list_coordinators(false, 10, None).unwrap()["agents"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a["id"] != "channel:3")
        );
        // Capability restrictions are enforced by the harness, not only prompts.
        h.store.add_task("worker", "batch", 1, 2, "work").unwrap();
        run.child = true;
        run.owner = "worker".into();
        for call in [
            ToolCall {
                id: "find".into(),
                name: "list_agents".into(),
                arguments: json!({"kind":"coordinators"}).into(),
            },
            ToolCall {
                id: "tell".into(),
                name: "tell".into(),
                arguments: json!({"id":"channel:3","message":"forbidden"}).into(),
            },
        ] {
            assert!(h.execute_tool(&mut run, &call).await.is_err());
        }
        run.child = false;
        run.owner = "channel:1".into();
        h.execute_tool(
            &mut run,
            &ToolCall {
                id: "tell".into(),
                name: "tell".into(),
                arguments: json!({"id":"channel:3","message":"please share findings"}).into(),
            },
        )
        .await
        .unwrap();
        mock.started.notified().await;
        {
            let requests = mock.requests.lock().await;
            assert_eq!(requests.len(), 1);
            let input = requests[0]["input"].to_string();
            assert!(input.contains("[channel:1] please share findings"));
            // Coordinators intentionally share instance history; delivery stays channel-owned.
            assert!(input.contains("private source channel note"));
        }
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2),async {
            loop {
                let ready:bool=h.store.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM outbox WHERE channel='3' AND text='neighbor response')",[],|r|r.get(0)).unwrap();
                if ready {break;}
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.unwrap();
        let timeline = drain(&h);
        assert!(timeline.iter().any(
            |m| m.channel == 1 && m.text.contains("↗ message sent to Coordinator [channel:3]")
        ));
        let answer = timeline
            .iter()
            .find(|m| m.text == "neighbor response")
            .unwrap();
        assert_eq!(answer.channel, 3);
        assert!(answer.reply_to.is_none());
        assert!(answer.user.is_none());
        assert!(timeline.iter().any(|m| {
            m.channel == 3
                && m.text
                    .contains("incoming coordinator message from channel:1")
        }));
        assert!(
            h.channel(3)
                .await
                .unwrap()
                .memory
                .lock()
                .await
                .export_html()
                .contains("please share findings")
        );
        assert!(
            run.memory
                .memory
                .lock()
                .await
                .export_html()
                .contains("neighbor response")
        );
        h.shutdown.cancel();
        server.abort();
    }

    #[tokio::test]
    async fn worker_tools_and_notifications_stay_private_until_detached_work_finishes() {
        // Hold the detached shell until the pre-completion assertions finish.
        // A fixed sleep can expire under CPU contention (especially debug Nix
        // tests), incorrectly testing the already-completed branch instead.
        let shell_command = "while [ -e hold-detached-shell ]; do sleep 0.01; done; printf Hi";
        let calls = json!({"status":"completed","output":[
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Private worker progress"}]},
            {"type":"function_call","call_id":"delay","name":"shell","arguments":json!({"command":shell_command}).to_string()},
            {"type":"function_call","call_id":"error","name":"write","arguments":"{\"path\":\"../escape\",\"text\":\"bad\"}"}
        ]});
        let (dir, mut h, mut run, mock, server) = fixture(
            "openai",
            vec![
                calls,
                final_response("openai", "Hi too early"),
                final_response("openai", "Hi"),
            ],
        )
        .await;
        let shell_barrier = dir.path().join("hold-detached-shell");
        std::fs::write(&shell_barrier, b"").unwrap();
        Arc::get_mut(&mut h)
            .unwrap()
            .config
            .agent
            .shell_background_after_seconds = 0;
        let spawn = ToolCall {
            id: "spawn".into(),
            name: "spawn".into(),
            arguments: json!({"tasks":[{"name":"Delayed Greeter","task":"Wait, then say hi"}]})
                .into(),
        };
        let response: Value =
            serde_json::from_str(&h.execute_tool(&mut run, &spawn).await.unwrap()).unwrap();
        let id = response["ids"][0].as_str().unwrap();
        mock.started.notified().await;
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.children.lock().await.contains_key(id) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(h.store.queued(1).unwrap().is_empty());
        assert_eq!(h.store.agent(id).unwrap().unwrap().report, "Hi too early");
        assert!(h.store.refresh_activities(1, true).unwrap());
        let mut timeline = drain(&h);
        assert_eq!(timeline.len(), 1);
        assert!(timeline[0].text.contains("↗ spawned Delayed Greeter"));
        assert!(!timeline[0].text.contains("shell ·"));
        assert!(!timeline[0].text.contains("write ·"));
        std::fs::remove_file(shell_barrier).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.store.agent_events(id).unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        h.ensure_agent(id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.store.queued(1).unwrap().is_empty() || h.children.lock().await.contains_key(id)
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let reports = h.store.queued(1).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].text, format!("[{id}] Hi"));
        timeline.extend(drain(&h));
        let visible = timeline
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            visible
                .matches("↙ incoming agent message from Delayed Greeter")
                .count(),
            1
        );
        assert!(!visible.contains("shell ·"));
        assert!(!visible.contains("write ·"));
        assert!(!visible.contains("shell completion"));
        assert!(!visible.contains("Private worker progress"));
        let trace = Memory::open(
            h.config.state_dir.join("subagents").join(id),
            h.config.agent.view_bytes,
        )
        .unwrap();
        let private = trace.export_html();
        assert!(private.contains(shell_command));
        assert!(private.contains("../escape"));
        assert!(private.contains("background"));
        assert!(private.contains("Error:"));
        assert!(
            !run.memory
                .memory
                .lock()
                .await
                .export_html()
                .contains(shell_command)
        );
        let requests = mock.requests.lock().await;
        assert!(requests[2].to_string().contains("0|note: subagent"));
        assert!(requests[2].to_string().contains("Private worker progress"));
        assert!(requests[2].to_string().contains("Hi too early"));
        assert!(requests[2].to_string().contains("stdout"));
        h.shutdown.cancel();
        server.abort();
    }

    #[tokio::test]
    async fn coordinator_tool_calls_remain_visible() {
        let calls = json!({"status":"completed","output":[{"type":"function_call","call_id":"write","name":"write","arguments":"{\"path\":\"root-file\",\"text\":\"ok\"}"}]});
        let (_dir, h, run, mock, server) =
            fixture("openai", vec![calls, final_response("openai", "Done")]).await;
        mock.release.notify_one();
        h.clone().run_agent(run).await.unwrap();
        let timeline = drain(&h);
        assert!(timeline.iter().any(|m| m.text.contains("✓ write ·")));
        assert!(timeline.iter().all(|m| !m.text.contains("Coordinator /")));
        assert!(timeline.iter().any(|m| m.text.contains("Done")));
        server.abort();
    }

    #[tokio::test]
    async fn skill_loading_reaches_the_model_without_changing_cached_prefixes() {
        let call = json!({"status":"completed","output":[{"type":"function_call","call_id":"load-guide","name":"skill","arguments":"{\"action\":\"load\",\"id\":\"research\"}"}],"usage":{}});
        let (_directory, h, run, mock, server) = fixture(
            "openai",
            vec![
                call,
                final_response("openai", "Ready to compare the products"),
            ],
        )
        .await;
        let task = tokio::spawn(h.run_agent(run));
        mock.started.notified().await;
        mock.release.notify_one();
        task.await.unwrap().unwrap();
        let requests = mock.requests.lock().await;
        assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        assert_eq!(requests[0]["input"][0], requests[1]["input"][0]);
        let output = requests[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["call_id"] == "load-guide" && item["type"] == "function_call_output")
            .unwrap();
        let guide: Value = serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
        assert_eq!(guide["id"], "research");
        assert!(
            guide["text"]
                .as_str()
                .unwrap()
                .contains("Compare like-for-like")
        );
        server.abort();
    }

    #[tokio::test]
    async fn skill_publication_pins_current_turn_and_keeps_memory_and_prefixes_unchanged() {
        let (_directory, h, mut run, _mock, server) = fixture("openai", vec![]).await;
        let before_memory = run.memory.memory.lock().await.export_html();
        let before_system = h.master_system.clone();
        let old = run
            .skills
            .execute(&json!({"action":"load","id":"research"}))
            .unwrap();
        let proposal:crate::skill_library::Proposal=serde_json::from_value(json!({
            "changes":[{"id":"research","expected_revision":1,"retire":false,"files":{"SKILL.md":"---\nname: research\ndescription: Compare products using verified constraints.\n---\nDiscover constraints, compare primary sources and verify the recommendation.","references/checks.md":"Check current product specifications."}}],
            "task_family":"Product comparison","triggers":"Choosing between products","procedure":"Discover constraints and compare sources","variables":"Products, budget and constraints","verification":"Check specifications against constraints","limits":"Prices may change","reason":"Concrete comparison method","evidence":[1]
        })).unwrap();
        h.skills.publish(&proposal).unwrap();
        let call = ToolCall {
            id: "load".into(),
            name: "skill".into(),
            arguments: json!({"action":"load","id":"research"}).into(),
        };
        let pinned: Value =
            serde_json::from_str(&h.execute_tool(&mut run, &call).await.unwrap()).unwrap();
        assert_eq!(pinned["text"], old["text"]);
        assert_eq!(pinned["revision"], 1);
        assert!(
            run.skills
                .execute(&json!({"action":"load","id":"research","file":"references/checks.md"}))
                .is_err()
        );
        run.skills = h.skills.snapshot().unwrap();
        let current: Value =
            serde_json::from_str(&h.execute_tool(&mut run, &call).await.unwrap()).unwrap();
        assert_eq!(current["revision"], 2);
        assert_eq!(
            run.skills
                .execute(&json!({"action":"load","id":"research","file":"references/checks.md"}))
                .unwrap()["text"],
            "Check current product specifications."
        );
        assert_eq!(h.master_system, before_system);
        assert_eq!(run.memory.memory.lock().await.export_html(), before_memory);
        let cancel = CancellationToken::new();
        h.curators.lock().await.insert(1, cancel.clone());
        h.cancel_curator(1).await.unwrap();
        assert!(cancel.is_cancelled());
        let cancel = CancellationToken::new();
        h.curators.lock().await.insert(1, cancel.clone());
        let input = Input {
            id: "arriving-prompt".into(),
            channel: 1,
            user: 2,
            text: "New user work".into(),
        };
        assert!(h.admit_prompt(&input).unwrap());
        assert!(!cancel.is_cancelled());
        let cancel = CancellationToken::new();
        h.curators.lock().await.insert(1, cancel.clone());
        assert!(!h.admit_prompt(&input).unwrap());
        assert!(!cancel.is_cancelled());
        server.abort();
    }
    #[tokio::test]
    async fn mcp_error_results_reach_anthropic_and_activity_keeps_arguments_private() {
        let call = json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"integration-call","name":"mcp","input":{"action":"call","server":"demo","tool":"fail","arguments":{"password":"private-fixture-password"}}}],"usage":{}});
        let (directory, mut h, run, mock, server) = fixture(
            "anthropic",
            vec![
                call,
                final_response("anthropic", "The integration reported a failure"),
            ],
        )
        .await;
        let script = r#"import sys,json
for line in sys.stdin:
 r=json.loads(line)
 if 'id' not in r:continue
 v={'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}} if r['method']=='initialize' else {'isError':True,'content':[{'type':'text','text':'fixture operation failed'}]}
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':v}),flush=True)
"#;
        let config = crate::mcp::McpConfig {
            servers: std::collections::BTreeMap::from([(
                "demo".into(),
                crate::mcp::ServerConfig {
                    command: Some("python3".into()),
                    args: vec!["-u".into(), "-c".into(), script.into()],
                    ..Default::default()
                },
            )]),
        };
        Arc::get_mut(&mut h).unwrap().mcp =
            crate::mcp::Mcp::new(&config, directory.path(), directory.path()).unwrap();
        let task = tokio::spawn(h.clone().run_agent(run));
        mock.started.notified().await;
        mock.release.notify_one();
        task.await.unwrap().unwrap();
        let requests = mock.requests.lock().await;
        let result = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .find(|item| item["type"] == "tool_result" && item["tool_use_id"] == "integration-call")
            .unwrap();
        assert_eq!(result["is_error"], true);
        let timeline = drain(&h);
        assert!(timeline.iter().any(|m| m.text.contains("mcp demo.fail")));
        assert!(
            timeline
                .iter()
                .all(|m| !m.text.contains("private-fixture-password")
                    && !m.text.contains("fixture operation failed"))
        );
        server.abort();
    }
    include!("runtime_attachment_tests.rs");
    include!("runtime_curator_tests.rs");
    include!("runtime_optmem_tests.rs");
    include!("runtime_compaction_parity_tests.rs");
    include!("runtime_request_retry_tests.rs");
    include!("runtime_native_parity_tests.rs");
    include!("runtime_route_parity_tests.rs");
    #[tokio::test]
    async fn zai_tool_loop_preserves_native_reasoning_but_never_publishes_or_logs_it() {
        let calls = json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","content":"Working","reasoning_content":"private-glm-native-reasoning","tool_calls":[{"id":"a","type":"function","function":{"name":"read","arguments":"{\"path\":\"picture.png\"}"}},{"id":"b","type":"function","function":{"name":"write","arguments":"{\"path\":\"result.txt\",\"text\":\"verified\"}"}}]}}],"usage":{"prompt_tokens":20,"completion_tokens":10}});
        for model in ["zai/glm-5.3", "zai/glm-5.3-flash"] {
            let (dir, mut h, mut run, mock, server) =
                fixture("zai", vec![calls.clone(), final_response("zai", "Done")]).await;
            std::fs::write(dir.path().join("picture.png"), b"image-fixture").unwrap();
            Arc::get_mut(&mut h).unwrap().config.agent.show_reasoning = true;
            run.settings.0 = model.into();
            let memory = run.memory.clone();
            let task = tokio::spawn(h.clone().run_agent(run));
            mock.started.notified().await;
            mock.release.notify_one();
            task.await.unwrap().unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
                "verified"
            );
            let requests = mock.requests.lock().await;
            assert_eq!(requests.len(), 2);
            let messages = requests[1]["messages"].as_array().unwrap();
            assert_eq!(
                messages[2]["reasoning_content"],
                "private-glm-native-reasoning"
            );
            assert_eq!(messages[3]["tool_call_id"], "a");
            assert_eq!(messages[4]["tool_call_id"], "b");
            if model.ends_with("flash") {
                assert!(
                    messages.last().unwrap()["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v["type"] == "image_url")
                );
            } else {
                assert!(
                    messages[3]["content"]
                        .as_str()
                        .unwrap()
                        .contains("have NOT been viewed")
                );
                assert!(!requests[1].to_string().contains("data:image"));
            }
            assert!(
                !memory
                    .memory
                    .lock()
                    .await
                    .export_html()
                    .contains("private-glm-native-reasoning")
            );
            assert!(
                drain(&h)
                    .iter()
                    .all(|out| !out.text.contains("private-glm-native-reasoning"))
            );
            server.abort();
        }
    }
    #[tokio::test]
    async fn zai_malformed_tool_batch_executes_nothing_and_does_not_checkpoint_native_state() {
        let response = json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"reasoning_content":"private-malformed-state","tool_calls":[{"id":"a","type":"function","function":{"name":"write","arguments":"{\"path\":\"must-not-exist\",\"text\":\"bad\"}"}},{"id":"b","type":"function","function":{"name":"write","arguments":"{"}}]}}]});
        let (dir, h, run, mock, server) = fixture("zai", vec![response]).await;
        let memory = run.memory.clone();
        let task = tokio::spawn(h.clone().run_agent(run));
        mock.started.notified().await;
        mock.release.notify_one();
        assert!(task.await.unwrap().is_err());
        assert!(!dir.path().join("must-not-exist").exists());
        assert!(
            !memory
                .memory
                .lock()
                .await
                .export_html()
                .contains("private-malformed-state")
        );
        server.abort();
    }
    include!("runtime_prompt_mode_tests.rs");
    include!("runtime_global_memory_tests.rs");
}
