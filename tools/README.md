# Standalone operator tools

`offline-importer/` is a separate Rust crate for sealed original-history planning
and deterministic fake-summary fixture reconstruction. It is not a dependency of
Pantheon, a workspace member, part of a Nix/service package, or an executable the
daemon dispatches. See its README for conservative reference-repair limits,
provenance and validation scope.

**Not production migration readiness:** no real model backend, paid replay,
automatic state installation, runtime marker creation, or cutover is provided.
Real-export reference pairing and recall remain separate gates. Generated
bundles, raw histories, operator policies and private audit evidence must never
be committed. Do not use fake summaries as an agent memory or point a daemon at
a fixture bundle. This PR changes no daemon behavior or release version.
