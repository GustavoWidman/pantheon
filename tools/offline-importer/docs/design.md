# Design, ownership, and readiness contract

## Code boundary

This is a new workspace/crate, not a Pantheon workspace member and not a daemon
dependency. `native.rs` is a byte-identical prefix copy of frozen V5 memory.rs,
plus one narrow generic `push_record_at(kind,text,images,date)` wrapper around
its existing private append_entry date parameter. The wrapper has no old IDs,
source parser, alias map, replay modes, or action awareness. This local vendoring
avoids editing V5 or pulling in its daemon/provider/browser dependency graph.
`vendor/native-v5.rs` is the complete unchanged module for an independent reader
harness. The copied module, native tests, fixtures and prompt are hash-bound in
provenance. V5's Apache-2.0 attribution and notices are preserved. The importer
code is MIT; copied engine portions retain upstream/combined license notices.

Future maintenance should extract an ordinary native-memory library upstream
only through a separate reviewed change, not link migration logic into the
daemon. This implementation is not such an extraction.

## Pipeline

1. Validate private absolute paths, reject symlinks, hardlinked files, special
   files and overlapping export/output/staging paths. Existing unrelated
   output directories are never replaced. A separate O_NOFOLLOW flock protects
   one output namespace before inspection/mutation. Parent path components
   must already exist and must not be symlinks. This is not a sandbox against
   a hostile same-UID process changing directories during syscalls.
2. Read exactly the two sealed export files within source byte/record limits.
   Validate schema, originals hash, count/order/size, UTF-8/valid Unicode and
   timestamps. No summary-store/input format guessing or live state discovery.
   The seal is an operator assertion of checkpoint scope, not a live-freeze
   implementation. Explicit result_links are sealed causal-pairing assertions;
   the importer corroborates result shape/source content but cannot establish
   causality from text alone. Source hashing is not a cryptographic signature.
3. Plan all structured reference repairs in memory. Compute native 30k UTF-16
   text pages plus right-aligned inert images, temporary source→page spans,
   direct tool/date rewrites and corroborated historical echo/nested copies.
   Iterate from immutable originals to a fixed point; reject invalid,
   unresolved, cyclic, split/multi-span or misaligned targets. No guessed
   numeric fallback, ad hoc prose regex, or silent partial successful plan.
   Native call visibility includes the logged tool call itself (old runtime
   appended it before invocation), but not unproven subsequent concurrent
   writes. Accept self-reference through the same bounded fixed-point pass.
4. Replay closed originals chronologically using native push + supplied exact
   original timestamp. A logical original is one admission event even when
   native text/images produce several leaf IDs. No legacy `more`/job state is
   imported. Native pump, 64-job bound, main/compactor views and context cutoffs
   remain the phase-one engine's behavior.
5. Fake summary requests snapshot source/native compactor context and reserve
   job/input/output-byte budgets before fake admission. Tasks include native
   compact prompt/task bytes. Results are deterministic short strings containing
   key and SHA256(source NUL context); they do not summarize meaning. There is
   no provider/auth/network API or executable plugin. Future paid inference
   must be a separately reviewed/authorized extension, including tokenizer,
   reasoning/cache accounting, unknown-outcome reconciliation and cancellation.
6. Every virtual tick commits a complete fsynced memory/state generation.
   Validate native concatenation, page count/order/kind/text/date/image
   right-alignment and original group closure at EOF. Full-tree draining is
   permitted only at EOF for a complete fixture, not between each original.
7. Publish a bundle with ordinary memory + external immutable source copy,
   exact repair audit, event trace, config and accounting. READY binds every
   other file. Fsync every new file and directory, then rename the bundle into
   the previously absent output path and fsync its parent. Native runtime
   needs only `memory/`; it cannot load the external audit or temporary map.
   READY says fixture/fake, not operational migration readiness. Drop staging
   after success. A crash after publication can leave staging for inspection;
   repeated run verifies final output and never reapplies history.

## Deterministic virtual schedule (not historical timing recovery)

There is no fictitious upstream debounce timer. Unii accumulates at append/group
closure, pump/fit publication and actual asynchronous completion events. Original
journals cannot reveal a counterfactual Unii completion schedule. This importer
records an explicit simulation instead:

- Each positive virtual tick admits at most one complete original in source
  order. Text/image pages are one native closed group.
- After ingress, complete all previously admitted fake jobs due at/before that
  tick. Default order: `(due,key)` ascending; alternate: reverse that batch.
- Then capture newly reserved native work, preserving its native launch-time
  compactor-context snapshot, and admit fake responses for `tick+latency`.
- No whole-tree settle per record and no all-transcript context. Free nodes
  stay local to native pump rather than fake paid jobs. Pending jobs remain
  concurrent up to native 64. At EOF, continue ticks until all work settles.
- Two supported fixture schedules (normal/reversed completion order) are
  tested. Their storage validity is established, not equal semantic recall.

All completions, source/context hashes, admission bytes, ingress and event ticks
are recorded. Pending snapshots and deterministic outcomes are checkpointed.
Restart does not recompute admitted fake outcomes from a newer context. Native
reopen may reconstruct its own busy contexts, but resumed admitted results use
the original saved snapshots and must still correspond to reserved native
keys. Native summary persistence/frontier/publication rules remain unchanged.
No old summaries, old tree partitions, paid receipts, or active jobs are read.

## Crash/write protocol

- `.OUTPUT.import-lock` is outside the bundle/staging directory and held for
  the whole operation; it remains as an inert empty flock file after use.
- `.OUTPUT.partial/OWNER` binds source identity + compiled implementation ID.
  Implementation ID hashes native module/prompt and all transformation,
  storage, seal and replay source, library declarations and Cargo manifest/lock. State also pins full config. `CURRENT`
  points to a numeric generation and SHA256 of its exact file inventory.
- Generation contains memory journals/indexes/view and pending fake outcomes,
  budget counters, next original, virtual tick and event trace. Files and
  directories are synced before generation rename, then CURRENT is replaced
  atomically and parent synced. Keep current and prior generation; older
  generations are collected only after pointer durability. Bound snapshot
  bytes and projected staging bytes before copying. This deliberately trades
  CPU/I/O for a simple coherent synthetic checkpoint; not scale-benchmarked.
- `work/` is disposable. On resume, require matching source/code/policy and
  verify every checkpoint file. Recreate work from the committed generation,
  never from a partially mutated working journal. An incomplete/orphan next
  generation is ignored/replaced. Corrupt current checkpoint fails closed,
  with no automatic fallback that could silently discard admitted work.
- Every error exits immediately. No READY is emitted on budget exhaustion,
  unresolved references, write/sync/copy failure or partial replay. Neither
  process nor daemon treats staging as a ready native store. A native marker
  can exist inside staging for checkpoint purposes; callers must honor the
  offline bundle's READY protocol, not discover/copy partial directories.
- Publication directory is fully inventoried including byte-verbatim archive.
  `READY.json` is the last file. The final path becomes visible via one rename.
  Power-loss durability assumes filesystem support for fsync/atomic rename;
  no distributed-filesystem or machine-crash stress test has been performed.
- Test fixtures interrupt at a coherent checkpoint, corrupt disposable work,
  add orphan/torn pointer files, reject corrupt checkpoints and changed
  policies/ownership, and prove final event equivalence after resume. Copied
  native regression tests cover journal/index short-write/fsync poison paths.
  These are not a formal crash proof or real-provider at-most-once guarantee.

## External audit, no runtime aliasing

For each changed original, the transformation audit records the exact original
JSONL-line hash, source/output text hashes, JSON path `/text`, source UTF-8 byte
range and replacement string, reason and implicitly the importer implementation
identity bound by READY. The archived source + splice reconstructs output;
unchanged metadata is replayed without synthetic marker/tool insertions.
Native numeric references are normal repaired tool text, still inert historical
data. Parent summary bodies stay observed original output; this does not remove
all traces of old summaries from original conversations. Parent node spans and
child spans are independently checked. The implementation may conservatively
reject an intermediate unrepresentable fixed-point plan even if a hypothetical
expansion policy could later make it representable; it never guesses a path.

Free prose/embedded arbitrary examples may contain stale addresses that are not
recognized structured envelopes. No claim of universal intent repair is made.
A real sealed export will need exception review; clipped/error/unknown source
schemas and multi-record expansion are explicitly not production-ready paths.

## Gates still closed

- No live export, production reads/writes, paid inference, inference retry or
  unknown-outcome reconciliation, operational migration, shadow service,
  credentials transfer, Discord fencing/dedup, handoff tail, cutover or deploy.
- No quality/cost/cache claim from fake summaries. Semantic recall requires a
  bounded authorized real-model pilot and prewritten blinded recall tests.
- No completed workflow can prove a checkpoint label really fences old writers
  or captures append-only inbox/outbox/external receipts. Those remain separate
  stateful-migration gates, not responsibilities smuggled into native memory.

## Publication uncertainty

The final rename precedes parent-directory fsync and staging cleanup. A failure
in either later step may report an error while leaving a complete final bundle.
Errors therefore do not universally mean "no READY output". Preserve both paths,
inspect and verify the final bundle against its sealed input/policy, and reconcile
before resumption. Never delete evidence or blindly replay on a command failure.
This caveat is distinct from injected pre-rename failures that leave no output.
