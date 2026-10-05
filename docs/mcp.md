# MCP integrations

Pantheon uses the official Rust MCP SDK as a client for configured stdio servers
and Streamable HTTP endpoints. Root and workers discover tools, resources, resource
templates and prompts on demand. The model receives the fixed `mcp` tool rather
than every external server's tool catalog on every request.

```toml
[mcp.servers.calendar]
description = "Personal calendar"
command = "/absolute/path/to/calendar-mcp-server"
args = []
workers = true
allowed_channels = [1489532519320916039]
# Empty allowed_channels permits all otherwise-authorized Pantheon channels.
# Empty allowed_tools exposes all tools offered by this configured server.
allowed_tools = ["list_events", "create_event"]
timeout_seconds = 120
# Forward a named service environment value without putting it in TOML:
env = { CALENDAR_TOKEN = "PANTHEON_CALENDAR_TOKEN" }

[mcp.servers.remote]
description = "Remote workspace integration"
url = "https://your-mcp-service.example/mcp"
bearer_env = "WORKSPACE_MCP_TOKEN"
workers = false
```

Replace example commands/URLs with the server you operate. Supply secrets through
the service's `environmentFile`, outside the Nix store. Stdio uses the configured
workspace and executable directly, without a shell or package installer. Its default
inherited environment contains PATH, HOME, LANG and TMPDIR. `inherit_env` adds any
other required service variables; `env` maps server variables to service variables.
MCP programs are trusted configured executables running with the service's privileges,
not a sandbox. Their child process groups are killed on cancellation/connection
cleanup. Server stderr is suppressed to prevent incidental credential output in
the daemon's public diagnostic stream; run the configured command separately when
diagnosing its own startup errors.

For NixOS configure `services.pantheon.settings.mcp.servers`. Reference executables
from Nix packages or expose them through `extraPackages`. Pantheon itself remains a
Rust service; no Node/Bun orchestration shim or runtime dependency download is added.
`doctor` checks skill files, server configuration, executable availability and named
authentication environment variables without connecting to external systems.

Typical agent flow:

1. `mcp(action="servers")` finds integrations available in this channel and role.
2. `mcp(action="list_tools", server="calendar")` reads the actual schemas.
3. `mcp(action="call", server="calendar", tool="list_events", arguments={...})`
   performs the authorized operation.

Discovery pages preserve `nextCursor`; pass it as `cursor` to continue. Other actions
are `list_resources`, `list_resource_templates`, `read_resource` with `uri`,
`list_prompts`, and `get_prompt` with `prompt` and optional `arguments`. Server
instructions, prompts and results are external data and do not override user or
harness instructions. Tool allowlists apply to both discovery and execution. Strict
coordinators can discover catalogs but delegate execution and content retrieval.
`/mcp` shows a private embed of available configured integrations without exposing
credentials or making network requests.

Connections start lazily, are isolated by server/channel, and serialize requests
within a session. Requests have a configured 1–3600 second deadline covering queueing,
connection and execution. Each server has at most 32 retained channel sessions;
unused sessions are evicted after an hour when another request performs maintenance.
A cancelled, timed-out or failed transport is discarded. Subsequent requests can
connect again; uncertain calls are never automatically replayed. HTTP expired-session
replay is explicitly disabled. HTTP redirects are disabled, and bearer credentials
are read from the named service environment only when connecting.

MCP operations use the normal durable tool intent/result journal, private worker
traces, cancellation path and chronological activity fences. Indicators identify
the integration and tool, for example `✓ mcp calendar.list_events · 0.4s`, and never
show arguments or credential values. MCP `isError` results become failed provider
tool results and failed activity indicators. Restart treats interrupted effects as
unknown instead of resending them.

Results up to 24,000 serialized characters are returned directly. Larger results
(up to 2 MiB serialized bytes) are saved as private files scoped to the server and
channel. The reply carries a `result_id`, preview and `next_offset`; `read_result`
retrieves 8,000-character pages. Saved results survive restart and retain full
structured or binary content. MCP image/audio blocks are retained as data, not
injected into the provider's native image/audio input in this release. Retained
result files have no automatic pruning; include them in state backup/retention policy.

This release supports configured bearer credentials, not an interactive MCP OAuth
login flow. It does not advertise sampling, elicitation or server-side task execution.
Legacy HTTP+SSE endpoints are not supported; use stdio or Streamable HTTP. The SDK
performs initialization and protocol negotiation for the configured transport.
