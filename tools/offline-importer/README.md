# Pantheon offline memory importer — fixture phase

A standalone Rust CLI, **not part of the Pantheon daemon build or dependency graph**.
It rebuilds ordinary native memory from sealed original records. It does not
read any service configuration, discover live state, import old summary stores,
resolve runtime aliases, dispatch tools, send messages, start jobs, invoke a
shell, fetch attachments, or call a model provider. The only summarizer is an
explicit deterministic **fake**. Its output is not a useful semantic memory.

Frozen engine target: Pantheon V5 / 0.21.0, derived from UniiChat
`0500bf6a8ea4bf0ce047488562022b1b6532cc30`. See `vendor/provenance.json`
and `docs/design.md`. Original V5 and running Pantheon are unchanged.

## Commands

```sh
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
cargo run --offline -- plan --export /absolute/synthetic/export
cargo run --offline -- import-fake --export /absolute/synthetic/export \
  --output /absolute/unused/fixture-bundle
cargo run --offline -- verify --output /absolute/unused/fixture-bundle
```

A committed synthetic input is under `tests/synthetic-export`. Do not point
these commands at live memory. All generated artifacts for validation stay in
this task's workspace. The import command is named `import-fake`, and its
`READY.json` says `pantheon-fixture-import-bundle-v1` and `fake_summaries:true`.
This means **fixture reconstruction finished**, not migration/cutover readiness.

`--policy /absolute/policy.json` supplies the fully specified `Config` in
`src/replay.rs`: source-record/byte limits, summary job/input/output byte
admission limits, virtual tick, checkpoint and staging byte limits, virtual
latency, and completion order. Input accounting includes native prompt/task/
context bytes, not tokenizer billing or reasoning. No real inference exists.

Rerun exactly the same command to resume a validated checkpoint or verify an
already completed bundle. Changed sealed input, implementation, or policy is
rejected. Never point the daemon at `.partial` or at the bundle root. The
ordinary native store is `bundle/memory`; archival originals, audit, event
trace, policy and fixture receipt are its siblings, not runtime inputs. There
is no runtime mapping file. Successfully completed staging is removed.

**Uncertain publication:** an error after the final bundle rename (for example,
parent-directory fsync or staging-cleanup failure) can leave complete output with
`READY.json` even though the command reports failure. Do not infer that output is
absent and do not blindly retry/replay. Inspect the final path and staging, run
`verify` against any final bundle, and reconcile the exact sealed input/policy
before resuming. `READY` still means fake-fixture completion, never cutover safety.

## Input contract

The export directory must contain exactly `seal.json` and `originals.jsonl`.
Each original line uses released Pantheon fields: `i`, `kind`, `text`, `size`,
`date`, optional `source_id`. IDs must be contiguous from zero in source order;
`size = byte_len(kind) + 2 + byte_len(text)` is verified. All JSON fields are
strict; malformed Unicode, duplicate known fields, unknown fields, invalid
RFC3339 dates, gaps, duplicate IDs, hashes/count/length errors are rejected.
Source bytes are archived verbatim, including wrappers/newlines. Source
`source_id` metadata is archived, not reused as native admission identities.

The seal pins format `pantheon-original-export-v1`, source version
`pantheon-0.20.4-originals`, channel, nonempty checkpoint label, count, exact
byte length, SHA-256, and explicit `result_links:[{call,result}]`. A fixture-only
source version `pantheon-original-fixture-v1` additionally permits inert
`images` references, to test native UTF-16/image right-alignment behavior.
Export preparation/sealing from live journals is **not implemented or run**.
An external operator must later provide a coherent, independently verified
checkpoint; the importer does not prove a label corresponds to a live freeze.
Hashes provide consistency, not authenticity against a malicious same-user
writer. Operate in an exclusively controlled private workspace.

## Reference repair and limits

Only top-level `tool` envelopes `zoom {"id":...,"n":...}` and
`date {"id":...}` are recognized. Old zoom has no `at`. Integers must be valid,
old binary spans aligned and wholly within the prefix through the calling
original. Released runtime logs a tool call before invoking it, so a call may
address itself. Self-reference is transformed with the same bounded fixed-point
policy. Targets after the logged call are rejected: the export schema does not
prove additional concurrent writes were visible at invocation. Unknown
arguments, malformed/future calls and unrepresentable targets block planning.
External `read(page)`, MCP offsets, browser/attachment IDs and free prose are
unchanged. A zoom of a split original cannot silently become its first page.
No multi-record expansion policy is implemented/approved; multi-span targets
or differing parent-child boundaries block, rather than guessing.

Results require **explicit sealed link assertions** and corroborating source:
leaf echo body must exactly equal the authoritative original, parent echo must
have exactly its two old child headers, and date echo must equal the original
timestamp. No adjacency pairing. Nested copies are repaired only by copying
the transformed authoritative target's text into its verified leaf echo.
Parent summary bodies remain historical observed tool output, never native
precomputed summaries. There is no summary-store reader. Unpaired
address-bearing echoes block; missing results are not regenerated. Linked
error/clipped/ambiguous outputs block pending a separately reviewed resolution
policy. No attempt is made to turn old errors into successful calls.

Repairs iterate deterministic page plans to a fixed point (max 32); cycles,
unstable or unrepresentable intermediate plans fail closed. Audit records bind
the original line/text and resulting text hashes, `/text` byte splice and
reason. Archive hashes plus the audit reconstruct every transformed text.
Only the importer temporarily calculates old→native spans. Neither the native
store nor its runtime needs this mapping. Free-prose old numeric references
remain a residual ambiguity, not automatically corrected memory addresses.

## Not established

No paid replay, semantic recall, production provider scheduling, cache savings,
live-history migration, service isolation/shadow/cutover, wrapped browser
packaging, rollback of post-handoff actions, or deployment is validated. Real
inference requires a separate approved budget/pilot, durable reservation and
receipt/reconciliation design; this fake-only CLI must not be represented as
production migration-ready.
