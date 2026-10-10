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

## Moving an existing instance

Use the same Discord application, token, allowed users and model settings to keep
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

If startup verification fails, stop the new service and restore the old executable
and service configuration against the **current** state. Never roll memory back to
the snapshot once the new instance may have received messages. Keep the snapshot
for inspection and recovery.
