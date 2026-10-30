# Native OptMem, reference 0500bf6

The target is Uniichat's model-visible memory behavior, pinned to commit
`0500bf6a8ea4bf0ce047488562022b1b6532cc30`. Pantheon retains Discord ownership
and delivery, attachment/tool integrations, provider authentication, and its
stronger fsync/locking. Storage layout, cloud billing, imports and in-flight
process handover are not being replaced.

The runtime uses one native schema and positional page/span addresses. Startup
validates the root namespace and existing banks before opening writer files.
This change reads existing v0.21 native banks; no reset or historical rewrite
is needed. Report-role metadata is additive, and older originals keep their
stored attribution. The daemon remains a single writer.

See [compactor-adapter.md](compactor-adapter.md) for root/worker request
construction and [behavioral-parity-matrix.md](behavioral-parity-matrix.md) for
provider and transport boundaries. Source parity cannot guarantee the same
stochastic answers, recall quality, cache-hit rates or subscription quotas.

## Native mechanism contract

- Every original page is a leaf. Text pages count **30,000 UTF-16 code units**,
  not bytes or Rust scalar characters. Prefer the last newline within the limit,
  otherwise never split a surrogate pair. Empty text is one page. Concatenating
  text pages preserves the admitted Unicode string exactly.
- Text/image pages are right aligned. Each page has at most 20 image references.
  Memory stores immutable names; attachment/media ownership stays outside this
  engine. Ordinary runtime inputs continue to use Pantheon's attachment tools.
- `Memory::push(..., more=true)` withholds the trailing group. Each page is
  already durably journaled but is absent from views, zoom/date, readiness and
  visible count. A later closing push makes the whole group visible together.
  That closing behavior is only for an uninterrupted process. Restart abandons
  the active unfinished group and resets `open=0`; later appends reuse only its
  unpublished positions. Physical journal records are never rewritten.
  Same-source interrupted admission retries write the complete native group,
  rather than guessing a suffix. Recovery performs no tool/provider action.
  `say(start,end)` rejoins text;
  `zoom(id,1)` retrieves one native page, never the whole group.
- `(level,index)` covers `[index*2^level,(index+1)*2^level)`. Children are adjacent
  halves. Sources <=512 UTF-8 bytes are stored locally in batches. Parents use
  flattened child lines; original leaf sources include author/image references.
- Native zoom spans require aligned ID and power-of-two count. Zoom `at` is a
  **page-local UTF-16 cursor**, including a cursor inside a surrogate pair or
  beyond the end. Native provider output preserves those code units; persisted
  originals apply the reference `Memory.root` well-formed conversion. Original
  text is not overwritten by compaction. A parent returns the two child lines,
  or the native not-yet-summarized placeholder.
- Main view: 128,000 UTF-8 payload bytes high, half low. Independent `seen`
  compactor view: 32,000 high, half low. Headers, wrappers and placeholders do
  not count toward these payload budgets. Watermarks are not hard prompt caps.
- Merge only adjacent siblings with built parent. Rank by
  `(view_end+1)/2^level-index`, oldest on ties. Integer cross-products implement
  the same comparison without floating-point rounding.
- Above high, compute a candidate batch to half. At a message boundary it can
  publish partial progress; between messages publish only if candidate payload
  is <=`max(high/2, previous_size-high)`. Preserve the batch flag and partition
  across restart. No memory debounce timer exists.
- The compactor view reaches only the first unfinished leaf. A node's launch
  context ends at `min(node_ctx, first_unfinished_leaf)`, where `node_ctx` is
  leaf ID or parent exclusive end. Freeze that context when the native pump
  reserves the job, not later when an async executor happens to dispatch it.
- Reserve up to 64 jobs, ready only through the 64th unfinished leaf; sort by
  context position and descending level at a tie (merge first). A failure keeps
  its reservation while the provider owner retries. Completion order need not
  match dispatch order. Settle requires leaves, not all parent jobs.
- Compaction preserves the memory conventions but restores the pre-parity
  `compact.txt`-only system prompt and no tools or skills catalogue. Ordinary
  turns omit compaction-only instructions (a named prompt-safety difference).
  A neutral 512-dash ruler replaces contentful examples. Up to five
  answered attempts see the original context/task plus at most the latest size
  correction: never previous answers, reasoning or accumulated corrections.
  Persist the shortest answer, even if still >512 bytes (upstream behavior).
  Supported explicit model failure/refusal/incomplete and completed-empty retain
  the prior shortest answer; with no prior candidate they clip flat source.
  Typed terminal SSE Responses semantics apply to public OpenAI AND Codex:
  `model_parts` maps Codex to OpenAI. The prior claim of a separate generic
  Codex path was an audit error; authenticated fake Codex wire tests exercise
  this actual path, with no speculative transport change. Refusal prose is never
  a summary candidate. Reply-entry normalization matches pinned upstream.
  Each answered length retry appends one latest unmarked correction block to
  the original User item; no previous model output is replayed.
  Generic transport/setup errors retry the same frozen correction after an
  abortable 60 seconds, without consuming an answered attempt, publishing best
  prematurely, or releasing the native reservation. Inner host transport retry
  cadence is native Agent-style for public OpenAI/Anthropic; configured generic
  search/Codex/Z.ai transports remain named separate domains. **512 bytes is a target, not a hard cap.**
  Anthropic `pause_turn` carries all native blocks/signatures into continuation;
  adjacent same-role items merge on the wire. Only final-step reply entries
  determine the answer. Continuation count is not artificially capped; caller
  cancellation owns every request. Messages SSE assembles incremental blocks,
  replacing signature deltas per pinned upstream, with cache release at
  `message_start`. Supported compactor wire profiles are compared against actual
  upstream request builders. The task/correction is unmarked, so Anthropic's
  top-level automatic-end cache mark is absent for compaction.
  See [behavioral-parity-matrix.md](behavioral-parity-matrix.md) and the adapter
  outcome table for exact shared-domain evidence and remaining differences.
- Views form opening block, four-line blocks, and incomplete tail plus closing
  wrapper. Mark only the last whole block, or opening block when none exists.
  The wire concatenation preserves every view byte.

## Storage and retained Pantheon guarantees

`main/*.jsonl` and `tree/*.jsonl` are authoritative append-only journals.
`main.index` and `tree.index` are disposable sparse fixed-record disk indexes.
Tree slots follow `(2*index+1)*2^level-1`. Disk indexes hold only positions,
not original/summary bodies. Native originals and zoom/date records are read on
request. Native zoom is already paged; root/worker and private-research provider
results preserve the whole page and header without generic tool-result clipping.
Shell/browser/external result clipping remains unchanged. The resident summary map contains view nodes and ancestors; discarded
children are pruned once neither independent partition needs them. The ready
queue contains unfinished work, which can naturally grow with a large backlog.
External admission provenance IDs remain indexed in RAM for exact deduplication.

One writer owns a kernel advisory lock. A journal append is one write, then
`fsync`; only then are indexes, IDs and node results published. A short write,
append/fsync/index error or checkpoint error poisons the writer. No subsequent
mutation or snapshot is allowed. Checkpoints use synced temporary file, atomic
rename and synced parent directory. Native schema validation happens before
writer/index creation. File creation also syncs the containing directories.

Private snapshots serialize a frozen view plus canonical bank path, visible
original count and tree-serial frontier, rather than cloning history. Reads
cannot cross either frontier; summaries completed later remain invisible even
when their old-message range would fit. Serialized snapshots need their native
bank and disposable indexes; they are not portable archive exports. Native
journal text is immutable. Moving/removing the bank while readers exist is not
supported.

Provider cache routing identities remain in the durable store and are unchanged
by this engine. Prompt reuse is measured by provider usage, never inferred from
an affinity key. Restarts restore exact saved partitions and do not repeat
completed nodes; interrupted not-yet-durable jobs become ordinary pending work.

## Deliberate differences and limits

1. Pantheon keeps per-write fsync and fail-closed errors rather than UniiChat's
   group-flush waiter semantics. Native visible positional IDs are immutable.
   Like UniiChat, restart resets `open=0` and abandons trailing unfinished
   groups: later appends reuse only those unpublished positions. The physical
   fsynced records remain in the append-only journal; disposable indexes select
   the latest unfinished-frontier replacement. Same-source admission retries
   write the complete group again, not an inferred suffix. No unrelated closing
   append can publish the abandoned prefix. Originals are well-formed Unicode,
   matching upstream `Memory.root(...).text.toWellFormed()`.
2. Startup **streams all journal records to rebuild/validate indexes** instead
   of trusting persistent index frontiers. Bodies are dropped immediately, so
   resident text is bounded, but cold startup is still O(history bytes).
   This is a reliability-first performance difference, not a bounded-startup
   claim. Sidecar corruption is recoverable; journal ID gaps are fatal.
3. Merge selection uses a simple deterministic candidate scan, rather than
   UniiChat's linked-list/heap. The policy and output are differential-tested;
   large-backlog CPU performance is not yet benchmarked.
4. There is one live scheduling side per Pantheon chat. UniiChat's separately
   billed/held import side, live credit holds, browser act ledger and live
   process-to-process provider call handover are not Pantheon features.
   There is no import mode in this runtime. Normal scheduling and cancellation
   use Pantheon's existing executor and cancellation token. A process restart
   does not preserve an in-flight remote inference call.
5. Root/worker memory instructions follow the pinned source, with actor naming
   adaptations and Pantheon harness/skills instructions appended. The attachment
   service remains Pantheon-owned. It does not duplicate UniiChat's image resizing,
   cloud billing, UI or server act format. Native image references/pages follow
   the reference placement and rendering rules.
6. Cache blocks are implemented in the adapters, but provider capabilities
   differ. Anthropic gets system and near-tail memory marks; automatic end is
   absent for an unmarked compactor task, retained for ordinary host turns;
   supported public OpenAI models get the explicit near-tail mark. Moving a
   lone OpenAI breakpoint can reduce reuse; these tests make no cache-hit claim.
   Codex strips unsupported marks and retains its existing durable affinity.
   Z.ai sends an unchunked user message and has no claimed Anthropic semantics.
   Shared request coordination follows UniiChat's real `AskAI.wait/lead`:
   the longest live marked prefix either waits for its writer, refreshes its
   TTL/LRU entry, or leads one extension. `Provider` clones share the coordinator.
   Anthropic uses 300s and supported public OpenAI uses 1800s. SHA-256 prefix
   keys include actual wire controls, system/tools, native blocks and durable
   routing scope. Leaders begin at the reference SSE event (`message_start` or
   `response.output_item.added`); failure/cancellation before it releases the
   next waiter, and a begun leader does not block until stream completion.
   JSON transports have no observable begun event, so they conservatively wait
   for a complete valid JSON response instead. Coordination state is transient
   and cleared on process restart; durable routing identities are unchanged.
   Unlike UniiChat's single process-global cache across all chats, Pantheon
   retains durable affinity/private-domain isolation. It never guesses cache
   residency for Codex/Z.ai or unmarked public models. These transport/scope
   differences remain explicit; coordination is a cache-write hint, not proof
   that a provider wrote or read KV state. No paid cache benchmark was run.
7. Native public OpenAI/Anthropic turns use the pinned eight-attempt
   exponential/header/event-idle lifecycle. Codex/Z.ai/search retain their named
   generic host policy. Native calls without `yields` do not wait on upstream
   busy-model rooms; no import scheduling is added to the legacy-blind engine.
   Above inner transport retries, the compactor owns the reference patient
   60-second same-call retry loop. Shutdown cancels owned tasks. Publication admission linearizes at a
   shutdown-token check while holding the native memory lock, immediately before
   synchronous `Memory.finish` (no intervening await). If cancellation is observed
   there, no summary journal entry or node/view update starts. A write/fsync already
   admitted before cancellation may finish and remains durable on restart; shutdown
   is not rollback or an interrupt of synchronous storage. The lock-wait race and
   completed-before-cancel ordering are tested through the actual daemon compactor
   and native reopen.
   Direct compactor cancellation can retain an already answered shortest result;
   no candidate means cancellation error, not fabricated source fallback.
   Pending native compactions may be scheduled on a subsequent fresh process;
   completed nodes and historical tools are never replayed by memory recovery.
8. Fresh text-turn admission now journals queued originals before selecting the
   Memory.whole raw suffix (128k/20 image references, last page always), settles
   only before that boundary and recomputes it for new same-activity inputs.
   Complete native originals remain available even when only a suffix is sent
   raw. Pantheon UI activity grouping, attachment resolution and system notices
   remain product integrations, not a clone of Unii Server/Tools/cloud objects.
   Native image-reference policy is tested; attachment resolution still occurs
   through Pantheon's separate ownership/budget service.

## Validation

`tests/fixtures/generate-uniichat.ts` imports the actual pinned upstream functions
with network disabled. It replaces only persistence/remote job execution with
in-memory fakes, and records page digests, image placement, four-line rendering,
reservations, frozen launch contexts and view/frontier/batch states after
append and arbitrarily ordered completion events. Regenerate with:

```
bun tests/fixtures/generate-uniichat.ts /path/to/uniichat-0500bf6 \
  > tests/fixtures/uniichat-0500bf6.json
```

Rust compares its complete deterministic event trace and UTF-16/image pages
against that fixture. Restart differential cases run the actual upstream
`Memory.load` open/count recovery (only storage/indexing is faked), then compare
unrelated closing input and complete same-text resubmission. Additional native-only
fixtures test hidden groups, different-source retries, unkeyed talk/echo groups,
repeated restart, torn replacement groups, rejection of visible-ID supersession,
restart, arbitrary completion order, immutable originals/cursors, snapshots,
RAM pruning, source deduplication, schema/writer guards, and injected original,
tree and checkpoint write failures. Runtime/provider fixtures test compactor
request shapes, retries and durable cache scopes. Shared request fixtures cover
leader/follower/extension, cancellation/failure release, 64-waiter wakeups,
expiry/LRU, marked-prefix controls and unsupported-provider bypass. Local HTTP
fixtures exercise the actual cloned provider before send, failure/cancellation,
and release at the exact OpenAI stream event before leader body completion.
No live API or history bank
is used by these tests.

`tests/fixtures/generate-memory-contract.ts` additionally executes actual
`Chat.call`, `Tools.ask`, `Tools.at_work`, `Memory.agent_show`, `Memory.zoom`,
`Memory.date` and `Memory.root` against temporary upstream banks. It captures
root task/view digests, canonical prompts, hot/cold worker log requests, named
log pages, missing spans/dates, and UTF-16 cursor/normalization edges. Network is
disabled. Regenerate with the same pinned checkout:

```
bun tests/fixtures/generate-memory-contract.ts /path/to/uniichat-0500bf6 \
  > tests/fixtures/memory-contract-0500bf6.json
```

The engine oracle also includes deterministic randomized append/completion/fit
traces across three watermarks and completion orders. Runtime tests exercise the
actual channel actor, worker spawn/tell/resume, live root recall, named private
recall, channel boundaries, report attribution, and provider serialization.
The ignored live behavior evaluation checks ordinary work, compaction, and
conversation without executing proposed tools. It is a behavior sample, not a
proof that future model answers always choose correctly.

No storage migration, service restart, deployment or memory wipe is performed
by this implementation or its tests.
