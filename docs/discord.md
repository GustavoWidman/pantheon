# Discord transport and delivery

Pantheon uses the Discord v10 gateway directly, with REST delivery through a pooled Rustls client. The gateway maintains sequence numbers, resumes dropped sessions, applies heartbeat jitter, reconnects on missing acknowledgments, and stops on invalid credentials or unsupported intents. There is one gateway shard per deployment. Slash commands register globally at service startup.

The configured user allowlist applies to both messages and slash commands. An empty allowlist prevents startup. DMs are accepted from authorized users; server channels and threads require an explicit bot mention for each prompt. Bots and webhook messages are ignored. Enable the **Message Content** privileged intent in the Discord Developer Portal. Install the bot with `bot` and `applications.commands` scopes, and grant View Channels, Send Messages, Send Messages in Threads, and Read Message History where it should operate.

The transport acknowledges authorized slash commands with an ephemeral deferred response before handing them to the runtime. It uses a separate task for each acknowledgment so REST latency cannot block gateway heartbeats. Unauthorized command invocations receive a private denial. The runtime edits the ephemeral response after executing the command. Interaction tokens expire, so interactions are control responses rather than durable final-output destinations.

Registered commands:

| Command | Purpose |
| --- | --- |
| `/context` | Inspect durable context and cache state |
| `/model [id]` | Inspect or choose the channel model |
| `/reasoning [level]` | Inspect or choose reasoning effort |
| `/stop` | Cancel the current run and its background agents |
| `/status` | Inspect active work and delivery |
| `/subagents` | List background agents |
| `/browser [action] [browser_id] [resume_token] [url]` | List/open desktops, hand off, explicitly resume, or close |
| `/wakeup action [schedule] [prompt] [id]` | Add, list, or cancel durable prompts |
| `/monitor action [command] [interval_seconds] [prompt] [id]` | Add, list, or cancel command monitors; interval minimum five seconds |

Command handling and validation belong to the runtime; Discord transports the command name and the options array without changing them. Wakeup schedules accept `in 5m`, `every 1h`, or `once <RFC3339 timestamp>`. Normal messages retain their Discord snowflake ID, which the durable inbox uses for replay deduplication. Incoming prompts should steer the active session at a model boundary; gateway code never cancels and recreates an agent implicitly.

## Final messages and tool activity

`split_message` counts UTF-16 code units, preserving emoji while staying below Discord's 2000-unit budget. It closes and reopens ordinary backtick or tilde fences at chunk boundaries, retaining the language label. Fences longer than 64 delimiters or opening lines longer than 256 units are treated as plain text to keep splitting memory and overhead bounded.

Final response text receives one explicit owner mention in its first chunk. `send` does not insert mentions: it permits only the owner supplied by the durable outbox entry, with all automatically parsed mentions disabled. Agent-written `@everyone`, role mentions, and other user mentions therefore cannot trigger notifications. Commentary and tool activity use no permitted mentions.

`render_tool` emits only the status marker, a sanitized tool name, and elapsed duration. Tool arguments, shell commands, output bodies, provider errors, browser credentials, and interaction tokens never appear in tool rows. The durable runtime records outcomes before delivering a final response.

## Delivery guarantees

Every message send uses a deterministic 25-character nonce derived from the durable outbox item ID, with Discord's `enforce_nonce` option. Retrying one item within Discord's recent-nonce window returns the existing message rather than creating another. Discord documents this window as the past few minutes; it is not a permanent idempotency key. A crash after Discord accepts a send but before the local outbox commit can produce a duplicate if recovery happens beyond that window. Local durability alone cannot eliminate this external delivery ambiguity.

The daemon enables `with_state_dir`, creating a private `discord-ingress.sqlite` journal with SQLite WAL and `synchronous=FULL`. Each authorized message and its gateway replay cursor commit in one transaction before the message enters the in-process queue. READY session metadata and ignored dispatch cursors also checkpoint. An earlier pending message remains in the journal even when later ignored events advance the cursor. No bot or interaction tokens enter this journal.

On startup, unacknowledged messages replay before any Discord network request. The runtime deduplicates them in its own durable inbox, then calls `acknowledge(id)` to delete the transport's pending row. A crash between admission and acknowledgment replays a duplicate safely. A failed checkpoint rolls back both cursor and message and stops the gateway, preserving the prior replay position. Revoked users are filtered again during replay. Saved state is tied to the Discord application ID.

The saved gateway session, sequence, resume URL, and bot ID also allow Discord resume after a daemon restart. Graceful shutdown drops the connection without an invalidating normal-close handshake. Remote resume remains subject to Discord's session retention: prolonged downtime, expired sessions, or a nonresumable gateway close require a new session. Messages received before the last journal transaction can be recovered through remote resume while that session remains valid. If Discord invalidates that session first, those uncommitted messages are not recovered automatically from channel history. Once a prompt reaches the local ingress journal, its replay survives independently of remote session validity. Slash commands are ephemeral control operations and are not replayed after a crash.

REST calls retry rate limits, connection failures, and HTTP 5xx responses at most six attempts. Rate-limit delays above sixty seconds fail the current delivery attempt so the runtime can retry durably later. HTTP 4xx responses other than rate limits fail immediately. Requests have a twenty-second timeout; initial interaction acknowledgments have a two-second deadline and execute no command if acknowledgment fails. Error messages contain status codes and fixed descriptions, never token-bearing URLs or response bodies.

Protocol references: [Gateway](https://docs.discord.com/developers/events/gateway), [Message resource](https://docs.discord.com/developers/resources/message), and [Interaction responses](https://docs.discord.com/developers/interactions/receiving-and-responding).
