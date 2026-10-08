# Skills

Pantheon starts with four task guides: `research`, `browser-activities`, `learning`
and `engineering`. They seed one **evolving library** shared by root and workers.
The curator can revise, merge, split or retire them just like guides it creates.
Origin (`seed` or `curator`) records provenance, not authority. Ordinary conversation
and simple requests need no workflow ceremony. Skills never grant permissions or
override the user's instructions.

A skill is a directory containing `SKILL.md` with YAML frontmatter:

```markdown
---
name: product-comparison
description: Compare purchases using current prices, relevant constraints and primary sources.
---
Use when choosing between products with identifiable constraints. Discover the
criteria, compare like-for-like prices and specifications from primary sources,
and check that the recommendation satisfies those criteria. Prices, products and
budgets are inputs to discover for each task.
```

Descriptions guide discovery; the body supplies the method. Supporting UTF-8 text,
including references and scripts, may live alongside the guide. Import snapshots
all supporting text with the main guide. Scripts are returned as text; loading them
does not execute them. Materialize a loaded script in the workspace only when its
execution is authorized. Binary assets are not supported by the text skill loader.

Configure seed libraries or individual directories:

```toml
[skills]
bundled = true
directories = ["/var/lib/pantheon/seed-skills"]

[curator]
enabled = true
# Omit model to use the source channel's selected chat model.
reasoning = "low"
interval_seconds = 3600
idle_seconds = 120
minimum_tasks = 4
timeout_seconds = 300
max_steps = 6
max_input_chars = 128000
token_budget = 100000
```

For NixOS use `services.pantheon.skillDirectories`, `bundledSkills` and
`skillCurator`. Other curator settings belong in `services.pantheon.settings.curator`.
Store-backed seed paths work. No libraries download at runtime. Duplicate seed IDs
fail startup; disable `bundled` if importing a separate seed library with those IDs.

On first import, seeds become revision 1 in `state_dir/skills.sqlite`, a separate
SQLite WAL database with full synchronization and an exclusive writer lock. Learned
revisions and retirements survive restarts. Changing `bundled` or `directories`
changes which seeds are offered on subsequent startups; it does not erase existing
heads. Changed package seeds are available to the curator for reconciliation and
never overwrite learned heads or resurrect retired guides. Preserve this database
alongside the rest of the service state. Existing chat logs, summary trees and views
are never rewritten by the curator. No memory wipe or migration is required.

Every revision contains its complete main guide and supporting text. Publication
atomically compares the expected head of every changed skill, records new revisions,
updates the heads, saves the review/usage and advances the reviewed-experience cursor.
A stale head rolls back the entire proposal. Interrupted drafts remain saved; their
source records stay pending. Rollback restores an older revision as a new head,
preserving the intervening history. The active catalog is cached in memory and
invalidated only after a durable publication.

Each root or worker turn pins a snapshot, including supporting files. Later turns
see new heads without a service restart. The system prompt contains fixed discovery
instructions rather than a changing catalog. Skill bodies and metadata enter ordinary
tool results, keeping the system/tool prefix stable throughout a turn.

## Discovery and controls

`skill(action="list")` returns eight entries with origin and revision, plus
`next_offset`. `skill(action="load", id="product-comparison")` loads the guide.
Use a relative `file` for supporting text, and `offset`/`next_offset` for paged reads.
`skill(action="history", id="product-comparison")` shows revision history, including
retirements. Absolute paths, traversal and links escaping seed directories are
rejected; supporting-resource symlinks are rejected during import. IDs are lowercase
letters/digits/hyphens, at most 64 bytes. Text files are bounded to 512,000 bytes,
64 files/2 MB per skill, 256 active skills/32 MB of serialized active content, and
64 configured seed directories. Unknown YAML metadata remains in the guide text.
`disable-model-invocation: true` marks an explicit-only workflow; it is guidance,
not a security boundary.

`/skills` and `/skills id:<name>` show private catalog/guide previews. Additional
`action` choices provide:

- `history id:<name>`: recent revisions and their rationale.
- `rollback id:<name> revision:<number>`: restore a recorded revision.
- `curator`: enabled state, pending task count and latest pass state.
- `curate`: request an idle background pass, retaining the evidence minimum.
- `proposals`: list recent proposals from this channel.
- `proposal id:<attempt-id>`: inspect a proposal's scope, method and limits.

Full drafts, the bounded evidence/guide/plan context sent to the reviewer, review
observations and provider usage remain in `skills.sqlite`.
Proposal previews are channel-scoped. Final procedural guides are instance-wide,
so the curator must omit credentials, account facts and incident-specific details.

## Background curation

The default curator waits for four new task activities in one channel and two
minutes of service idle time, and starts automatic passes at most once per hour. A manual request bypasses the
cadence gate but still waits for idle time and enough evidence. Multiple root turns
and worker wakeups from one activity count as one task. This is a scheduling gate,
not a requirement that a workflow recur four times. A single difficult experience
may reveal useful technique; its claimed scope still needs evidence.

It examines bounded excerpts of recent tasks, their tool observations and supporting
worker traces. At most 256 turn excerpts are retained, each up to 64 KB; source chat
journals remain complete and untouched. Capture starts after this feature is enabled,
without importing or editing historical memory. Each pass considers up to eight
root task activities plus bounded supporting records. It withholds the newest
activity, including that activity's workers and earlier turns, from drafting.

The curator uses the same startup operator instructions as the root agent.
The drafting agent has only skill/history/seed/draft reads, reads of the supplied
training cases, and `propose`. It must explain a recognizable task family, triggers,
concrete procedure, variable inputs, observable verification, limits and inspected
evidence. It should improve existing guides before creating duplicates, parameterize
incidental details, and leave unexplained workarounds in experience. A completed
model turn is not proof that the task succeeded. **No change is a successful outcome.**
Saved proposals can be reconsidered in later passes with fresh evidence.

For a proposal, two fresh provider requests rehearse baseline and candidate plans
on the withheld task without exposing its recorded outcome. A separate reviewer
then compares those plans against the actual recorded observations and the drafting
evidence. Automatic publication requires an explicit approval, a concrete improvement
on a relevant withheld case, an explicit finding that the scope is transferable
rather than too vague/specific or unsupported, observed meaningful variation and
added procedural information, and no regression or insufficient evidence on other
relevant cases. An unrelated or equivalent case alone cannot justify publication.
One logical proposal can atomically change up to four guides, allowing consolidation
or splitting without half-published changes.

**This first evaluator performs offline model-based plan rehearsal, not executable
sandbox practice or an empirical success benchmark.** Its judgement can be wrong;
scoped guidance, durable rationale and rollback are necessary. It must not claim
that a proposed fix was executed or verified when the records do not establish it.
Uncertain drafts remain inactive rather than becoming new rules automatically.

There is one curator globally. Chat prompts and agent work cancel an in-flight pass;
read-only slash commands do not interrupt it. Queued/running roots, workers and shells
prevent startup. It has no shell, browser,
MCP, messaging, scheduler or memory-write capabilities. Defaults bound drafting to
six provider steps, plus two rehearsals and up to two review steps, a five-minute
whole-pass deadline, 128,000 characters per request, and a 100,000 reported-token
budget. The token threshold is checked after each response and before further work
or publication, so a single request can overshoot it; provider-native usage omissions
cannot establish an exact cost. Proposals are limited to 48 KB. Failed/preempted
passes never promote a draft or consume its experience cursor.

The engineering seed draws on concepts reviewed in
[pstack](https://github.com/cursor/plugins/tree/e5a8186d7b43be8d6ac4452440fbead5f1a51c70/pstack).
The curator design draws on incremental procedural refinement in
[ACE](https://arxiv.org/abs/2510.04618) and reusable skills with practice in
[Voyager](https://arxiv.org/abs/2305.16291). These inform the design; their benchmark
results do not establish Pantheon's curator quality.
