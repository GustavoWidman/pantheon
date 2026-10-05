# Provider authentication and subscription access

`openai/model` uses `OPENAI_API_KEY` with the Platform Responses API. `anthropic/model` uses `ANTHROPIC_API_KEY` with the Messages API. `codex/model` uses a ChatGPT subscription login with the Codex Responses endpoint. Selecting one is explicit: Pantheon never silently falls back to a billed API key when subscription access fails.

## Codex / ChatGPT

Pantheon reuses a file-based Codex login at `auth.codex_home/auth.json`. If that setting is omitted, it uses `CODEX_HOME`, then `~/.codex`. The Nix package includes the unmodified official Codex CLI, pinned by nixpkgs, for login and token refresh.

Sign in with the same configuration and operating-system user that will run the service:

```sh
pantheon --config pantheon.toml login-codex --device-auth
```

The command invokes the official `codex login` flow with file credential storage. Omit `--device-auth` to use its browser callback. Signing in requires the user's participation; Pantheon never performs a hidden login. `doctor` checks that the selected credential files/keys are present without making live requests.

For subscription-only inference and compaction, select available Codex models for **both** settings:

```toml
[agent]
model = "codex/gpt-5.6-sol"
compactor_model = "codex/gpt-5.6-sol"

[auth]
codex_home = "/var/lib/pantheon/.codex"

[web]
search_model = "codex/gpt-5.6-sol"
```

These examples use a model tested against the Codex backend during implementation. Available model slugs, search capabilities, and quotas depend on the account; an API model name is not necessarily a Codex model slug. `/model id:codex/<available-slug>` changes the channel model; the compactor and optional separate search model remain configuration settings.

Every request rereads the canonical file, so an existing CLI refresh or relogin takes effect immediately. Pantheon serializes its refresh requests and asks the official app-server's `account/read` method to refresh expiring tokens. The CLI writes the updated canonical cache; Pantheon never stores an independent refresh-token copy or implements its own OAuth rotation. A rejected access token gets one refresh/reload retry. Provider response bodies and auth-helper diagnostics are excluded from traces.

For an always-on NixOS service, use a dedicated login under the service's writable state directory. Its home defaults to `/var/lib/pantheon`, so the default auth path is `/var/lib/pantheon/.codex`. The service's `ProtectHome` prevents borrowing another user's home cache directly. A separate login also avoids refresh contention with an interactive CLI using the same OAuth grant. Credentials stay outside the Nix store; back up and protect them alongside the service state. Keyring-only and ephemeral Codex logins are not imported by this adapter: use the official login command above to create a file-based service login.

The Codex transport uses `instructions`, `store:false` and streaming responses; it buffers until a complete response before dispatching tools. Encrypted reasoning and complete native output items remain in the ephemeral turn transcript. It strips API-only explicit cache metadata for this backend without rewriting message content, and advertises its own Pantheon identity. It does not wrap the Codex agent loop, add Codex's execution tools, or let Codex create foreground workers. Pantheon's coordinator, memory, steering and background-agent rules stay in charge.

OpenAI documents [subscription login and credential storage](https://learn.chatgpt.com/docs/auth) and [app-server authentication/refresh](https://learn.chatgpt.com/docs/app-server#authentication-endpoints). This adapter targets personal/local open-source use. OpenAI distinguishes that from commercial/hosted integrations, for which its [Sign in with ChatGPT integration](https://developers.openai.com/siwc) is the relevant route.

## Claude subscriptions

Hermes has code that reads Claude Code's credential store/setup tokens and sends OAuth requests with Claude Code identity headers and a Claude Code system-prompt prefix. That demonstrates a technical implementation, rather than establishing a stable supported inference interface for a separate harness.

Anthropic's current [authentication and credential-use guidance](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use) directs third-party applications to API keys/cloud credentials and distinguishes them from a user's login inside an **unmodified Claude Code binary**. Pantheon's native Claude provider therefore uses API keys; it does not import Claude Code tokens or pretend to be Claude Code. A future adapter can delegate a bounded task to the real CLI signed into by the operator, but that is a separate execution backend with its own tool loop and context behavior, rather than a replacement key for Pantheon's Messages API client.

## Research references

The implementation was informed by Hermes at commit `7157422022ff06f3e632d1dd394ee1253b17ad37`: its [Codex auth/refresh lifecycle](https://github.com/NousResearch/hermes-agent/blob/7157422022ff06f3e632d1dd394ee1253b17ad37/hermes_cli/auth_codex.py), [Codex request headers](https://github.com/NousResearch/hermes-agent/blob/7157422022ff06f3e632d1dd394ee1253b17ad37/agent/codex_headers.py), [native web search provider](https://github.com/NousResearch/hermes-agent/blob/7157422022ff06f3e632d1dd394ee1253b17ad37/plugins/web/openai_native/provider.py), and [Anthropic adapter](https://github.com/NousResearch/hermes-agent/blob/7157422022ff06f3e632d1dd394ee1253b17ad37/agent/anthropic_adapter.py). Pantheon implements its own Rust transport; no Hermes Python runtime is required.
