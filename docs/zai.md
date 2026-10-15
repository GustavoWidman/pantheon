# Z.ai / GLM inference

Z.ai is a separate **opt-in** provider, not an alias for OpenAI Responses. It uses
exactly `https://api.z.ai/api/coding/paas/v4/chat/completions` with bearer
`ZAI_API_KEY`. There is no `latest` model, automatic provider substitution,
Responses shortcut, or fallback to the separately billed `/api/paas/v4` endpoint.
Keep the key in the service's protected environment file, never TOML/Nix store.
No key is bundled, and merely configuring this provider does not switch live
agents or establish account quota/entitlement.

```toml
[zai]
enabled = true                 # default false; key presence alone is insufficient
max_in_flight = 2              # shared across clones: main, workers, maintenance
max_output_tokens = 16384      # 1–131072; lower this for bounded tests/short tasks

# Optional main-agent selection. Otherwise select zai/model only when spawning
# a worker or via /model; keep existing main and compactor settings unchanged.
# [agent]
# model = "zai/glm-5.3"
# reasoning = "high"

# If the config main model is GLM and the curator is enabled, pin it explicitly:
# [curator]
# model = "codex/<your-existing-maintenance-model>"
# Or explicitly choose a supported zai/model for curator inference.

[web]
# Retain an already authorized, search-capable provider and its credentials.
# Example only: use an exact ID from the configured account's catalog.
# search_model = "codex/<your-search-capable-model>"
```

The current explicit allowlist is `zai/glm-5.3` (text-only) and
`zai/glm-5.3-flash` (native images). Older IDs auto-route upstream and are not
advertised as pinned older models; FlashX is not a Coding Plan model. Model
suggestions are labeled **static documentation**, not live account discovery.
Both expose `low`, `high`, `max` reasoning, default `max`. Unsupported levels
(including `none`/`medium`) fail before a request; a UI model switch chooses a
compatible advertised default. Thinking is enabled and preserved explicitly.
There is no fabricated mapping from OpenAI's effort enum and no unsupported
`parallel_tool_calls` parameter. Multiple returned tool calls are supported.

## Protocol, privacy and lifecycle

Tools use nested Chat Completions functions and `tool_choice:auto`.
`stream:true` / `tool_stream:true` assemble text, tool argument fragments keyed by
index, and complete `reasoning_content` without alteration. The decoder requires
a terminal reason **and** `[DONE]`; malformed/missing IDs, duplicate IDs,
non-object/incomplete JSON, missing indices, refusals and truncation fail closed.
No call executes until the entire response and tool batch validate. Tool results
match IDs and all results precede the next user/assistant message. Usage retains
raw fields plus normalized input/output/cache counters for Pantheon's telemetry.

GLM reasoning is kept verbatim in **the active ephemeral tool-loop transcript**,
not exposed as response text, chat/subagent history, summaries, log payloads, or
Discord output—even when `show_reasoning=true`. Like existing providers, a fresh
or resumed worker turn constructs a new native transcript from durable visible
memory/inbox. Native reasoning is deliberately **not checkpointed to disk**;
this adapter does not add resumable provider-native sessions. Interrupted calls
are not turned into executable tool results. Existing durable job/inbox state
continues to determine work on resume rather than replaying native tool calls.

Model settings are pinned for an active turn; main-agent provider switches apply
only to fresh turns. Request validation rejects foreign Responses items,
encrypted reasoning, Anthropic blocks and mismatched tool-result history rather
than guessing a translation. It does not transcode an active native transcript.

`max_in_flight` (1–32) is a harness limit, **not** a claimed plan concurrency
limit. Clones share one cancellation-safe semaphore, including the bounded retry
sequence. Existing `agent.request_max_attempts` / `request_backoff_seconds` and
Retry-After handling apply to HTTP 429/5xx and classified transient failures.
Cancellation drops request/wait/backoff futures and releases admission. Terminal
validation errors do not dispatch tools. HTTP/provider errors expose only safe
status/classification metadata, not prompts, provider bodies, headers, or keys.

## Attachments and web tools

Capabilities are exact model-specific, not inferred from `glm` in a name.

| Input | glm-5.3 | glm-5.3-flash |
| --- | --- | --- |
| PNG/JPEG/WebP/static GIF | explicit unavailable-vision notice; original preserved | labeled inline `image_url` data input |
| UTF-8 text/JSON read | bounded original text | bounded original text |
| PDF open/read | one page's extracted text **only**, with explicit visual limitations | one page's extracted text and rendered image |
| Other documents/video/audio/binary | original path + explicit unsupported notice | original path + explicit unsupported notice |

This release intentionally does **not** claim Flash native-file/video parity:
those APIs use different schemas/limits. PDF extraction is not visual analysis;
diagrams, handwriting, layout, or image-only pages cannot be inferred from a
text extract, and an empty extract does not prove an empty page. Text-only PDF
reads skip unnecessary image rendering. Failed/over-budget native media produces
a notice, not silently discarded evidence. Incoming attachments, reopened
originals, `read`, steering, reviewer input and browser screenshots use the same
capability rules. Images are appended after the complete tool-result batch.
Z.ai media use a conservative harness attachment/input allowance of 32 MB and
20 images (0 for text-only), not a vendor-account quota claim or a complete
serialized-request cap: system/tools and model token-context fullness are not
enforced by this media allowance.

MCP currently returns **textual JSON** for every provider; its encoded media and
retained previews are not native perception. GLM MCP results carry an explicit
notice preserving original result data/IDs and `isError`. A configured analysis
MCP can return its own textual analysis, but this adapter never secretly invokes
it or any other vision provider. Select a capable worker/model or an explicit
analysis tool when visual evidence is required.

`web_fetch` is unchanged: independent credential-free HTTP extraction, its
existing URL safety/cache/cancellation behavior, and external-data warnings.
Implicit maintenance routing is not changed by selecting GLM for the main agent.
With no explicit curator model, a channel switched to GLM keeps the configured
non-GLM main model for curator inference/review. If the config main model itself
is GLM, configuration validation requires an explicit `curator.model` or disabled
curator. Explicit `curator.model = "zai/glm-5.3"` or `/model kind:curator model:none`
is intentional opt-in; the latter inherits the selected main model. Picker,
model/reasoning commands and actual jobs use the same rule. Compaction remains
on its existing separate `agent.compactor_model` route.

`web_search` stays a separate hosted-search request in roots, workers **and curator
research/review**. Configure `web.search_model`
to an existing authorized OpenAI/Codex/Anthropic search-capable model. If search
would inherit GLM, it fails clearly **before network access** rather than
inventing a search answer or sending GLM an unsupported server tool.

## Validation

Routine tests use local synthetic HTTP/SSE fixtures only; no key or paid calls
belong in CI. Coverage includes fragmented Unicode, mixed text/tools, multiple
calls and continuation, malformed/truncated batches, EOF, errors, cancellation,
shared admission, provider-switch guards, exact capabilities and PDF fallback.
Authorized live QA is separate evidence; a passing request is not deployment or
proof of every account/model/long-task boundary. No Responses, native video/file,
automatic vision routing, live-service switch or native reasoning checkpointing
is implemented here.

Primary protocol/capability sources:
- [Current Coding models](https://docs.z.ai/devpack/latest-model)
- [GLM-5.3](https://docs.z.ai/guides/llm/glm-5.3)
- [GLM-5.3-Flash](https://docs.z.ai/guides/vlm/glm-5.3-flash)
- [Thinking state](https://docs.z.ai/guides/capabilities/thinking-mode)
- [Streaming tools](https://docs.z.ai/guides/overview/migrate-to-glm-new)
- [Function calling](https://docs.z.ai/guides/capabilities/function-calling)
- [Chat API](https://docs.z.ai/api-reference/llm/chat-completion)
