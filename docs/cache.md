# Prompt-cache diagnostics

Pantheon keeps OptChat's strict fresh-turn loop. Ordinary conversation takes a
settled memory view before logging the new input, then creates a new provider
conversation. It does not retain the previous native conversation for short chats.
Short kind-prefixed messages fitting 512 bytes become verbatim tree leaves without
a model call. Older adjacent summaries merge in batches after the view exceeds its high watermark. Originals
remain durable and accessible through `zoom`.

Fresh conversations can still reuse an unchanged prompt prefix. Constant system
instructions and tool schemas precede the memory view. Appending recent leaves
preserves older view text; batches merge from 128,000 down to 64,000 rendered
bytes by default. Pairs are ranked from their last message, preserving old lines
longer. A batch changes the prefix from the first modified line onward. The exact
view partition and unfinished-batch state survive restart in `view.json`; only
legacy chats without a checkpoint need a one-time migration fold. Compaction does not inherently invalidate every earlier token. Within a
tool loop, Pantheon replays the complete native transcript, including encrypted
reasoning/signatures, with queued steering after the outstanding tool results.

`/cache` reports actual provider counters, the last six requests, and a weighted
cache-read ratio over up to 100 root requests in this channel. Missing counters are
unknown, not zero. OpenAI input includes cached and cache-written tokens; Anthropic
input totals add fresh input, cache reads and cache creation. Cache writes are shown
separately. Worker and compactor requests do not overwrite root accounting. `/context`
uses the same normalization and displays uncategorized input when reuse is unknown.

The fresh-view diagnostic compares hashes and byte lengths at complete-line
boundaries. It reports a lower bound on unchanged view bytes and detects model,
reasoning, system or schema changes. These are local observations of prepared
requests, not proof of a server cache hit. Snapshots and the bounded counter history
survive restart without storing prompt text or native reasoning in the diagnostic
tables. Provider KV entries are remote and have their own retention policy.

Codex requests send matching `session-id` and `prompt_cache_key` values. The
hyphenated header supplies ChatGPT's cache affinity; a cache key alone does not
provide the same routing behavior. Random UUIDs persist in `runtime.sqlite`, with
separate identities for each coordinator, worker, channel compactor, agent's
hosted search, channel curator and channel reviewer slot. They survive fresh turns, worker resumptions and service restarts.
Private curator and reviewer hosted searches derive separate, stable identities
from their persisted parent affinity, so research requests do not compete with
their drafting or review prefix.
Separate state databases receive separate identities. These values control routing
only: every fresh turn still constructs a new provider transcript, and compaction
can change its reusable prefix. Public API requests keep their existing contract.

No cache renewal requests, native-tail retention, or provider cache-policy changes
are introduced. Codex keeps its subscription transport contract; public API cache
metadata remains omitted there. The opt-in
`fresh_turn_cache_probe_reports_real_provider_counters` test measures a cold request,
identical repeated requests with reopened providers/stores and two appended fresh
views against the configured authenticated Codex model. It prints counters and
never executes model tool calls.
Zero reported reuse is a valid finding, not a test failure or evidence of caching
success. Investigate transport/model behavior separately from local prefix stability.

The public API's current prefix matching and usage fields are documented in
[OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching).
The underlying memory/fresh-turn layout follows
[OptChat](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449).

The corrected merge priority and batched persistence follow the
[October 8 recipe revision](https://gist.githubusercontent.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449/raw/3c190e06f34aba0c69f49042c526093269604935/optchat.md).
Its 98.6% turn-cache figure is an Anthropic cache simulation, not a measured
Pantheon result. These root-view changes do not adopt the recipe's separate
16–32 KB compactor view, shared turn/compactor prompt, or four-line Anthropic
blocks; Pantheon's existing compactor and API block policy remain as documented.

## Skill catalogue and curator checks

The complete active catalogue is the last system section. Its exact bytes and
usage/refinement counter snapshots persist per channel. A successful skill load
updates library counters, not the system string of a running turn or short-gap
fresh turn. A fresh turn after the configured idle period may refresh this section
once memory settles; that is an explicit prefix-change boundary, not a provider
cache-TTL guarantee. Worker turns use the same channel catalogue generation.

Approved changes produce appended notifications: idle notices occupy the final
new-input block after the old memory view; active notices follow the complete
native tool-result batch. Announcing a revision updates available guide data, not
system or tool schemas. Root memory/native history remain authoritative; private
forks have independent affinities and cannot overwrite root usage diagnostics.

Bounded mock tests cover exact system/schema equality across counter updates,
short-gap fresh turns and restart, an actual idle refresh, OpenAI encrypted items,
Anthropic signatures, complete tool-result batches, and loadable announced revisions.
The opt-in `curator_catalogue_cache_probe` makes five greeting-only requests with
`codex/gpt-6-luna` by default: cold, frozen catalogue after a counter update,
appended publication notice, an exact repeat of that request, then a fresh turn
with a refreshed catalogue. It
prints native input/cache/output counters and executes no tools. Run only this
probe when checking the feature; do not run the entire ignored live suite.
