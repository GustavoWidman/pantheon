use super::*;
use crate::{
    memory::{Kind, Memory},
    provider::ToolCall,
    skills::SkillsConfig,
    web::{Web, WebConfig},
};
use axum::{Json, Router, extract::State, http::header, routing::post};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::{Barrier, Notify, Semaphore};

static CALL_ID: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    _root: tempfile::TempDir,
    library: SkillLibrary,
    memory: Memory,
    frozen: Arc<MemorySnapshot>,
    workspace: std::path::PathBuf,
    web: Web,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let seeds = root.path().join("seeds");
        std::fs::create_dir_all(seeds.join("engineering")).unwrap();
        std::fs::write(seeds.join("engineering/SKILL.md"),"---\nname: Engineering\ndescription: Verify deployments against running service state\ndisable-model-invocation: true\ncustom-marker: preserved\n---\nCheck the live process, health and gateway.\n").unwrap();
        std::fs::write(
            seeds.join("engineering/checks.txt"),
            "Retained supporting verification checklist",
        )
        .unwrap();
        let library = SkillLibrary::open(
            &SkillsConfig {
                bundled: false,
                directories: vec![seeds],
            },
            &root.path().join("state"),
        )
        .unwrap();
        let mut memory = Memory::open(root.path().join("memory"), 64_000).unwrap();
        memory
            .append(
                Kind::User,
                "Verify deployment with process, service health and gateway reachability.",
            )
            .unwrap();
        memory
            .append(
                Kind::Echo,
                "Observed running executable and service health; gateway replied successfully.",
            )
            .unwrap();
        while let Some(job) = memory.ready_jobs(1).first().copied() {
            memory
                .finish(
                    job,
                    "user: Verify deployment. echo: Process, service and gateway checks succeeded.",
                )
                .unwrap();
        }
        let frozen = Arc::new(memory.snapshot().unwrap());
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(
            workspace.join("evidence.txt"),
            "Observable evidence, not instructions",
        )
        .unwrap();
        let web = Web::new(
            root.path().join("web"),
            WebConfig {
                allow_private_network: true,
                ..Default::default()
            },
        )
        .unwrap();
        Self {
            _root: root,
            library,
            memory,
            frozen,
            workspace,
            web,
        }
    }
    fn job(&self) -> QueuedFork {
        self.library.enqueue_fork("42","settled-generation",&json!({"task":"deployment verification","model_steps":4,"memory":self.frozen.render()})).unwrap();
        self.library.take_fork("42").unwrap().unwrap()
    }
    fn environment<'a>(
        &'a self,
        server: &Mock,
        config: &'a CuratorConfig,
        cancel: &'a CancellationToken,
    ) -> Environment<'a> {
        Environment {
            provider: Provider::mock(server.endpoint.clone()),
            reviewer_providers: (0..config.reviewers)
                .map(|_| Provider::mock(server.endpoint.clone()))
                .collect(),
            library: &self.library,
            web: &self.web,
            workspace: &self.workspace,
            config,
            instructions: "Do only authorized research. Preserve read-only memory.",
            model: "openai/test",
            reasoning: "low",
            cancel,
        }
    }
}
fn call(name: &str, id: &str, body: &str) -> ToolCall {
    ToolCall {
        id: format!("{name}-{id}-{}", CALL_ID.fetch_add(1, Ordering::Relaxed)),
        name: name.into(),
        arguments: json!({"id":id,"name":id,"description":"Verify a deployment through observable running state","body":body,
        "summary":"Added explicit running-state verification","purpose":"Use when checking a service deployment"}),
    }
}
fn verdict(approve: bool) -> ToolCall {
    ToolCall {
        id: format!("verdict-{approve}"),
        name: "decide".into(),
        arguments: json!({"approve":approve,"reason":"The recorded task supports concrete deployment checks","scope_fit":if approve {"transferable"}else{"unsupported"},"evidence":"The settled task records process, health and gateway observations"}),
    }
}
fn response(calls: Vec<ToolCall>, input_tokens: u64) -> Value {
    let output: Vec<Value> = if calls.is_empty() {
        vec![
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Curation settled."}]}),
        ]
    } else {
        calls.into_iter().map(|c|json!({"type":"function_call","id":format!("item-{}",c.id),"call_id":c.id,"name":c.name,"arguments":c.arguments.to_string()})).collect()
    };
    json!({"status":"completed","output":output,"usage":{"input_tokens":input_tokens,"output_tokens":10}})
}
struct MockState {
    draft: Mutex<VecDeque<Value>>,
    verdicts: Mutex<VecDeque<Value>>,
    requests: Mutex<Vec<Value>>,
    draft_count: AtomicUsize,
    reviewers_running: AtomicUsize,
    max_reviewers_running: AtomicUsize,
    reviewer_barrier: Barrier,
    hold_reviews: AtomicBool,
    review_paused: Notify,
    review_release: Semaphore,
    pause_at: usize,
    paused: Notify,
    release: Notify,
}
struct Mock {
    endpoint: String,
    state: Arc<MockState>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn new(draft: Vec<Value>, verdicts: Vec<Value>, pause_at: usize) -> Self {
        let reviewers = verdicts.len();
        let state = Arc::new(MockState {
            draft: Mutex::new(draft.into()),
            verdicts: Mutex::new(verdicts.into()),
            requests: Mutex::new(vec![]),
            draft_count: AtomicUsize::new(0),
            reviewers_running: AtomicUsize::new(0),
            max_reviewers_running: AtomicUsize::new(0),
            reviewer_barrier: Barrier::new(reviewers.max(1)),
            hold_reviews: AtomicBool::new(false),
            review_paused: Notify::new(),
            review_release: Semaphore::new(0),
            pause_at,
            paused: Notify::new(),
            release: Notify::new(),
        });
        async fn serve(
            State(state): State<Arc<MockState>>,
            Json(body): Json<Value>,
        ) -> impl axum::response::IntoResponse {
            let review = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "decide");
            state.requests.lock().unwrap().push(body);
            let output = if review {
                let concurrent = state.reviewers_running.fetch_add(1, Ordering::SeqCst) + 1;
                state
                    .max_reviewers_running
                    .fetch_max(concurrent, Ordering::SeqCst);
                // Neither reviewer can return until both requests reached the server.
                tokio::time::timeout(Duration::from_secs(3), state.reviewer_barrier.wait())
                    .await
                    .unwrap();
                if state.hold_reviews.load(Ordering::SeqCst) {
                    state.review_paused.notify_one();
                    state.review_release.acquire().await.unwrap().forget();
                }
                let result = state
                    .verdicts
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected reviewer request");
                state.reviewers_running.fetch_sub(1, Ordering::SeqCst);
                result
            } else {
                let step = state.draft_count.fetch_add(1, Ordering::SeqCst) + 1;
                if step == state.pause_at {
                    state.paused.notify_one();
                    state.release.notified().await;
                }
                state
                    .draft
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected draft request")
            };
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                format!(
                    "event: response.completed\ndata: {}\n\n",
                    json!({"type":"response.completed","response":output})
                ),
            )
        }
        let app = Router::new()
            .route("/", post(serve))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            endpoint: format!("http://{address}/"),
            state,
            task,
        }
    }
    async fn wait_paused(&self) {
        tokio::time::timeout(Duration::from_secs(3), self.state.paused.notified())
            .await
            .unwrap();
    }
}
fn current_status(f: &Fixture) -> Value {
    f.library.channel_status("42").unwrap()["latest"].clone()
}
fn assert_inactive(f: &Fixture, status: &str) {
    assert_eq!(current_status(f)["status"], status);
    assert!(
        !f.library
            .snapshot()
            .unwrap()
            .entries
            .contains_key("deployment-checks")
    );
    assert!(f.library.notifications("42").unwrap().is_empty());
}

#[tokio::test]
async fn staged_changes_accumulate_until_settle_then_parallel_review_publishes_atomic_notes() {
    let f = Fixture::new();
    let job = f.job();
    let mut first_edit = call(
        "edit_skill",
        "engineering",
        "Check running process and health.",
    );
    first_edit.arguments["files"] =
        json!({"new-check.txt":"A newly staged supporting check survives later edits"});
    let server = Mock::new(
        vec![
            response(
                vec![
                    call("create_skill", "deployment-checks", "Check process first."),
                    first_edit,
                ],
                100,
            ),
            response(
                vec![
                    call(
                        "edit_skill",
                        "deployment-checks",
                        "Check process, service health and gateway.",
                    ),
                    call(
                        "edit_skill",
                        "engineering",
                        "Check executable, service health, gateway and report evidence.",
                    ),
                ],
                100,
            ),
            response(vec![], 100),
        ],
        vec![
            response(vec![verdict(true)], 100),
            response(vec![verdict(true)], 100),
        ],
        3,
    )
    .await;
    server.state.hold_reviews.store(true, Ordering::SeqCst);
    let config = CuratorConfig::default();
    let cancel = CancellationToken::new();
    let before_memory = f.memory.export_html();
    let execution = run(
        f.environment(&server, &config, &cancel),
        &job,
        f.frozen.clone(),
    );
    let inspect = async {
        server.wait_paused().await;
        let p = f.library.proposal("42", &job.id).unwrap();
        assert_eq!(p.changes.len(), 2);
        assert!(
            !f.library
                .snapshot()
                .unwrap()
                .entries
                .contains_key("deployment-checks")
        );
        assert_eq!(f.library.heads().unwrap()[0]["revision"], 1);
        assert!(f.library.notifications("42").unwrap().is_empty());
        let created = p
            .changes
            .iter()
            .find(|c| c.id == "deployment-checks")
            .unwrap();
        assert_eq!(created.expected_revision, 0);
        assert!(created.files["SKILL.md"].contains("service health and gateway"));
        server.state.release.notify_one();
        tokio::time::timeout(
            Duration::from_secs(3),
            server.state.review_paused.notified(),
        )
        .await
        .unwrap();
        let status = current_status(&f);
        assert_eq!(status["phase"], "review");
        assert_eq!(status["reviewers_spawned"], 2);
        assert_eq!(status["reviewers_running"], 2);
        assert_eq!(status["reviewers_finished"], 0);
        assert!(
            !f.library
                .snapshot()
                .unwrap()
                .entries
                .contains_key("deployment-checks")
        );
        assert!(f.library.notifications("42").unwrap().is_empty());
        server.state.review_release.add_permits(2);
    };
    let (result, ()) = tokio::join!(execution, inspect);
    result.unwrap();
    assert_eq!(current_status(&f)["status"], "published");
    assert_eq!(server.state.max_reviewers_running.load(Ordering::SeqCst), 2);
    assert_eq!(current_status(&f)["reviewers_spawned"], 2);
    assert_eq!(current_status(&f)["reviewers_finished"], 2);
    let engineering = f.library.revision_files("engineering", 2).unwrap();
    assert_eq!(
        engineering["checks.txt"],
        "Retained supporting verification checklist"
    );
    assert_eq!(
        engineering["new-check.txt"],
        "A newly staged supporting check survives later edits"
    );
    assert!(engineering["SKILL.md"].contains("disable-model-invocation: true"));
    assert!(engineering["SKILL.md"].contains("custom-marker: preserved"));
    let notes = f.library.notifications("42").unwrap();
    assert_eq!(notes.len(), 2);
    assert!(notes.iter().any(|n| n["kind"] == "add"));
    assert!(notes.iter().any(|n| n["kind"] == "modify"));
    assert!(
        notes
            .iter()
            .all(|n| n["summary"].as_str().is_some_and(|s| !s.is_empty())
                && n["purpose"].as_str().is_some_and(|s| !s.is_empty()))
    );
    assert_eq!(f.memory.export_html(), before_memory);
    let requests = server.state.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    for request in requests.iter() {
        assert_eq!(request["model"], "test");
        assert_eq!(request["reasoning"]["effort"], "low");
        assert!(request.to_string().contains("gateway reachability"));
        assert!(request.to_string().contains("Do only authorized research"));
    }
    let reviews: Vec<_> = requests
        .iter()
        .filter(|r| {
            r["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "decide")
        })
        .collect();
    assert_eq!(reviews.len(), 2);
    assert!(reviews.iter().all(|r| {
        r["input"][0]["content"]
            .as_str()
            .unwrap()
            .contains("private curator reviewer")
    }));
    assert!(reviews.iter().all(|r| {
        !r["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "edit_skill")
    }));
}

#[tokio::test]
async fn any_reviewer_denial_keeps_proposal_inactive() {
    let f = Fixture::new();
    let job = f.job();
    let server = Mock::new(
        vec![
            response(
                vec![call(
                    "create_skill",
                    "deployment-checks",
                    "Check observable state.",
                )],
                100,
            ),
            response(vec![], 100),
        ],
        vec![
            response(vec![verdict(true)], 100),
            response(vec![verdict(false)], 100),
        ],
        0,
    )
    .await;
    let config = CuratorConfig::default();
    let cancel = CancellationToken::new();
    run(
        f.environment(&server, &config, &cancel),
        &job,
        f.frozen.clone(),
    )
    .await
    .unwrap();
    assert_inactive(&f, "denied");
    assert_eq!(f.library.proposal("42", &job.id).unwrap().changes.len(), 1);
}

#[tokio::test]
async fn unsupported_write_spawn_and_unknown_tools_return_errors_without_effects() {
    let f = Fixture::new();
    let job = f.job();
    let calls = ["write", "shell", "spawn", "monitor", "missing_tool"]
        .map(|name| ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: json!({"path":"must-not-exist","text":"unexpected"}),
        })
        .to_vec();
    let server = Mock::new(vec![response(calls, 100), response(vec![], 100)], vec![], 0).await;
    let config = CuratorConfig::default();
    let cancel = CancellationToken::new();
    let before = f.memory.export_html();
    run(
        f.environment(&server, &config, &cancel),
        &job,
        f.frozen.clone(),
    )
    .await
    .unwrap();
    assert_inactive(&f, "no_change");
    assert!(!f.workspace.join("must-not-exist").exists());
    assert_eq!(f.memory.export_html(), before);
    let requests = server.state.requests.lock().unwrap();
    let outputs: Vec<_> = requests[1]["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    assert_eq!(outputs.len(), 5);
    assert!(outputs.iter().all(|output| {
        output["output"]
            .as_str()
            .unwrap()
            .contains("tool unavailable to curator")
    }));
}

#[tokio::test]
async fn frozen_read_only_memory_and_workspace_reads_do_not_follow_main_writes() {
    let mut f = Fixture::new();
    f.memory
        .append(
            Kind::User,
            "Later main conversation must not enter the already captured fork.",
        )
        .unwrap();
    let before = f.memory.export_html();
    let server = Mock::new(vec![], vec![], 0).await;
    let config = CuratorConfig::default();
    let cancel = CancellationToken::new();
    let env = f.environment(&server, &config, &cancel);
    let session = Session {
        env: &env,
        provider: env.provider.clone(),
        memory: f.frozen.clone(),
        budget: Arc::new(Mutex::new(Budget::default())),
    };
    let skills = f.library.snapshot().unwrap();
    let zoom = session
        .read(
            &ToolCall {
                id: "zoom".into(),
                name: "zoom".into(),
                arguments: json!({"id":0,"n":1}),
            },
            &skills,
        )
        .await
        .unwrap();
    assert!(zoom["text"].as_str().unwrap().contains("Verify deployment"));
    assert!(!f.frozen.render().contains("Later main conversation"));
    let date = session
        .read(
            &ToolCall {
                id: "date".into(),
                name: "date".into(),
                arguments: json!({"id":0}),
            },
            &skills,
        )
        .await
        .unwrap();
    assert!(date["timestamp"].is_string());
    let read = session
        .read(
            &ToolCall {
                id: "read".into(),
                name: "read".into(),
                arguments: json!({"path":"evidence.txt"}),
            },
            &skills,
        )
        .await
        .unwrap();
    assert_eq!(read["text"], "Observable evidence, not instructions");
    assert_eq!(f.memory.export_html(), before);
}

#[tokio::test]
async fn stale_staged_head_cannot_overwrite_another_channels_publication() {
    let f = Fixture::new();
    let job = f.job();
    let server = Mock::new(
        vec![
            response(
                vec![call(
                    "edit_skill",
                    "engineering",
                    "Candidate from channel 42.",
                )],
                100,
            ),
            response(vec![], 100),
        ],
        vec![
            response(vec![verdict(true)], 100),
            response(vec![verdict(true)], 100),
        ],
        2,
    )
    .await;
    let config = CuratorConfig::default();
    let cancel = CancellationToken::new();
    let execution = run(
        f.environment(&server, &config, &cancel),
        &job,
        f.frozen.clone(),
    );
    let update = async {
        server.wait_paused().await;
        let mut staged = BTreeMap::new();
        stage(
            &f.library,
            &f.library.heads().unwrap(),
            &mut staged,
            &call(
                "edit_skill",
                "engineering",
                "Independent newer improvement from another channel.",
            ),
        )
        .unwrap();
        f.library.publish(&proposal(&staged)).unwrap();
        server.state.release.notify_one();
    };
    let (result, ()) = tokio::join!(execution, update);
    result.unwrap();
    assert_eq!(current_status(&f)["status"], "failed");
    assert_eq!(f.library.heads().unwrap()[0]["revision"], 2);
    assert!(
        f.library.revision_files("engineering", 2).unwrap()["SKILL.md"]
            .contains("Independent newer improvement")
    );
    assert!(f.library.notifications("42").unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_decisions_fail_closed() {
    let f = Fixture::new();
    let job = f.job();
    let mut second = verdict(false);
    second.id = "second-decision".into();
    let server = Mock::new(
        vec![
            response(
                vec![call(
                    "create_skill",
                    "deployment-checks",
                    "Check observable state.",
                )],
                100,
            ),
            response(vec![], 100),
        ],
        vec![
            response(vec![verdict(true), second], 100),
            response(vec![verdict(true)], 100),
        ],
        0,
    )
    .await;
    let config = CuratorConfig::default();
    let cancel = CancellationToken::new();
    run(
        f.environment(&server, &config, &cancel),
        &job,
        f.frozen.clone(),
    )
    .await
    .unwrap();
    assert_inactive(&f, "failed");
    assert!(
        current_status(&f)["report"]
            .as_str()
            .unwrap()
            .contains("conflicting decisions")
    );
}

#[tokio::test]
async fn cancellation_retains_staged_proposal_without_publishing() {
    let f = Fixture::new();
    let job = f.job();
    let server = Mock::new(
        vec![
            response(
                vec![call(
                    "create_skill",
                    "deployment-checks",
                    "Check observable state.",
                )],
                100,
            ),
            response(vec![], 100),
        ],
        vec![],
        2,
    )
    .await;
    let config = CuratorConfig::default();
    let cancel = CancellationToken::new();
    let execution = run(
        f.environment(&server, &config, &cancel),
        &job,
        f.frozen.clone(),
    );
    let interrupt = async {
        server.wait_paused().await;
        cancel.cancel();
        server.state.release.notify_one();
    };
    let (result, ()) = tokio::join!(execution, interrupt);
    result.unwrap();
    assert_inactive(&f, "interrupted");
    assert_eq!(f.library.proposal("42", &job.id).unwrap().changes.len(), 1);
}

#[tokio::test]
async fn reported_token_budget_exhaustion_retains_draft_without_review_or_publication() {
    let f = Fixture::new();
    let job = f.job();
    let server = Mock::new(
        vec![
            response(
                vec![call(
                    "create_skill",
                    "deployment-checks",
                    "Check observable state.",
                )],
                100,
            ),
            response(vec![], 1500),
        ],
        vec![],
        0,
    )
    .await;
    let config = CuratorConfig {
        token_budget: 1000,
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    run(
        f.environment(&server, &config, &cancel),
        &job,
        f.frozen.clone(),
    )
    .await
    .unwrap();
    assert_inactive(&f, "failed");
    assert!(
        current_status(&f)["report"]
            .as_str()
            .unwrap()
            .contains("token budget exhausted")
    );
    assert_eq!(server.state.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn fixed_deadline_releases_a_hung_draft_without_publication() {
    let f = Fixture::new();
    let job = f.job();
    let server = Mock::new(
        vec![
            response(
                vec![call(
                    "create_skill",
                    "deployment-checks",
                    "Check observable state.",
                )],
                100,
            ),
            response(vec![], 100),
        ],
        vec![],
        2,
    )
    .await;
    let config = CuratorConfig {
        timeout_seconds: 10,
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    run(
        f.environment(&server, &config, &cancel),
        &job,
        f.frozen.clone(),
    )
    .await
    .unwrap();
    assert_inactive(&f, "failed");
    assert!(
        current_status(&f)["report"]
            .as_str()
            .unwrap()
            .contains("deadline exceeded")
    );
    assert_eq!(f.library.proposal("42", &job.id).unwrap().changes.len(), 1);
}
