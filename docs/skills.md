# Evolving skills and private curation

Pantheon starts with `research`, `browser-activities`, `learning` and `engineering`.
These are editable starting seeds in one instance-wide library: the curator can
revise, merge, split or retire them just like guides it creates. Origin records
provenance, not authority. Skills never grant permissions or override user requests.

## Catalogue and cache generations

The orchestrator and workers receive every active skill in their system catalogue,
including explicit-only guides. Each entry contains ID, name, revision, invocation
count, curator refinement count, explicit-only status and a bounded description.
`curator.description_chars` defaults to **240 Unicode characters**, including the
ellipsis when elided. Curators and reviewers receive this exact limit so they can
put useful invocation conditions first. There is no 16-skill preview cutoff.

Invocation count means a successful main-guide load, once per skill per ordinary
agent turn; pagination and repeated loads do not inflate it. Previews, supporting
resources and curator/reviewer inspection do not count. Refinements count approved
curator modifications, excluding initial creation, seed import and manual rollback.
These are historical usage/refinement counts, **not success rates or proof of
battle-tested behavior**.

The exact catalogue text and counter values are frozen per channel and persisted
across restart. Continuous turns keep these bytes unchanged. On the next fresh
orchestrator turn after the configurable idle period (300 seconds by default),
once memory has settled, the complete catalogue refreshes. The catalogue appears
last in the system prompt so changing it preserves the earlier constant prefix.
Five minutes is our refresh policy, not a measurement of provider cache expiry.
Within a turn, the system and tool schemas never change.

`skill(action="list")` still provides paged discovery. `preview` returns a bounded
introduction; `load` returns the main guide or a supporting text resource with
`file`, `offset` and `max_chars`; `history` exposes durable revisions. Published
notifications refresh available skill revisions at steering boundaries, leaving
the system text unchanged. This makes an announced new guide immediately loadable.

## Private forks and per-channel queues

Each channel has its **own durable queue and one active curation job**, including
review. Different channels can curate concurrently. Repeated eligible settles
coalesce into the latest pending work in that channel; there can be one active job
plus one pending job. A processed, superseded or cancelled generation cannot be
automatically queued again.

An ordinary orchestrator turn must complete at least `minimum_steps` model
iterations (default three) before it is eligible. These are successful model loop
iterations, not Discord messages, transport retries, or curator/reviewer requests.
Separate single-response chats never accumulate into eligibility. Worker-only
turns do not enqueue curator jobs. Eligible work waits for the channel to have no
ordinary queued/running roots, workers or shells, settled memory, and five minutes
of idle time since its last orchestrator turn settled. Other channels do not gate
this one. `/curator action:run` bypasses the idle debounce for already eligible
queued work; it cannot manufacture eligibility or bypass memory settlement.

At startup the job captures an immutable, read-only copy of the orchestrator's
settled memory view and original messages/summary nodes. `zoom` and `date` read
that frozen snapshot, including after the main conversation resumes. Private
model conversations are constructed from this same settled context under curator
or reviewer instructions. Cross-model forks do not replay another model's opaque
native reasoning/signatures. They inherit the service's operator constraints.
Model and effort are pinned for the entire job and all its reviewers.

New conversation never waits for, cancels, or receives the private transcript of
a running curator. Curators and reviewers use separate cache affinities and do
not occupy ordinary worker slots or overwrite main usage accounting. They cannot
write memory, tell the orchestrator, continue its task or create background work.

## Tools and publication

The drafting fork receives only:

- `zoom`, `date`, `read`, `web_search`, `web_fetch`;
- `skill`: list, preview, load, history, immutable revision and package seed offers;
- `create_skill`, `edit_skill`, `retire_skill`: private staged changes.

Create/edit requires ID, name, description, body, a factual change summary and a
purpose explaining when it helps. `body` is the complete Markdown body without
YAML front matter; the harness generates the header from the separate fields and
preserves existing metadata. A leading YAML metadata block returns a staging error
without changing the proposal, allowing correction in the same drafting pass
before reviewers run. Markdown horizontal rules and fenced YAML examples are
allowed. Optional supporting files replace only supplied paths; other resources
and YAML metadata remain intact. Repeated edits collapse
into one final before/after change. Retirements require summary and purpose.
At most four related guides can change together, bounded to 48 KB of staged JSON.
Drafting uses the pinned source revisions, so concurrent publications cause a
stale-head rejection rather than silently overwriting a newer guide.

When the curator finishes normally, no changes is a successful outcome. Otherwise
the harness starts parallel private reviewer loops (two by default), sharing the
same frozen memory, model and effort. They receive read-only tools and `decide`,
not skill mutation or messaging tools. One focuses on procedure and evidence;
another focuses on transfer, duplication and preservation. All reviewers must
approve the complete atomic change set with observed evidence and transferable
scope. A denial, missing verdict, timeout, stale head or cancellation cannot publish.
One difficult task can teach a useful technique; there is no arbitrary repeat-count
requirement or unrelated withheld-task gate.

This is **model-based evidence review**, not executed sandbox practice or an
empirical improvement benchmark. Recorded tool checks are evidence; an agent's
claimed success is not proof. Research may add current information but reviewers
must distinguish it from what the original work established. Private proposals,
review context (including the frozen rendered memory view), verdicts and usage are
retained for inspection. Failed/interrupted work is not silently published or
replayed after restart; saved proposals survive.

## Notifications and controls

Approved publication atomically records revisions and channel notifications in
`skills.sqlite`. Independent delivery ledgers track Discord receipts and main
context admission. The bridge into `runtime.sqlite` and memory uses deterministic
IDs and durable acknowledgements, rather than claiming a transaction across two
WAL databases.

Activity receipts use the existing chronological fence accumulation:

```text
✦ curator · created skill deployment-verification
↻ curator · modified skill engineering
⊖ curator · retired skill legacy-browser-handoff
```

Receipts never admit a main-thread prompt. While the orchestrator is idle, change
notes wait for the next real user, wakeup, monitor or worker input. The incoming
message is prefixed after the unchanged memory blocks:

```xml
<system-notification>
  <curator-skill-modify name="engineering" revision="7">Added executable and
  gateway verification. Use for deployment work.</curator-skill-modify>
</system-notification>
```

During an active turn, notes append at an ordinary steering boundary after every
outstanding tool result. Notifications arriving after the final steering boundary
remain pending for the next real input. Only approved procedural change metadata
enters the main journal; private research/reviewer transcripts do not. Journal
provenance deduplicates admissions if a crash occurs before the notification ack.

`/skills` opens a private dashboard with a home page, paginated skill selector,
metadata/counters, complete guide pages, supporting resources, history and revision
comparisons. Large content is paginated rather than clipped to one embed. Controls
are bound to the originating channel and user and checked before interaction
updates. Existing `action:history`, `action:rollback`, `action:proposals` and
`action:proposal` remain available.

`/curator` shows this channel's phase, queue, pinned/current settings, spawned,
running and finished reviewers, and verdict. `action:run` requests queued work;
`action:cancel` cancels this channel's pending/active job without stopping its main
conversation. `/model kind:curator model:none` explicitly inherits the main model;
`model:default` restores the configured default. `/reasoning kind:curator
level:inherit` explicitly inherits main effort; `level:default` restores config.
Chat/compact/curator selections are channel-local and survive restart.

## Configuration and durability

```toml
[skills]
bundled = true
directories = ["/var/lib/pantheon/seed-skills"]

[curator]
enabled = true
# model = "codex/YOUR_MODEL"  # omitted means main model
# reasoning = "low"          # omitted means main effort
idle_seconds = 300
minimum_steps = 3
description_chars = 240
reviewers = 2
timeout_seconds = 900
max_steps = 8
review_steps = 4
max_input_chars = 256000
max_research_calls = 4
```

Defaults bound the whole pass to fifteen minutes, eight drafting steps, four steps
per reviewer, 256,000 characters per request and four hosted-search calls shared
across the job. Each hosted search allows at most two provider continuations and
records reported usage for diagnostics. There is no cumulative token cap: rereading
inherited context, including cached input, never aborts drafting or review. Legacy
`curator.token_budget` settings are accepted and ignored. The configurable
`timeout_seconds` deadline starts when the fork executes, after the idle wait, and
covers drafting plus all reviewers together. Cancellation, step, request-size and
research-call limits still apply.

For NixOS use `services.pantheon.skillDirectories`, `bundledSkills`, `skillCurator`
and `settings.curator`. Seed files include `SKILL.md` with YAML name/description
and supporting UTF-8 text. Scripts are returned as text, never executed implicitly.
Binary resources and symlinks are unsupported. Import bounds: 512,000 bytes/file,
64 files/2 MB per guide, 256 active skills/32 MB active serialized content and 64
configured seed directories. All active skills appear in the catalogue; request
budgets fail explicitly instead of silently dropping entries.

Seeds initialize revision one in separate SQLite WAL state with full sync and an
exclusive writer lock. Changed package seeds become reconciliation offers, never
overwrite learned heads or revive retirements. Publication compares expected
revisions and commits all changes together. Rollback restores old content as a
new revision, retaining history. Preserve `skills.sqlite` with the service state.
Existing memory journals, trees and cached views are never rewritten or wiped.
