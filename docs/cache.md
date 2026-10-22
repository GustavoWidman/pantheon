# Prompt cache behavior

Ordinary conversation starts each fresh turn with the native OptMem view and
new whole input. Within a turn, provider-native tool/reasoning messages retain
their exact order. A persistent backend session is not the memory bank and is
not necessary for prompt-prefix reuse.

## Native blocks and compaction

Native views emit an opening `<chat>` block, groups of four complete summary
lines, and the incomplete tail/closing wrapper. The near-tail stable cache
mark is on the last full group, or the opening block if there are no full
groups. Concatenation preserves all view bytes. Main and compactor views have
independent durable partitions. Between messages, the native half-budget batch
publication gate avoids a prefix rewrite on every parent completion.

Compaction uses the same root system and frozen skill catalogue as channel
turns, but has no executable tools. Each retry consists of the original view
and task plus at most one latest size note; it never accumulates previous
answers or thoughts. Its launch view is frozen at job reservation and bounded
by its node and earliest-unfinished-leaf frontier.

## Transport distinctions

- Anthropic requests mark system text and the last whole view block (or
  opener), plus the automatic request-end mark. This remains within the
  provider's four-slot budget; no three large character-offset marks remain.
- Supported public OpenAI models receive an explicit near-tail breakpoint.
  Unsupported models have that field stripped. A moving lone boundary can
  reduce reuse under OpenAI's matching rules; block tests are not proof of
  provider cache hits.
- Codex requests omit unsupported API breakpoint metadata. Existing durable
  random `prompt_cache_key` affinity stays scoped to root, worker, compactor
  and search owner. A stable key does not imply an actual hit.
- Z.ai has its own unchunked-message adapter. No Anthropic cache semantics are
  claimed for that transport.

UniiChat has real shared request coordination: `AskAI.wait/lead` is called by
`Agent.request`; only its reported hit-rate tests are synthetic. Pantheon now
implements the corresponding marked-prefix leader/waiter and TTL/LRU policy in
`provider_cache.rs`. All clones of the service provider share one coordinator.
The keys include the actual model/control/system/tools/prefix bytes and durable
routing scope; task text after the mark and later transcript items do not alter
an already-written prefix. A leader starts immediately before socket submission,
not while credentials or provider admission are pending. SSE followers wake at
`message_start` (Anthropic) / `response.output_item.added` (OpenAI), not merely
headers or `response.created`. Failure/cancellation releases an unfinished
leader. JSON transports conservatively release on valid body completion.

TTL refresh and expired LRU-prefix eviction follow the reference (300s Anthropic,
1800s public OpenAI). This is process-local cache knowledge, not durable KV state:
a new process starts cold while retaining Pantheon's durable routing identities.
Private routing domains remain isolated rather than sharing UniiChat's global
cache state across unrelated chats. No coordinator entry is inferred for Codex,
Z.ai or unmarked models. These differences are intentional, not claims about
unsupported transport capabilities.

## Diagnostics and durable routing

`/cache` reports actual provider cache-read/write counters where supplied,
separately from fixed-prefix compatibility fingerprints. The root fresh-turn
view fingerprint distinguishes append growth, unchanged views, coarsening,
and model/system/tool changes without recording prompt text. Closing wrappers
are excluded when measuring the stable append prefix. Tool-loop versus fresh
usage remains distinct. Snapshot storage and durable cache identities survive
service restart; the engine does not replay a provider-native transcript as
fresh memory.

See [optmem-parity.md](optmem-parity.md) for exact mechanism limits and fake-only
validation. Actual inference/cache usage needs a separately authorized live
pilot; no paid requests are required by normal tests.

## Owned native prefix values

Public native request preparation retains full ECMAScript JSON values instead
of hashing a lossy Rust Value projection as native authority. The original
coordination keys remain byte-identical for unaffected host-created prefixes.
If an owned value inside the included marked prefix differs despite a colliding
host projection (for example two distinct lone UTF16 values), the scoped prefix
key splits. Differences only in the excluded final task or subsequent turns do
not split. Invocation/attempt ownership is ephemeral and never key material.
These fake endpoint assertions are not provider KV-cache acceptance or a hit-rate
measurement; durable routing identities and restart-prefix invariants are
unchanged.
