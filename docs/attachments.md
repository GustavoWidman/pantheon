# Discord attachments

Pantheon accepts text with files and attachment-only messages. Existing activation
rules apply: allowed users only, a bot mention in guild channels, or a DM. Discord
controls incoming upload sizes, including Nitro allowances. Pantheon imposes no
incoming file-size ceiling. Downloads, workspace copies and outgoing snapshots
stream to disk; a stalled transfer times out without rejecting files by size.

Gateway checkpoints persist attachment metadata together with the replay cursor.
A separate durable receiving queue renews expired CDN URLs by fetching the
original Discord message. Messages enter a channel in arrival order after their
files download or acquire a visible failure in the manifest. Other channels keep
running. An active root waits for pending same-channel inputs before its next
inference, so downloading a steering attachment cannot cause extra model loops
or a premature final reply. `/stop` cancels pending receiving work in that channel.
Admission and consumption of the receiving queue are atomic and idempotent.

Every file has an ID scoped to its original message, filename, MIME type, byte
count and SHA-256 hash. The prompt/history manifest exposes these identifiers
and a path under `.pantheon-attachments/<channel>/<id>/`. It never includes signed
CDN URLs or base64. Metadata remains after local bytes expire. Downloads and
snapshots are kept beneath `state_dir/attachments`; user-facing workspace copies
are independent files, never hard links to an immutable original.

## Reading files

Supported images are native vision input on the receiving turn. Animated GIFs
remain workspace files; extract individual frames with tools before model input. For public
OpenAI Responses models, supported PDFs, documents, spreadsheets, text and code
are `input_file` content on the original user message. Native PDFs include text
and page images; DOCX/PPTX do not expose embedded images/charts, and spreadsheet
processing can summarize/truncate rows. Detailed tabular work should use the
original workspace file. These are provider behaviors, not attachment admission
limits. See [OpenAI file inputs](https://developers.openai.com/api/docs/guides/file-inputs).

The Codex subscription endpoint is separate from the public API. Native file
support there is undocumented; `codex_native_files` is opt-in, disabled by
default. PDFs still work through the bundled Poppler fallback: `read` or
`attachment open` reads one page of text and returns its page image. Pass `page`
(1-based) to inspect subsequent pages. Public OpenAI PDF inputs can also select
an explicit page to use this path. Nix packages and the development shell ship
`pdftotext` and `pdftoppm`.

```
attachment(action="list", offset=0)
attachment(action="open", id="<id>")
attachment(action="open", id="<id>", page=2)
read(path=".pantheon-attachments/<channel>/<id>/document.pdf", page=3)
```

`attachment open` restores expired originals from Discord when the original
message/file is still accessible. It reports unavailability instead of inventing
file content. Ordinary UTF-8 `read` remains a text result with bounded head/tail;
images and supported binary documents become new provider content after tool
results. Unsupported files remain accessible to workspace/shell/browser tools.
Workers can inspect attachments and return artifact paths privately. Curators
and reviewers can only list/open attachments and read files; their file evidence
never writes main memory or sends public messages.

Provider request allowances apply across the entire native transcript, including
steering and repeated file opens. Oversized/unsupported files are accepted and
materialized, but require tool inspection rather than direct whole-file input.
Model context/vision constraints still apply. Native inputs use inline data, so
there is no uploaded provider file to leave behind. Existing request retries
reuse the same bytes. New files append user content; they do not rewrite earlier
messages, tool definitions, system prompts, or cache-affinity keys. Main/worker
history records identifiers and findings, not binary payloads; frozen curator
memory follows the existing durable memory model.

## Sending files

```
send_file(path="reports/report.pdf", caption="Here’s the report.")
```

Only the orchestrator can publish. The tool freezes the bytes in a durable outbox
snapshot and returns a delivery ID with `state="queued"`; that is not a delivery
receipt. Streaming multipart sends preserve channel order, reply anchors and
restricted mentions. Source-file changes cannot change a retry. There is no
hard-coded outgoing byte limit in Pantheon: Discord accepts or rejects the upload
under its applicable limits. See [Discord uploading files](https://docs.discord.com/developers/reference#uploading-files).

Rate limits honor Discord’s `retry_after`. Permanent rejection becomes a durable
root notification and a visible channel error instead of an infinite retry.
Discord only deduplicates recent nonces. After two minutes from an ambiguous
attempt, Pantheon checks recent messages for its nonce/author receipt; if it
cannot find one, it reports ambiguity and does not blindly resend. This is not
an exactly-once guarantee. Manual replay requires deciding whether the original
send took effect.

## Cleanup and retention

```toml
[attachments]
retention_seconds = 604800        # seven days since last use; 0 retains originals
cleanup_interval_seconds = 300
codex_native_files = false
```

The equivalent Nix settings are `services.pantheon.settings.attachments`.
Pending receiving, active roots/workers/shells, queued or active enabled curators,
and pending file deliveries protect their files. Cleanup conservatively protects
all files in an active channel. It never refreshes retention merely because a
channel is active; actually opening/receiving a file refreshes last use.

Once consumers finish, cleanup removes unchanged workspace copies. It hashes
copies outside runtime threads and SQLite locks, then rechecks activity and
retention transactionally before deletion. Modified copies, symlink-replaced
paths and user project files are preserved. Incoming originals expire after the
configured retention. Sent/terminally failed outgoing snapshots are released
once the channel is idle; the original generated workspace file remains.

```
attachment(action="keep", id="<id>")     # retain indefinitely
attachment(action="release", id="<id>")  # return to configured retention
```

Partial downloads and PDF page renders are temporary. Cancellation cleans them,
and startup cleans crash leftovers and tracked incomplete workspace copies.
Metadata remains available after byte cleanup. Cleanup is disk housekeeping,
not a model task, and does not alter conversation memory or cached prompts.
