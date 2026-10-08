<img src="assets/icon.svg" width="64" height="64" alt="">

# Pantheon

An always-on Discord agent harness in Rust, packaged as a NixOS service. Memory uses the design in [OptChat](https://gist.githubusercontent.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449/raw/3c190e06f34aba0c69f49042c526093269604935/optchat.md): an immutable chat log, durable binary summary tree, incremental cache-friendly view, and fresh provider session each turn.

Pantheon includes OpenAI Responses and Anthropic Messages adapters, background-only subagents, prompt steering, durable wakeups and change monitors, and bundled Camoufox windows with a shared profile and separate authenticated noVNC viewers. It runs its own agent loop; Prime Agent is not required.

## NixOS

Add this repository as a pinned flake input and import `pantheon.nixosModules.default`:

```nix
services.pantheon = {
  enable = true;
  applicationId = 123456789012345678;
  allowedUsers = [ 123456789012345678 ];
  environmentFile = "/run/secrets/pantheon.env";
  settings.agent = {
    model = "openai/gpt-5.6";
    compactor_model = "openai/gpt-5-mini";
    reasoning = "medium";
  };
};
networking.firewall.interfaces.tailscale0.allowedTCPPortRanges = [
  { from = 6080; to = 6180; }
];
```

The environment file contains `DISCORD_TOKEN` and any selected API provider keys (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY`). `codex/model` instead uses a ChatGPT subscription login from the service account's Codex credential cache; the Nix package includes the official CLI for login/refresh. Keep secrets outside the Nix store. The service account owns `/var/lib/pantheon`; its configured workspace is the tool working directory. Browsers, Python driver, Xvfb, x11vnc and noVNC are all part of the pinned Nix closure, with no runtime browser download. See [provider authentication](docs/auth.md) for existing-login reuse and service setup.

Create a Discord bot, enable the **Message Content Intent**, and invite it with `bot` and `applications.commands` scopes and View Channel, Send Messages, Read Message History and Embed Links permissions. Only configured `allowedUsers` can invoke it. DMs are admitted directly; guild messages must mention the bot. Mention the bot again to steer a running guild turn. Replies from other bots are ignored.

## Development

```sh
nix develop
cp pantheon.toml.example pantheon.toml
# Edit application_id, allowed_users, and workspace; load protected credentials into your environment.
cargo run -- --config pantheon.toml doctor
cargo run -- --config pantheon.toml run
```

`nix run . -- --config /absolute/path/pantheon.toml run` uses the complete packaged runtime. Without Nix, the Rust daemon builds with Cargo, but browser actions require the runtime paths described in [browser.md](docs/browser.md). `doctor` performs local checks without calling paid APIs.

## Discord interaction

| Command | Behavior |
|---|---|
| `/context` | Square grids for recorded model-window usage and compacted memory; cache and token breakdown |
| `/cache` | Measured cache reads/writes, recent root requests and fresh-view prefix stability |
| `/status` | Active agent phases, shell jobs, message queues, schedules and delivery |
| `/model id:provider/model` | Persist model selection for the next turn |
| `/reasoning level:medium` | Persist reasoning effort for the next turn |
| `/stop` | Cancel active master and its background agents |
| `/subagents` | Named workers, settings and state; idle archived workers are hidden |
| `/skills` | Task guide catalog; inspect one with `id:<name>` |
| `/mcp` | Configured integrations available in this channel |
| `/browser` | List owned browser health and viewer links; manage handoff/resume |
| `/wakeup action:add schedule:in 10m prompt:…` | Durable one-shot or repeating wake |
| `/monitor action:add command:… interval_seconds:30` | Wake when a bounded command's output changes |

Slash replies are private. Ordinary prompts and agent output use the originating channel. Progress prose and activity are plain messages; only the completion after all current work has finished replies to the loop's original prompt and notifies its author. Further input steers that loop without replacing its reply target. Fenced activity accumulates consecutive tool events; prose closes the fence, and subsequent events open a new one at the bottom of the conversation. Full fences roll over without dropping events. Named spawns show IDs, model/effort and incoming reports arrive individually as workers finish. Worker tool calls and owned shell/scheduler notifications remain private; the main timeline shows coordinator actions and delivered worker reports. Input reactions show 📥 received, 🧠 submitted to the model, and ✅ settled; steers share the loop's final settlement. Typing covers inference, context settling and background work. Slash commands use branded embeds with structured fields. Activity exposes names and status rather than arguments/output. Long replies split on Discord's UTF-16 budget with code fences preserved. Provider reasoning can be shown with `agent.show_reasoning=true`, without persisting it locally.

Every subagent runs in the background and reports to the master. There are no foreground children or wait/poll tools. By default the root has the full execution toolkit: it handles short or tightly connected work directly and delegates substantial independent work, parallel investigations and noisy exploration. Workers inherit the settled channel memory at spawn and can zoom into earlier findings. Set `agent.coordinator_root = true` for a strict coordinator that delegates all execution; existing configurations that explicitly set it retain that behavior. Root and workers have [web search and fetch](docs/web.md): hosted search with cited source URLs, and credential-free HTTP fetching with a durable, revalidating cache. Browser windows share one durable `pantheon-shared` profile and X display; each window group gets its own loopback VNC and noVNC listener on `0.0.0.0`. Browser handoff pauses automation until explicit resume. Returned LAN/Tailscale URLs are candidates; routing and remote firewall access cannot be established by enumerating local interfaces.

Idle workers and coordinators leave default agent listings after one hour (`agent.agent_idle_seconds = 3600`). Their identities, histories and schedules remain durable. All agents can discover and message same-channel workers; coordinators can also find and message known neighboring coordinators using `list_agents(kind="coordinators")` and `tell(id="channel:<id>", message="...")`. `include_archived=true` finds hidden identities; `tell` automatically restores and wakes them, as do user input and scheduled notifications. Memory stays channel-owned; coordinator messages explicitly carry information across channels.

Root and workers have [skill discovery/loading](docs/skills.md) and [MCP integrations](docs/mcp.md). Bundled guides cover product research, browser activities, learning and engineering. Configured SKILL.md libraries load on demand behind a small startup index. MCP supports stdio and Streamable HTTP tools, resources and prompts, with role/channel controls and durable retrieval of large results. Schemas stay fixed while external catalogs enter tool results. The shared behavior guide treats explicit requests about the user's own environment and authorized credential entry as ordinary work, carries tasks through to evidence, and scales process to everyday interactions.

## State and checks

[Cache diagnostics](docs/cache.md) distinguish provider-reported reuse from local
prefix stability. [Transport diagnostics](docs/transport.md) correlate provider failures with timing, stream progress and underlying transport causes. [Tool activity](docs/tool-activity.md) shows compact previews,
line counts, exit codes and original-message updates for detached shells. Steering
queues behind requested tool execution; `/stop` explicitly cancels work.

Back up the state directory. `chats/<channel>/main/` and `tree/` contain immutable daily JSONL records; operational SQLite journals contain inbox/outbox, tasks and schedules; `subagents/` holds private child traces; `browsers/pantheon-shared/profile/` holds shared browser state, and the other browser directories hold viewer metadata and screenshots. State is never committed to this repository.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked --all-targets
nix flake check
```

Stop the service before offline memory operations:

```sh
pantheon --config pantheon.toml export --channel 123 --output memory.html
pantheon --config pantheon.toml import --channel 123 --file old-history.txt
```

Crash recovery resumes queued prompts, wakeups and unsent output. Interrupted turns are reported rather than replaying uncertain tool side effects. Discord delivery is at least once; nonce deduplication has a finite remote window. Complete provider responses are rendered per step; token-by-token prose streaming is not implemented yet. Live bot/model validation requires your credentials.

See [architecture](docs/architecture.md), [memory](docs/memory.md), [Discord](docs/discord.md), [browsers](docs/browser.md), [web tools](docs/web.md) and [authentication](docs/auth.md) for invariants, tradeoffs and recovery details.

## Development and releases

Use Conventional Commits, for example `feat(browser): share login state` or `fix(runtime): deliver a completion once`. PR titles and development commits are checked in CI. `Cargo.toml` is the version source; `Cargo.lock` must agree and the Nix package reads that version directly.

Like Thoth, PRs to `main` require a stable SemVer increase unless labeled `no-release` for changes that do not warrant a release. Protected `main` requires an up-to-date PR with passing `version`, `rust` and `nix` checks, squash/rebase merges, and no force push or administrator bypass. On `main`, Rust formatting, Clippy, tests, Nix builds and the live packaged-browser test must pass before CI tags the exact validated commit and publishes a GitHub release. Existing tags never move; an interrupted release can be repaired by rerunning at its original commit. Install a release with `nix run github:GustavoWidman/pantheon/v0.5.0` or pin that tag in your NixOS flake. Service deployment remains controlled by the consuming NixOS configuration.

Rust and Nix checks run when a PR is opened, updated or reopened, not on development-branch pushes. The release workflow reuses those checks on `main` before publishing.

Both root and workers can own wakeups and monitors. Shell commands return a background job ID after five seconds by default; their results reach the owning agent’s durable inbox. An idle worker resumes under its existing ID. Successful worker turns with running shells or queued inbox work keep their report private until that work finishes; future scheduled wakeups and monitor intervals do not hold reports. See [architecture](docs/architecture.md) for delivery, cancellation and recovery behavior, and [browser ownership](docs/browser.md) for shared storage and handoff details.

[Model discovery and chat-local overrides](docs/models.md) cover `/model` and `/reasoning` autocomplete and the agent’s `models` catalog tool. `pantheon models` performs live discovery without inference. Agents can compare dated official API prices and Codex credit rates, including cache/context bands; included subscription quota and API references remain separate.

### Nix CI compilation

After installing Nix, CI uses `DeterminateSystems/magic-nix-cache-action` to
cache build outputs in GitHub Actions across runs. GitHub Actions caching is
explicitly enabled and FlakeHub caching is disabled; no additional credentials
or OIDC permissions are required. The reusable release checks use the same
cache setup. Cache misses still build normally, and the first run may be cold.

The Nix package keeps its optimized Rust executable (`pantheon.unwrapped`)
separate from the browser-worker/runtime wrapper. Cargo's source set includes
`Cargo.toml`, `Cargo.lock`, `src/`, embedded `skills/`, and Rust integration tests;
changes to documentation, Python tests or the browser worker do not recompile
Rust. Browser-worker changes still produce a new final package and run the
packaged-browser smoke test. `tests/test_nix_packaging.py` checks these cache
boundaries by evaluating real derivations after source mutations (Nix required).

The installed executable retains the release profile, including thin LTO.
Nix runs all Rust test targets and doctests using Cargo's normal test profile rather than
release LTO: Rust tests require unwinding, so they cannot reuse the release
binary's abort-on-panic build. This avoids expensive redundant test optimization
without dropping tests. The Rust CI job still runs formatting, Clippy, Rust and
Python tests, and the Nix job still builds the package/runtime and exercises the
packaged browser. Opt-in credential-dependent tests remain opt-in.

Superseded PR check runs are cancelled; reusable release checks use a unique
run group and are never cancelled by a newer PR or release run.
