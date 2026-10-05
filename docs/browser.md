# Browser ownership and durable profiles

Every `browser.open` starts one Camoufox with its own Xvfb display, persistent Firefox profile, loopback-only x11vnc listener and always-running noVNC server. Browser operations use a packaged Python Playwright driver over local JSON-line pipes; the harness, ownership checks and supervision are Rust. The Nix closure contains the Camoufox release and driver, so service startup never downloads a browser or installs packages.

Xvfb allocates displays atomically with `-displayfd`. The `display_start` configuration field is reserved for compatibility; displays are selected by the X server rather than forcing a shared display number. Browsers are independent within the service, while the service user and agent workspace remain a shared trust boundary.

The browser tool accepts these actions:

```json
{"action":"open","url":"https://example.com"}
{"action":"navigate","browser_id":"…","url":"https://example.com"}
{"action":"snapshot","browser_id":"…"}
{"action":"click","browser_id":"…","role":"button","name":"Sign in"}
{"action":"type","browser_id":"…","role":"textbox","name":"Email","text":"…"}
{"action":"screenshot","browser_id":"…"}
{"action":"handoff","browser_id":"…"}
{"action":"resume","browser_id":"…","resume_token":"…"}
{"action":"list"}
{"action":"close","browser_id":"…"}
```

`open`, `list` and `handoff` return viewer URLs. During agent ownership, noVNC is view-only. `handoff` changes the VNC server to interactive mode and establishes a human lease. Rust rejects automation while the lease exists; the worker independently checks the same state. The lease never expires and disconnected viewers never silently resume automation. `resume` requires the explicit lease token and switches viewers back to view-only before permitting automation. The agent should resume only after the user says they have finished. The browser ID belongs to its creating root agent or background subagent; another agent cannot list or control it while that child is active.

When a background subagent finishes, the harness transfers its live browsers to the parent channel before reporting completion. The supervisor atomically replaces and fsyncs each persistent ownership record before updating the live owner under its session lock. The browser process, display, viewer URL, bearer token and any human lease remain unchanged. The parent can then list, hand off, resume or close the adopted browser, and can reopen its retained profile after a restart. Adoption preserves a paused human lease and requires the existing resume token; it does not silently resume automation.

Profiles and their ownership records remain after close or a daemon restart. Reopen an existing profile with `{"action":"open","browser_id":"the previous UUID"}` from the same owner. Viewer tokens rotate on every open, and an old human lease ends with its process. Cookies, disk cache and local browser storage use that durable profile; unsaved in-memory browser state cannot survive a crash. A supervisor action timeout poisons the RPC stream, so the agent must close and reopen before issuing further actions. Shutdown attempts a graceful profile flush and then terminates the worker process group. NixOS additionally kills the entire service cgroup on stop.

## Viewer networking

noVNC binds `0.0.0.0` within the configured inclusive port range (default 6080–6180). VNC itself binds only loopback. WebSocket connections require a random bearer token, with exact token lookup through websockify's `TokenFile` plugin. Static noVNC files contain no secret. Viewer links include the token in their WebSocket path and grant access to this browser; share them only with the intended user. HTTP carries no transport encryption, so use a trusted network such as Tailscale, or place HTTPS in front of the service.

The Rust supervisor enumerates OS IPv4 interfaces and returns candidate links for loopback, LAN and Tailscale addresses present on the host. It does not invoke `tailscale serve` and does not claim a local firewall observation proves remote reachability. Firewall rules, policy routing, cloud security groups and remote ACLs can independently block a candidate link. IPv6 links are not returned because this listener binds IPv4.

The NixOS module leaves browser firewall ports closed unless `openFirewall = true`; that switch opens the full range on all interfaces. For Tailscale-only access, keep it false and configure the interface:

```nix
networking.firewall.interfaces.tailscale0.allowedTCPPortRanges = [
  { from = 6080; to = 6180; }
];
```

The worker atomically binds a socket from the configured range, skips occupied ports and transfers that same socket to websockify. Port assignment among Pantheon's own workers is serialized, and an exhausted range fails startup cleanly. Each live browser costs its own browser, framebuffer, VNC server and WebSocket proxy; opening 101 browsers with the default range is the configured ceiling, not a throughput guarantee.

## NixOS service

```nix
{
  imports = [ inputs.pantheon.nixosModules.default ];
  services.pantheon = {
    enable = true;
    applicationId = 123456789012345678;
    allowedUsers = [ 123456789012345678 ];
    environmentFile = "/run/secrets/pantheon.env";
    settings.agent = {
      model = "anthropic/claude-sonnet-4-6";
      reasoning = "high";
    };
  };
}
```

The environment file holds `DISCORD_TOKEN` and the selected provider's API key. It must be outside the Nix store. Generated TOML contains settings only. State defaults to `/var/lib/pantheon`, owned by the dedicated service user with mode 0700. The service restarts automatically, waits for networking, uses private temporary directories and retains state across upgrades. Agent processes can write the configured workspace and state directory; add deliberate `ReadWritePaths` overrides if their tasks require other paths.

The package supplies Bash, coreutils, Git, ripgrep, curl and findutils on the agent PATH. Add project-specific tools through `services.pantheon.extraPackages = [ pkgs.cargo pkgs.nodejs ];`. `pantheon doctor` checks the packaged executable, worker, noVNC files and required commands without making API calls.

Supported Nix targets are x86_64 Linux and aarch64 Linux. The browser derivation pins Camoufox `156.0.1-beta.34`, validates architecture-specific release SHA256 digests and patches ELF dependencies into the immutable closure. Playwright and noVNC are fixed by `flake.lock`. Driver compatibility is checked with the shipped browser, rather than depending on a separately downloaded stock Firefox. Pantheon uses Camoufox's patched browser through Playwright directly; it does not promise wrapper-generated anti-detection fingerprints.
