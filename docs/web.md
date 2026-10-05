# Web search and fetch

Workers have `web_search` and `web_fetch`; the coordinator root delegates research through `spawn`/`tell`. Setting `agent.coordinator_root=false` also exposes these tools on the root. Calls use the usual durable tool intents/results and cancellation path. Sources are untrusted data; workers are instructed to cite clickable URLs and ignore instructions embedded in search results or pages.

```json
{"query":"Rust Instant monotonic clock documentation","max_results":5,"domains":["doc.rust-lang.org"]}
```

`web_search` runs an isolated request containing only the provider's hosted search tool. It returns a concise answer, deduplicated source URLs and provider usage, without persisting reasoning or the helper's native transcript. `max_results` defaults to 5 and accepts 1–10; optional `domains` restricts search to host names. Calls that do not actually search, return an unexpected client tool, fail, or exceed the continuation budget produce an error rather than an offline guess.

By default the search model is the worker's current model. `web.search_model` can instead select a dedicated model, such as `codex/gpt-5.6-sol`. This lets Claude workers use Codex subscription-backed search. `codex/` uses a ChatGPT login; `openai/` uses an API key; `anthropic/` uses its API key and native Messages web search, including bounded `pause_turn` continuation. This configuration determines the billing source. A subscription login is not a Platform API key, and quotas/model/tool availability remain account-specific. Search has no local result cache: freshness-sensitive queries really reach the selected backend.

Pantheon's `web_search` remains a local tool in the worker loop, so it has a durable result and normal delivery behavior. On the Codex wire it is named `pantheon_web_search` to avoid collision with OpenAI's native hosted tool; the harness translates the call name for execution while retaining the native output item for replay.

```json
{"url":"https://doc.rust-lang.org/std/time/struct.Instant.html","max_chars":20000,"refresh":true}
```

`web_fetch` uses credential-free HTTP, without starting a browser. It supports HTML, text, JSON and XML. HTML extraction prefers an article/main region, preserves links and preformatted code, and omits script/style/navigation/hidden elements. The result includes the requested and final URLs, title, text, content type, `fetched_at`, `validated_at`, `cached`, `revalidated`, `truncated` and `untrusted` fields. These timestamps are Unix seconds. `max_chars` defaults to 20,000 and accepts 100–25,000. Network bodies default to a 2 MiB limit, including chunked transfers; at most ten redirects and one total fetch timeout are allowed.

HTTP connections are pooled. Concurrent fetches of the same URL coalesce, and extracted pages plus ETag/Last-Modified validators are saved with fsync and atomic replacement under `state/web/cache/`. A fresh cache entry survives daemon restart. The default TTL is five minutes, bounded further by server `max-age`/`no-cache`; `no-store` responses are not retained in the fetch cache. `refresh:true` forces conditional revalidation, including valid HTTP 304 reuse. The tool's normal durable trace still records its returned result. Fetch failures never masquerade as fresh cached data.

Fetch sends no provider token, browser cookie, arbitrary request header, or URL basic-auth credential. It disables environment proxies so DNS/address checks apply to the actual destination. HTTP(S) schemes, literal addresses, DNS results and every redirect are checked; private/loopback/link-local/CGNAT/reserved destinations are blocked by default. Set `web.allow_private_network=true` explicitly to allow trusted intranet fetching, including local/Tailscale addresses. This does not provide browser-session authentication.

Use the bundled browser for JavaScript-rendered or login-dependent content, and browser/shell for PDFs or other binary documents. Fetch does not decode non-identity HTTP encodings or execute page scripts. Configurable network/search timeouts and cache limits are shown in `pantheon.toml.example`.

For an opt-in live subscription check, outside CI:

```sh
PANTHEON_TEST_CODEX_MODEL=codex/gpt-5.6-sol cargo test --locked --test web_live -- --ignored --nocapture
```

These checks require the operator's existing file-based Codex login and use subscription quota. They search an official source, fetch it, verify cache reuse after restarting the fetch manager, and exercise native client-tool replay with post-tool steering. Automated CI uses local HTTP/RPC fixtures and never receives subscription credentials.
