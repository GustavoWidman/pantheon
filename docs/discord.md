# Discord transport and delivery

Pantheon uses the Discord v10 gateway directly, with REST delivery through a pooled Rustls client. The gateway maintains sequence numbers, resumes dropped sessions, applies heartbeat jitter, reconnects on missing acknowledgments, and stops on invalid credentials or unsupported intents. There is one gateway shard per deployment. Slash commands register globally at service startup.

The configured user allowlist applies to both messages and slash commands. An empty allowlist prevents startup. DMs are accepted from authorized users; server channels and threads require an explicit bot mention for each prompt. Bots and webhook messages are ignored. Enable the **Message Content** privileged intent in the Discord Developer Portal. Install the bot with `bot` and `applications.commands` scopes, and grant View Channels, Send Messages, Send Messages in Threads, Read Message History, Embed Links, and Add Reactions where it should operate.

The transport acknowledges authorized slash commands with an ephemeral deferred response before handing them to the runtime. It uses a separate task for each acknowledgment so REST latency cannot block gateway heartbeats. Unauthorized command invocations receive a private denial. The runtime edits the ephemeral response after executing the command. Interaction tokens expire, so interactions are control responses rather than durable final-output destinations.

Registered commands:

| Command | Purpose |
| --- | --- |
| `/context` | Inspect model-window and memory grids, token counts and prompt-cache reuse |
| `/cache` | Inspect provider cache reads/writes, recent requests and fresh-view prefix stability |
| `/model [kind] [provider] [model]` | Chat-local chat/compactor/curator overrides; model and provider autocomplete |
| `/reasoning [kind] [level]` | Select chat, compactor or curator effort with model-aware autocomplete |
| `/stop` | Cancel the current run and its background agents |
| `/status` | Inspect active agent phases, shell jobs, message queues, schedules and delivery |
| `/subagents` | List background agents |
| `/skills [id]` | Private paginated skill dashboard: catalogue, content, resources, history and comparisons |
| `/curator [action]` | This channel’s curation phase, queue, reviewer counts and run/cancel controls |
| `/mcp` | Show integrations configured for this channel |
| `/browser [action] [browser_id] [resume_token] [url]` | List/open desktops, hand off, explicitly resume, or close |
| `/wakeup action [schedule] [prompt] [id]` | Add, list, or cancel durable prompts |
| `/monitor action [command] [interval_seconds] [prompt] [id]` | Add, list, or cancel command monitors; interval minimum five seconds |

Command handling and validation belong to the runtime; Discord transports the command name and the options array without changing them. Wakeup schedules accept `in 5m`, `every 1h`, or `once <RFC3339 timestamp>`. Normal messages retain their Discord snowflake ID, which the durable inbox uses for replay deduplication. Incoming prompts should steer the active session at a model boundary; gateway code never cancels and recreates an agent implicitly.

## Context and work status

`/context` uses a 10×10 model-window grid: blue is cached input, purple is fresh input, orange is output, and dark squares are remaining capacity. Each cell represents a rounded one percent; the legend retains exact provider-reported counts. Cached input occupies the window normally. Anthropic input totals include cache reads and writes. This is the **last recorded coordinator request**, not a live counter. Its model remains attached to the snapshot when `/model` changes. Worker search usage cannot overwrite it.

A separate green memory grid shows the compacted view against `agent.view_bytes`, in bytes rather than estimated tokens. Durable history can be much larger than this loaded view. Memory state indicates whether compaction has settled.

Model capacities come from exact per-model `agent.context_windows` overrides or the Codex CLI's cached model metadata. Metadata is read locally without contacting a provider or refreshing credentials. Missing capacities remain explicitly unavailable; legacy usage without a recorded model does not invent an occupancy percentage. The capacity setting affects display only. For API models or absent Codex metadata, configure the verified model limit:

```toml
[agent.context_windows]
"anthropic/YOUR_MODEL" = 200000 # Replace with your model and its verified token limit.
```

`/status` includes a **Version** field with the running binary’s compiled-in package version (from `Cargo.toml`), followed by execution details: active agent names/phases, shell jobs, prompt and worker-message queues, pending deliveries, active wakeups/monitors, and whether memory is settling. Its color changes from amber during work to green when ready; `/context` uses blue.

## Final messages and tool activity

`split_message` counts UTF-16 code units, preserving emoji while staying below Discord's 2000-unit budget. It closes and reopens ordinary backtick or tilde fences at chunk boundaries, retaining the language label. Fences longer than 64 delimiters or opening lines longer than 256 units are treated as plain text to keep splitting memory and overhead bounded.

Only the completion after all current workers, detached commands and queued reports have finished uses Discord's native reply reference and enables `allowed_mentions.replied_user`. It targets the original loop-starter message and its author. All progress prose, acknowledgments and activity are plain channel messages without references or notifications. Inputs received while the coordinator is idle but its workers are active remain steers of the same loop. Automatically parsed mentions are disabled. Synthetic inputs without a message reference retain an explicit requester mention as a fallback.

Consecutive presentation events accumulate in one fenced message. Every prose message closes the current accumulator; the next activity opens a new fence at the end of the timeline. Tool rows show the tool name and elapsed time, for example `✓ shell · 0.4s`, and update in their original fence from running to success, failure, skipped or background. Spawns appear as `↗ spawned <name> [id] · <model> · <effort>` and each worker report produces `↙ incoming agent message from <name> [id]` immediately when that worker finishes. Coordinator-owned wakeup and monitor deliveries use `↙ wakeup notification` and `↙ monitor notification` without a redundant recipient label; sent messages use `↗`. Successful spawn/tell calls have no duplicate generic tool row. Worker tool calls, tool errors and worker-owned shell, wakeup, monitor and tell notifications stay in private execution state and traces. Only a worker report actually delivered to the coordinator produces a public incoming marker. Arguments, shell commands, outputs, credentials and provider errors stay out of the activity log. Twelve-row fences roll over without eliding events. Segments, original references, identities, events and Discord receipts survive restart in SQLite, separately from model context. Report admission, worker idle state and the incoming fence commit atomically.

Each accepted input receives 📥. That becomes 🧠 when an actual model request includes the input; a steer buffered during an existing request stays 📥 until the next request includes it. Once the loop, all current background work and context compaction settle, the starter and its steers become ✅. Reaction desires and delivery acknowledgments are durable and monotonic; retries reconcile earlier phases using only the bot's own reactions. Future scheduled wakeups and monitor intervals do not keep an otherwise idle loop busy.

A four-second heartbeat renews typing while the coordinator, workers, detached shell jobs, queued reports or context compaction are busy. Activity edits are limited to one per two seconds per channel and use a two-second transport deadline; durable retries preserve backoff. New fences and prose can pass delayed edits of existing messages, while an unsent earlier fence preserves the order of new messages.

Reasoning autocomplete uses this chat model’s live advertised efforts. Known unsupported manual choices are rejected with the available levels. Live catalog metadata takes precedence over older CLI cache metadata. Legacy internal `minimal` requests can map to `low`; the picker does not advertise unsupported levels. See [model discovery](models.md).

Slash commands return private, branded embeds with structured model, effort, context usage, cache and worker fields. Errors use a distinct error color. Embed and message limits count UTF-16 units, including emoji.

## Delivery guarantees

Every message send uses a deterministic 25-character nonce derived from the durable outbox item ID, with Discord's `enforce_nonce` option. Retrying one item within Discord's recent-nonce window returns the existing message rather than creating another. Discord documents this window as the past few minutes; it is not a permanent idempotency key. A crash after Discord accepts a send but before the local outbox commit can produce a duplicate if recovery happens beyond that window. Local durability alone cannot eliminate this external delivery ambiguity.

The daemon enables `with_state_dir`, creating a private `discord-ingress.sqlite` journal with SQLite WAL and `synchronous=FULL`. Each authorized message and its gateway replay cursor commit in one transaction before the message enters the in-process queue. READY session metadata and ignored dispatch cursors also checkpoint. An earlier pending message remains in the journal even when later ignored events advance the cursor. No bot or interaction tokens enter this journal.

On startup, unacknowledged messages replay before any Discord network request. The runtime deduplicates them in its own durable inbox, then calls `acknowledge(id)` to delete the transport's pending row. A crash between admission and acknowledgment replays a duplicate safely. A failed checkpoint rolls back both cursor and message and stops the gateway, preserving the prior replay position. Revoked users are filtered again during replay. Saved state is tied to the Discord application ID.

The saved gateway session, sequence, resume URL, and bot ID also allow Discord resume after a daemon restart. Graceful shutdown drops the connection without an invalidating normal-close handshake. Remote resume remains subject to Discord's session retention: prolonged downtime, expired sessions, or a nonresumable gateway close require a new session. Messages received before the last journal transaction can be recovered through remote resume while that session remains valid. If Discord invalidates that session first, those uncommitted messages are not recovered automatically from channel history. Once a prompt reaches the local ingress journal, its replay survives independently of remote session validity. Slash commands are ephemeral control operations and are not replayed after a crash.

REST calls retry rate limits, connection failures, and HTTP 5xx responses at most six attempts. Rate-limit delays above sixty seconds fail the current delivery attempt so the runtime can retry durably later. HTTP 4xx responses other than rate limits fail immediately. Requests have a twenty-second timeout; initial interaction acknowledgments have a two-second deadline and execute no command if acknowledgment fails. Error messages contain status codes and fixed descriptions, never token-bearing URLs or response bodies.

Protocol references: [Gateway](https://docs.discord.com/developers/events/gateway), [Message resource](https://docs.discord.com/developers/resources/message), and [Interaction responses](https://docs.discord.com/developers/interactions/receiving-and-responding).

Idle identities are hidden from `/subagents` after one hour by default, with history retained. Busy or queued workers remain visible. The agent-facing `list_agents` directory is paginated, can include archived identities, and supports coordinator discovery across channels; worker discovery stays local. `tell` automatically restores and wakes an idle identity; user input and scheduled notifications do the same. Incoming cross-channel coordinator messages have their own visible activity marker. A message that wakes a settled coordinator starts a new activity without replying to an old Discord message or automatically mentioning its author.

Model selections are scoped to this chat and survive restart; `model:default` restores config defaults. See [model discovery](models.md) for catalog sources, reasoning compatibility and compactor transitions.

## Curator publication and skill browsing

Approved skill changes emit `✦ curator · created skill ...`, `↻ curator · modified
skill ...` or `⊖ curator · retired skill ...` in the existing activity fences.
They accumulate in chronological order and do not wake the orchestrator. Private
research and reviewer reports never appear in Discord or main memory. Approved
change summaries wait for actual input while idle, or steer at the next active
model boundary. `/curator` exposes the separate channel-local queue/review lifecycle.

The `/skills` home page and dropdowns paginate every active/retired guide. Complete
bodies, supporting text, revision history and comparisons have bounded embed pages.
Component IDs are bound to channel and user; authorization precedes Discord’s
component-update acknowledgement. These controls edit only the private dashboard.

## Attachments

Allowed users can send attachment-only messages or text with files under the same mention/DM rules. The orchestrator can reply with `send_file`; workers return artifact paths privately. See [attachment input, delivery and cleanup](attachments.md).
