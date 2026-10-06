# Durable memory and cache layout

Pantheon implements the [OptChat specification](https://gist.githubusercontent.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449/raw/f51fe5c910427fd6f384d22823140b1693c76207/optchat.md). Every completed master entry becomes permanent history. Reasoning is displayed and replayed in the live provider conversation, but never enters this log. Subagent internals stay outside the main history; their reports enter as `user` messages beginning `[id] `.

Each conversation owns a directory:

```text
chat/
  lock
  main/YYYY-MM-DD.jsonl
  tree/YYYY-MM-DD.jsonl
```

The `main` stream stores `{i, kind, text, size, date}`, plus an optional `source_id` for externally admitted messages. IDs are global, zero-based, and permanent. Dates include the local UTC offset. Size counts UTF-8 bytes of `kind + ": " + text`; provenance metadata never enters summaries or changes that count. The `tree` stream stores `{l, i, text, size}`, with `(l, i)` covering `[i·2^l, (i+1)·2^l)`.

`append_with_id(kind, text, source_id)` durably records an opaque admission ID alongside the original message. Repeating the same ID, kind, and exact text returns the original message index without another journal write; changed content is rejected. `lookup_source` uses an in-memory index rebuilt from ROOT on startup, so runtime SQLite and the append-only history can recover independently after a crash between their writes. Ordinary `append` remains unchanged and omits `source_id`, and older journals without provenance still load. Admission IDs must be nonempty and unique within the conversation; namespace IDs when multiple sources can share the same identifier. A process restart must preserve accepted but interrupted inputs as unanswered history, using these IDs to avoid duplicate appends.

Both streams use one append `write` followed by `fsync` for each record. The containing directory is also synced when creating a journal file. Short writes or sync failures poison further writes until the memory is reopened: the process must not guess whether an ambiguous write committed. The next open reports and skips invalid JSON, including torn UTF-8, and appends a missing newline before future records. Recovery preserves every existing byte. Valid records with missing trailing newlines remain valid. Unexpected gaps or duplicate IDs cause startup to fail instead of silently changing IDs.

The writer holds an exclusive `flock` on `lock` for its entire lifetime and explicitly unlocks when `Memory` is dropped. Explicit release prevents a concurrent shell/browser spawn from briefly retaining the lock through an inherited descriptor before `exec` closes it. This substitutes a kernel-managed file lock for the reference socket lock: process exit and crashes release ownership automatically, and there are no PIDs, lease timeouts, stale-socket deletion races, or lock-file removal races. The inode is retained permanently. Use a local filesystem with working advisory locks, as with the NixOS service's state directory.

## Tree and compactor

The tree is strictly binary. Each leaf summarizes one message; each parent summarizes exactly two adjacent children. Sources of at most 512 UTF-8 bytes become nodes verbatim with no provider call. Short user messages therefore remain exact until a later merge needs compression. Nodes are persisted, retained forever, and never recomputed on restart.

The source-ready frontier holds only the oldest missing leaf and source-ready parents. The pump does not rescan the complete message history after every job. A leaf can run only when all earlier view lines are built; a merge can run only when both children exist and all view lines through its end are built. Several independent merges can run alongside that one leaf. The async harness owns busy jobs and retries; callers must filter busy/retry candidates before taking their concurrency allowance.

Compactor requests use `src/compact.txt`, the reference prompt with the agent name changed to Pantheon. Context comes first, wrapped in `<chat>`, containing bare summarized text without view IDs. The step contains the whole original message or the two flattened child summaries. No tool is available to the compactor. The provider supplies a realistic, exactly 512-byte scale example, up to five shortening attempts in the same conversation, and byte-boundary-safe feedback. It keeps the shortest trimmed attempt, accepting a small overshoot rather than truncating a summary. Failed jobs retry after ten seconds indefinitely; only their first failure is reported.

## View and fresh turns

The view covers every message oldest first, using `id+n|text` lines and no dates. It stores only summaries, with a placeholder for an unbuilt leaf. The model never receives that placeholder: master turns and subagent spawns wait until the view settles. Cancellation may end this wait while leaving the user's message in history.

The live view only appends and merges. While its text size exceeds the default 128,000-byte budget, it chooses the eligible adjacent pair with the greatest `(T - start) / 2^(level+2)` age. Parents must already be built. Comparisons use exact integer arithmetic, with the chronologically earliest pair winning ties. A maintained set of mergeable pairs avoids scanning an entire unsummarized tail when no parent is available; fits with no candidates take constant time. The view may temporarily exceed the budget while a parent is missing; it never splits a part, substitutes cut original text, or retiles on a read. Startup reconstructs the view by replaying `append + fit` from message zero using each replay step's length in the age rule. This matches the reference fold rather than saving transient live layouts or renumbering summaries.

A fresh turn renders the settled view **before** appending its new user input. It sends that input whole in the next block. All subsequent talk, tool calls, capped tool results, and consumed steering messages are committed as they finish. `zoom(id, n)` opens a binary node into its two children; `zoom(id, 1)` returns the original message and newlines verbatim. `date(id)` returns its recorded local ISO timestamp. HTML export escapes untrusted text and contains the view, all original messages, and all tree levels with ranges, time spans, and byte sizes.

## Cache boundaries

[Cache diagnostics](cache.md) explain measured reuse, unknown counters and prefix
stability. `/cache` observes the strict fresh-turn design without retaining a native
conversation tail or changing provider cache policy.

Request prefixes remain ordered: constant tools, constant system prompt, view, whole new input, and the turn's verbatim provider conversation. `cache_chunks` cuts the view at the preceding newline for 50,000, 80,000, and 100,000 Unicode characters, skipping marks beyond the end and duplicate cuts. Concatenating the blocks always reproduces the original view exactly. Supporting API providers attach the same cache marks on every step, plus the request-end automatic breakpoint. Codex subscription requests preserve the chunks but omit unsupported API cache metadata. There are no one-hour cache entries or renewal pings.

`cap_tool_result` retains equal head/tail portions with an omission notice, keeping the complete returned string at or below 30,000 Unicode characters. The cap is applied before provider replay and before the permanent `echo` append. It never splits UTF-8 characters; original user messages are not capped.

Memory tests exercise restart without new summaries, repeated admission across restart, torn UTF-8 recovery, writer exclusion, ordered compression, exact binary zoom, free nodes, coarsening without splits, exact startup equivalence with the reference age rule, a large unsummarized import frontier, Unicode cache cuts and result caps, and HTML escaping. Unit tests use actual filesystem journals and locks.

`cargo run --release --example memory_bench -- 10000 20000` measures startup with prewritten valid original messages and no synthetic summaries. Fixture construction is outside the timer. Each size reports the minimum, median, and maximum of five `Memory::open` calls, covering lock acquisition, JSON parsing and validation, view replay, and frontier construction. The benchmark verifies that no summary was manufactured and only the first missing leaf becomes eligible. `fit` reads a cached byte count, maintained on replay appends, live appends, node completion, and merging; it does not sum the complete view on every fit.

## Channel scope and conversational turns

Runtime memory lives under `chats/<channel>/`. Each channel owns its immutable log, tree, stable message addresses and incrementally folded view. Workers read their channel's memory snapshot and `zoom` that tree; their private execution traces stay in `subagents/<id>/`. A coordinator message across channels is recorded as work input only in the destination's log. It neither combines logs nor changes another channel's view implicitly. Archiving an agent from the directory does not remove memory.

Ordinary conversation follows OptChat section 7 exactly as task turns do: take a settled view, render it before appending new input, and start a fresh provider conversation. A short user message or reply whose kind-prefixed source fits 512 bytes becomes a verbatim free leaf immediately; it does not require a summarizer call. Larger leaves and merges still compact in the background, with the next fresh turn waiting only for its view to settle. No agent needs to spawn a worker or call a tool just to converse. Input received while the provider is busy remains durable and is injected at the next safe boundary, including after a final response with no tools; multiple queued inputs can share one turn. Already submitted provider requests cannot be retroactively edited.

Shared memory is not automatically enabled. A cache-friendly extension would be a separately scoped shared notebook with explicit retrieval and coordinator-approved writes: retrieve a relevant snapshot as a tool result instead of prepending a continually changing global chat view ahead of every channel's context. That keeps local cache prefixes stable, preserves provenance and leaves each channel's message IDs unambiguous. Today's coordinator messaging can transfer a selected fact without creating a global memory stream.

The compactor model can be overridden per channel through `/model kind:compact`. Newly scheduled jobs capture that selection; already running jobs and persisted summaries retain theirs. See [model discovery](models.md).
