# Tool activity

Coordinator calls render one bounded line per call inside chronological fences.
Consecutive calls accumulate; prose closes the current fence, and later calls start
another. Fences roll over after 12 entries without dropping rows. Each tool event
retains its original segment and Discord receipt, so progress and completion always
edit that exact row even after intervening prose or newer fences.

```text
◇ shell · sleep 20 · ↑ 1 · 1.2s
↗ shell · sleep 20 · ↑ 1 · 5.0s
↙ shell · sleep 20 · ↑ 1 ↓ 0 lines · exit 0 · 20.0s
```

Those are successive versions of one row, not three posts. `◇` means executing,
`↗` means detached after the configured threshold, and `↙` means that detached job
returned. Foreground completion uses `✓`; tool errors/cancellation use `✗`.
Detached nonzero exits retain the return arrow and display their exit code.
Durations under one second use milliseconds after completion; running rows tick
once per second. Discord delivery still throttles edits and honors rate limits,
so not every local tick needs a network edit. Typing refresh stays every four seconds.

Shell previews prefer effects and test commands over setup, comments and imports,
and recognize Python heredocs/inline scripts and common shell/Nix wrappers. They are
descriptors, not a complete shell parser. Read/write show paths, browser actions show
their target, search shows the query, and integrations show their operation. Preview
selection happens before clipping. Newlines, fence delimiters, credential assignments,
long blobs and URL credentials/query parameters are removed from incidental previews.
Password-entry text and MCP arguments never appear there. Full authorized tool
arguments and results remain in the private transcript.

`↑` counts supplied shell/script or write-text lines. `↓` counts actual read-file
lines or combined shell stdout/stderr lines, including lines omitted from retained
output. Shell output counts also update during execution when available. Legacy
truncated results can only provide a lower bound, marked `≥`. Counts are line counts,
not tokens. Empty completed output is zero; unavailable counts are omitted.

Detached shell completion updates and its durable owner-inbox event commit together. A job
finishing during the detach transition stays terminal; the background receipt cannot
reset it to running. Restart marks interrupted rows and edits their original fences,
without replaying shell effects. Worker tool activity stays private.

New user input accumulates durably while the model's requested tool batch finishes.
All results precede queued steering in the next provider request. Steering does not
skip requested tools. `/stop` remains the explicit cancellation mechanism.
