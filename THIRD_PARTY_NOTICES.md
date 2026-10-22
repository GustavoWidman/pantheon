# Third-party notices

## UniiChat OptMem

The native memory algorithm, compaction task/ruler, view-block policy, and
paging implementation in `src/memory.rs`, and the adapted compaction
instructions in `src/compact.txt`, and request-coordination policy in
`src/provider_cache.rs`, are derived from Victor Taelin's UniiChat:

- Project: https://github.com/VictorTaelin/uniichat
- Reference: `0500bf6a8ea4bf0ce047488562022b1b6532cc30`
- Sources: `Memory.ts`, `Clip.ts`, `prompt.txt`, `AskAI.ts`, `Agent.ts`
- Copyright: Victor Taelin and UniiChat contributors
- License: Apache License, Version 2.0

Changes: translated to Rust; adapted actor names, native durable schema and
provider integration; retained Pantheon's fail-closed fsync/locking and durable
cache identities. See `docs/optmem-parity.md` for intentional differences.

The original Apache-2.0 license is reproduced in
`licenses/uniichat-Apache-2.0.txt`. Pantheon's existing MIT license continues to
cover its original code; this notice does not relicense the upstream material.
