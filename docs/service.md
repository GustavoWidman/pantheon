# Persistent service and migration

The NixOS module is the system service installation: import
`pantheon.nixosModules.default` and enable `services.pantheon`. It creates a
dedicated account, starts `pantheon.service` at boot, and keeps state and workspace
under `/var/lib/pantheon` by default. Pin a published release in the consuming
flake. Keep the environment file and subscription login outside the Nix store.

On a machine where the operator cannot rebuild NixOS, a persistent systemd user
service can run the same pinned Nix package. Use absolute paths outside `/tmp`:

| Data | Suggested location |
|---|---|
| Configuration and operator instructions | `~/.config/pantheon/` |
| Protected environment file | `~/.config/pantheon/environment` |
| State (memory, journals, skills, browser profiles, attachments) | `~/.local/share/pantheon/state/` |
| Agent workspace and existing projects | `~/.local/share/pantheon/workspace/` |
| Rollback snapshots and deployment records | `~/.local/state/pantheon/deployments/` |

Create directories with mode `0700` and private files with mode `0600`. Set the
absolute `state_dir`, `workspace` and `instructions_file` paths in the configuration.
Keep the existing canonical Codex login location rather than duplicating refresh
tokens. Retain the package with a Nix GC root, such as an indirect root created
with `nix-store --add-root /absolute/path/to/package --indirect --realise
/nix/store/PINNED-PACKAGE`.

Install `~/.config/systemd/user/pantheon.service` with the following structure,
substituting the actual absolute paths and published package:

```ini
[Unit]
Description=Pantheon Discord agent harness
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=/absolute/path/to/workspace
EnvironmentFile=/absolute/path/to/environment
Environment=CODEX_HOME=/absolute/path/to/existing/codex-home
ExecStart=/nix/store/PINNED-PACKAGE/bin/pantheon --config /absolute/path/to/pantheon.toml run
Restart=always
RestartSec=5
TimeoutStopSec=90
KillMode=control-group
UMask=0077
LimitNOFILE=65536

[Install]
WantedBy=default.target
```

Enable user lingering (`loginctl enable-linger USER`, requiring administrator
access when not already enabled), then run `systemctl --user daemon-reload` and
`systemctl --user enable --now pantheon.service`. Lingering starts the user manager
at boot and keeps it running after logout. This uses the existing user account;
the NixOS module additionally provides its dedicated account and service sandbox.

## 0.21.0 native-state boundary

**0.21.0 is not an in-place upgrade of earlier state.** Start with an empty
`state_dir`; the daemon durably initializes its native `state-format` marker.
A nonempty unmarked namespace or unsupported marker is refused read-only, before
creating a daemon lock, Discord ingress/operational databases, gateway or queued
workers. Under the writer lock, startup checks again and validates every native
chat/subagent bank, including idle banks. A lone regular `daemon.lock` is the only
allowed interrupted-empty-bootstrap residue. Do not create or copy markers by
hand to bypass admission. The guard validates native schemas, not historical
formats; it never decodes, converts or replays another format.

An external converter and sealed-history preflight are separate from this
runtime. A synthetic converter test does not establish real-history readiness.
Preserve the old instance/archive unchanged until export, paid-replay authority,
exact-history reference repair, recall checks, delivery reconciliation, ownership
fencing and rollback have separately passed. Never copy old jobs into a new
instance or run both bots against the same Discord identity. No migration,
service activation or self-deployment is implied by this source change.

Merging a version-bumped PR triggers this repository's validated tag/release
workflow, not a service switch. Consumers still choose their package and state
paths. An unattended consumer that automatically updates and restarts against an
older state directory will be refused; pin the old package until an explicit
migration/cutover is ready. The NixOS module does not perform conversion. The `models` command persists a
catalogue, so it uses the same native namespace admission and writer lock; stop
the daemon before standalone model discovery. `doctor` does not initialize state.

## Moving an existing compatible native instance

The instructions below apply only to an already compatible native schema, not
an upgrade from an earlier memory format. Use the same Discord application, token,
allowed users and model settings to keep
the bot identity and channel mapping. The existing Discord display name does not
change just because the service changes its name.

1. Check roots, workers, shells, receiving attachments, queued deliveries and
   active curator/reviewer jobs have settled. Run the packaged `doctor` with the
   preserved configuration and protected environment; it performs no inference.
2. Stop the old service and check again for in-flight work. Snapshot the entire
   state and workspace while stopped, including SQLite WAL files, immutable JSONL
   histories, memory trees/views, skill revisions and browser profiles. Keep the
   previous unit and configuration for rollback. Do not copy a live SQLite file
   without its WAL or use an old snapshot to overwrite new conversation history.
3. Move the state and workspace into their permanent locations, updating only the
   configured filesystem paths. Preserve memory and skill content byte for byte;
   importing histories or starting a fresh skill library is unnecessary.
4. Preserve compatibility for old absolute paths in historical messages, project
   metadata and attachment records with symlinks to the new locations. Recreate
   these aliases at boot, using user tmpfiles or an `ExecStartPre` helper that
   refuses to overwrite unexpected files. The durable data resides in the new
   directories, even if an old path is referenced by a previous conversation.
5. Disable the old service before enabling the new one so two processes never
   consume the same Discord gateway or write the same state. Verify the running
   executable, gateway checkpoint progress, slash command registration, database
   integrity, preserved history/skill revisions and boot enablement.

For a same-format move only, if startup verification fails, stop the new service
and restore the old executable
and service configuration against the **current** state. Never roll memory back to
the snapshot once the new instance may have received messages. Keep the snapshot
for inspection and recovery.

## Global-memory successor boundary

The instance-global coordinator uses `state/memory` and the outer marker
`pantheon-global-state-v2`. Earlier native channel banks (`state/chats`) are
rejected before state mutation, even when their inner native format is valid.
Do not point this successor at dummy3 or main live state. No automatic transition
is provided. A future offline transition must retain all messages received after
a staged snapshot, channel-owned operational queues and stable cache identities.
See [global-memory.md](global-memory.md).
