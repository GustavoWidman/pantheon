use crate::{
    browser::BrowserManager,
    config::Config,
    discord::{Discord, Inbound, ToolStatus, render_tool, split_message},
    memory::{COMPACT, Kind, Memory, NodeKey},
    provider::{Provider, ToolCall, model_parts},
    store::{Input, Job, Store},
    tools,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

pub struct Harness {
    pub config: Config,
    pub store: Store,
    pub discord: Arc<Discord>,
    provider: Provider,
    web: crate::web::Web,
    browser: BrowserManager,
    channels: Mutex<HashMap<u64, Arc<Channel>>>,
    children: Mutex<HashMap<String, Child>>,
    capacity: Arc<Semaphore>,
    shell_capacity: Arc<Semaphore>,
    shell_jobs: Mutex<HashMap<String, (u64, CancellationToken)>>,
    shutdown: CancellationToken,
    master_system: String,
    child_system: String,
}
struct Channel {
    memory: Mutex<Memory>,
    changed: Notify,
    incoming: Notify,
    cancel: Mutex<Option<CancellationToken>>,
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
    history: Vec<Value>,
    inputs: Vec<String>,
    trace: Option<Memory>,
    steering: Vec<String>,
}
impl Harness {
    pub fn new(
        config: Config,
        discord: Arc<Discord>,
        shutdown: CancellationToken,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&config.state_dir)?;
        std::fs::create_dir_all(&config.workspace)?;
        let store = Store::open(&config.state_dir.join("runtime.sqlite"))?;
        store.recover()?;
        let instructions = config.instructions()?;
        let h = Self {
            provider: Provider::new(config.agent.request_timeout_seconds)?
                .with_auth(config.auth.clone()),
            web: crate::web::Web::new(config.state_dir.join("web/cache"), config.web.clone())?,
            browser: BrowserManager::new(config.state_dir.join("browsers"), config.browser.clone()),
            capacity: Arc::new(Semaphore::new(config.agent.max_subagents)),
            shell_capacity: Arc::new(Semaphore::new(config.agent.max_shell_jobs)),
            shell_jobs: Mutex::new(HashMap::new()),
            master_system: format!("{}\n{}", include_str!("master.txt"), instructions),
            child_system: format!("{}\n{}", include_str!("child.txt"), instructions),
            config,
            store,
            discord,
            channels: Mutex::new(HashMap::new()),
            children: Mutex::new(HashMap::new()),
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
            memory: Mutex::new(Memory::open(
                self.config.state_dir.join("chats").join(id.to_string()),
                self.config.agent.view_bytes,
            )?),
            changed: Notify::new(),
            incoming: Notify::new(),
            cancel: Mutex::new(None),
        });
        for input in self.store.unlogged(id)? {
            c.memory
                .lock()
                .await
                .append_with_id(Kind::User, &input.text, &input.id)?;
            self.store.mark_logged(&input.id)?;
        }
        channels.insert(id, c.clone());
        let h = self.clone();
        let cc = c.clone();
        tokio::spawn(async move {
            if let Err(e) = h.clone().compactor(cc).await {
                tracing::error!(channel=id,error=%e,"compactor stopped");
                h.shutdown.cancel();
            }
        });
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
                        Inbound::Prompt{id,channel,user,text}=>{
                            let admitted=self.store.admit(&Input{id:id.clone(),channel,user,text})?;self.discord.acknowledge(&id)?;if admitted {self.channel(channel).await?.incoming.notify_one();}
                        }
                        Inbound::Command{id:_,token,channel,user,name,options}=>{
                            let h=self.clone();let permit=command_capacity.clone().try_acquire_owned();
                            commands.spawn(async move {
                                let text=match permit {
                                    Ok(_permit)=>match h.command(channel,user,&name,&options).await {Ok(text)=>text,Err(e)=>format!("Command failed: {e}")},
                                    Err(_)=>"Too many active commands; try again shortly.".into(),
                                };
                                if h.discord.reply_interaction(&token,&text).await.is_err(){tracing::warn!(channel,"interaction reply failed");}
                            });
                        }
                    }
                }
            }
        }
        self.shutdown.cancel();
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
            let _ = out.await;
            let _ = jobs.await;
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
            .append_with_id(Kind::User, &input.text, &input.id)?;
        c.changed.notify_waiters();
        Ok(())
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
            let view = self.settle(&c, &cancel).await;
            if let Err(e) = view {
                // Preserve cancelled admission in the immutable log without answering it.
                for input in &queued {
                    self.store.input_state(&input.id, "cancelled")?;
                    Self::append_input(&c, input).await?;
                    self.store.mark_logged(&input.id)?;
                }
                self.notice(
                    &format!("cancel:{}", queued[0].id),
                    channel,
                    Some(queued[0].user),
                    &e.to_string(),
                )?;
                *c.cancel.lock().await = None;
                continue;
            }
            let mut ids = vec![];
            let mut texts = vec![];
            for input in &queued {
                self.store.input_state(&input.id, "running")?;
                Self::append_input(&c, input).await?;
                self.store.mark_logged(&input.id)?;
                ids.push(input.id.clone());
                texts.push(input.text.clone());
            }
            let settings = self.store.settings(
                channel,
                &self.config.agent.model,
                &self.config.agent.reasoning,
            )?;
            let (vendor, _) = model_parts(&settings.0)?;
            let run = Run {
                channel,
                user: queued[0].user,
                owner: format!("channel:{channel}"),
                child: false,
                memory: c.clone(),
                cancel: cancel.clone(),
                history: Provider::start(vendor, &view.unwrap(), &texts.join("\n\n")),
                settings,
                inputs: ids,
                trace: None,
                steering: vec![],
            };
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
            let defs = tools::definitions(run.child, self.config.agent.coordinator_root);
            let system = if run.child {
                &self.child_system
            } else {
                &self.master_system
            };
            let run_id = uuid::Uuid::new_v4().to_string();
            let outcome = self
                .run_steps(&mut run, &vendor, &defs, system, &run_id)
                .await;
            if !run.child {
                for id in &run.inputs {
                    self.store
                        .input_state(id, if outcome.is_ok() { "done" } else { "failed" })?;
                }
            }
            outcome
        })
    }
    async fn run_steps(
        self: &Arc<Self>,
        run: &mut Run,
        vendor: &str,
        defs: &[Value],
        system: &str,
        run_id: &str,
    ) -> Result<String> {
        let mut transcript = String::new();
        for step in 0..self.config.agent.max_steps {
            if run.cancel.is_cancelled() {
                bail!("cancelled");
            }
            self.steer(run, vendor).await?;
            for text in std::mem::take(&mut run.steering) {
                run.history.push(Provider::user(vendor, &text));
            }
            let response = tokio::select! {
                r=self.provider.step(&run.settings.0,&run.settings.1,system,&run.history,defs)=>r?,
                _=run.cancel.cancelled()=>bail!("cancelled"),
            };
            if !run.child {
                self.store.usage(run.channel, &response.usage)?;
            }
            Provider::append_response(vendor, &mut run.history, &response);
            let final_steered = if response.calls.is_empty() {
                self.steer(run, vendor).await?
            } else {
                false
            };
            for (text, thought) in &response.texts {
                if text.is_empty() {
                    continue;
                }
                if *thought {
                    if self.config.agent.show_reasoning && !run.child {
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
                let mut text = response
                    .texts
                    .iter()
                    .filter(|(_, thought)| !*thought)
                    .map(|(t, _)| t.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if text.is_empty() && response.calls.is_empty() && !final_steered {
                    text = "The model completed without a visible text reply.".into();
                    self.log_run(run, Kind::Talk, &text).await?;
                }
                if !text.is_empty() {
                    if response.calls.is_empty() && !final_steered {
                        let completed = self.store.complete_turn(
                            &run.inputs,
                            &format!("{run_id}:{step}:final"),
                            run.channel,
                            run.user,
                            &split_message(&text, Some(run.user)),
                        )?;
                        if !completed {
                            self.notice(
                                &format!("{run_id}:{step}:intermediate"),
                                run.channel,
                                None,
                                &text,
                            )?;
                            self.steer(run, vendor).await?;
                            continue;
                        }
                    } else {
                        self.notice(
                            &format!("{run_id}:{step}:intermediate"),
                            run.channel,
                            None,
                            &text,
                        )?;
                    }
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
                        .filter(|(_, thought)| !*thought)
                        .map(|(text, _)| text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                } else {
                    transcript
                });
            }
            for (index, call) in response.calls.iter().enumerate() {
                // Each boundary can receive steering; the next request keeps the exact prior prefix.
                let steered = self.steer(run, vendor).await? || !run.steering.is_empty();
                if run.cancel.is_cancelled() {
                    bail!("cancelled");
                }
                self.log_run(
                    run,
                    Kind::Tool,
                    &format!("{} {}", call.name, call.arguments),
                )
                .await?;
                let tool_id = format!("{run_id}:{step}:tool:{index}");
                self.store.tool_start(&tool_id, run.channel, &call.name)?;
                let start = Instant::now();
                if !run.child {
                    self.notice(
                        &tool_id,
                        run.channel,
                        None,
                        &render_tool(&call.name, ToolStatus::Running, Duration::ZERO),
                    )?;
                }
                let result = if steered {
                    Ok("Skipped because new steering arrived; reconsider this call before executing.".to_string())
                } else {
                    self.execute_tool(run, call).await
                };
                let error = result.is_err();
                let output = match result {
                    Ok(v) => v,
                    Err(e) => format!("Error: {e}"),
                };
                let output = crate::memory::cap_tool_result(&output);
                self.log_run(run, Kind::Echo, &output).await?;
                let image = if call.name == "browser"
                    && call.arguments["action"] == "screenshot"
                    && !error
                    && !steered
                {
                    let meta: Value = serde_json::from_str(&output)?;
                    let path = tools::string(&meta, "path")?;
                    let bytes = tokio::fs::read(path).await?;
                    ensure!(
                        bytes.len() <= 12_000_000,
                        "screenshot too large for provider input"
                    );
                    use base64::Engine;
                    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
                } else {
                    None
                };
                Provider::append_result_with_image(
                    vendor,
                    &mut run.history,
                    call,
                    &output,
                    error,
                    image.as_deref(),
                );
                self.store.tool_done(&tool_id)?;
                if !run.child {
                    self.notice(
                        &tool_id,
                        run.channel,
                        None,
                        &render_tool(
                            &call.name,
                            if error {
                                ToolStatus::Error
                            } else {
                                ToolStatus::Done
                            },
                            start.elapsed(),
                        ),
                    )?;
                }
            }
        }
        bail!("maximum tool steps reached")
    }
    async fn log_run(&self, run: &mut Run, kind: Kind, text: &str) -> Result<()> {
        if let Some(trace) = run.trace.as_mut() {
            trace.append(kind, text)?;
        } else {
            Self::append(&run.memory, kind, text).await?;
        }
        Ok(())
    }
    async fn steer(&self, run: &mut Run, _vendor: &str) -> Result<bool> {
        let mut received = false;
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
            for input in self.store.queued(run.channel)? {
                self.store.input_state(&input.id, "running")?;
                Self::append_input(&run.memory, &input).await?;
                self.store.mark_logged(&input.id)?;
                run.steering.push(input.text);
                run.inputs.push(input.id);
                received = true;
            }
        }
        Ok(received)
    }
    async fn execute_tool(self: &Arc<Self>, run: &mut Run, call: &ToolCall) -> Result<String> {
        ensure!(
            tools::definitions(run.child, self.config.agent.coordinator_root)
                .iter()
                .any(|tool| tool["name"] == call.name),
            "tool {} is unavailable to this agent",
            call.name
        );
        let a = &call.arguments;
        let value = match call.name.as_str() {
            "zoom" => {
                return run
                    .memory
                    .memory
                    .lock()
                    .await
                    .zoom(tools::number(a, "id")?, tools::number(a, "n")?);
            }
            "date" => return run.memory.memory.lock().await.date(tools::number(a, "id")?),
            "read" => {
                let p = tools::workspace_path(
                    &self.config.workspace,
                    tools::string(a, "path")?,
                    false,
                )?;
                return tools::read_file(&p).await;
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
                return self.shell_tool(run, tools::string(a, "command")?).await;
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
                let model = self
                    .config
                    .web
                    .search_model
                    .as_deref()
                    .unwrap_or(&run.settings.0);
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
                let value = tokio::select! {
                    value = tokio::time::timeout(Duration::from_secs(self.config.web.search_timeout_seconds), self.provider.search(model, tools::string(a, "query")?, limit, &domains)) => value.context("web search timed out")??,
                    _ = run.cancel.cancelled() => bail!("web search cancelled"),
                };
                self.store.usage(run.channel, &value["usage"])?;
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
                let tasks: Vec<String> = tasks
                    .iter()
                    .map(|t| t.as_str().map(String::from).context("task must be text"))
                    .collect::<Result<_>>()?;
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
                for (id, task) in ids.iter().zip(&tasks) {
                    self.store
                        .add_task(id, &batch, run.channel, run.user, task)?;
                }
                for ((id, task), permit) in ids.iter().zip(tasks).zip(permits) {
                    self.store
                        .register_agent(id, &run.settings.0, &run.settings.1)?;
                    self.start_child(ChildStart {
                        id: id.clone(),
                        channel: run.channel,
                        user: run.user,
                        task,
                        previous: String::new(),
                        settings: run.settings.clone(),
                        memory: run.memory.clone(),
                        view: Some(view.clone()),
                        cancel: run.cancel.child_token(),
                        permit,
                    })
                    .await?;
                }

                json!({"batch":batch,"ids":ids,"mode":"background"})
            }
            "tell" => {
                ensure!(!run.child, "child cannot tell other agents");
                let id = tools::string(a, "id")?;
                let agent = self.store.agent(id)?.context("unknown agent ID")?;
                let channel = agent.channel;
                ensure!(channel == run.channel, "agent belongs to another channel");
                self.store.admit_event(
                    &Input {
                        id: uuid::Uuid::new_v4().to_string(),
                        channel: run.channel,
                        user: run.user,
                        text: tools::string(a, "message")?.to_string(),
                    },
                    id,
                )?;
                self.ensure_agent(id).await?;
                json!({"delivered":id})
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
                previous,
                settings,
                memory,
                view,
                cancel,
                permit,
            } = start;
            let _permit = permit;
            let outcome=async {
                let view=match view {Some(view)=>view,None=>h.settle(&memory,&cancel).await?};
                let mut trace=Memory::open(h.config.state_dir.join("subagents").join(&id),h.config.agent.view_bytes)?;
                let prompt=if previous.is_empty() {task.clone()} else {format!("Original task: {task}\nYour previous report: {previous}\nContinue on the new inbox notifications. Inspect saved tool effects before repeating work.")};
                trace.append(Kind::User,&prompt)?;
                let (vendor,_)=model_parts(&settings.0)?;
                let run=Run {channel,user,owner:id.clone(),child:true,memory:memory.clone(),cancel,settings:settings.clone(),history:Provider::start(vendor,&view,&prompt),inputs:vec![],trace:Some(trace),steering:vec![]};
                h.clone().run_agent(run).await
            }.await;
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
            if let Err(error) = h.store.finish_agent(&id, &report, &delivery_id) {
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
            view: None,
            cancel: self.shutdown.child_token(),
            permit,
        })
        .await?;
        Ok(())
    }
    async fn shell_tool(self: &Arc<Self>, run: &Run, command: &str) -> Result<String> {
        ensure!(!command.is_empty(), "empty command");
        let permit = self
            .shell_capacity
            .clone()
            .try_acquire_owned()
            .context("background shell capacity full")?;
        let id = uuid::Uuid::new_v4().to_string();
        self.store
            .shell_start(&id, &run.owner, run.channel, run.user, command)?;
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
        tokio::spawn(async move {
            let _permit = permit;
            let result = tools::shell(
                &h.config.workspace,
                &command,
                h.config.agent.shell_timeout_seconds,
                &cancel,
            )
            .await;
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
                self.store.add_job(&Job{id:id.clone(),channel,user,kind:kind.into(),payload,due,interval})?;
                self.notice(&format!("job:{id}:queued"),channel,None,&format!("◷ {kind} `{id}` saved; due <t:{due}:R>"))?;
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
    ) -> Result<String> {
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
        let (mut model, mut reasoning) = self.store.settings(
            channel,
            &self.config.agent.model,
            &self.config.agent.reasoning,
        )?;
        let text = match name {
            "model" => {
                if let Some(id) = args["id"].as_str() {
                    model_parts(id)?;
                    model = id.into();
                    self.store.set_settings(channel, &model, &reasoning)?;
                }
                format!("Model: `{model}`. Changes apply to the next fresh turn.")
            }
            "reasoning" => {
                if let Some(level) = args["level"].as_str() {
                    crate::config::validate_reasoning(level)?;
                    reasoning = level.into();
                    self.store.set_settings(channel, &model, &reasoning)?;
                }
                format!("Reasoning: `{reasoning}`. Changes apply to the next fresh turn.")
            }
            "context" | "status" => {
                let stats = c.memory.lock().await.stats();
                let operational = self.store.stats(channel)?;
                let active = c
                    .cancel
                    .lock()
                    .await
                    .as_ref()
                    .is_some_and(|token| !token.is_cancelled());
                let children = self
                    .children
                    .lock()
                    .await
                    .values()
                    .filter(|child| child.channel == channel)
                    .count();
                let usage = &operational["last_request_usage"];
                let cached = usage["input_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .or_else(|| usage["cache_read_input_tokens"].as_u64())
                    .unwrap_or(0);
                let usage_text = if usage.as_object().is_some_and(|o| !o.is_empty()) {
                    format!(
                        "Last request: {} input · {} output · {cached} cached tokens",
                        usage["input_tokens"], usage["output_tokens"]
                    )
                } else {
                    "No provider usage recorded yet.".into()
                };
                format!(
                    "Model: `{model}` · reasoning: `{reasoning}\nMaster: {} · {children} background agents\nMemory: {} messages · {} summaries\nView: {} / {} bytes · {} lines · {}\nQueue: {} prompts · {} pending Discord deliveries\n{usage_text}",
                    if active { "working" } else { "idle" },
                    stats["messages"],
                    stats["summaries"],
                    stats["view_bytes"],
                    stats["view_budget_bytes"],
                    stats["view_lines"],
                    if stats["settled"] == true {
                        "settled"
                    } else {
                        "summarizing"
                    },
                    operational["queued_prompts"],
                    operational["pending_delivery"]
                )
            }
            "stop" => {
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
                let tasks = self.store.tasks(channel)?;
                let rows: Vec<String> = tasks
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(12)
                    .map(|t| {
                        format!(
                            "`{}` · {} · {}",
                            t["id"].as_str().unwrap_or(""),
                            t["state"].as_str().unwrap_or(""),
                            t["task"]
                                .as_str()
                                .unwrap_or("")
                                .replace(['\n', '\r', '`'], " ")
                                .chars()
                                .take(70)
                                .collect::<String>()
                        )
                    })
                    .collect();
                if rows.is_empty() {
                    "No background agents registered in this channel.".into()
                } else {
                    rows.join("\n")
                }
            }
            "wakeup" | "monitor" => {
                let result = self.manage_jobs(channel, user, "*", name, &args)?;
                if let Some(rows) = result.as_array() {
                    if rows.is_empty() {
                        format!("No active {name} jobs.")
                    } else {
                        rows.iter()
                            .take(12)
                            .map(|j| {
                                format!(
                                    "`{}` · due <t:{}:R>{}",
                                    j["id"].as_str().unwrap_or(""),
                                    j["due"],
                                    j["interval_seconds"]
                                        .as_i64()
                                        .map(|s| format!(" · repeats every {s}s"))
                                        .unwrap_or_default()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                } else if let Some(id) = result["id"].as_str() {
                    format!("Saved {name} `{id}` · due <t:{}:R>", result["due"])
                } else {
                    format!("Cancelled: {}", result["cancelled"])
                }
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
                        .map(|b| {
                            if let Some(id) = b["closed"].as_str() {
                                return format!("Closed `{id}`; its profile is retained.");
                            }
                            let links = b["view_urls"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(|url| format!("<{url}>"))
                                .collect::<Vec<_>>()
                                .join("\n");
                            let lease = b["resume_token"]
                                .as_str()
                                .map(|t| {
                                    format!("\nResume token: `{t}` — resume only when finished.")
                                })
                                .unwrap_or_default();
                            format!(
                                "Browser `{}` · {}\n{links}{lease}",
                                b["browser_id"].as_str().unwrap_or(""),
                                b["state"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n")
                }
            }
            _ => bail!("unknown command"),
        };
        Ok(text)
    }
    fn notice(&self, id: &str, channel: u64, mention: Option<u64>, text: &str) -> Result<()> {
        for (i, chunk) in split_message(text, mention).iter().enumerate() {
            self.store.enqueue(
                &format!("{id}:{i}"),
                channel,
                if i == 0 { mention } else { None },
                chunk,
            )?;
        }
        Ok(())
    }
    async fn outbox_worker(&self) -> Result<()> {
        let mut workers = tokio::task::JoinSet::new();
        let mut active = HashSet::new();
        loop {
            if self.shutdown.is_cancelled() {
                break;
            }
            while workers.len() < 8 {
                let Some(out) = self
                    .store
                    .next_outbound_excluding(&active.iter().copied().collect::<Vec<_>>())?
                else {
                    break;
                };
                active.insert(out.channel);
                let discord = self.discord.clone();
                workers.spawn(async move {
                    let result = if let Some(receipt) = &out.receipt {
                        discord.edit(out.channel, receipt, &out.text).await
                    } else {
                        discord
                            .send(out.channel, &out.text, out.user, &out.nonce)
                            .await
                    };
                    (out, result)
                });
            }
            tokio::select! {
                Some(done)=workers.join_next(),if !workers.is_empty()=>{
                    let (out,result)=done.context("delivery worker failed")?;active.remove(&out.channel);
                    match result {Ok(receipt)=>self.store.delivered(&out,&receipt)?,Err(_)=>{self.store.retry_outbound(&out.id)?;tracing::warn!(channel=out.channel,"Discord delivery pending retry");}}
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
        loop {
            if self.shutdown.is_cancelled() {
                break;
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
                        h.notice(
                            &format!("job:{}:{}:fired", job.id, job.due),
                            job.channel,
                            None,
                            &format!("◷ {} `{}` checked", job.kind, job.id),
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
    async fn compactor(self: Arc<Self>, c: Arc<Channel>) -> Result<()> {
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
                if workers.len() >= 8 {
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
                workers.spawn(async move { (key, h.compress(key, &context, &source).await) });
            }
            tokio::select! {
                Some(done)=workers.join_next(),if !workers.is_empty()=>{
                    match done {
                        Ok((key,Ok(text)))=>{busy.remove(&key);retry.remove(&key);c.memory.lock().await.finish(key,&text)?;c.changed.notify_waiters();}
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
    async fn compress(&self, key: NodeKey, context: &str, source: &str) -> Result<String> {
        let (vendor, _) = model_parts(&self.config.agent.compactor_model)?;
        let example = "user: Build Pantheon in Rust as an always-on NixOS Discord agent; preserve every message durably and start each turn with a stable binary summary view. Background workers summarize in order; zoom retrieves exact history. echo: inspected Thoth's gateway and found steering, mention and delivery races. work: browser implementation assigns separate displays and permanent noVNC URLs, with explicit handoff leases. talk: chosen append-only daily files and a durable inbox/outbox; live deployment still needs credentials.";
        let mut scale = example.to_string();
        while scale.len() > 512 {
            scale.pop();
        }
        while scale.len() < 512 {
            scale.push(' ');
        }
        let step = format!(
            "For scale, this line is exactly 512 bytes:\n{scale}\n\n{} into one line, in at most 512 bytes:\n{source}",
            if key.level == 0 {
                "Compress this message"
            } else {
                "Merge these two lines"
            }
        );
        let mut history = Provider::start(vendor, context, &step);
        let mut best: Option<String> = None;
        for _ in 0..5 {
            let response = self
                .provider
                .step(
                    &self.config.agent.compactor_model,
                    "medium",
                    COMPACT,
                    &history,
                    &[],
                )
                .await?;
            let line = response
                .texts
                .iter()
                .filter(|(_, thought)| !*thought)
                .map(|(t, _)| t.as_str())
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            ensure!(!line.is_empty(), "empty summary");
            Provider::append_response(vendor, &mut history, &response);
            if best.as_ref().is_none_or(|s| line.len() < s.len()) {
                best = Some(line.clone());
            }
            if line.len() <= 512 {
                break;
            }
            let mut end = 512;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            history.push(Provider::user(vendor,&format!("That line is {} bytes; the limit is 512. It must end where it is cut here:\n{}| ← LIMIT",line.len(),&line[..end])));
        }
        best.context("no summary")
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
    async fn respond(State(state): State<Arc<Mock>>, Json(body): Json<Value>) -> Json<Value> {
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
        Json(
            state
                .responses
                .lock()
                .await
                .pop_front()
                .expect("unexpected provider request"),
        )
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
        config.agent.coordinator_root = false;
        config.agent.model = format!("{vendor}/test");
        let discord = Arc::new(Discord::new("mock-token".into(), 1, vec![2]).unwrap());
        let mut h = Harness::new(config, discord, CancellationToken::new()).unwrap();
        Arc::get_mut(&mut h).unwrap().provider = Provider::mock(format!("http://{address}/"));
        let c = Arc::new(Channel {
            memory: Mutex::new(Memory::open(directory.path().join("memory"), 128000).unwrap()),
            changed: Notify::new(),
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
            settings: (format!("{vendor}/test"), "medium".into()),
            history: Provider::start(vendor, &view, &input.text),
            inputs: vec![input.id],
            trace: None,
            steering: vec![],
        };
        (directory, h, run, mock, server)
    }
    fn final_response(vendor: &str, text: &str) -> Value {
        if vendor == "openai" {
            json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],"usage":{"input_tokens_details":{"cached_tokens":123}}})
        } else {
            json!({"stop_reason":"end_turn","content":[{"type":"text","text":text}],"usage":{"cache_read_input_tokens":123}})
        }
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
    async fn scheduler_tools_are_owned_by_each_agent() {
        let (_directory, h, mut run, _mock, server) = fixture("openai", vec![]).await;
        run.child = true;
        run.owner = "child".into();
        let add = ToolCall {
            id: "job".into(),
            name: "wakeup".into(),
            arguments: json!({"action":"add","schedule":"in 5s","prompt":"check"}),
        };
        let reply: Value =
            serde_json::from_str(&h.execute_tool(&mut run, &add).await.unwrap()).unwrap();
        let cancel = ToolCall {
            id: "cancel".into(),
            name: "wakeup".into(),
            arguments: json!({"action":"cancel","id":reply["id"]}),
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
    async fn idle_child_resumes_same_identity_on_durable_notification() {
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
            arguments: json!({"tasks":["handle background work"]}),
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
            .admit_event(
                &Input {
                    id: "completion".into(),
                    channel: 1,
                    user: 2,
                    text: "[shell job] completed successfully".into(),
                },
                id,
            )
            .unwrap();
        h.ensure_agent(id).await.unwrap();
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
                id: "second".into(),
                channel: 1,
                user: 2,
                text: "change the plan".into(),
            })
            .unwrap();
        mock.release.notify_one();
        task.await.unwrap().unwrap();
        let out = drain(&h);
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
    async fn anthropic_steering_follows_all_tool_results_and_skips_pending_effects() {
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
        assert!(!d.path().join("should-not-exist").exists());
        assert!(!d.path().join("also-missing").exists());
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
    async fn fresh_turn_starts_from_view_and_not_previous_native_transcript() {
        let (_d, h, run, mock, server) =
            fixture("openai", vec![final_response("openai", "first answer")]).await;
        mock.release.notify_one();
        h.clone().run_agent(run).await.unwrap();
        let requests = mock.requests.lock().await;
        assert_eq!(requests[0]["input"].as_array().unwrap().len(), 2);
        assert_eq!(
            requests[0]["input"][1]["content"].as_array().unwrap().len(),
            2
        );
        assert_eq!(requests[0]["input"][1]["content"][1]["text"], "do work");
        server.abort();
    }
    #[tokio::test]
    async fn spawn_returns_before_children_finish_and_delivers_one_batch_report() {
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
            arguments: json!({"tasks":["independent task one","independent task two"]}),
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
        assert!(h.store.queued(1).unwrap().is_empty());
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.store.queued(1).unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let reports = h.store.queued(1).unwrap();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].text.contains("first child report"));
        assert!(reports[0].text.contains("second child report"));
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
}
