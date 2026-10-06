# Model selection and discovery

`/model [kind] [provider] [model]` changes this Discord chat's model override.
`kind` autocompletes `chat` and `compact`, defaulting to `chat`. Provider suggestions
include authenticated transports only. With a provider selected, model suggestions
come from that provider; without one, they search all available providers. Suggested
values contain the full `provider/model-id`, avoiding ambiguous names across providers.
Select `default` to return to the corresponding config default. Manually entered
IDs are accepted with an available provider; catalog membership is not proof of quota
or endpoint access. Mismatched provider/model pairs are rejected before any write.

Overrides live in the operational SQLite database, keyed by Discord channel ID.
Another channel uses its own selection or config defaults. Nothing writes the global
config. Chat changes apply to the next fresh turn. A compactor job captures its model
when scheduled and keeps it through shortening attempts; later jobs use the latest
chat-local compactor selection. Existing summaries remain durable and are never
recomputed because a model changed. Compactor reset removes the override so later
config defaults can take effect. Model and reasoning commands serialize within each
channel to prevent concurrent changes from overwriting one another.

`/reasoning level` autocompletes the selected chat model's advertised efforts. Known
unsupported manual choices fail with the available levels. Models exposing `max` or
`ultra` can use those values. A model switch retains a compatible effort or selects an
advertised default/compatible level. Legacy internal `minimal` requests can map to
`low` where advertised, while the picker never claims `minimal` exists if it does not.
For catalog entries with no effort metadata, autocomplete returns no guessed levels;
manual levels retain transport validation. Claude catalogs advertising adaptive
thinking use adaptive thinking and `output_config.effort` during inference.

Discovery runs in the background at startup and every five minutes. Interaction
autocomplete reads a local snapshot and replies with Discord callback type 8 within
the callback deadline; it never waits for a provider or executes a command. Unauthorized
autocomplete receives an empty result. Choices are filtered, bounded to 25, and comply
with Discord's value/name limits. Last-known catalog snapshots are atomically persisted
with fsync, retaining suggestions during a listing outage and across restart.

Codex uses its authenticated native `/models` endpoint with account/residency headers
and compatibility version `0.160.0`, independently of the bundled CLI used for credential
rotation. This avoids older helpers returning a smaller picker catalog or downgrading
the CLI's own model-cache file. The protocol version is a compatibility parameter,
not a fixed model list. Visible entries and their effort metadata come from the server.
OpenAI and Anthropic API-key transports call their `/v1/models` endpoints; Anthropic
pagination is followed, while non-text OpenAI model families are filtered from selection.
Discovery does not start a model conversation or spend inference tokens. On cold
startup, the Codex CLI cache and configured IDs provide explicitly labeled fallbacks.

All agents have the `models` tool with optional `provider`, `query`, `offset`, and
`limit` filters. Results include exact IDs, advertised efforts/defaults, observation
time and source, provider counts, and `next_offset` for pagination. A fixed prompt
instruction tells agents to consult this tool before choosing worker overrides;
catalog refreshes do not rewrite the system prompt or tool schemas. Prices are `null`
when catalogs do not expose them, rather than guessed from model names. Codex subscription
quota is distinguished from API billing. The current catalogs do not expose token prices.

For live, non-inference discovery outside Discord:

```sh
pantheon --config /path/to/pantheon.toml models
pantheon --config /path/to/pantheon.toml models --provider codex --query luna
```

This command can run alongside the daemon; it does not open canonical memory or
acquire the daemon writer lock.

Protocol references: [OpenAI models](https://developers.openai.com/api/reference/resources/models/methods/list),
[Claude models](https://platform.claude.com/docs/en/api/models/list),
[Discord autocomplete callbacks](https://docs.discord.com/developers/interactions/receiving-and-responding).
