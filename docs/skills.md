# Skills

Pantheon ships four task guides: `research`, `browser-activities`, `learning`
and `engineering`. They are available to root and workers, including strict
coordinators. Ordinary conversation and simple requests need no workflow ceremony.

A skill is a directory containing `SKILL.md` with YAML frontmatter:

```markdown
---
name: product-comparison
description: Compare purchases using current prices, relevant constraints and primary sources.
---
Investigate the user's actual decision. Read the relevant product pages and
return a supported recommendation with material tradeoffs.
```

Descriptions guide discovery; the body supplies the method. Supporting text can
live under `references/`. Scripts and assets may live alongside the guide, but
loading a skill does not execute its scripts or expand permission to act.

Configure libraries or individual skill directories:

```toml
[skills]
bundled = true
directories = ["/var/lib/pantheon/skills"]
```

For NixOS use `services.pantheon.skillDirectories` and
`services.pantheon.bundledSkills`. Store-backed paths work, so a pinned skill
repository can be deployed with the service. Nothing downloads or installs at
runtime. Directory names are stable lowercase IDs, allowing hyphens. Human-readable
frontmatter names can differ. Unknown metadata is preserved in the guide text
without imposing Cursor-specific behavior. Duplicate IDs fail startup instead of
silently overriding bundled guides; disable `bundled` for a replacement library.

`skill(action="list")` returns eight entries and a `next_offset` for further pages.
`skill(action="load", id="product-comparison")` loads the guide. Use a relative
`file` for supporting text, and `offset`/`next_offset` for paged reading. Absolute
paths, traversal and symlinks escaping the skill root are rejected. Text files are
bounded to 512,000 bytes, and results remain valid JSON within the model tool-result
budget. The service accepts up to 256 skills across 64 configured directories.

The index contains a small preview; the full catalog stays discoverable through
the tool. Descriptions, index and main guides freeze at startup. Supporting files
are read on demand, so deployment should pin them alongside the main guide for
reproducible runs. A service restart reloads the index and main guides. Selected
content enters the ordinary tool transcript, preserving the fixed system/tool
prefix throughout a turn. Neither dates nor task status enter that prefix.

`disable-model-invocation: true` marks an explicit-only workflow. It remains listed
for discovery but is absent from the automatic startup preview. Invocation guidance
belongs to the agent prompt; this flag is not a security boundary. `/skills` presents
catalog previews as private embeds, and `/skills id:<name>` previews one guide.

The loader can read standard SKILL.md files from pstack and other libraries. Their
instructions still need review and adaptation: Cursor's Task calls, cloud execution,
model aliases and plugin dependencies are not Pantheon APIs. Pantheon's tool schemas,
background-only workers, memory model, ownership and user authorization remain
applicable. Import curated skills gradually rather than attaching an entire
engineering workflow to every everyday request.

The engineering guide draws on concepts reviewed in
[pstack](https://github.com/cursor/plugins/tree/e5a8186d7b43be8d6ac4452440fbead5f1a51c70/pstack),
including behavior-based verification, exclusive write scope and structural fixes
for recurring mistakes. The bundled guides are tailored to Pantheon's own tools.
