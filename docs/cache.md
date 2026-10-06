# Prompt-cache diagnostics

Pantheon keeps OptChat's strict fresh-turn loop. Ordinary conversation takes a
settled memory view before logging the new input, then creates a new provider
conversation. It does not retain the previous native conversation for short chats.
Short kind-prefixed messages fitting 512 bytes become verbatim tree leaves without
a model call. Older adjacent summaries merge as the view needs room. Originals
remain durable and accessible through `zoom`.

Fresh conversations can still reuse an unchanged prompt prefix. Constant system
instructions and tool schemas precede the memory view. Appending recent leaves
preserves older view text; merging changes the prefix from the first modified line
onward. Compaction does not inherently invalidate every earlier token. Within a
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

No cache renewal requests, native-tail retention, or provider cache-policy changes
are introduced. Codex keeps its subscription transport contract; public API cache
metadata remains omitted there. The opt-in
`fresh_turn_cache_probe_reports_real_provider_counters` test measures a cold request,
an identical repeated request and two appended fresh views against the configured
authenticated Codex model. It prints counters and never executes model tool calls.
Zero reported reuse is a valid finding, not a test failure or evidence of caching
success. Investigate transport/model behavior separately from local prefix stability.

The public API's current prefix matching and usage fields are documented in
[OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching).
The underlying memory/fresh-turn layout follows
[OptChat](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449).
