# Native compactor and ordinary adapter contract

Pinned reference: VictorTaelin/uniichat
`0500bf6a8ea4bf0ce047488562022b1b6532cc30`.
This is the V10 candidate contract, not a blanket parity, model-quality,
cache-rate or migration-readiness claim. Exact artifacts/tests/review must be
bound externally; parent V9c package evidence does not cover this successor.

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

The shared system prefix starts by distinguishing ordinary turns from tasks
beginning `Compaction:`. Only the latter apply the Compactions instructions;
ordinary turns act on the request and answer the user. This distinction must
precede both instruction sections: without it, ordinary chat can follow the
appended compactor instructions and return a memory line without taking action.
The prefix remains shared; no output-text classifier or reply suppression is used.

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
- Root instruction content, device/agent status bytes, tool names and explicit
  hosted-search delegation are app differences, not unidentified normalization.
- Per-write fsync, fail-closed journals, single writer, stable/private routing,
  strict coordinates, publication cancellation and restart non-replay remain.
- Cutoff reasons are bounded/sanitized before user-visible persistence; literal
  sensitive provider/account strings are not a parity requirement.
- Cloud billing, import scheduling and in-flight cross-process payer handover
  are outside the native daemon domain. An upstream curator actor does not exist;
  Pantheon's curator chooses the explicit ordinary construction contract.
- Actual recall, cache residency/rates, subscription quota, performance, ARM
  package validation, current-tail migration and live cutover are unvalidated.

Fake deterministic tests do not authorize inference, the stopped V4 pilot,
legacy replay, canonical/live writes, commits/pushes/merges or deployment.


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
