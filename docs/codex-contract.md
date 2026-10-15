# Independent Codex subscription request adapter

Pantheon implements its own agent loop, tools, durable memory and HTTP transport.
This request adapter does not import Codex workspace crates, launch a Codex agent,
or delegate tool execution. Authentication/refresh is unchanged by this patch;
see [authentication](auth.md) for the existing separate login lifecycle.

## Mode selection

For `codex/<exact-slug>`, Pantheon retains `use_responses_lite` and
`supports_reasoning_summary_parameter` from the native model catalog. A discovered
catalog capability record takes precedence. When no such record exists (including
an old Pantheon catalog snapshot), the adapter reads only that exact model's
metadata from `models_cache.json` under configured `auth.codex_home`, `CODEX_HOME`,
or the documented default home. The cache reader is bounded to 4 MiB. It does not
read credentials when selecting the request mode, and it does not mutate the CLI
cache. There is no model-name heuristic or forced Sol/Luna mode.

Only an explicit Boolean `use_responses_lite: true` selects Lite. Missing, null,
malformed, unreadable, oversized, or unknown-model capability data selects the
conservative **standard** contract. An explicit discovered false/unknown record
is not overridden by the fallback CLI cache. Last-known catalog capabilities may
remain stale during discovery outages; they are not a server acceptance guarantee.
The selected mode is fixed for a logical request and all its existing retry
attempts. There is no rejection-triggered mode fallback or additional replay.
Missing reasoning-summary support defaults to true, matching native ModelInfo;
explicit false omits the summary parameter.

## Coordinated wire shape

Both subscription modes serialize a typed Rust request directly through reqwest:
`model` and `stream` precede potentially large `input` in the actual HTTP body.
This is not achieved by serializing through a sorted `serde_json::Value` map.
Base instructions are a developer-role message in input, rather than a top-level
`instructions` field. Nonempty instruction prefixes have deterministic `msg_`
UUIDv5 IDs scoped to Pantheon's stable routing affinity and instruction payload.
Locally assembled shorthand messages acquire `type: message` on the wire;
canonical history is not mutated. Subscription requests continue to set
`store: false`, request encrypted reasoning, and omit public-API output limits.

- **Standard:** no Lite header, top-level `tools` (including an empty array),
  `tool_choice: auto`, parallel tool calls enabled, no `reasoning.context`.
- **Lite:** `x-openai-internal-codex-responses-lite: true`, no top-level `tools`,
  parallel tool calls disabled, and `reasoning.context: all_turns` when reasoning
  is requested. Nonempty tools become a developer-role `additional_tools` input
  item before base instructions; empty tools produce no such item. Flat functions
  are coalesced in the default `functions` namespace; hosted built-ins stay
  outside that namespace. Additional-tools IDs are deterministic `at_` UUIDv5 IDs
  scoped to the affinity and serialized tool payload.

Hosted search retains its explicitly selected built-in tool, filters, include
fields, and search-specific tool choice. Public `openai/`, Anthropic and Z.ai
contracts do not use this subscription projection.

## Calls and continuation

The response's optional `namespace` is a separate part of function identity.
Only absent/null/empty/default `functions` namespaces dispatch to Pantheon's
local tool names. An unfamiliar or malformed namespace rejects the response
before tool execution; it is never silently stripped or mapped to a same-named
local tool. The existing `pantheon_web_search` alias still dispatches to Pantheon's
`web_search`, without rewriting the saved native item.

Complete native function items (including namespace, call ID and encrypted
fields) and reasoning remain in ephemeral continuation history. Function outputs
match the original `call_id`; no fabricated namespace is required on an output.
Images and tool-output images retain their content. Only unsupported public-API
cache-breakpoint metadata is removed from subscription content blocks.

## Evidence and limits

Source behavior is pinned to native Codex
[`4ca84a10696623fcd7c200db3df9c41671d3bad0`](https://github.com/openai/codex/tree/4ca84a10696623fcd7c200db3df9c41671d3bad0):

- [`core/src/client.rs` 934–1096](https://github.com/openai/codex/blob/4ca84a10696623fcd7c200db3df9c41671d3bad0/codex-rs/core/src/client.rs#L934-L1096): mode-dependent reasoning, prefix IDs, tools and parallel preference.
- [`core/src/client.rs` 2486–2493](https://github.com/openai/codex/blob/4ca84a10696623fcd7c200db3df9c41671d3bad0/codex-rs/core/src/client.rs#L2486-L2493): paired Lite mode header.
- [`tools/src/tool_spec.rs` 95–143](https://github.com/openai/codex/blob/4ca84a10696623fcd7c200db3df9c41671d3bad0/codex-rs/tools/src/tool_spec.rs#L95-L143): functions namespace coalescing.
- [`protocol/src/models.rs` 1107–1171](https://github.com/openai/codex/blob/4ca84a10696623fcd7c200db3df9c41671d3bad0/codex-rs/protocol/src/models.rs#L1107-L1171): optional namespace and call/output identity.
- [`codex-api/src/common.rs` 278–304](https://github.com/openai/codex/blob/4ca84a10696623fcd7c200db3df9c41671d3bad0/codex-rs/codex-api/src/common.rs#L278-L304): routing-first typed serialization.

Synthetic source-derived fixtures execute Pantheon's real Rust serializer, and
loopback tests cover the HTTP header/body path. They are not server acceptance
tests. Other independent clients still use older standard-style fields;
differences from native Codex do not establish that those requests are invalid.
This patch is compatibility alignment, **not proof of the historical provider
errors' cause**. No live inference, auth change, retry-policy change, deployment,
or capture enablement is implied by local source/test clearance.
