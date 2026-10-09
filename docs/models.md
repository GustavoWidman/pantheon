# Model selection and discovery

`/model [kind] [provider] [model]` changes this Discord chat's model override.
`kind` autocompletes `chat`, `compact` and `curator`, defaulting to `chat`. Provider suggestions
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

`/reasoning [kind] level` autocompletes the selected chat, compactor or curator model's advertised efforts, defaulting to chat. Known
unsupported manual choices fail with the available levels. Models exposing `max` or
`ultra` can use those values. A model switch retains a compatible effort or selects an
advertised default/compatible level. Legacy internal `minimal` requests can map to
`low` where advertised, while the picker never claims `minimal` exists if it does not.
For catalog entries with no effort metadata, autocomplete returns no guessed levels;
manual levels retain transport validation. Claude catalogs advertising adaptive
thinking use adaptive thinking and `output_config.effort` during inference.

Curator overrides apply to newly started jobs; all parallel reviewers inherit
that job’s pinned model and effort. `model:none` explicitly inherits the current
main model even when configuration supplies a curator model. `model:default`
clears the override to configuration. `level:inherit` explicitly inherits main
effort for the curator; `level:default` clears a compactor/curator effort override.
The actual effort `none` remains distinct from inheritance. Existing compactor
model reset semantics are unchanged. Autocomplete restores channel-local kind
selections before a channel actor is first opened after restart.

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
catalog refreshes do not rewrite the system prompt or tool schemas.

Pricing is discovered separately from the official
[OpenAI pricing](https://developers.openai.com/api/docs/pricing) and
[Claude pricing](https://platform.claude.com/docs/en/about-claude/pricing) pages, plus
the [Codex credit rate card](https://learn.chatgpt.com/docs/pricing#token-rates), using
their machine-readable `.md` versions. No third-party price fixture is bundled.
The parser selects the Standard text-token table, excluding Batch, Flex, Fast,
training, audio, tools and cloud-provider tables. Schema changes fail closed,
retaining the last dated snapshot instead of silently interpreting another table.
Prices refresh every six hours alongside catalog discovery; failed refreshes retry
on the next five-minute discovery cycle. Successful snapshots are atomically saved
with fsync to `state_dir/model-pricing.json`, and retained across restart and outages.
`observed_at` records when the document was fetched, not its publication date.
Snapshots older than 24 hours (or dated in the future) are marked `stale`.

API model rows expose `pricing` with `currency: USD`, `unit: per_1m_tokens`,
`service_tier: standard`, source URL, age, match identity/method and separate
context-band `rates`. These contain input, output, cached input, cache write and,
for Claude, one-hour cache write rates when advertised. Missing rate categories
are `null`, not zero. Numeric context bounds are extracted only when explicitly
published in the OpenAI document; otherwise consult the source's context labels.
Claude uses the main base-rate table; any model-specific long-context exceptions
still require consulting the source. These public list prices omit account discounts,
geography/tier premiums, tool fees and taxes, and are not invoice estimates.

OpenAI prices require an exact model-ID match; unknown IDs, snapshots and fine-tunes
are not guessed from a family name. Claude table labels are normalized (for example,
`Claude Sonnet 4.6` becomes `claude-sonnet-4-6`) and matched against the exact catalog
ID, or its provider-advertised display name. The result states `matched_model` and
`matched_by`. Prices do not grant access or add models to autocomplete.
Codex `pricing` uses `unit: credits_per_1m_tokens`, `currency: null` and
`applicability: credit_billed_usage_only`. These are published Standard rates for
credit-billed usage, with exact normalized public labels (`GPT-6.1 Sol` maps to
`gpt-6.1-sol`); cyber aliases and image-modality rows are not collapsed into text
model IDs. Input, cached input and output rates are explicit. Codex has no separate
cache-write charge; the null cache-write category does not mean zero input cost.
Credit purchase prices/conversion depend on the plan or agreement, and some Enterprise
customers still use a legacy rate card. Included subscription quota weights and
remaining limits stay unknown: OpenAI explicitly says credit rates alone do not
predict included usage. No dollar conversion is guessed. Where an exact OpenAI ID
matches, `api_price_reference` contains a separate USD API comparison with
`applicability: api_reference_only`; this is not subscription billing or quota.

The root orchestration prompt instructs agents to choose economical capable workers
for routine work and stronger models/reasoning for difficult work, honoring user
preferences and considering total context, output and retries. It explicitly says
prices are not capability scores, unknown is not free, credits are distinct from
dollars, and neither rate card can be substituted for included subscription quota.
Rates remain in on-demand tool
results, not a changing system-prompt table, preserving the fixed cache prefix.

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
