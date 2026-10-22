# Global single-user coordinator memory (source-only successor)

Baseline: accepted dummy-bot3 DM-fenced source from `pantheon-shadow-preparation/source`.
Pristine baseline hashes are saved separately. The accepted source, bank, runtime,
configuration and service are not edited. This is not a deployment or migration.

## Ownership

* A `Harness` installation opens exactly one `state/memory` bank. Its shared `Arc`
  owns one memory mutex, publication notification, global native numeric IDs and
  scheduler. Channel actors never reopen it. Private worker traces remain separate
  in `state/subagents/<worker>`; zoom/date intentionally query shared coordinator
  memory, not those unrelated private trace IDs. Private curator snapshots use the
  same shared bank's immutable frontiers.
* One compactor starts with the runtime, including idle restarts. Its 64-worker
  bound applies once per installation, not once per channel. Its persisted model,
  effort, catalogue and provider cache affinity use reserved scope zero (Discord
  snowflakes are nonzero), never whichever channel happens to open first.
  `/model kind:compact` and `/reasoning kind:compact` are instance-global.
  Coordinator/curator model settings remain channel-owned.
* Coordinator turns serialize across channels with a cancellation-aware permit
  acquired before batch selection, view capture or active-turn presentation.
  Channel queues, steering, cancellation, task ownership, deliveries, reply target,
  user identity, attachment permissions, tool authorization and schedules remain
  channel-owned. `tell` enqueues, never synchronously waits for another coordinator.
  Children remain concurrent and retain their private transcripts; only their
  returned notifications are later admitted to shared coordinator memory.
* Newly journaled input includes `[channel:<id> user:<id>]` provenance. It is
  untrusted source content, never authorization or a destination selector. The
  operational source ID is unchanged; ordinary capture and crash recovery use
  identical text and append-before-ack. Runtime history is linearized by the
  shared writer mutex; coordinator turns by permit acquisition. These orders are
  NOT claimed to reconstruct external Discord wall-clock arrival order. Startup
  recovers nonqueued/unlogged inputs in durable `created,rowid` order across all
  channels before actors start. New queued batches follow coordinator admission;
  stopped queued batches are preserved but not answered. On shutdown, queued
  inputs remain durable, not acknowledged or executed by recovery.
* Transport cache identity stays explicitly scoped to the channel coordinator,
  worker or global compactor, as appropriate. Shared native history does not make
  two different model sessions one session. Stable store identities survive restart.
  No claim of Anthropic cache behavior on unsupported Codex transport is introduced.

## `/context` truth

The current main default is **128,000 bytes = 125 KiB**, not 64 or 32KB. Dummy3
omits an override. The independent compactor default is **32,000 bytes = 31.25
KiB**. No budget is changed. Stats measure summed UTF-8 node-text payload bytes,
not the serialized model request. The actual view can exceed its target while
native batching/fitting waits for available summaries; this is not a truncation
cap. Rendering adds `<chat>`, ID/count delimiters and line endings; request shaping
adds system/tools/current input/media. Model token-window usage remains a separate
provider-derived section. `/context` now shows both actual used/target pairs,
exact decimal bytes and binary KiB, and instance-global scope. A configured main
budget is reported as configured, not replaced with a constant.

## Fail-closed state transition boundary

New namespace marker `pantheon-global-state-v2` rejects older marked state and any
`state/chats` path BEFORE opening the bank, operational DB, workers or gateway.
It does not automatically pick a bank, merge channels, alias historical IDs,
reuse old channel trees, or silently present empty global history. CLI export and
note append address global memory and no longer accept a misleading `--channel`.
Native bank format, saved views, fsynced journals and publication semantics are
unchanged. Separate installations never share memory merely because code is global.

Deploying this to an existing instance would require a separately authorized,
stopped/offline transition that preserves **all** latest originals, including
messages appended in dummy3 after #13015, plus operational state/queues, source-ID
idempotence, private worker ownership, stable cache identity and reference checks.
No such operation or transition utility is implemented or authorized here.
The existing DM-only admission fence is unchanged and independent of storage
scope; global memory does not enable guild messages.
