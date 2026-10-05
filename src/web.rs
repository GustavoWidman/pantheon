//! Credential-free HTTP fetching with pooled connections and durable validators.
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use reqwest::{
    Url,
    dns::{Addrs, Name, Resolve, Resolving},
};
use scraper::{ElementRef, Html, Selector, node::Node};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::Mutex;

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub search_model: Option<String>,
    pub request_timeout_seconds: u64,
    pub search_timeout_seconds: u64,
    pub cache_ttl_seconds: u64,
    pub max_response_bytes: usize,
    pub allow_private_network: bool,
}
impl Default for WebConfig {
    fn default() -> Self {
        Self {
            search_model: None,
            request_timeout_seconds: 45,
            search_timeout_seconds: 120,
            cache_ttl_seconds: 300,
            max_response_bytes: 2 * 1024 * 1024,
            allow_private_network: false,
        }
    }
}
impl WebConfig {
    pub fn validate(&self) -> Result<()> {
        if let Some(model) = &self.search_model {
            crate::provider::model_parts(model)?;
        }
        ensure!(
            self.request_timeout_seconds > 0 && self.search_timeout_seconds > 0,
            "web timeouts must be positive"
        );
        ensure!(
            (1024..=16 * 1024 * 1024).contains(&self.max_response_bytes),
            "web.max_response_bytes must be 1 KiB–16 MiB"
        );
        ensure!(
            self.cache_ttl_seconds <= 86_400,
            "web cache TTL must be at most one day"
        );
        Ok(())
    }
}
pub struct Web {
    config: WebConfig,
    http: reqwest::Client,
    cache_dir: PathBuf,
    locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
}
#[derive(Clone, Deserialize, Serialize)]
struct CachedPage {
    requested_url: String,
    url: String,
    title: String,
    text: String,
    content_type: String,
    etag: Option<String>,
    last_modified: Option<String>,
    fetched_at: i64,
    validated_at: i64,
    fresh_for_seconds: u64,
    truncated: bool,
}
impl Web {
    pub fn new(cache_dir: PathBuf, config: WebConfig) -> Result<Self> {
        config.validate()?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_seconds))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(PublicDns {
                allow_private: config.allow_private_network,
            }))
            .user_agent(concat!("Pantheon/", env!("CARGO_PKG_VERSION")))
            .build()?;
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            config,
            http,
            cache_dir,
            locks: Mutex::default(),
        })
    }
    pub async fn fetch(&self, url: &str, max_chars: usize, refresh: bool) -> Result<Value> {
        ensure!(
            (100..=25_000).contains(&max_chars),
            "max_chars must be 100–25000"
        );
        let mut url = Url::parse(url).context("web_fetch requires an absolute HTTP(S) URL")?;
        url.set_fragment(None);
        validate_url(&url, self.config.allow_private_network)?;
        tokio::time::timeout(
            Duration::from_secs(self.config.request_timeout_seconds),
            self.fetch_inner(url, max_chars, refresh),
        )
        .await
        .context("web fetch timed out")?
    }
    async fn fetch_inner(&self, requested: Url, max_chars: usize, refresh: bool) -> Result<Value> {
        let key = hex::encode(Sha256::digest(requested.as_str().as_bytes()));
        let lock = {
            let mut locks = self.locks.lock().await;
            if locks.len() > 1024 {
                locks.retain(|_, lock| lock.strong_count() > 0);
            }
            if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(key.clone(), Arc::downgrade(&lock));
                lock
            }
        };
        let _guard = lock.lock().await;
        let path = self.cache_dir.join(format!("{key}.json"));
        let old = read_cache(&path)
            .await
            .filter(|page| page.requested_url == requested.as_str());
        let now = crate::store::now();
        if !refresh
            && self.config.cache_ttl_seconds > 0
            && let Some(page) = &old
            && now >= page.validated_at
            && now - page.validated_at
                < self.config.cache_ttl_seconds.min(page.fresh_for_seconds) as i64
        {
            validate_url(&Url::parse(&page.url)?, self.config.allow_private_network)?;
            return Ok(render(page, max_chars, true, false));
        }
        let mut url = requested.clone();
        for redirects in 0..=10 {
            validate_url(&url, self.config.allow_private_network)?;
            let mut request = self.http.get(url.clone()).header("Accept-Encoding", "identity")
                .header("Accept", "text/html,application/xhtml+xml,text/plain,application/json,application/xml;q=0.9");
            if let Some(page) = &old
                && page.url == url.as_str()
            {
                if let Some(etag) = &page.etag {
                    request = request.header("If-None-Match", etag);
                }
                if let Some(modified) = &page.last_modified {
                    request = request.header("If-Modified-Since", modified);
                }
            }
            // reqwest transport errors include URLs; strip them before durable logging.
            let response = request
                .send()
                .await
                .map_err(|error| error.without_url())
                .context("web fetch transport failed")?;
            let status = response.status();
            let cache_control = response
                .headers()
                .get(reqwest::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok());
            let (no_store, fresh_for_seconds) =
                cache_policy(cache_control, self.config.cache_ttl_seconds);
            if status == reqwest::StatusCode::NOT_MODIFIED {
                let mut page = old
                    .clone()
                    .filter(|page| page.url == url.as_str())
                    .context("unexpected HTTP 304 without a matching cached page")?;
                page.validated_at = crate::store::now();
                if cache_control.is_some() {
                    page.fresh_for_seconds = fresh_for_seconds;
                }
                let value = render(&page, max_chars, true, true);
                if no_store {
                    let _ = tokio::fs::remove_file(&path).await;
                } else {
                    write_cache(path, page).await?;
                }
                return Ok(value);
            }
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                ensure!(redirects < 10, "web fetch exceeded ten redirects");
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .context("web redirect has no Location")?
                    .to_str()?;
                url = url.join(location).context("invalid web redirect URL")?;
                url.set_fragment(None);
                continue;
            }
            ensure!(status.is_success(), "web fetch returned HTTP {status}");
            ensure!(
                response
                    .content_length()
                    .is_none_or(|n| n <= self.config.max_response_bytes as u64),
                "web response exceeds byte limit"
            );
            let headers = response.headers();
            let content_type = headers
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/plain")
                .to_owned();
            let kind = content_type
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            ensure!(
                kind.starts_with("text/")
                    || [
                        "application/json",
                        "application/xml",
                        "application/xhtml+xml"
                    ]
                    .contains(&kind.as_str())
                    || kind.ends_with("+json")
                    || kind.ends_with("+xml"),
                "unsupported web content type; use browser or shell for binary documents"
            );
            ensure!(
                headers
                    .get(reqwest::header::CONTENT_ENCODING)
                    .is_none_or(|v| v == "identity"),
                "server ignored identity encoding; use the browser for this page"
            );
            let etag = headers
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let last_modified = headers
                .get(reqwest::header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk
                    .map_err(|error| error.without_url())
                    .context("web response interrupted")?;
                ensure!(
                    bytes.len() + chunk.len() <= self.config.max_response_bytes,
                    "web response exceeds byte limit"
                );
                bytes.extend_from_slice(&chunk);
            }
            let source = String::from_utf8_lossy(&bytes).into_owned();
            let base = url.clone();
            let html = kind == "text/html" || kind == "application/xhtml+xml";
            let (title, text) = tokio::task::spawn_blocking(move || {
                if html {
                    extract_html(&source, &base)
                } else {
                    (String::new(), source)
                }
            })
            .await
            .context("web text extraction failed")?;
            let truncated = text.chars().count() > 100_000;
            let text = text.chars().take(100_000).collect();
            let now = crate::store::now();
            let page = CachedPage {
                requested_url: requested.to_string(),
                url: url.to_string(),
                title,
                text,
                content_type,
                etag,
                last_modified,
                fetched_at: now,
                validated_at: now,
                fresh_for_seconds,
                truncated,
            };
            let value = render(&page, max_chars, false, false);
            if no_store {
                let _ = tokio::fs::remove_file(&path).await;
            } else {
                write_cache(path, page).await?;
            }
            return Ok(value);
        }
        unreachable!();
    }
}
fn cache_policy(header: Option<&str>, ttl: u64) -> (bool, u64) {
    let mut no_store = false;
    let mut fresh = ttl;
    for directive in header
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
    {
        if directive == "no-store" {
            no_store = true;
            fresh = 0;
        }
        if directive == "no-cache" {
            fresh = 0;
        }
        if let Some(max_age) = directive
            .strip_prefix("max-age=")
            .and_then(|s| s.trim_matches('"').parse::<u64>().ok())
        {
            fresh = fresh.min(max_age);
        }
    }
    (no_store, fresh)
}
fn render(page: &CachedPage, max_chars: usize, cached: bool, revalidated: bool) -> Value {
    let mut result = json!({"requested_url":page.requested_url,"url":page.url,"title":page.title,
        "content_type":page.content_type.chars().take(500).collect::<String>(),"text":"",
        "truncated":false,
        "cached":cached,"revalidated":revalidated,"fetched_at":page.fetched_at,"validated_at":page.validated_at,
        "untrusted":true});
    // Stay below the harness's 30k-character cap even for JSON/quoted text.
    // Truncate the field rather than allowing that cap to break the JSON object.
    let mut budget = 28_000usize.saturating_sub(result.to_string().chars().count());
    let mut text = String::new();
    let mut characters = 0;
    for character in page.text.chars().take(max_chars) {
        let cost = match character {
            '"' | '\\' | '\n' | '\r' | '\t' => 2,
            c if c.is_control() => 6,
            _ => 1,
        };
        if budget < cost {
            break;
        }
        text.push(character);
        characters += 1;
        budget -= cost;
    }
    result["truncated"] = json!(page.truncated || page.text.chars().count() > characters);
    result["text"] = json!(text);
    result
}
async fn read_cache(path: &Path) -> Option<CachedPage> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes).await.ok()?;
    if bytes.len() > 1_048_576 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
async fn write_cache(path: PathBuf, page: CachedPage) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            serde_json::to_writer(&mut file, &page)?;
            file.flush()?;
            file.sync_all()?;
            std::fs::rename(&tmp, &path)?;
            std::fs::File::open(path.parent().unwrap())?.sync_all()?;
            Ok::<_, anyhow::Error>(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    })
    .await
    .context("web cache writer failed")?
}
fn extract_html(source: &str, base: &Url) -> (String, String) {
    let document = Html::parse_document(source);
    let title = document
        .select(&Selector::parse("title").unwrap())
        .next()
        .map(|e| {
            e.text()
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .chars()
                .take(500)
                .collect()
        })
        .unwrap_or_default();
    let main = document
        .select(&Selector::parse("article, main, [role=main]").unwrap())
        .next()
        .or_else(|| document.select(&Selector::parse("body").unwrap()).next());
    let mut text = String::new();
    if let Some(main) = main {
        walk(main, base, &mut text, false, 0);
    }
    let text = text
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    (title, text.trim().to_owned())
}
fn walk(element: ElementRef<'_>, base: &Url, out: &mut String, pre: bool, depth: usize) {
    if depth >= 128 || out.len() > 500_000 {
        return;
    }
    let value = element.value();
    let tag = value.name();
    if [
        "script", "style", "noscript", "template", "nav", "aside", "footer", "svg", "canvas",
        "iframe", "input", "button",
    ]
    .contains(&tag)
        || value.attr("hidden").is_some()
        || value.attr("aria-hidden") == Some("true")
    {
        return;
    }
    let block = [
        "p",
        "div",
        "section",
        "article",
        "main",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "li",
        "tr",
        "pre",
        "blockquote",
        "br",
    ]
    .contains(&tag);
    if block && !out.ends_with('\n') {
        out.push('\n');
    }
    if tag == "li" {
        out.push_str("- ");
    }
    let link = if tag == "a" {
        value
            .attr("href")
            .and_then(|href| base.join(href).ok())
            .filter(|u| {
                ["http", "https"].contains(&u.scheme())
                    && u.username().is_empty()
                    && u.password().is_none()
            })
    } else {
        None
    };
    if link.is_some() {
        out.push('[');
    }
    for child in element.children() {
        match child.value() {
            Node::Text(text) => {
                if pre || tag == "pre" {
                    out.push_str(&text.text);
                } else {
                    let value = text.text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !value.is_empty() {
                        out.push_str(&value);
                        out.push(' ');
                    }
                }
            }
            Node::Element(_) => {
                if let Some(child) = ElementRef::wrap(child) {
                    walk(child, base, out, pre || tag == "pre", depth + 1);
                }
            }
            _ => {}
        }
    }
    if let Some(link) = link {
        out.push_str(&format!("]({link}) "));
    }
    if tag == "td" || tag == "th" {
        out.push_str(" | ");
    }
    if block && !out.ends_with('\n') {
        out.push('\n');
    }
}
fn validate_url(url: &Url, allow_private: bool) -> Result<()> {
    ensure!(
        ["http", "https"].contains(&url.scheme()),
        "web fetch supports only HTTP(S)"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "web fetch does not accept URL credentials"
    );
    ensure!(url.as_str().len() <= 8192, "web URL is too long");
    let host = url.host_str().context("web URL has no host")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        ensure!(
            allow_private || public_ip(ip),
            "web fetch blocks private or reserved addresses"
        );
    } else {
        let host = host.trim_end_matches('.');
        ensure!(
            allow_private
                || !host.eq_ignore_ascii_case("localhost") && !host.ends_with(".localhost"),
            "web fetch blocks localhost"
        );
    }
    Ok(())
}
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || a == 100 && (64..=127).contains(&b)
                || a == 169 && b == 254
                || a == 172 && (16..=31).contains(&b)
                || a == 192 && (b == 168 || b == 0 && (c == 0 || c == 2))
                || a == 198 && (b == 18 || b == 19 || b == 51 && c == 100)
                || a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(v4.into());
            }
            let words = ip.segments();
            (words[0] & 0xe000) == 0x2000
                && !(words[0] == 0x2001 && (words[1] == 0xdb8 || words[1] == 0))
                && words[0] != 0x2002
        }
    }
}
struct PublicDns {
    allow_private: bool,
}
impl Resolve for PublicDns {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let allow = self.allow_private;
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .take(64)
                .collect::<Vec<_>>();
            if addresses.is_empty() || !allow && addresses.iter().any(|a| !public_ip(a.ip())) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "web DNS resolved to a private or reserved address",
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn html_extracts_article_links_and_code_without_scripts_or_navigation() {
        let source = "<title>Docs</title><nav>noise</nav><main><h1>API</h1><script>secret()</script><p>Read <a href='/guide'>the guide</a>.</p><pre>a\n  b</pre><span hidden>hidden</span></main>";
        let (title, text) = extract_html(source, &Url::parse("https://example.com/page").unwrap());
        assert_eq!(title, "Docs");
        assert!(text.contains("https://example.com/guide") && text.contains("a\n  b"));
        for absent in ["noise", "secret", "hidden"] {
            assert!(!text.contains(absent));
        }
    }
    #[test]
    fn blocks_literal_metadata_loopback_tailscale_and_mapped_private_addresses() {
        for address in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "100.100.100.100",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "2001:db8::1",
        ] {
            assert!(!public_ip(address.parse().unwrap()), "{address}");
        }
        for address in ["8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_ip(address.parse().unwrap()));
        }
        for url in [
            "file:///etc/passwd",
            "https://user:pass@example.com",
            "http://127.0.0.1/",
            "http://localhost/",
            "http://[::1]/",
        ] {
            assert!(validate_url(&Url::parse(url).unwrap(), false).is_err());
        }
    }
    #[tokio::test]
    async fn coalesces_fetches_persists_cache_and_revalidates_after_restart() {
        use axum::{
            Router,
            extract::State,
            http::{HeaderMap, StatusCode},
            response::{IntoResponse, Response},
            routing::get,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        async fn page(State(hits): State<Arc<AtomicUsize>>, headers: HeaderMap) -> Response {
            hits.fetch_add(1, Ordering::SeqCst);
            if headers.get("If-None-Match").is_some_and(|v| v == "\"v1\"") {
                return StatusCode::NOT_MODIFIED.into_response();
            }
            ([("Content-Type", "text/html"),("ETag", "\"v1\"")], "<title>Source</title><article><p>Durable content</p><a href='/next'>Next</a></article>").into_response()
        }
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new().route("/", get(page)).with_state(hits.clone()),
            )
            .into_future(),
        );
        let directory = tempfile::tempdir().unwrap();
        let config = WebConfig {
            allow_private_network: true,
            ..Default::default()
        };
        let web = Web::new(directory.path().into(), config.clone()).unwrap();
        let url = format!("http://{address}/");
        let values =
            futures_util::future::join_all((0..8).map(|_| web.fetch(&url, 1000, false))).await;
        assert!(values.iter().all(|v| {
            v.as_ref().unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("Durable content")
        }));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        drop(web);
        let web = Web::new(directory.path().into(), config).unwrap();
        assert_eq!(web.fetch(&url, 1000, false).await.unwrap()["cached"], true);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let revalidated = web.fetch(&url, 1000, true).await.unwrap();
        assert_eq!(revalidated["revalidated"], true);
        assert!(
            revalidated["text"]
                .as_str()
                .unwrap()
                .contains("Durable content")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        server.abort();
    }
    #[tokio::test]
    async fn rejects_chunked_overflow_and_private_redirects_and_honors_no_store() {
        use axum::{
            Router,
            body::{Body, Bytes},
            http::StatusCode,
            response::IntoResponse,
            routing::get,
        };
        use std::future::IntoFuture;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .route(
                        "/large",
                        get(|| async {
                            Body::from_stream(futures_util::stream::iter([
                                Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 700])),
                                Ok(Bytes::from(vec![b'x'; 700])),
                            ]))
                        }),
                    )
                    .route(
                        "/redirect",
                        get(|| async {
                            (
                                StatusCode::FOUND,
                                [("Location", "http://169.254.169.254/latest/meta-data/")],
                            )
                                .into_response()
                        }),
                    )
                    .route(
                        "/volatile",
                        get(|| async {
                            (
                                [
                                    ("Cache-Control", "no-store"),
                                    ("Content-Type", "text/plain"),
                                ],
                                "ephemeral",
                            )
                        }),
                    ),
            )
            .into_future(),
        );
        let directory = tempfile::tempdir().unwrap();
        let mut web = Web::new(
            directory.path().into(),
            WebConfig {
                max_response_bytes: 1024,
                ..Default::default()
            },
        )
        .unwrap();
        // Pin a test hostname to the fixture; production uses the filtering resolver.
        web.http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve("public.test", address)
            .build()
            .unwrap();
        let url = format!("http://public.test:{}/", address.port());
        assert!(
            web.fetch(&format!("{url}large"), 1000, false)
                .await
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        assert!(
            web.fetch(&format!("{url}redirect"), 1000, false)
                .await
                .unwrap_err()
                .to_string()
                .contains("private")
        );
        assert_eq!(
            web.fetch(&format!("{url}volatile"), 1000, false)
                .await
                .unwrap()["cached"],
            false
        );
        assert_eq!(
            web.fetch(&format!("{url}volatile"), 1000, false)
                .await
                .unwrap()["cached"],
            false
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        assert!(
            PublicDns {
                allow_private: false
            }
            .resolve("localhost".parse().unwrap())
            .await
            .is_err()
        );
        server.abort();
    }
}
