# Third-party notices

## UniiChat OptMem

The native memory algorithm, compaction task/ruler, view-block policy, and
paging implementation in `src/memory.rs`, and the adapted compaction
instructions in `src/compact.txt`, and request-coordination policy in
`src/provider_cache.rs`, together with the native provider/compactor behavior
adaptations documented in `docs/compactor-adapter.md`, are derived from Victor Taelin's UniiChat:

- Project: https://github.com/VictorTaelin/uniichat
- Reference: `0500bf6a8ea4bf0ce047488562022b1b6532cc30`
- Sources: `Memory.ts`, `Clip.ts`, `prompt.txt`, `AskAI.ts`, `Agent.ts`, `OpenAI.ts`, `Anthropic.ts`
- Copyright: Victor Taelin and UniiChat contributors
- License: Apache License, Version 2.0

Changes: translated to Rust; adapted actor names, native durable schema and
provider integration; retained Pantheon's fail-closed fsync/locking and durable
cache identities. See `docs/optmem-parity.md` for intentional differences.

The original Apache-2.0 license is reproduced in
`licenses/uniichat-Apache-2.0.txt`. Pantheon's existing MIT license continues to
cover its original code; this notice does not relicense the upstream material.

## ryu-js

The ephemeral native JSON serializer uses the crates.io dependency
`ryu-js` 1.0.3 (David Tolnay / boa-dev), under Apache-2.0 OR BSL-1.0.
Source: https://github.com/boa-dev/ryu-js . Cargo.lock pins the registry checksum.
This is a dependency, not a change to Pantheon's durable/cache JSON serializer.


## rmcp 3.5.1 (vendored scoped patch)

The `vendor/rmcp` library source is from the official Model Context Protocol
Rust SDK: https://github.com/modelcontextprotocol/rust-sdk . Version 3.5.1 is
licensed Apache-2.0 as declared in its Cargo.toml. The Apache-2.0 text is included
at `vendor/rmcp/LICENSE`; no upstream NOTICE was present in the crates.io source
package used here. See `vendor/rmcp/PANTHEON-PATCH.md` for the scoped modifications
to request-owned raw arguments, metadata serialization and mirrored headers.
The stock authenticated/session-aware HTTP and stdio/process-lifecycle writers
remain in use. Vendored library source does not imply independent acceptance of
all SDK optional features or upstream crate tests omitted from this source tree.
