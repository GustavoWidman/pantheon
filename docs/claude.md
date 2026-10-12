# Optional Claude Code background tasks

Pantheon can expose a `claude` tool only when it is explicitly enabled. It is
**not** a model provider, OAuth relay, history adapter, or replacement for
Pantheon tools: each call starts one independent, noninteractive Claude Code
process and returns its report through the calling agent's durable inbox.
Treat the returned text as external/untrusted work, review it, and use ordinary
Pantheon tools for any consequential follow-up.

## Installation and service path

Install the current official native CLI yourself for the same OS user that runs
Pantheon, following Anthropic's installer:

```sh
curl -fsSL https://claude.ai/install.sh | bash
```

Sign in directly as that service user, then verify `claude auth status`. Do not
put a Claude credential in Pantheon configuration or its environment.

Pantheon deliberately does **not** bundle Claude Code or install it through
Nix. Native Claude Code updates independently. For NixOS, arrange for the
manually installed executable (commonly `~/.local/bin/claude`) to be on the
service's actual `PATH`—for example by extending the systemd service PATH or a
small wrapper. `services.pantheon.extraPackages` may help construct PATHs for
other commands, but it must not supply a Claude package.

## Configuration

The default is disabled; disabled installations do not resolve or inspect a
Claude executable. Enabling it makes daemon startup run a bounded `claude auth
status` check. It fails clearly unless `claude` is discoverable on the service
PATH and reports a Claude.ai login. API-key/backend override environment
variables are removed only from the child process so an inherited Pantheon API
key cannot silently bill Claude API usage.

```toml
[claude]
enabled = true
# executable = "claude"       # resolved through the daemon's PATH
model = "opus"                # official CLI alias
# effort = "medium"           # low, medium, high, xhigh, max
max_jobs = 2
timeout_seconds = 900
max_task_chars = 12000
max_output_chars = 30000
# Read-only default. Deliberately opt into any broader built-in CLI tools.
allowed_tools = ["Read", "Glob", "Grep"]
```

A call accepts a required `task`, plus optional `name`, workspace-relative
`cwd`, `model`, and `effort`. Prompt text goes over stdin, never through a
shell. The subprocess uses print/JSON mode, safe/restricted mode, no session
persistence, no settings/skills/hooks/plugins/MCP, `dontAsk` permissions, and
the configured allow-list. It runs with a process group: `/stop`, shutdown and
timeout kill descendants. A restart records an interrupted report and never
replays a task.

Claude's `--tools` allow-list is an operational permission boundary, not an OS
sandbox. Keep the default read-only set unless the user has authorized a
specific broader capability and the service's filesystem/network isolation is
appropriate.
