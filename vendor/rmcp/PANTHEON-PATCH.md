# Scoped rmcp 3.5.1 patch (Apache-2.0)

Request-owned `RawToolArguments` preserves canonical ECMAScript JSON.stringify
arguments through the existing serializers in stdio and streamable HTTP. The
existing request/session/auth/cancellation/process ownership remains unchanged.
CallToolRequestParams serialization, the request metadata merge (opaque top-level
RawValue fields, typed _meta only), and outgoing standard-header projection are
changed. Header projection must come from the same canonical arguments;
annotated scalar lone UTF16 is not yet supported or cleared. Native endpoint
captures and lifecycle acceptance are required before a parity claim.

Upstream: https://github.com/modelcontextprotocol/rust-sdk, rmcp 3.5.1.
Crates.io package VCS revision: 79437f291b2c44053d00dcd5db969fd0cca7c887
(path crates/rmcp), from the registry package .cargo_vcs_info.json.
The included crate is Apache-2.0, as declared in Cargo.toml. See LICENSE.
Pantheon scoped modifications are described above; no upstream NOTICE was
present in the crates.io source package used as the baseline.

RawToolArguments owns canonical wire JSON and derives scalar header projection
and unpaired-scalar markers internally by streaming opaque RawValue fields.
Nested JSON is never converted wholesale to Value. The header builder refreshes
its clone from the raw owner's projection even if callers mutate the public
legacy arguments field; with_arguments explicitly clears raw ownership. Raw
arguments themselves are not a separately mutable public field.
