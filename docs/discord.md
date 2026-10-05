# Discord transport and delivery

Pantheon uses the Discord v10 gateway directly, with REST delivery through a pooled Rustls client. The gateway maintains sequence numbers, resumes dropped sessions, applies heartbeat jitter, reconnects on missing acknowledgments, and stops on invalid credentials or unsupported intents. There is one gateway shard per deployment. Slash commands register globally at service startup.

The configured user allowlist applies to both messages and slash commands. An empty allowlist prevents startup. DMs are accepted from authorized users; server channels and threads require an explicit bot mention for each prompt. Bots and webhook messages are ignored. Enable the **Message Content** privileged intent in the Discord Developer Portal. Install the bot with `bot` and `applications.commands` scopes, and grant View Channels, Send Messages, Send Messages in Threads, Read Message History, and Embed Links where it should operate.

The transport acknowledges authorized slash commands with an ephemeral deferred response before handing them to the runtime. It uses a separate task for each acknowledgment so REST latency cannot block gateway heartbeats. Unauthorized command invocations receive a private denial. The runtime edits the ephemeral response after executing the command. Interaction tokens expire, so interactions are control responses rather than durable final-output destinations.

Registered commands:

| Command | Purpose |
| --- | --- |
| `/context` | Inspect model-window and memory grids, token counts and prompt-cache reuse |
| `/model [id]` | Inspect or choose the channel model |
| `/reasoning [level]` | Inspect or choose reasoning effort |
| `/stop` | Cancel the current run and its background agents |
| `/status` | Inspect active agent phases, shell jobs, message queues, schedules and delivery |
| `/subagents` | List background agents |
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

`/status` focuses on execution: active agent names/phases, shell jobs, prompt and worker-message queues, pending deliveries, active wakeups/monitors, and whether memory is settling. Its color changes from amber during work to green when ready; `/context` uses blue.

## Final messages and tool activity

`split_message` counts UTF-16 code units, preserving emoji while staying below Discord's 2000-unit budget. It closes and reopens ordinary backtick or tilde fences at chunk boundaries, retaining the language label. Fences longer than 64 delimiters or opening lines longer than 256 units are treated as plain text to keep splitting memory and overhead bounded.

Responses use Discord's native reply reference to the original prompt, including asynchronous worker reports, shell completions and wakeups after newer prompts arrive. Only a settled completion enables `allowed_mentions.replied_user`; acknowledgments while background work remains, intermediate prose and activity do not ping. Automatically parsed mentions are disabled. Synthetic inputs without a message reference retain an explicit requester mention as a fallback.

Each request has an accumulating fenced activity log. Tool rows update in place from running to success, failure, skipped or background, with elapsed time. Named worker spawns include their ID, model and reasoning effort; incoming reports appear as `-> incoming agent message from <name> [id]`. Arguments, shell commands, outputs, credentials and provider errors stay out of the activity log. Fixed twelve-row pages preserve earlier messages as activity grows. Original references, identities, event rows and Discord receipts are durable SQLite presentation metadata, separate from model context.

A four-second heartbeat renews typing while the coordinator, workers, detached shell jobs, queued reports or context compaction are busy. The activity footer remains visible between typing renewals and describes the current phase. Activity edits are limited to one per two seconds per channel and use a two-second transport deadline; durable retries preserve backoff. Completion replies pass queued activity edits so presentation cannot hold up the answer.

Slash commands return private, branded embeds with structured model, effort, context usage, cache and worker fields. Errors use a distinct error color. Embed and message limits count UTF-16 units, including emoji.

## Delivery guarantees

Every message send uses a deterministic 25-character nonce derived from the durable outbox item ID, with Discord's `enforce_nonce` option. Retrying one item within Discord's recent-nonce window returns the existing message rather than creating another. Discord documents this window as the past few minutes; it is not a permanent idempotency key. A crash after Discord accepts a send but before the local outbox commit can produce a duplicate if recovery happens beyond that window. Local durability alone cannot eliminate this external delivery ambiguity.

The daemon enables `with_state_dir`, creating a private `discord-ingress.sqlite` journal with SQLite WAL and `synchronous=FULL`. Each authorized message and its gateway replay cursor commit in one transaction before the message enters the in-process queue. READY session metadata and ignored dispatch cursors also checkpoint. An earlier pending message remains in the journal even when later ignored events advance the cursor. No bot or interaction tokens enter this journal.

On startup, unacknowledged messages replay before any Discord network request. The runtime deduplicates them in its own durable inbox, then calls `acknowledge(id)` to delete the transport's pending row. A crash between admission and acknowledgment replays a duplicate safely. A failed checkpoint rolls back both cursor and message and stops the gateway, preserving the prior replay position. Revoked users are filtered again during replay. Saved state is tied to the Discord application ID.

The saved gateway session, sequence, resume URL, and bot ID also allow Discord resume after a daemon restart. Graceful shutdown drops the connection without an invalidating normal-close handshake. Remote resume remains subject to Discord's session retention: prolonged downtime, expired sessions, or a nonresumable gateway close require a new session. Messages received before the last journal transaction can be recovered through remote resume while that session remains valid. If Discord invalidates that session first, those uncommitted messages are not recovered automatically from channel history. Once a prompt reaches the local ingress journal, its replay survives independently of remote session validity. Slash commands are ephemeral control operations and are not replayed after a crash.

REST calls retry rate limits, connection failures, and HTTP 5xx responses at most six attempts. Rate-limit delays above sixty seconds fail the current delivery attempt so the runtime can retry durably later. HTTP 4xx responses other than rate limits fail immediately. Requests have a twenty-second timeout; initial interaction acknowledgments have a two-second deadline and execute no command if acknowledgment fails. Error messages contain status codes and fixed descriptions, never token-bearing URLs or response bodies.

Protocol references: [Gateway](https://docs.discord.com/developers/events/gateway), [Message resource](https://docs.discord.com/developers/resources/message), and [Interaction responses](https://docs.discord.com/developers/interactions/receiving-and-responding).
