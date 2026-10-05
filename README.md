<img src="assets/icon.svg" width="64" height="64" alt="">

# Pantheon

An always-on Discord agent harness in Rust, packaged as a NixOS service. Memory follows [OptChat](https://gist.githubusercontent.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449/raw/f51fe5c910427fd6f384d22823140b1693c76207/optchat.md): an immutable chat log, durable binary summary tree, incremental cache-friendly view, and fresh provider session each turn.

Pantheon includes OpenAI Responses and Anthropic Messages adapters, background-only subagents, prompt steering, durable wakeups and change monitors, and bundled Camoufox browsers with separate displays and permanent authenticated noVNC viewers. It runs its own agent loop; Prime Agent is not required.

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

The environment file contains `DISCORD_TOKEN` and `OPENAI_API_KEY`, or `ANTHROPIC_API_KEY` for Anthropic models. Keep secrets outside the Nix store. The service account owns `/var/lib/pantheon`; its configured workspace is the tool working directory. Browsers, Python driver, Xvfb, x11vnc and noVNC are all part of the pinned Nix closure, with no runtime browser download.

Create a Discord bot, enable the **Message Content Intent**, and invite it with `bot` and `applications.commands` scopes and View Channel/Send Messages permissions. Only configured `allowedUsers` can invoke it. DMs are admitted directly; guild messages must mention the bot. Mention the bot again to steer a running guild turn. Replies from other bots are ignored.

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
| `/context`, `/status` | Memory, queue, delivery and last provider usage |
| `/model id:provider/model` | Persist model selection for the next turn |
| `/reasoning level:medium` | Persist reasoning effort for the next turn |
| `/stop` | Cancel active master and its background agents |
| `/subagents` | List task IDs and states |
| `/browser` | List owned live browser links; manage handoff/resume |
| `/wakeup action:add schedule:in 10m prompt:…` | Durable one-shot or repeating wake |
| `/monitor action:add command:… interval_seconds:30` | Wake when a bounded command's output changes |

Slash replies are private. Ordinary prompts and agent output use the originating channel. Final replies mention the requester once; intermediate prose and tool rows do not ping. Tool rows reveal names and status rather than arguments/output. Long replies split on Discord's UTF-16 budget with code fences preserved. Provider reasoning can be shown with `agent.show_reasoning=true`, without persisting it locally.

Every subagent runs in the background and reports to the master. There are no foreground children or wait/poll tools. Every browser keeps its own persistent profile, X display, loopback VNC and noVNC listener on `0.0.0.0`. Browser handoff pauses automation until explicit resume. Returned LAN/Tailscale URLs are candidates; routing and remote firewall access cannot be established by enumerating local interfaces.

## State and checks

Back up the state directory. `chats/<channel>/main/` and `tree/` contain immutable daily JSONL records; operational SQLite journals contain inbox/outbox, tasks and schedules; `subagents/` holds private child traces; `browsers/` contains profiles and screenshots. State is never committed to this repository.

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

See [architecture](docs/architecture.md), [memory](docs/memory.md), [Discord](docs/discord.md) and [browsers](docs/browser.md) for invariants, tradeoffs and recovery details.
