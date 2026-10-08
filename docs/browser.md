# Shared browser profile and window ownership

Pantheon runs one Camoufox process with one durable profile at `browsers/pantheon-shared/profile`. Cookies, logins, local storage, IndexedDB, permissions and disk cache are naturally shared across windows. Logging out or changing same-site storage in one window can affect another, as in a normal browser.

Each `browser.open` creates a separately owned window and tab group, with its own loopback x11vnc listener and authenticated noVNC URL on `0.0.0.0`. The windows share one atomically allocated Xvfb display. The supervisor places windows in separate framebuffer regions; each viewer exports only its group's region. Each viewer framebuffer is 1920×1080 (Full HD). Browser windows are inset 16 pixels on each edge (1888×1048 outer window) so borders stay within their region. Page layout follows the real window size; browser chrome and browser privacy settings may reduce the reported content viewport, so page screenshots are not necessarily 1920×1080. noVNC may scale the Full HD framebuffer to fit the client without changing its resolution. Existing running backends retain their old geometry until the service is restarted. File paths, screenshots, viewer tokens and ownership records remain per browser ID. These are logical ownership boundaries within the shared service user, workspace, display and browser profile.

The packaged Python bridge controls the shared browser through Playwright and private Unix pipes. Its profile lock prevents a second backend from unlinking a live socket or opening the same profile. Window workers have separate process groups; killing or closing one worker closes its tabs and viewer without terminating the shared browser. The Nix closure contains the browser and driver, with no runtime installs or downloads.

`max_windows` defaults to 16, bounded additionally by the available viewer ports. Xvfb allocates a framebuffer sized for that capacity, approximately 158 MiB at the default layout (20 Full HD cells, including the hidden keeper and spare grid cells, at four bytes per pixel), while all groups reuse the same browser process and cache. Reduce the window limit to reduce framebuffer memory; the accepted limit is 1–256. `display_start` is retained for configuration compatibility; Xvfb chooses the actual display using `-displayfd`.

The browser tool accepts these actions:

```json
{"action":"open","url":"https://example.com"}
{"action":"navigate","browser_id":"…","url":"https://example.com"}
{"action":"snapshot","browser_id":"…"}
{"action":"tabs","browser_id":"…"}
{"action":"new_tab","browser_id":"…"}
{"action":"select_tab","browser_id":"…","tab_id":"…"}
{"action":"close_tab","browser_id":"…","tab_id":"…"}
{"action":"claim","browser_id":"…"}
{"action":"click","browser_id":"…","role":"button","name":"Sign in"}
{"action":"type","browser_id":"…","role":"textbox","name":"Email","text":"…"}
{"action":"screenshot","browser_id":"…"}
{"action":"upload","browser_id":"…","selector":"input[type=file]","paths":["document.pdf"]}
{"action":"download","browser_id":"…","url":"https://example.com/attachment"}
{"action":"download","browser_id":"…","role":"link","name":"Download attachment"}
{"action":"handoff","browser_id":"…"}
{"action":"resume","browser_id":"…","resume_token":"…"}
{"action":"list"}
{"action":"close","browser_id":"…"}
```

`open`, `list` and `handoff` return viewer URLs. During agent ownership, noVNC is view-only. `handoff` changes the VNC server to interactive mode and establishes a human lease. Rust rejects automation while the lease exists; the worker independently checks the same state. The lease never expires and disconnected viewers never silently resume automation. `resume` requires the explicit lease token and switches viewers back to view-only before permitting automation. The agent should resume only after the user says they have finished. The browser ID belongs to its creating worker or slash-command operator. Another worker cannot list or control it while its owner is active. Tab IDs are scoped to their window group. New tabs and popups are assigned to their opener; unattributed windows are not exposed as another worker’s tabs.

When a background subagent finishes, the harness transfers its live browsers to the parent channel before reporting completion. The supervisor atomically replaces and fsyncs each persistent ownership record before updating the live owner under its session lock. The browser process, display, viewer URL, bearer token and any human lease remain unchanged. The parent can delegate another worker to list and `claim` an adopted live browser before continuing work. Human slash commands can also manage the adopted browser. Adoption preserves a paused human lease and requires the existing resume token; it does not silently resume automation.

Closing a group closes only its pages and viewer. The shared profile remains open and keeps its cache and login state. Browser IDs and ownership records remain after close; reopening an ID from its owner creates a fresh window using the shared profile, with a new viewer token. Service shutdown gracefully flushes the shared profile and then terminates the backend group. A browser-process crash affects every window; close the failed groups and reopen them. Disk-backed profile state survives normal restarts; unsaved in-memory state cannot survive a crash. Old per-browser profile directories are retained during upgrade but are not merged into the new shared profile.

Firefox/GTK has one core keyboard focus. Agent input operations are serialized. During a human handoff, other windows can navigate and take semantic snapshots, but operations that change focus (including typing, clicking, selecting tabs, screenshots and opening windows) are rejected until explicit resume. Only one group can hold a human input lease at once. Each VNC server uses its own protected control file, so handing off one viewer does not enable input in the others. Clipboard forwarding is receive-only during an explicit human handoff; view-only viewers cannot change the clipboard. Clipboard contents are never exported from the shared display to any viewer. The backend clears display-wide selections and legacy cut buffers on handoff, resume, owner-window close/worker disconnect, and when a viewer disconnects during its lease. Disconnecting a viewer clears clipboard data but **does not** release the human lease or resume automation. Other windows cannot paste during that lease, and the next lease begins with an empty remote clipboard.

### Pasting during handoff

Open noVNC's sidebar **Clipboard** panel, paste text into its text area using your local shortcut (Mac **Cmd+V**), then focus the remote browser field and use **Ctrl+V** or its **Paste** context-menu action. The remote browser runs Linux: Cmd+V is not its paste shortcut. This route sends VNC ClientCutText rather than reading your system clipboard automatically, and works over a Tailscale HTTP viewer. Direct system-clipboard integration depends on the local browser and secure-context/permission restrictions; an HTTP noVNC page cannot generally read it through the browser Clipboard API.

The bundled x11vnc/noVNC legacy ClientCutText route supports Latin-1 text; characters outside that range may be replaced by noVNC. This change does not add extended Unicode clipboard negotiation.

Only the remote clipboard is cleared by the server. Your local system clipboard and noVNC text area remain under your local browser's control: clear them after pasting sensitive text or close the viewer tab. Clipboard cleanup does not erase text already pasted into a page, undo a login, or isolate same-site storage in the shared profile. Same-window viewer URLs still grant the same lease to their intended user; do not share them with others.

## Authenticated file transfers

`upload` sets the selected file input directly (including hidden inputs), avoiding
native OS file pickers. Supply `selector` or `role`/`name` plus `paths`, an array
of 1–20 existing regular files. Relative paths are resolved against the configured
workspace; absolute paths must also remain inside it. Symlink escapes, directories
and missing files are rejected. Each file is limited to 50 MiB. The response
reports `files` with canonical `path` and byte `size`, and `done`; it does **not**
mean the website accepted or submitted the form. Verify any upload status and
complete the site's final confirmation before claiming submission.

`download` accepts **either** an HTTP(S) `url` **or** a `selector`/`role`/`name`
click target. URL mode performs a cookie-authenticated GET using the active browser
context, supports inline PDFs and leaves the current form/page untouched. It does
not reproduce JavaScript-added authorization headers; use click mode for downloads
that depend on page logic. Click mode arms the download event before clicking.

The supervisor generates a unique `browser-download-<uuid>` file in the configured
workspace. Download requests cannot choose destinations, and remote suggested
filenames are returned as metadata only, never used as local paths. The result
contains `path`, `suggested_filename`, `size` in bytes and the download `url`.
Successful artifacts are flushed to disk and survive browser closure/restart.
Existing files are never overwritten and partial artifacts are removed on write
failure. Saved downloads are limited to 50 MiB and actions use the normal browser
deadline. This is an acceptance/storage limit, not a hard network or memory cap:
Playwright buffers URL responses before the final size check (Content-Length is
checked early when available); browser-triggered downloads also finish before saving. Transfers require the
same owner and explicit handoff resume as other automation; other windows also
cannot transfer files while a user holds keyboard focus. Website content and
filenames remain untrusted.

## Viewer networking

noVNC binds `0.0.0.0` within the configured inclusive port range (default 6080–6180). VNC itself binds only loopback. WebSocket connections require a random bearer token, with exact token lookup through websockify's `TokenFile` plugin. Static noVNC files contain no secret. Viewer links include the token in their WebSocket path and grant access to this browser; share them only with the intended user. HTTP carries no transport encryption, so use a trusted network such as Tailscale, or place HTTPS in front of the service.

The Rust supervisor enumerates OS IPv4 interfaces and returns candidate links for loopback, LAN and Tailscale addresses present on the host. It does not invoke `tailscale serve` and does not claim a local firewall observation proves remote reachability. Firewall rules, policy routing, cloud security groups and remote ACLs can independently block a candidate link. IPv6 links are not returned because this listener binds IPv4.

The NixOS module leaves browser firewall ports closed unless `openFirewall = true`; that switch opens the full range on all interfaces. For Tailscale-only access, keep it false and configure the interface:

```nix
networking.firewall.interfaces.tailscale0.allowedTCPPortRanges = [
  { from = 6080; to = 6180; }
];
```

The worker atomically binds a socket from the configured range, skips occupied ports and transfers that same socket to websockify. Port assignment among Pantheon's own workers is serialized, and an exhausted range fails startup cleanly. Each live group costs a window, VNC server and WebSocket proxy. The browser process, profile, disk cache and framebuffer are shared; the window limit and available port range bound concurrency.

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

The environment file holds `DISCORD_TOKEN` and any selected API provider keys. Codex models instead use the service account’s separate ChatGPT login cache; see [authentication](auth.md). Secrets must stay outside the Nix store. Generated TOML contains settings only. State defaults to `/var/lib/pantheon`, owned by the dedicated service user with mode 0700. The service restarts automatically, waits for networking, uses private temporary directories and retains state across upgrades. Agent processes can write the configured workspace and state directory; add deliberate `ReadWritePaths` overrides if their tasks require other paths.

The package supplies Bash, coreutils, Git, ripgrep, curl and findutils on the agent PATH. Add project-specific tools through `services.pantheon.extraPackages = [ pkgs.cargo pkgs.nodejs ];`. `pantheon doctor` checks the packaged executable, worker, noVNC files and required commands without making API calls.

Supported Nix targets are x86_64 Linux and aarch64 Linux. The browser derivation pins Camoufox `156.0.1-beta.34`, validates architecture-specific release SHA256 digests and patches ELF dependencies into the immutable closure. Playwright and noVNC are fixed by `flake.lock`. Driver compatibility is checked with the shipped browser, rather than depending on a separately downloaded stock Firefox. Pantheon uses Camoufox's patched browser through Playwright directly; it does not promise wrapper-generated anti-detection fingerprints.
