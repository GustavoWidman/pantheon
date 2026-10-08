#!/usr/bin/env bash
# Only local fake SSE traffic; no provider credentials or host network changes.
set -euo pipefail
cd "$(dirname "$0")/.."
binary=$(cargo test --locked --lib transient_packet_loss_does_not_override_provider_request_deadline --no-run --message-format=json | python3 -c 'import sys,json; artifacts=[json.loads(line) for line in sys.stdin]; print(next(a["executable"] for a in artifacts if a.get("reason")=="compiler-artifact" and a.get("executable") and "lib" in a["target"]["kind"]))')
[[ -n "$binary" ]]
export PANTHEON_PARENT_NETNS=$(readlink /proc/self/ns/net)
exec unshare --user --map-root-user --net sh -c '
  ip link set lo up
  export PANTHEON_ISOLATED_TRANSPORT_TEST=1
  exec "$1" --ignored --exact provider::tests::transient_packet_loss_does_not_override_provider_request_deadline --nocapture
' sh "$(realpath "$binary")"
