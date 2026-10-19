# Synthetic validation matrix

This table identifies fixture assertions, not operational or semantic readiness.
Exact completion results/logs are in the external validation report, bound to
its frozen source manifest. Old unsuccessful development logs are superseded;
only the final validation-v1 chain is final-source evidence.

| Area | Coverage |
|---|---|
| Unmodified native behavior | All 33 copied V5 memory regression tests; upstream fixture bytes unchanged; independent reader compiles entirely unchanged module |
| Paging | UTF-16 emoji boundary, preferred newline, empty text, original concatenation, native multibyte fixtures |
| Images | Native fixture tests plus importer end-to-end 21 image references/right alignment/page dates; no image decoding/fetch |
| Native isolation | Complete generated bundle reopens using unchanged V5 module; bundle hash check before/after reader |
| Archive | Exact JSONL/seal bytes preserved, line hashes and text hashes, source order/count/size and native page-order verification |
| Zoom calls/results | Leaf numeric shifts, nested authoritative copied calls, parent aligned offset, equally split parent children, date shift, self-reference visibility |
| Rejection | Multiple spans, split leaf, different parent midpoint, malformed/duplicate/negative/future/unknown-cursor arguments; unpaired/mismatched echo |
| Envelope/schema | Source hash/length/count/order/gap/duplicate errors, derived fields/extra files, malformed surrogate/duplicate fields, size mismatch |
| Scheduling | 66 long originals/100 tick latency reach native 64 pending cap; each launch context excludes own/future sentinel; two completion order policies |
| Restart | Deterministic resumed event trace/render equals uninterrupted; pending snapshot/outcome preserved; complete final checkpoint publishes at exact max tick |
| Crash staging | Dispose torn work, ignore orphan generation/torn pointer; reject corrupt current checkpoint; no stale source/policy/ownership resume |
| Write failure | Test-only injected partial state/CURRENT/READY writes leave no final output, then coherent resume; native tests inject short write/fsync/index failure |
| Admission | No-summary job budget, input byte budget, staging budget and policy mismatch; positive virtual latency required |
| Filesystem ownership | Absolute resolved paths, no overlap, preexisting unrelated output preserved, symlink rejection, flock single-writer; special/hardlink guards inspected but not directly fault-tested |
| Finalization | No final path/READY on stopped/corrupt/budget failed runs, inventory tamper rejected; idempotent repeat verifies completed output |
| Inertness | Hostile command/action-looking original strings remain data; no shell/provider/daemon/action-dispatch dependency in importer |

Remaining independent review should concentrate on source fidelity and
reference boundaries, checkpoint directory/pointer ordering, publication
identity/inventory, failure semantics, and whether all stated exclusions are
clear. No real journal, original export sealer, paid model, useful recall,
provider cost/cache saving, production deployment or handoff has been tested.
