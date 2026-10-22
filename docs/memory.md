# Native OptMem

Pantheon follows the native memory behavior of
[UniiChat 0500bf6](https://github.com/VictorTaelin/uniichat/blob/0500bf6a8ea4bf0ce047488562022b1b6532cc30/Memory.ts).
The complete mechanism, parity checklist, safety invariants, limitations and
fixture regeneration instructions are in [optmem-parity.md](optmem-parity.md).

The engine accepts empty destinations or its own explicit native schema only.
Daemon startup also requires an empty state namespace or the native root marker,
and preflights the global and every private subagent bank before opening operational databases or
starting workers. This is a fresh-state break, not an in-place conversion; see
[service.md](service.md#0210-native-state-boundary).
Original pages and summary nodes are immutable fsynced daily JSONL records.
Only closed groups publish native positional IDs. On restart, unfinished trailing
groups are abandoned; new appends may reuse those unpublished positions without
rewriting physical journal bytes or superseding any visible ID. Rebuilt indexes
select the replacement, matching UniiChat logical visibility. Views/checkpoints and sparse on-disk indexes are auxiliary. Single-writer locks,
write-failure poisoning, external admission deduplication and durable cache
routing identities are retained. Reasoning never enters the original journal.

Native text pages count 30,000 UTF-16 code units; each page gets its own ID.
The binary summary tree and zoom use aligned `id+n` spans. `zoom(id,1,at)` reads
one original page, with `at` a UTF-16 read cursor (default zero). Numeric spans
are never interpreted as another coordinate space.

Both views independently grow then batch-coarsen: main 128,000 -> 64,000 UTF-8
payload bytes; compactor 32,000 -> 16,000. Asynchronous publication is gated to
avoid continually rewriting the prompt prefix. Up to 64 jobs run within the
64th unfinished-leaf frontier; completion can be out of order. Original sources
that already fit 512 bytes require no model call. Compaction has a neutral ruler,
shared root system and bounded original-input-only retries. Five oversized
answers retain the shortest, matching the reference; 512 is not a strict cap.

Resident memory retains views and ancestors, not original bodies. Zoom/date use
the sparse journal indexes. Startup currently streams journals to validate and
rebuild indexes, so it is O(history bytes), not constant-time recovery. Private
snapshots freeze original and tree frontiers and read the immutable bank on
demand instead of cloning its contents.

No active replay, import mode, alias resolver, compatibility reader or migration
logic is part of this runtime. An offline tool is a separate work phase.

## Instance-global scope

Coordinator history is `state/memory`, shared across all admitted channels of one
installation. Separate bots/state directories and private worker traces are not
merged. Channel queues, replies, cancellation, tools and schedules retain their
original ownership. The outer `pantheon-global-state-v2` marker rejects old
channel-partitioned state read-only; there is no automatic merge or import.
See [global-memory.md](global-memory.md) for ordering, provenance, settings and
explicit offline transition requirements. `/context` reports main and compactor
node-text occupancy independently, not total serialized request bytes.
