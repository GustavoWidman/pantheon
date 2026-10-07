# Provider transport diagnostics

Failed chat, worker, compactor and hosted-search requests emit a `provider request failed` warning. The warning includes a JSON `diagnostic` record; its `id` also appears as `[diagnostic UUID]` in the returned error, including Discord notices and worker reports. These warnings are enabled by the default `pantheon=info` log filter.

For NixOS, retrieve the matching record with:

```sh
journalctl -u pantheon.service --grep 'DIAGNOSTIC_UUID' --no-pager
```

For a systemd user service, use `journalctl --user -u SERVICE_NAME`. Services configured to append output to a file must be searched in that file instead. Preserve the complete warning line, including its timestamp and span fields. `provider_step` spans identify the channel, agent owner, run and step; `compaction` spans identify the channel and memory node.

The record contains the model, attempt, configured request timeout, time to headers, total elapsed time, HTTP status/version, provider request ID and Cloudflare ray ID when supplied. Body diagnostics include bytes/chunks received, time to first byte, time since the last chunk, parsed SSE event count, the last recognized event type and completed output-item count.

Failure stages distinguish `headers`, `http_status`, `body`, `premature_eof`, `sse_decode`, `json_decode` and `size_limit`. A transport error additionally records reqwest's timeout/connect/body/decode/request flags, bounded underlying error causes, and OS error information when available. Unknown values remain `null`; for example, a clean SSE EOF without a completed response has no reqwest error flags. A body failure after HTTP 200 does not imply successful model completion. Compare elapsed time with the configured timeout and retain the provider request ID when investigating upstream failures.

Request bodies, response bodies, tool arguments, reasoning, credentials, account IDs, cookies and arbitrary headers are excluded. Only selected opaque response identifiers and recognized event names are recorded. URLs are removed from reqwest errors and redacted in transport causes. Payload-decoding errors never render their underlying error strings into the diagnostic record. Cause chains and fields are bounded to keep failures from generating unbounded logs.

These diagnostics do not add inference retries or change tool dispatch: tools are still dispatched only after a complete provider response. “No tools from this response executed” refers to that failed response; earlier steps can already have completed tool effects.
