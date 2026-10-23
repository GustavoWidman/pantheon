# Native memory and provider parity acceptance

Reference: VictorTaelin/uniichat `0500bf6a8ea4bf0ce047488562022b1b6532cc30`.
The target is memory behavior with Pantheon integrations and durability retained.
This matrix names deterministic test coverage and boundaries; it does not imply
identical model outputs, provider cache-hit rates or migration/deployment approval.
The D1-D12 cases below cover native provider ownership independently of the
memory engine and root/worker request construction.

## Scope and carry-forward mechanisms

| Domain | Implemented behavior and test boundary |
|---|---|
| Original pages | Native 30,000 JavaScript UTF16-unit paging, newline/scalar handling, empty text, 20-image placement, ECMAScript trim and UTF8 budgets. `memory_tests` compares actual pinned `Clip`/`Memory` fixtures. Rust originals are valid Unicode; provider observations join UTF16 before well-formed UTF8 conversion. |
| Binary tree | Native positional power-of-two spans, immutable published IDs, exact aligned zoom/date and read-on-demand original pages. No legacy resolver, aliases or tree fallback. |
| Scheduler | Independent main 128,000-byte and compactor 32,000-byte views, age/tie ordering, half-budget batch fitting, staged publication, earliest unfinished frontier, frozen reservation contexts, bounded 64-window scheduling and arbitrary completion ordering. |
| Restart | Saved native partitions/prefixes; unfinished trailing groups remain hidden and may reuse only unpublished positions. No unrelated closing append admits an abandoned prefix. No historical tools or provider-native executable state are recovered. |
| Storage | One writer, fsynced journals, atomic synced checkpoints, fail-closed poisoned writer, schema preflight before mutation, lazy indexed originals/tree, bounded resident bodies and private snapshots. |
| Summaries | Five answered length tries; shortest nonempty UTF8 answer, first tie, oversize retained rather than clipped. Only latest correction plus original context; refusal/empty fallback and patient retry ownership remain distinct from transport retries. |
| Native public transport | OpenAI/Anthropic eight-attempt native retry/event-idle lifecycle and Anthropic pause continuation remain implemented. Do not infer a four-attempt native defect from the separate generic retry helper. Existing tests cover terminal SSE, idle refresh and request cancellation. |
| Cache | Four-line blocks and supported actual marks; explicit durable private routing affinity. Longest-prefix/one-extension coordination, TTL/LRU and leader/follower failure/cancellation behavior. No Codex/Z.ai Anthropic-cache or real cache-hit claim. |
| Root request construction | Actual `Chat.call` oracle compared with the production channel actor: canonical shared memory prompt, `Turn:` header, marked raw suffix and unmarked agent status/notices. `actual_upstream_chat_call_reaches_harness_provider_with_its_turn_header`. |
| Worker memory | Canonical subagent/view instructions without Compactions; full private original log with logical entry numbering, named paged recall, new settled view for idle `tell`, retained view for background continuation, empty view on cold restore. `actual_upstream_worker_prompt_and_log_request_boundaries`, `native_worker_recall_tell_and_cold_resume_use_original_private_log`. |
| Attribution and retrieval | Trusted worker reports render as `work`; user text cannot impersonate that role. Missing zoom/date return reference text; UTF16 cursor units survive native and Codex writers. `actual_upstream_work_role_and_utf16_cursor_edges_survive_reopen`, `owned_recall_cursor_preserves_utf16_through_native_and_subscription_writers`. |
| Product integration | Discord channel ownership, attachments and execution tools remain Pantheon-owned. Numeric worker recall reads the live root; maintenance readers use frozen snapshots. Durable journals, locks and private provider affinity remain. |

These rows describe tested behavior, not a percentage or all-product equivalence
claim. The engine oracle also compares randomized completion/fit schedules.

## Finite successor acceptance cases

The following test groups state what must be observed. Passing reference fixtures
alone do not establish native execution. A/B/C denote separate boundaries:
A is pre-RPC Use classification; B is actual outgoing RPC bytes and headers;
C is the actual next provider HTTP body. Reopen assertions use complete ordered
original `(kind, body)` values through the unchanged native reader.

| Gate | Required native evidence / representative tests |
|---|---|
| D1 Values | 36 initial/delta corpus cases, absent vs null, malformed/empty arguments, arrays/primitives, raw unpaired UTF16 in partial JSON keys/values, split pairs and invalid syntax. `owned_d1_d2_36_corpus_uses_rpc_http_stdio_and_next_http_bytes`, `owned_utf16_raw_corpus_both_vendors_use_and_next_request`, `owned_input_absence_null_malformed_array_primitive_and_normalization`. |
| D2 A/B/C | Completed owned inputs retain insertion/index/duplicate ordering, binary64 rounding/nonfinite children and escaped UTF16. Actual authorized rmcp HTTP and stdio writers, not serializer-only tests; actual next provider body preserves native blocks and raw OpenAI argument strings. |
| D2-H Headers | Actual negotiated modern method/name/annotated scalar headers, version override through its real metadata owner, cached/uncached schemas, body/header agreement, auth/session/Accept/custom ownership. `owned_modern_mcp_actual_body_headers_versions_annotations_and_utf16_limit`. The annotated scalar lone-UTF16 boundary below remains a named open decision, not a parity pass. |
| D3 Ownership | Duplicate IDs within/across steps and concurrent invocations, initial/delta replacement and passing-retry request-byte identity. No ambient call-ID AST registry. Opaque provider IDs compare raw UTF16 units, not well-formed display strings; Back IDs are structurally encoded from the owned identity. `native_owned_root_worker_duplicate_ids_retry_and_real_stdio_rpc`, `native_owned_utf16_call_ids_keep_raw_association_back_ids_cancel_and_reopen`. Existing numeric/prototype observation-buffer policy remains explicit. |
| D4 Continuation | Pause/pause/final, passing retry, terminal failure and cancellation; partial tools inert, complete signatures/server observations retained, final-only compactor answer. UTF16 delta/text joining and ignored-vs-empty Entry boundaries are exercised by `native_owned_utf16_presentation_failure_cancel_kind_body_reopen` and the existing ordinary/compactor continuation tests. |
| D5 Routes | Actual root/worker loop, private draft/reviewer Session and compactor request adapters. `native_owned_private_sessions_duplicate_ids_js_inputs_and_next_http` exercises owned private A/C; private maintenance does not advertise remote MCP B. Production spawn/actor/private-affinity/snapshot tests remain required, not inferred from a standalone encoder. |
| D6 Cache | Existing unaffected prefix keys byte-identical; excluded final task/turn AST does not split; included AST collisions in the host projection do split. `owned_cache_prefix_keys_follow_actual_included_ast_not_attempts_or_tail` observes the real provider request/probe. Existing coordinator lifecycle tests cover waiting, expiry, LRU and failure/cancel. |
| D7 Durability | Native success/failure/cancel/server/client traces reopen identically, no executable state or hidden AST metadata in journals/store/cache/activity/diagnostics. Mutex-held Tool/Echo batches preserve order; admitted fsync may finish after cancellation. Existing storage failure tests retain their scope. |
| D8 Server observations | All 23 saved pinned cases must be measured separately through actual Harness and native reopen. Former surrogate/overflow errors are not accepted as parity. `native_completed_server_entries_match_pinned_wire_or_expose_f2` retains its historical test name but now expects every case to match. Additional UTF16 presentation/coercion cases exercise dedup before well-formed conversion, stdout/stderr concatenation and Infinity coercion. |
| D9 HTTP lifecycle | Existing negotiated/authenticated transport and actual JSON/SSE outcomes; nondefault event bounds below/at/above for both POST and GET/resume methods. `owned_mcp_reqwest_nondefault_sse_bound_on_post_get_and_resume`; the stock bounded reqwest methods remain the writers, not an unbounded wrapper or second ad-hoc HTTP client. `owned_mcp_integrated_reconnect_cancel_json_sse_and_awaited_session_delete` additionally exercises the actual worker reconnect schedule, Last-Event-ID, JSON/SSE, typed 401, reserved-header rejection and awaited single session DELETE. |
| D10 Child lifecycle | Actual synthetic child RPC bytes plus response, cancellation, awaited close/kill/reap and no closed-handle write. `native_stdio_owned_raw_call_close_and_cancel_kill_reap`. This is local single-child evidence, not all-platform or arbitrary descendant-process proof. |
| D11 Builtins | Irrelevant nested JS-only members do not reject a valid builtin. Only consumed scalar/OS fields have named host boundaries. `native_owned_builtin_consumed_scalar_only_and_exact_reopen`; remote tools/call UTF16 is never excused by a builtin boundary. |
| D12 Freeze | Exact source inventory/hash/mode/size and parent delta; vendored source/licenses included, no runtime legacy reader/importer. Explicit fail-fast test/fmt/Clippy commands and bound logs. Independent source/behavior review precedes an exact-source package build. |

## Named adaptations and open protocol boundary

* Per-record fsync/fail-closed writes, single-writer ownership and durable private
  cache affinity are preserved Pantheon safety properties, not literal upstream
  persistence/global-cache equality. A held multi-record memory batch is not a
  crash transaction: disk failure may retain an append prefix.
* Cold startup streams authoritative journals to rebuild/validate disposable
  indexes. Resident text is bounded; cold startup is not O(1). A deterministic
  candidate scan replaces the upstream heap with the same tested selection
  policy; large-backlog CPU performance is unbenchmarked.
* Arrays at client tool-input root are rejected (SAFE4), invalid coordinates
  fail explicitly, compactor tools do not execute, and prototype-ID data loss is
  not reproduced. These are observable differences, not equivalent traces.
* Cutoff reasons retain bounded secret sanitization. For example `max_tokens`
  can trigger the existing sanitizer; source reason tests distinguish correct
  JS coercion from the intentionally redacted durable presentation.
* Parser nesting/response/event safety limits, host attachment/media workflows,
  UI/agent kinds, skill maintenance, OS tools, billing, import/yield rooms and
  process-to-process handover are named product domains. Shared native behavior
  is not excluded merely because these products differ.
* Codex subscription auth/projection and generic Codex/Z.ai/hosted-search retry
  policy are not literal public Unii transport equality. Codex terminal-SSE
  semantics have their own synthetic tests. No live vendor/cache acceptance is
  established by fake endpoints.
* **Open protocol decision:** modern negotiated MCP annotated string headers
  require lossless scalar UTF8/body agreement. A lone UTF16 scalar has no such
  encoding. The candidate fails precisely this annotated scalar before POST;
  it does not replace characters, omit required headers, silently downgrade,
  or reject ordinary nested/unannotated/stdio remote UTF16 bodies. This named
  interoperability limitation still needs an explicit disposition; it is not
  silently counted as full parity. Pinned Unii RPC does not negotiate these
  headers. An explicit test-only per-request version override is not runtime
  fallback behavior.
* `get_prompt` is a separate typed host/protocol adapter, not upstream native
  tools/call. Consumed unpaired prompt string members fail explicitly; unknown
  unrelated builtin members and raw tools/call retain their normal semantics.

No local fake test or reviewed package authorizes paid calls, stopped-pilot
resume, history replay, migration, tail reconciliation, shadow/cutover, live
configuration/service changes or publication. Recall/real-model quality and
migration acceptance remain separate.

### Budget boundary

Host projections are never budget authority for an opaque native value. Normal
host-created budgets remain unchanged; canonical native byte cost compensates
for a large string/key/subtree that disappears from its projection. Private
Session input guards conservatively count opaque native source before another
HTTP submission. Budget-only projections are not provider/MCP encoding, cache
material or journal metadata. Regressions cover large strings and non-scalar
keys/subtrees plus actual before-second-dispatch Session enforcement.
