# Native compactor and ordinary adapter contract

Pinned reference: VictorTaelin/uniichat
`0500bf6a8ea4bf0ce047488562022b1b6532cc30`.
The parity target is model-visible memory behavior. Pantheon retains Discord
routing, its tools/attachments, durable storage, locks and provider integrations.
Cloud billing, imports and process-to-process call handover are outside that
scope. Deterministic oracles establish behavior for the covered inputs; they do
not establish identical model answers, cache-hit rates or provider quotas.

## Compactor outcome ownership

One native reservation/context/source owns a chain of at most five answered
length attempts. Only the original User/context plus the latest unmarked
correction block is sent; no previous answer/reasoning transcript is replayed.
UTF-8 byte length determines the first shortest nonempty result. Five oversized
answers retain the shortest even above512 bytes; an answered result is not clipped.
Completed empty or structured model failure retains prior shortest, else clips
flat source. Refusal prose never becomes a summary candidate. Generic outage
retries the same frozen corrected call after60 seconds, without consuming an
answered attempt, releasing the reservation or publishing best prematurely.

Anthropic pause_turn carries all opaque blocks/signatures, combines adjacent
assistant items on the wire and continues the same call. Only final-step reply
entries become a compaction answer. No artificial continuation cap exists.
Signature deltas replace, not concatenate. Client tools from a compactor are
rejected without execution. Cancellation cannot publish a summary after the
under-lock admission check observes stop; synchronous/fsync publication already
admitted may complete. Direct compressor behavior and daemon publication safety
are distinct. This does not authorize an offline importer to retry/resume.

## Native public request lifecycle

Native public OpenAI/Anthropic root, worker, curator and compactor requests use
an eight-request Agent-style inner budget. Literal x-should-retry=true/false
overrides status; otherwise408/409/429 and5xx are busy. Numeric Retry-After uses
JavaScript Number semantics and positive values replace exponential2/4/8/16/
32/64/128-second waits. HTTP dates are not parsed as future dates. Bun timer
overflow/Infinity/<1ms normalize to1ms rather than infinite Rust duration.
Network, malformed event/JSON, EOF and event-idle failures retry; answered model
failure does not. OpenAI server_error is busy; other structured failed/incomplete
outcomes are model failure. Anthropic error events are busy.

Native OpenAI idle is1200 seconds; Anthropic300 seconds. The deadline starts
before headers and refreshes only on a parsed JSON data line, never a comment or
partial byte chunk. The native HTTP client has no absolute response timeout and
no Linux30-second TCP_USER_TIMEOUT. Generic hosted-search, Codex and Z.ai use
their separate configurable transport policy; see transport.md. Native model
busy-room waiting is absent because upstream native calls have no yields keys.
No import scheduling belongs in this daemon.

Local admission/submitted-input errors and native size safety limits fail-stop,
not provider busy retries. Provider diagnostics remain typed/bounded/redacted.

## Ordinary turns are not compactions

Prompt mode is selected by the host, not by user text. Ordinary root turns and
workers never receive `compact.txt` (including its "Call no tools" instruction).
The compactor uses `compact.txt` alone, exactly the pre-#49 prompt-selection
boundary: no chat prefix, harness instructions, skills catalogue or operator task
rules. Its requests still have no executable tools. Harness rules follow the memory prompt and operator instructions
remain last. `Turn:` / `Compaction:` task labels and native memory behavior remain.

This is an intentional prompt-safety difference from the pinned UniiChat source.
Upstream `Tools.MASTER` contains both Turns and Compactions; `Chat.share` supplies
it to ordinary turns and `Memory.ask`, relying on task prefixes to select behavior.
Pantheon #49 adopted that shared construction; #53 aligned its complete prompt and
removed the earlier compaction-only scope sentence. Sharing a prefix is no longer
preferred over separating contradictory task instructions. The upstream fixtures
remain unchanged as provenance, not as a requirement to leak compaction rules into
ordinary requests. This reverts prompt selection only, not the OptMem algorithm,
worker recall, retry/outcome policy, provider wire profiles or cache identities.
No cache-hit or model-alignment improvement is promised. Deterministic tests
inspect actual outgoing system/message
roles and tool lists; they do not prove a particular model's response or speed.

`Chat.call` puts `Turn: the user's new message:\n\n` before the selected raw
suffix. The production channel actor now does the same. Agent status and product
notices are separate unmarked blocks before that marked task. They do not alter
the memory blocks or become part of the task label. Compaction tasks still start
with `Compaction:` and use the native ruler/input form. Workers receive the
upstream subagent/view instructions without the root Compactions section.

`Tools.ask` supplies a worker's entire private original log, not summaries or a
synthetic task/previous-report pair. Worker journals retain Pantheon's durable
pages; request construction rejoins them into logical `i|kind: text` entries.
A new worker begins with `note: subagent` and the task. Idle `tell` captures a
new settled root view; a background continuation keeps its process's captured
view. A cold restored worker gets its saved log without a captured root view.
Numeric zoom always reads the live shared root; named zoom reads the worker's
private log in its owning Discord channel. Reports enter root memory as `work`,
using trusted admission metadata rather than a text-prefix heuristic.

Ordinary tasks are marked; compactor tasks/corrections remain unmarked. OpenAI
ordinary requests omit explicit-end cache options; Anthropic ordinary requests
retain top-level ephemeral cache_control based on original task even after
unmarked steering. AskAI coordinator keys deliberately exclude the final block,
even when marked. Changing only that task changes wire caching, not coordinator
key equality. Private durable affinity remains a named stronger-isolation policy.

Ordinary public profiles use structured developer system for OpenAI,128000
output cap, streaming/default tier, and advertised Anthropic adaptive summarized
thinking/effort. Unsupported/unknown capability fallback is not guessed. Native
text entries concatenate within an OpenAI message, reasoning summary parts join
with two newlines, and both trim per ECMAScript. Anthropic adjacent text entries
merge then trim; thoughts/tools/results split runs. Ordinary model failure never
uses compactor source fallback or executes an observed tool.

Completed ordinary observations are drained by the real Harness into history
before final success/failure/cancel handling. Pause replies survive a later
refusal, outage or cancellation; final End.text remains final-step-only. Thoughts
are UI-only, as in Server.Chat.on; no invented thought originals are stored.
On failed streamed steps, only streamed seen entries survive with cutoff labels;
unstreamed terminal text is not substituted. Failed partial tool entries are
ordered inert Kind::Tool originals, not executable ToolCalls. Busy retries clear
prior attempt observations. Dropping an active request emits its stopped trace
before the Harness drains cancellation observations.

Executable arguments and continuation output are separate. Empty OpenAI JSON
arguments mean{}; malformed arguments/null remain invalid Uses. Anthropic captures
raw Use.input before normalizing native out.input null to{}. Internal raw-input
metadata is never copied into continuation wire messages. Invalid nonobject
arguments produce a tool error result without execution. Arrays are rejected by
Pantheon's object-only guard (an intentional stronger-safety restriction).

## Named domains and differences

- Codex subscription auth/projection/capabilities, absolute/configured retries,
  native tool namespaces and affinity have no UniiChat subscription analogue.
  Its existing typed Responses refusal/incomplete mapping remains tested.
- Z.ai has no pinned upstream adapter. Do not infer equivalent cache semantics.
- Unknown/legacy Anthropic capability fallback and real model limits remain
  provider constraints. Encrypted-reasoning include control is retained.
- Root memory instructions intentionally separate compaction from ordinary turns;
  worker memory instructions follow the pinned source. Harness
  tools, execution device descriptions, Discord provenance and hosted-search
  delegation remain Pantheon integrations. Background delegation remains the
  user-requested Pantheon policy.
- Per-write fsync, fail-closed journals, single writer, stable/private routing,
  strict coordinates, publication cancellation and restart non-replay remain.
- Cutoff reasons are bounded/sanitized before user-visible persistence; literal
  sensitive provider/account strings are not a parity requirement.
- Cloud billing, import scheduling and in-flight cross-process payer handover
  are outside the native daemon domain. An upstream curator actor does not exist;
  Pantheon's curator chooses the explicit ordinary construction contract.
- Actual recall, cache residency/rates, subscription quota, performance, ARM
  package validation, current-tail migration and live cutover are unvalidated.

Validation uses isolated synthetic banks and loopback endpoints. The opt-in live
behavior test makes three controlled inference calls without executing tools.
Neither kind of test changes the running service or its memory.


### Owned native provider values and compactor continuation

Public OpenAI/Anthropic decode now retains one owned ECMAScript JSON AST per
native item, including UTF16 code units, binary64 numbers and first-insertion
object order with last duplicate value. It is not a global serde option or an
ambient call-ID sidecar. Arrays enumerate their integer-index keys as JavaScript
JSON.stringify does; nonfinite numbers stringify as null. Five-try summary and
patient/retry ownership remain separate from this representation.

Completed Uses capture input before Anthropic native-out null normalization.
Initial inputs are replaced by terminal delta input once, by block position;
repeated call IDs do not select an old argument. Opaque provider call IDs retain
UTF16 equality and native Back encoding even when distinct IDs share the same
well-formed display text. Raw OpenAI argument strings
remain raw in native continuation while execution parses them separately.
Partial/failed/cancelled attempts never provide executable Uses. Completed pause
blocks retain signatures/order but only the eventual final step provides Uses
and final compactor text.

Actual public HTTP bodies are prepared structurally from owned values and
serialized once. Remote MCP tools/call uses a scoped request-owned rmcp raw
argument seam through its existing session/auth/HTTP/stdio writers and metadata
merge. There is no placeholder substitution, detached raw POST client or durable
execution registry. Native observations join/deduplicate UTF16 before conversion
to well-formed original text; JSON.stringify escaping is not ordinary String
coercion (e.g. Infinity).

The read-only Rust projection exists for host adapters, budgets and allowlisted
diagnostic shapes. It is not authority for native continuation or RPC encoding.
Opaque native strings/keys/subtrees are charged to resource budgets from their
source rather than a smaller lossy host projection. Only consumed builtin
scalar/OS fields have named limitations; unrelated nested
JS-only members must not reject a valid call. Typed get_prompt is a separately
named protocol adapter; it cannot excuse loss in ordinary remote tools/call.

Marked-prefix coordination uses the same prepared native request. Existing
unaffected keys retain byte identity. Only differences actually inside the
included prefix split keys; excluded final task/later turns and invocation or
attempt IDs do not become cache material. Durable cache affinity remains intact.

See [the finite matrix](behavioral-parity-matrix.md) for D1-D12 assertions, named
adaptations and the open modern annotated-scalar UTF16 header boundary. Source,
frozen behavior, package, real model quality/recall and migration acceptance are
separate stages. No blanket parity or migration claim is made by this local
implementation.

Opaque provider call IDs are not native memory span IDs, historical zoom
references or migration aliases. Their UTF16 identity exists only within an
invocation and its inert results. The local file-delivery extension hashes raw
non-scalar identity in a separately framed domain; existing scalar delivery
keys remain unchanged. New opaque JSON/ID/raw-RPC Debug implementations expose
shape/length only, not hidden source representations. Explicit authorized
provider/RPC encoders still emit the original source data.

## Prompt-selection fix and existing memory banks

Restoring the pre-parity turn/compactor prompt boundary requires **no memory-bank
migration** from the currently installed native global state. The prompt is not a
storage version. `memory.rs`, `store.rs`, `state_namespace.rs`, original/tree
journal formats, saved views/snapshots, page/span IDs, worker banks and format
markers are unchanged. Existing originals and summaries are reused, not replayed,
rewritten, renumbered or re-compacted as part of this fix. The current summary
instructions in `compact.txt` are retained; only their selection changes.

This is not a rollback of #49/#53 or of the earlier global-state transition.
Historical channel-scoped/non-native banks remain subject to the existing
fail-closed admission rules. Do not remove or spoof a format marker, create a
fresh state directory, import/export history, or run a migration for this patch.
Normal operation will append future messages/summaries as before. Changing a
prompt cannot repair any previously lossy summary, but immutable originals and
numeric zoom remain available. Live deployment/restart is separate from local
validation and is not performed by the regression tests.
