# Provider transport diagnostics

Failed chat, worker, compactor and hosted-search requests emit a `provider request failed` warning. The warning includes a JSON `diagnostic` record; its `id` also appears as `[diagnostic UUID]` in the returned error, including Discord notices and worker reports. These warnings are enabled by the default `pantheon=info` log filter.

For NixOS, retrieve the matching record with:

```sh
journalctl -u pantheon.service --grep 'DIAGNOSTIC_UUID' --no-pager
```

For a systemd user service, use `journalctl --user -u SERVICE_NAME`. Services configured to append output to a file must be searched in that file instead. Preserve the complete warning line, including its timestamp and span fields. `provider_step` spans identify the channel, agent owner, run and step; `compaction` spans identify the channel and memory node.

The record contains the model, attempt, configured request timeout, time to headers, total elapsed time, HTTP status/version, provider request ID and Cloudflare ray ID when supplied. Body diagnostics include bytes/chunks received, time to first byte, time since the last chunk, parsed SSE event count, the last recognized event type and completed output-item count.

Failure stages distinguish `headers`, `http_status`, `body`, `premature_eof`, `provider_error`, `sse_decode`, `json_decode` and `size_limit`. A transport error additionally records reqwest's timeout/connect/body/decode/request flags, bounded underlying error causes, and OS error information when available. Unknown values remain `null`; for example, a clean SSE EOF without a completed response has no reqwest error flags. A body failure after HTTP 200 does not imply successful model completion. Compare elapsed time with the configured timeout and retain the provider request ID when investigating upstream failures.

Normal diagnostics exclude request bodies, response bodies, tool arguments, reasoning, credentials, account IDs, cookies and arbitrary headers. The separately gated, sensitive selected-error capture below is not an ordinary log. Only selected opaque response identifiers and recognized event names are recorded. URLs are removed from reqwest errors and redacted in transport causes. Payload-decoding errors never render their underlying error strings into the diagnostic record. Cause chains and fields are bounded to keep failures from generating unbounded logs.

Tools are still dispatched only after a complete provider response. “No tools from this response executed” refers to that failed response; earlier steps can already have completed tool effects.

## Generic configurable request retries

Codex/Z.ai inference and hosted-search requests use this configurable policy.
Native public OpenAI/Anthropic turns use the separate policy below:

```toml
[agent]
request_max_attempts = 4
request_backoff_seconds = 2
```

Attempts include the initial HTTP request. The default waits after failed attempts
are **2, 4 and 6 seconds**, with no jitter: the configured base multiplied by the
failed attempt number. `request_max_attempts` accepts 1–10; one disables retries.
`request_backoff_seconds` accepts 1–60. NixOS exposes the same fields through
`services.pantheon.settings.agent`. Existing configs inherit these defaults.

Retryable failures are HTTP 408, 429, 500, 502, 503, 504 and 529; connection errors,
request/body timeouts and broken HTTP bodies; premature SSE EOF; and recognized
transient SSE error codes/statuses. Invalid JSON/SSE, malformed tool arguments,
token-limit/incomplete output, ordinary 4xx errors and unknown stream failures
fail immediately. Retry classification uses typed failures and recognized codes,
never arbitrary provider error text. A Codex 401 can refresh credentials once,
without a backoff, if an attempt slot remains; it shares the total attempt cap.

A valid numeric or HTTP-date `Retry-After` raises the wait to at least that delay.
Values above 60 seconds stop this request rather than truncate the delay and
retry too early. Invalid header values use the configured linear delay.

Each retry reuses the exact prepared body, native reasoning/tool results, model,
effort and cache affinity. Partial output is discarded. Completed client tools
from earlier steps are not replayed; transport attempts do not increment model
iterations or curator eligibility. `/stop`, shutdown and enclosing tool/curator
deadlines cancel both an in-flight attempt and a backoff. The configured request
timeout applies separately to each attempt, not to the entire retry sequence.

Every failed exchange retains its diagnostic ID and actual attempt number. A
`provider request retry scheduled` warning records the next attempt, maximum and
delay without payloads. Exhaustion returns the final exchange's error/diagnostic.
Successful usage accounting covers the completed response; failed generations can
consume provider work whose usage was not returned. Request counts, existing
deadlines and curator bounds remain the limits on that additional work.

This generic policy applies to the named transports, not native public turns.
Discord REST and model/price discovery remain separate. The native compactor's
outer patient owner waits60 seconds on outage, retaining the frozen corrected
call/reservation; it does not consume an answered length attempt.

## Native public memory-turn lifecycle

Native public OpenAI/Anthropic root, worker, curator and compactor calls follow
pinned UniiChat Agent transport: at most8 requests, literal x-should-retry header
overrides,408/409/429 or5xx busy statuses,2/4/8/16/32/64/128-second exponential
backoff. Positive JavaScript-number Retry-After replaces that wait; HTTP dates
are not interpreted, Infinity/overflow timers normalize to1ms as in Bun.
OpenAI server_error is busy; answered semantic failures are not outages.
Anthropic error events are busy. Malformed/EOF/network/idle errors retry, while
local submission/admission and size-safety failures stop without redispatch.

Native HTTP has no absolute request timeout. Parsed JSON data lines refresh a
1200-second OpenAI or300-second Anthropic idle deadline, starting before headers.
Comments and partial byte chunks do not refresh it. Native Linux sockets disable
the unrelated30-second TCP_USER_TIMEOUT. Generic configuration fields still
control Codex/Z.ai/search and are not silently treated as native idle settings.
Cancellation drops requests/cache guards/sleeps. A native partial stream can
produce inert stopped/cutoff history; it never executes incomplete ToolCalls.
Thoughts remain UI-only; request diagnostics never contain those text bodies.
See compactor-adapter.md for ordinary observations and scope/safety differences.

## Linux TCP timeout policy and local reproduction

The generic provider client aligns Linux `TCP_USER_TIMEOUT` with
`agent.request_timeout_seconds`, instead of inheriting reqwest 0.12's independent
30-second default. Reqwest's overall request deadline still bounds the entire
request (including streaming), and `/stop` still cancels it immediately. TCP
keepalive remains enabled. Failed transient exchanges use the bounded retry policy
above; partial-tool dispatch remains prohibited.
Other platforms retain their existing TCP configuration.

`TCP_USER_TIMEOUT` bounds unacknowledged transmitted traffic, including failed
keepalive probes; it is **not** an SSE inactivity timer. A healthy connection may
legitimately have a long gap between SSE events. HTTP 200 followed by OS error 110
near 30 seconds is consistent with a lower-level TCP timeout, but logs alone do
not locate the original network fault.

To exercise the actual kernel behavior without credentials or external traffic:

```sh
# Linux: requires cargo, Python 3, unshare, ip and tc; user namespaces must be enabled.
# The slow test is ignored by normal cargo/Nix checks.
scripts/reproduce-provider-timeout.sh
```

The script creates an unprivileged, isolated user/network namespace. A loopback
mock sends HTTP 200 and `response.created`, then drops all packets in **that
namespace only** for 40 seconds before restoring connectivity and completing the
same response. The provider request budget is 70 seconds. The pre-fix client
fails with kernel `ETIMEDOUT`; the aligned client completes without retrying. This
isolated regression explicitly disables request retries to test the socket policy.
The test verifies isolation before changing the loopback qdisc and cleans it up
on exit. It never contacts a real provider, changes the host network, or submits
a tool. A fast Linux test also reads `TCP_USER_TIMEOUT` from the real client
socket; normal tests retain stalled-stream deadline, cancellation and rejection
of incomplete tool-bearing responses.

This reproduces the transport failure mechanism, not proof of the packet-level
cause of a particular production incident. Retain correlation IDs and capture
TCP telemetry if a production connection continues to fail.

## Structured provider-stream error diagnostics

For terminal HTTP-200 SSE `error`, `response.failed` and `response.incomplete`
events, the warning now includes `stream_error`: the recognized event name,
allowlisted error `code` and `type`, numeric error `status` (100–599), known
incomplete reason (`max_output_tokens` or `content_filter`), a diagnostic
category (`validation`, `context`, `auth`, `rate_limit`, `upstream`, `incomplete`
or `unknown`) and `retryable`. Unknown codes/types/reasons are `null`, not copied
from the provider. `code_state` and `type_state` distinguish `missing`, explicit
`null`, `recognized`, `unrecognized` strings, and `invalid_type` values. For
flat `error` events, `type_state=not_applicable`: `type: error` is the event name,
not an error category. `error_source` is `error`, `response.error`, `event`, or
`none`. Error selection uses the first actual object in that order; null and
non-object placeholders cannot hide a flat or nested error. Fields from
conflicting objects are never merged. The same selection drives bounded retries
of recognized transient errors; unknown errors still stop immediately.
Returned errors include these safe categories/codes and known incomplete reasons,
so Discord and worker notices identify recognized failures without raw messages.
The `retryable` flag means *eligible*; a request at its attempt limit still stops. HTTP status
is recorded separately; HTTP 200 plus `stream_error.status=503` is a provider
stream error, not an HTTP 503 response. Header request IDs and Cloudflare ray
IDs remain bounded as before; event-supplied IDs and messages are not logged.

`request_shape` contains only the JSON request's serialized byte count, number
of input/messages items, recognized function-call/output and reasoning item
counts, and tool definition count. These are structural clues, not the response
size, a valid-context guarantee or proof of malformed requests. Prompt/system
text, tool schemas/arguments, unknown item types, account identifiers and raw
provider error values are never included. This applies to failures on any
attempt; successful requests are not logged. Match a later diagnostic ID to its
exchange before diagnosing a recurring failure. Earlier incident records lack
these new fields and cannot be retroactively classified.

## Provider errors versus decoding failures

`stage=provider_error` now means that a JSON SSE provider-error event was parsed
(`error`, `response.failed`, or `response.incomplete` on Responses; an error
object or `type:error` on Z.ai). It is **not** a JSON decoding failure and does
not establish a refusal, outage or malformed request. `sse_decode` is reserved
for malformed stream framing/UTF-8/JSON or an invalid event schema. Existing
retry classification is unchanged: unfamiliar errors still stop immediately.
`stream_error.structure` records only fixed field names, presence/JSON kinds,
lengths/counts and process-local salted fingerprints of short code/type strings.
It never includes arbitrary keys, messages or values. Fingerprints correlate
identical code/type strings within one service process, not across restarts,
and cannot explain an unfamiliar error by themselves.

## Explicitly sensitive private error capture (off by default)

To recover a provider's unfamiliar explanation on a future recurrence, an
operator may explicitly enable this startup setting:

```toml
[agent]
provider_error_capture = true # default false; do not enable casually
```

This implementation does not enable it for you. It affects inference requests
sharing the runtime Provider (chat, workers, compactor, hosted search); it is not
a network trace or capture of HTTP error bodies. Only parsed SSE error events
are eligible. JSON syntax errors, EOFs and body/header failures have no selected
provider error to capture. The first selected error object has the same
precedence as classification (`error`, `response.error`, flat `error` event).
A flat event's `type:error` is an event marker, not a provider error category.

Each private file contains the diagnostic UUID, timestamp, safe structural
metadata and **only the selected string `code`, `type`, and `message` fields** (also numeric `code` for provider variants such as Z.ai).
No whole event, request, response, output/reasoning, tool data, arbitrary property
names or request/response headers is saved. Common bearer/basic authorization,
cookie lines, credential assignments, key/JWT patterns, URLs and long opaque
tokens are conservatively redacted. Fields over 16 KiB are omitted before
redaction; retained code/type are at most 256 bytes and message at most 4096
bytes, otherwise omitted. **This is sensitive diagnostic text, not guaranteed
anonymized data. Providers can echo prompt text, personal information or an
unrecognized credential format; redaction cannot guarantee their removal.**
Review files locally before sharing, and do not upload them with ordinary logs.

Storage is `state_dir/provider-error-events/`, created 0700; snapshot and lock
files are 0600. Existing directories/files with foreign ownership, group/other
permissions, unexpected entries, hard links or symlinks are rejected. Files are
created exclusively via the opened directory descriptor, with no symlink
following or overwriting. A nonblocking lock bounds concurrent writers; capture
is skipped if busy. Storage admits at most **16 snapshots, 8 KiB per snapshot,
128 KiB total**, plus one empty lock file. It **stops when full**, never silently
replaces old evidence. Retention is deliberately operator-owned: inspect and
remove snapshots promptly after diagnosis; disabling capture stops new writes
but does not delete existing evidence. Backups inherit this sensitivity. The
parent state path must be operator-controlled (same-UID attackers are outside
this filesystem isolation boundary).

Normal logs and user-facing errors still exclude selected text; they record
only `private_error_capture=disabled|not_applicable|saved|full|busy|size_limit|unavailable`
(or `unsupported_platform` on non-Unix). No file paths or capture I/O error text
are logged. Capture failure never replaces the provider failure, changes retry
eligibility or dispatches partial tools. A saved diagnostic file does not by
itself prove the provider's explanation is accurate. Unknown historical errors
cannot be recovered retroactively.
