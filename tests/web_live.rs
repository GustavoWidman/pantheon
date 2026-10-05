//! Opt-in check against the real Codex subscription backend; never runs in CI.
use pantheon::{
    provider::Provider,
    web::{Web, WebConfig},
};

#[tokio::test]
#[ignore = "requires a Codex ChatGPT login and uses subscription quota"]
async fn codex_search_and_fetch_with_existing_login() {
    let model = std::env::var("PANTHEON_TEST_CODEX_MODEL")
        .expect("set PANTHEON_TEST_CODEX_MODEL to an available codex/model");
    assert!(model.starts_with("codex/"));
    let result = Provider::new(120).unwrap().search(&model,
        "Find the official Rust standard library documentation for std::time::Instant. Search the web and return its documentation URL.",
        3, &["doc.rust-lang.org".into()]).await.unwrap();
    let sources = result["sources"].as_array().unwrap();
    let source = sources
        .iter()
        .find(|s| {
            s["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://doc.rust-lang.org/"))
        })
        .expect("official Rust documentation source");
    let directory = tempfile::tempdir().unwrap();
    let web = Web::new(directory.path().into(), WebConfig::default()).unwrap();
    let fetched = web
        .fetch(source["url"].as_str().unwrap(), 4000, false)
        .await
        .unwrap();
    assert!(fetched["text"].as_str().unwrap().contains("Instant"));
    drop(web);
    let web = Web::new(directory.path().into(), WebConfig::default()).unwrap();
    let cached = web
        .fetch(source["url"].as_str().unwrap(), 4000, false)
        .await
        .unwrap();
    assert_eq!(cached["cached"], true);
    assert_eq!(cached["text"], fetched["text"]);
    println!(
        "PASS: hosted Codex search, official-page fetch, persistent cache reuse; {} sources",
        sources.len()
    );
}

#[tokio::test]
#[ignore = "requires a Codex ChatGPT login and uses subscription quota"]
async fn codex_replays_client_tools_and_accepts_steering() {
    use serde_json::json;
    let model = std::env::var("PANTHEON_TEST_CODEX_MODEL").expect("set PANTHEON_TEST_CODEX_MODEL");
    let provider = Provider::new(120).unwrap();
    let tools = [
        json!({"name":"web_search","description":"Return a documentation URL","input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}}),
    ];
    let system = "Call web_search exactly once when asked to find documentation. Once its result arrives, answer using that result and follow the latest user instruction.";
    let mut history = vec![Provider::user(
        "openai",
        "Use web_search to find the official Rust Instant documentation.",
    )];
    let first = provider
        .step(&model, "low", system, &history, &tools)
        .await
        .unwrap();
    assert_eq!(first.calls.len(), 1);
    assert_eq!(first.calls[0].name, "web_search");
    Provider::append_response("openai", &mut history, &first);
    let native = history.clone();
    let url = "https://doc.rust-lang.org/std/time/struct.Instant.html";
    history.push(Provider::result(
        "openai",
        &first.calls[0],
        &json!({"url":url}).to_string(),
        false,
    ));
    history.push(Provider::user(
        "openai",
        "Reply with only the source URL, no other text.",
    ));
    let second = provider
        .step(&model, "low", system, &history, &tools)
        .await
        .unwrap();
    assert!(second.calls.is_empty());
    assert_eq!(&history[..native.len()], native.as_slice());
    let visible = second
        .texts
        .iter()
        .filter(|(_, reasoning)| !reasoning)
        .map(|(text, _)| text.as_str())
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(visible.trim(), url);
    println!("PASS: native Codex client-tool replay and post-tool steering");
}
