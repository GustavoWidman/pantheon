#!/usr/bin/env python3
"""JSON-line RPC; one shared Camoufox profile, private window viewers and authenticated noVNC.

Only the Rust supervisor starts this process. stdout is exclusively protocol frames.
Browser/driver installs never happen at runtime; website file transfers use the active session.
"""
import argparse
import asyncio
import contextlib
import ctypes
import ctypes.util
import math
import errno
import fcntl
import hashlib
import json
import logging
import os
from pathlib import Path
import socket
import stat
import io
import re
import shlex
import subprocess
import sys
import uuid


MAX_TRANSFER_BYTES = 50 * 1024 * 1024


def unix_socket_path(state_path):
    # Linux sockaddr_un allows at most 107 pathname bytes. Neither the state
    # root nor TMPDIR is length-bounded; keep both RPC endpoints under /tmp.
    # Canonical paths distinguish workspaces, windows and endpoint purposes.
    digest = hashlib.sha256(os.fsencode(state_path.resolve())).hexdigest()[:32]
    return Path("/tmp") / f"pantheon-sock-{os.getuid()}-{digest}" / "rpc.sock"


def prepare_unix_socket(state_path):
    path = unix_socket_path(state_path)
    path.parent.mkdir(mode=0o700, exist_ok=True)
    info = path.parent.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise PermissionError("browser socket directory must be a private, user-owned directory (0700)")
    # Only after verifying the directory may we remove a stale endpoint. The
    # backend's profile lock excludes another server for the same state root.
    path.unlink(missing_ok=True)
    return path


def remove_unix_socket(path):
    if path is not None:
        try:
            path.unlink(missing_ok=True)
            path.parent.rmdir()
        except FileNotFoundError:
            pass
        except OSError:
            # Never remove unexpected contents recursively, and do not let a
            # cleanup failure skip viewer/process/profile-lock teardown.
            logging.warning("could not remove browser socket directory %s", path.parent)


def workspace_file(workspace, value, writing=False):
    root = workspace.resolve(strict=True)
    path = Path(value).resolve(strict=not writing)
    if not path.is_relative_to(root):
        raise ValueError("file transfer path escapes workspace")
    if writing:
        if path.exists() or path.is_symlink():
            raise ValueError("download destination already exists")
        if not path.parent.is_dir():
            raise ValueError("download destination parent does not exist")
    elif not path.is_file():
        raise ValueError("upload path must be a regular file")
    return path


def save_artifact(destination, source):
    # Exclusive create: never overwrite an existing artifact or follow a symlink.
    created = False
    try:
        with destination.open("xb") as output:
            created = True
            size = 0
            while chunk := source.read(64 * 1024):
                size += len(chunk)
                if size > MAX_TRANSFER_BYTES:
                    raise ValueError("download exceeds 50 MiB limit")
                output.write(chunk)
            output.flush()
            os.fsync(output.fileno())
        directory = os.open(destination.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
        return size
    except Exception:
        if created:
            destination.unlink(missing_ok=True)
        raise


def locator_for(page, request):
    if request.get("role"):
        return page.get_by_role(request["role"], name=request.get("name", ""), exact=True)
    if request.get("selector"):
        return page.locator(request["selector"])
    raise ValueError("provide role/name or a CSS selector")


async def transfer(page, request, workspace):
    action = request["action"]
    if workspace is None:
        raise ValueError("browser file transfers require a configured workspace")
    if action == "upload":
        values = request.get("paths")
        if not isinstance(values, list) or not 1 <= len(values) <= 20 or not all(isinstance(v, str) for v in values):
            raise ValueError("upload requires 1 to 20 workspace files")
        paths = [workspace_file(workspace, value) for value in values]
        if any(path.stat().st_size > MAX_TRANSFER_BYTES for path in paths):
            raise ValueError("upload file exceeds 50 MiB limit")
        # Hidden file inputs are supported without opening the OS file picker.
        await locator_for(page, request).set_input_files([str(path) for path in paths])
        return {"done": True, "url": page.url, "files": [{"path": str(path), "size": path.stat().st_size} for path in paths]}
    has_url = isinstance(request.get("url"), str)
    has_locator = bool(request.get("role") or request.get("selector"))
    if has_url == has_locator:
        raise ValueError("download requires either url or role/name/selector")
    destination = workspace_file(workspace, request["path"], writing=True)
    if has_url:
        url = request["url"]
        if not url.startswith(("http://", "https://")):
            raise ValueError("download requires an http(s) URL")
        # Context request shares browser cookies, handles inline PDFs too, and
        # does not navigate away from an in-progress form.
        response = await page.context.request.get(url, timeout=30000)
        try:
            if not response.ok:
                raise ValueError(f"download HTTP status {response.status}")
            length = response.headers.get("content-length")
            if length and int(length) > MAX_TRANSFER_BYTES:
                raise ValueError("download exceeds 50 MiB limit")
            data = await response.body()
            if len(data) > MAX_TRANSFER_BYTES:
                raise ValueError("download exceeds 50 MiB limit")
            disposition = response.headers.get("content-disposition", "")
            match = re.search(r'filename="([^"\r\n]*)"', disposition)
            filename = match.group(1) if match else "download"
            size = save_artifact(destination, io.BytesIO(data))
            return {"path": str(destination), "suggested_filename": filename, "size": size, "url": response.url}
        finally:
            await response.dispose()
    # Arm before clicking, even when the response arrives immediately.
    async with page.expect_download(timeout=30000) as pending:
        await locator_for(page, request).click()
    download = await pending.value
    try:
        failure = await download.failure()
        if failure:
            raise ValueError(f"download failed: {failure}")
        with Path(await download.path()).open("rb") as source:
            size = save_artifact(destination, source)
        return {"path": str(destination), "suggested_filename": download.suggested_filename, "size": size, "url": download.url}
    finally:
        await download.delete()


# A viewer exports exactly one Full HD tile, including browser chrome. Keep a
# small inset so GTK window borders cannot cross into another viewer's region.
DESKTOP_WIDTH = 1920
DESKTOP_HEIGHT = 1080
WINDOW_INSET = 16


def cell_origin(slot, columns):
    return (slot % columns) * DESKTOP_WIDTH, (slot // columns) * DESKTOP_HEIGHT


def window_geometry(slot, columns):
    x, y = cell_origin(slot, columns)
    return (
        x + WINDOW_INSET,
        y + WINDOW_INSET,
        DESKTOP_WIDTH - 2 * WINDOW_INSET,
        DESKTOP_HEIGHT - 2 * WINDOW_INSET,
    )


def viewer_clip(slot, columns):
    x, y = cell_origin(slot, columns)
    return f"{DESKTOP_WIDTH}x{DESKTOP_HEIGHT}+{x}+{y}"


def emit(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)


def start(argv, **kwargs):
    return subprocess.Popen(argv, stdin=subprocess.DEVNULL, stderr=sys.stderr, **kwargs)


async def wait_listener(port, process):
    for _ in range(200):
        if process.poll() is not None:
            raise RuntimeError(f"desktop component exited with status {process.returncode}")
        try:
            reader, writer = await asyncio.open_connection("127.0.0.1", port)
            writer.write(b"GET /vnc.html HTTP/1.0\r\nHost: localhost\r\n\r\n")
            await writer.drain()
            status = await asyncio.wait_for(reader.readline(), 2)
            writer.close()
            await writer.wait_closed()
            if b" 200 " in status:
                return
            raise RuntimeError("noVNC static page unavailable; check packaged web directory")
        except (OSError, asyncio.TimeoutError):
            await asyncio.sleep(0.05)
    raise RuntimeError("desktop listener did not start")


class BrowserUnavailable(RuntimeError):
    """Terminal session error: never retry a page action automatically."""


def browser_health_error(closed, connected=True):
    if closed or not connected:
        return "shared browser context closed; close and reopen this browser_id (profile retained)"
    return None


async def run(args):

    children = []
    writer = None
    disconnect_server = None
    disconnect_tasks = set()
    disconnect_path = None
    token_file = args.profile / "viewer.tokens"
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        # Reserve the real socket atomically, then transfer it to the proxy.
        # Never probe a port and release it before starting the server.
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        reserved = {int(port) for port in args.reserved_ports.split(",") if port}
        for candidate in range(args.port, (args.port_end or args.port) + 1):
            if candidate in reserved:
                continue
            try:
                listener.bind(("0.0.0.0", candidate))
                args.port = candidate
                break
            except OSError as error:
                if error.errno != errno.EADDRINUSE:
                    raise
        else:
            raise RuntimeError("browser port range exhausted by existing listeners")
        listener.listen(128)
        reader, writer = await asyncio.open_unix_connection(str(unix_socket_path(args.shared_root / "backend.sock")))
        rpc_lock = asyncio.Lock()
        async def rpc(request):
            async with rpc_lock:
                return await unlocked_rpc(request)
        async def unlocked_rpc(request):
            try:
                writer.write((json.dumps(request) + "\n").encode())
                await writer.drain()
                frame = await reader.readline()
                if not frame:
                    raise BrowserUnavailable("shared browser backend exited; close and reopen this browser_id (profile retained)")
                response = json.loads(frame)
            except (OSError, ValueError) as error:
                raise BrowserUnavailable("shared browser backend connection lost; close and reopen this browser_id (profile retained)") from error
            if "error" in response:
                error_type = BrowserUnavailable if response.get("fatal") else RuntimeError
                raise error_type(response["error"])
            return response
        ready = await rpc({"action": "open", "browser_id": args.profile.name})
        display = ready["display"]
        control_file = args.profile / "viewer.control"
        control_file.write_text("")
        # x11vnc invokes this hook for every departing viewer. Keep the clipboard
        # on the owning backend connection; never clear a different group's lease.
        async def disconnected(reader, event_writer):
            try:
                if await asyncio.wait_for(reader.readline(), 2) == b"clear\n":
                    await rpc({"action": "clear_clipboard"})
            except Exception:
                pass  # Window teardown also clears the selection in the backend.
            finally:
                event_writer.close()
                with contextlib.suppress(Exception):
                    await event_writer.wait_closed()
        def start_disconnected(reader, event_writer):
            task = asyncio.create_task(disconnected(reader, event_writer))
            disconnect_tasks.add(task)
            task.add_done_callback(disconnect_tasks.discard)
        disconnect_path = prepare_unix_socket(args.profile / "clipboard-disconnect.sock")
        disconnect_server = await asyncio.start_unix_server(start_disconnected, path=str(disconnect_path))
        disconnect_path.chmod(0o600)
        gone = shlex.join([sys.executable, __file__, "--clipboard-disconnect", str(disconnect_path), "--web", str(args.web)])
        # -autoport starts at 5900 and lets the OS choose; parse x11vnc's PORT frame.
        # This is our private Xvfb, with no login/display manager. Initialize
        # selection ownership immediately rather than x11vnc's 45s DM grace period.
        vnc = start([os.environ.get("PANTHEON_X11VNC", "x11vnc"), "-display", display, "-localhost", "-autoport", "5900", "-forever", "-shared", "-nopw", "-viewonly", "-quiet", "-clip", ready["clip"], "-connect", str(control_file), "-novncconnect", "-seldir", "recv", "-nosetprimary", "-input", "KMBC,", "-gone", gone, "-nowireframe", "-noscrollcopyrect"], stdout=subprocess.PIPE, env=dict(os.environ, X11VNC_AVOID_WINDOWS="never"))
        children.append(vnc)
        async def vnc_port():
            while True:
                line = await asyncio.to_thread(vnc.stdout.readline)
                if not line:
                    raise RuntimeError("x11vnc exited before reporting its port")
                if line.startswith(b"PORT="):
                    return int(line.split(b"=", 1)[1])
        local_port = await asyncio.wait_for(vnc_port(), 20)
        token = os.environ["PANTHEON_BROWSER_TOKEN"]
        token_file.write_text(f"{token}: 127.0.0.1:{local_port}\n")
        token_file.chmod(0o600)
        # TokenFile routes only exact 256-bit bearer tokens. Missing/invalid tokens
        # fail before a VNC connection. Static noVNC files contain no credentials.
        proxy = start([sys.executable, __file__, "--proxy-fd", str(listener.fileno()), "--web", str(args.web), "--token-file", str(token_file)], stdout=sys.stderr, pass_fds=(listener.fileno(),))
        children.append(proxy)
        listener.close()
        await wait_listener(args.port, proxy)
        async def viewer_control(control):
            result = await asyncio.create_subprocess_exec(os.environ.get("PANTHEON_X11VNC", "x11vnc"), "-display", display, "-connect", str(control_file), "-sync", "-R", control, stdout=sys.stderr, stderr=sys.stderr)
            if await result.wait() != 0:
                raise RuntimeError("failed to change browser viewer ownership")
        human = False
        emit({"ready": True, "display": display, "port": args.port, "profile": "pantheon-shared"})
        while True:
            line = await asyncio.to_thread(sys.stdin.readline)
            if not line:
                break
            try:
                request = json.loads(line)
                action = request.get("action")
                if any(child.poll() is not None for child in children):
                    raise BrowserUnavailable("a browser desktop component exited; close and reopen this browser_id (profile retained)")
                if action == "close":
                    await viewer_control("viewonly")
                    await rpc({"action": "close"})
                    emit({"closed": True})
                    break
                if action in ("handoff", "resume"):
                    if action == "handoff":
                        human = True  # A partially applied VNC command fails closed.
                        await rpc({"action": "handoff"})
                    control = "noviewonly" if action == "handoff" else "viewonly"
                    await viewer_control(control)
                    if action == "resume":
                        await rpc({"action": "resume"})
                    human = action == "handoff"
                    emit({"state": "human" if human else "agent"})
                    continue
                if action == "health":
                    emit(await rpc(request))
                    continue
                if human:
                    raise RuntimeError("automation paused while human owns browser")
                if action == "screenshot":
                    request["path"] = str(args.profile / ("screenshot-" + uuid.uuid4().hex + ".png"))
                emit(await rpc(request))
            except Exception as error:
                emit({"error": str(error)[:2000], "fatal": isinstance(error, BrowserUnavailable)})
    finally:
        if disconnect_server:
            await close_connections(disconnect_server, disconnect_tasks)
        remove_unix_socket(disconnect_path)
        listener.close()
        for child in reversed(children):
            with contextlib.suppress(ProcessLookupError):
                child.terminate()
        for child in reversed(children):
            try:
                await asyncio.wait_for(asyncio.to_thread(child.wait), 3)
            except asyncio.TimeoutError:
                child.kill()
                await asyncio.to_thread(child.wait)
        # Stop interactive viewers before releasing backend focus/selection ownership.
        if writer:
            writer.close()
            with contextlib.suppress(Exception):
                await writer.wait_closed()
        token_file.unlink(missing_ok=True)



# Xlib is already in the browser closure. Keep window placement out of page JS
# except for a short-lived title marker used to identify a newly created window.
class XWindows:
    def __init__(self, display):
        self.x = ctypes.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")
        self.x.XOpenDisplay.restype = ctypes.c_void_p
        self.x.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.display = self.x.XOpenDisplay(display.encode())
        if not self.display:
            raise RuntimeError("cannot connect to shared X display")
        for name, restype, argtypes in (
            ("XDefaultRootWindow", ctypes.c_ulong, [ctypes.c_void_p]),
            ("XQueryTree", ctypes.c_int, [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)), ctypes.POINTER(ctypes.c_uint)]),
            ("XFetchName", ctypes.c_int, [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_void_p)]),
            ("XInternAtom", ctypes.c_ulong, [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]),
            ("XGetWindowProperty", ctypes.c_int, [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_long, ctypes.c_long, ctypes.c_int, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_void_p)]),
            ("XFree", ctypes.c_int, [ctypes.c_void_p]),
            ("XMoveResizeWindow", ctypes.c_int, [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int, ctypes.c_uint, ctypes.c_uint]),
            ("XSetSelectionOwner", ctypes.c_int, [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong]),
            ("XDeleteProperty", ctypes.c_int, [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong]),
            ("XSync", ctypes.c_int, [ctypes.c_void_p, ctypes.c_int]),
            ("XFlush", ctypes.c_int, [ctypes.c_void_p]),
            ("XCloseDisplay", ctypes.c_int, [ctypes.c_void_p]),
        ):
            function = getattr(self.x, name)
            function.restype, function.argtypes = restype, argtypes
        # Windows can disappear between enumeration and placement. Ignore BadWindow.
        self.error_handler = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)(lambda *_: 0)
        self.x.XSetErrorHandler.argtypes = [ctypes.c_void_p]
        self.x.XSetErrorHandler(self.error_handler)
        self.root = self.x.XDefaultRootWindow(self.display)

    def named(self, marker):
        root, parent = ctypes.c_ulong(), ctypes.c_ulong()
        children, count = ctypes.POINTER(ctypes.c_ulong)(), ctypes.c_uint()
        self.x.XQueryTree(self.display, self.root, ctypes.byref(root), ctypes.byref(parent), ctypes.byref(children), ctypes.byref(count))
        found = []
        try:
            for index in range(count.value):
                name = ctypes.c_void_p()
                actual, count_items, remaining, fmt = ctypes.c_ulong(), ctypes.c_ulong(), ctypes.c_ulong(), ctypes.c_int()
                atom = self.x.XInternAtom(self.display, b"_NET_WM_NAME", 0)
                self.x.XGetWindowProperty(self.display, children[index], atom, 0, 65536, 0, 0, ctypes.byref(actual), ctypes.byref(fmt), ctypes.byref(count_items), ctypes.byref(remaining), ctypes.byref(name))
                if not name.value:
                    self.x.XFetchName(self.display, children[index], ctypes.byref(name))
                if name.value:
                    try:
                        if marker in ctypes.string_at(name).decode("utf-8", "replace"):
                            found.append(children[index])
                    finally:
                        self.x.XFree(name)
        finally:
            if children:
                self.x.XFree(children)
        return found

    def place(self, window, slot, columns):
        self.x.XMoveResizeWindow(self.display, window, *window_geometry(slot, columns))
        self.x.XFlush(self.display)

    def clear_clipboard(self):
        # X selections are display-wide. Drop both owners plus legacy cut buffers
        # before releasing keyboard focus, so the next window cannot paste a secret.
        for name in ("CLIPBOARD", "PRIMARY", "SECONDARY"):
            atom = self.x.XInternAtom(self.display, name.encode(), 0)
            self.x.XSetSelectionOwner(self.display, atom, 0, 0)
        for index in range(8):
            atom = self.x.XInternAtom(self.display, f"CUT_BUFFER{index}".encode(), 0)
            self.x.XDeleteProperty(self.display, self.root, atom)
        self.x.XSync(self.display, 0)

    def close(self):
        self.x.XCloseDisplay(self.display)


class WindowGroup:
    def __init__(self, identity, slot):
        self.identity, self.slot = identity, slot
        self.pages, self.windows = {}, set()
        self.active = None
        self.human = False

    def add(self, page):
        if page not in self.pages.values():
            identity = uuid.uuid4().hex
            self.pages[identity] = page
            self.active = identity

    def live(self):
        self.pages = {identity: page for identity, page in self.pages.items() if not page.is_closed()}
        if self.active not in self.pages:
            self.active = next(iter(self.pages), None)
        return self.pages


async def close_connections(server, connections, timeout=5):
    deadline = asyncio.get_running_loop().time() + timeout
    if server:
        server.close()
    # Python 3.13+ Server.wait_closed waits for active client connections.
    # Cancel and close them first; otherwise Firefox death leaves the backend
    # blocked forever with its profile lock held and viewers falsely alive.
    tasks = list(connections)
    for task in tasks:
        task.add_done_callback(lambda done: None if done.cancelled() else done.exception())
        task.cancel()
    # A cancelled task can still block in its finally clause. asyncio.wait
    # bounds that cleanup without awaiting another round of cancellation.
    pending = set()
    if tasks:
        _, pending = await asyncio.wait(tasks, timeout=max(0, deadline - asyncio.get_running_loop().time()))
    if pending:
        logging.warning("browser client cleanup exceeded deadline (%d tasks)", len(pending))
        for task in pending:
            task.cancel()
    if server:
        # Python 3.13+ can abort accepted transports even if a client's
        # cancellation cleanup did not finish. Older versions still return at
        # the deadline instead of preventing browser/process cleanup.
        abort = getattr(server, "abort_clients", None)
        if pending and abort:
            abort()
        try:
            await asyncio.wait_for(server.wait_closed(), max(0, deadline - asyncio.get_running_loop().time()))
        except asyncio.TimeoutError:
            if abort:
                abort()
            logging.warning("browser server closure exceeded teardown deadline")


async def backend(args):
    from playwright.async_api import async_playwright
    args.shared_root.mkdir(mode=0o700, parents=True, exist_ok=True)
    profile_lock = (args.shared_root / "backend.lock").open("a+")
    fcntl.flock(profile_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    socket_path = None
    capacity = min(args.capacity, 256)
    columns = math.ceil(math.sqrt(capacity + 1))
    rows = math.ceil((capacity + 1) / columns)
    display_server = start([os.environ.get("PANTHEON_XVFB", "Xvfb"), "-displayfd", "1", "-screen", "0", f"{columns * DESKTOP_WIDTH}x{rows * DESKTOP_HEIGHT}x24", "-nolisten", "tcp"], stdout=subprocess.PIPE)
    browser = playwright = x = server = None
    connections = set()
    groups = {}
    stopped = asyncio.Event()
    stopping = False
    context_closed = False
    opening = asyncio.Lock()
    # Firefox/GTK has one core keyboard focus. Serialize input operations, and
    # do not let another agent steal it during an explicit human lease.
    focus = asyncio.Lock()

    async def identify(page, group):
        marker = "pantheon-window-" + uuid.uuid4().hex
        old = await page.title()
        try:
            await page.evaluate("title => document.title = title", marker)
            for _ in range(100):
                windows = x.named(marker)
                if windows:
                    group.windows.update(windows)
                    for window in windows:
                        x.place(window, group.slot, columns)
                    return
                await asyncio.sleep(0.02)
            # Background tabs use their already identified parent window.
            if not group.windows:
                raise RuntimeError("could not identify browser window")
        finally:
            if not page.is_closed():
                await page.evaluate("title => document.title = title", old)

    def owner_of(page):
        return next((group for group in groups.values() if page in group.pages.values()), None)

    async def new_page(page):
        # Explicit opens are registered by their RPC before classifying popups.
        await asyncio.sleep(0.1)
        if owner_of(page):
            return
        opener = await page.opener()
        group = owner_of(opener) if opener else None
        if group is None:
            await asyncio.sleep(0.5)
            if owner_of(page):
                return
            group = next((group for group in groups.values() if group.human), None)
        if group:
            group.add(page)
            await identify(page, group)
        elif not page.is_closed():
            # Unattributed windows never become visible to another agent.
            await page.close()

    async def close_group(group):
        groups.pop(group.identity, None)
        if group.human and x:
            x.clear_clipboard()
        group.human = False
        if stopping:
            return
        for page in list(group.pages.values()):
            if not page.is_closed():
                with contextlib.suppress(Exception):
                    await page.close()

    async def act(group, request):
        action = request.get("action")
        if action == "close":
            await close_group(group)
            return {"closed": True}
        if action == "clear_clipboard":
            if group.human:
                x.clear_clipboard()
            return {"done": True}
        if action == "handoff":
            async with focus:
                if any(other.human and other is not group for other in groups.values()):
                    raise RuntimeError("another window has human input ownership; resume it first")
                x.clear_clipboard()
                group.human = True
                if group.live():
                    await group.pages[group.active].bring_to_front()
            return {"state": "human"}
        if action == "resume":
            if not group.human:
                raise RuntimeError("this window has no human input lease")
            x.clear_clipboard()
            for identity, page in group.live().items():
                if await page.evaluate("document.visibilityState") == "visible":
                    group.active = identity
            group.human = False
            return {"state": "agent"}
        if group.human:
            raise RuntimeError("automation paused while human owns this window")
        if action in ("click", "type", "select_tab", "new_tab", "close_tab", "screenshot", "upload", "download"):
            async with focus:
                if any(other.human for other in groups.values()):
                    raise RuntimeError("shared browser keyboard focus is leased to the user; navigation and snapshots in other windows remain available")
                return await page_action(group, request)
        return await page_action(group, request)

    async def page_action(group, request):
        action = request.get("action")
        group.live()
        if not group.pages:
            # Closing every tab affects this group only; the hidden keeper holds
            # the shared profile alive. Recreate a blank window in the same slot.
            async with opening:
                page = await browser.new_page()
                group.add(page)
                await identify(page, group)
        page = group.pages[group.active]
        page.set_default_timeout(20000)
        if action == "navigate":
            url = request["url"]
            if not isinstance(url, str) or not url.startswith(("http://", "https://", "about:blank")):
                raise ValueError("navigate requires an http(s) URL")
            await page.goto(url, wait_until="domcontentloaded", timeout=30000)
            return {"url": page.url, "title": await page.title()}
        if action in ("snapshot", "tabs"):
            result = {"url": page.url, "tabs": [{"tab_id": identity, "url": tab.url, "active": identity == group.active} for identity, tab in group.live().items()]}
            if action == "snapshot":
                snapshot = await page.locator("body").aria_snapshot(timeout=20000)
                result.update(title=await page.title(), snapshot=snapshot[:50000], truncated=len(snapshot) > 50000)
            return result
        if action in ("select_tab", "close_tab"):
            identity = request["tab_id"]
            if identity not in group.pages:
                raise ValueError("tab does not belong to this browser window")
            if action == "select_tab":
                group.active = identity
                await group.pages[identity].bring_to_front()
            else:
                await group.pages[identity].close()
                group.live()
            return {"done": True, "tab_id": identity}
        if action == "new_tab":
            async with browser.expect_page() as pending:
                await page.evaluate("() => window.open('about:blank', '_blank')")
            tab = await pending.value
            group.add(tab)
            await identify(tab, group)
            return {"tab_id": group.active, "url": tab.url}
        if action in ("upload", "download"):
            return await transfer(page, request, args.workspace)
        if action in ("click", "type"):
            locator = locator_for(page, request)
            if action == "click":
                await locator.click()
            else:
                await locator.fill(request["text"])
            return {"url": page.url, "done": True}
        if action == "screenshot":
            destination = Path(request["path"]).resolve()
            if destination.parent.parent != args.shared_root.parent.resolve() or destination.suffix != ".png":
                raise ValueError("screenshot destination escapes browser state")
            await page.screenshot(path=str(destination), full_page=False)
            return {"path": str(destination), "mime_type": "image/png", "url": page.url}
        raise ValueError("unknown browser action")

    async def connection(reader, writer):
        nonlocal context_closed
        task = asyncio.current_task()
        connections.add(task)
        group = None
        try:
            while frame := await reader.readline():
                try:
                    request = json.loads(frame)
                    health_error = browser_health_error(context_closed,
                        browser.browser.is_connected() if browser.browser else True)
                    if health_error:
                        raise BrowserUnavailable(health_error)
                    if request.get("action") == "health":
                        # Cached connection flags do not prove responsiveness.
                        # This read-only RPC checks liveness without page/focus
                        # changes or disclosing any cookie values in the reply.
                        try:
                            await asyncio.wait_for(browser.cookies(), 3)
                        except Exception as error:
                            raise BrowserUnavailable("shared browser health check failed; close and reopen this browser_id (profile retained)") from error
                        response = {"healthy": True}
                    elif request.get("action") == "open":
                        async with opening, focus:
                            if group or request["browser_id"] in groups:
                                raise ValueError("browser window already open")
                            if any(existing.human for existing in groups.values()):
                                raise RuntimeError("resume the human window before opening a new window")
                            used = {existing.slot for existing in groups.values()}
                            slot = next((slot for slot in range(capacity) if slot not in used), None)
                            if slot is None:
                                raise RuntimeError("shared browser window capacity exhausted")
                            group = WindowGroup(request["browser_id"], slot)
                            groups[group.identity] = group
                            page = await browser.new_page()
                            group.add(page)
                            await identify(page, group)
                            response = {"display": display, "clip": viewer_clip(slot, columns)}
                    elif group is None:
                        raise ValueError("open a window first")
                    else:
                        response = await act(group, request)
                except Exception as error:
                    response = {"error": str(error)[:2000], "fatal": isinstance(error, BrowserUnavailable) or context_closed}
                writer.write((json.dumps(response) + "\n").encode())
                await writer.drain()
                if response.get("fatal"):
                    context_closed = True
                    stopped.set()
                    break
                if request.get("action") == "close":
                    break
        finally:
            try:
                if group:
                    await close_group(group)
            finally:
                writer.close()
                try:
                    with contextlib.suppress(Exception):
                        await asyncio.wait_for(writer.wait_closed(), 3)
                finally:
                    connections.discard(task)

    try:
        socket_path = prepare_unix_socket(args.shared_root / "backend.sock")
        number = await asyncio.wait_for(asyncio.to_thread(display_server.stdout.readline), 15)
        if not number.strip().isdigit():
            raise RuntimeError("Xvfb failed to allocate shared display")
        display = ":" + number.decode().strip()
        os.environ["DISPLAY"] = display
        executable = os.environ.get("PANTHEON_CAMOUFOX")
        if not executable or not Path(executable).is_file():
            raise RuntimeError("PANTHEON_CAMOUFOX must point to the packaged executable")
        playwright = await async_playwright().start()
        browser = await playwright.firefox.launch_persistent_context(str(args.shared_root / "profile"), executable_path=executable, headless=False, accept_downloads=True,
            # Let page layout track the real, placed window rather than emulating a
            # smaller viewport inside the Full HD desktop. Chrome uses some height.
            no_viewport=True,
            firefox_user_prefs={"browser.shell.checkDefaultBrowser": False, "browser.startup.homepage_override.mstone": "ignore",
                "browser.cache.disk.enable": True, "browser.cache.disk.capacity": 262144,
                "browser.link.open_newwindow": 3, "browser.link.open_newwindow.restriction": 0})
        x = XWindows(display)
        keeper = WindowGroup("keeper", capacity)
        keeper.add(browser.pages[0] if browser.pages else await browser.new_page())
        await identify(keeper.pages[keeper.active], keeper)
        # A restart keeps login state, but does not resurrect unowned stale tabs.
        for page in list(browser.pages):
            if page not in keeper.pages.values():
                await page.close()
        def on_page(page):
            task = asyncio.create_task(new_page(page))
            task.add_done_callback(lambda finished: finished.exception() if not finished.cancelled() else None)
        browser.on("page", on_page)
        def on_close():
            nonlocal context_closed
            context_closed = True
            stopped.set()
        browser.on("close", on_close)
        # Treat either lifecycle event as terminal and release the profile lock
        # for explicit reopen; never reconstruct windows or replay page actions.
        if browser.browser:
            browser.browser.on("disconnected", on_close)
        server = await asyncio.start_unix_server(connection, path=str(socket_path), limit=1_000_000)
        socket_path.chmod(0o600)
        emit({"ready": True, "display": display, "profile": "pantheon-shared"})
        stdin = asyncio.StreamReader()
        transport, _ = await asyncio.get_running_loop().connect_read_pipe(lambda: asyncio.StreamReaderProtocol(stdin), sys.stdin)
        read_task = asyncio.create_task(stdin.readline())
        close_task = asyncio.create_task(stopped.wait())
        try:
            await asyncio.wait([read_task, close_task], return_when=asyncio.FIRST_COMPLETED)
        finally:
            read_task.cancel()
            close_task.cancel()
            await asyncio.gather(read_task, close_task, return_exceptions=True)
            transport.close()
    except Exception as error:
        emit({"error": str(error)[:2000]})
        raise
    finally:
        stopping = True
        await close_connections(server, connections)
        if browser:
            with contextlib.suppress(Exception):
                await asyncio.wait_for(browser.close(), 10)
        if playwright:
            with contextlib.suppress(Exception):
                await asyncio.wait_for(playwright.stop(), 10)
        if x:
            x.close()
        display_server.terminate()
        with contextlib.suppress(Exception):
            await asyncio.wait_for(asyncio.to_thread(display_server.wait), 3)
        if display_server.poll() is None:
            display_server.kill()
            await asyncio.to_thread(display_server.wait)
        remove_unix_socket(socket_path)
        profile_lock.close()


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser()
    parser.add_argument("--clipboard-disconnect", type=Path)
    parser.add_argument("--profile", type=Path)
    parser.add_argument("--shared-root", type=Path)
    parser.add_argument("--workspace", type=Path)
    parser.add_argument("--backend", action="store_true")
    parser.add_argument("--capacity", type=int, default=16)
    parser.add_argument("--port", type=int)
    parser.add_argument("--port-end", type=int)
    parser.add_argument("--reserved-ports", default="")
    parser.add_argument("--web", type=Path, required=True)
    parser.add_argument("--proxy-fd", type=int)
    parser.add_argument("--token-file", type=Path)
    args = parser.parse_args()
    if args.clipboard_disconnect is not None:
        with contextlib.suppress(OSError):
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as notification:
                notification.settimeout(2)
                notification.connect(str(args.clipboard_disconnect))
                notification.sendall(b"clear\n")
        return
    if args.proxy_fd is not None:
        from websockify.websocketproxy import WebSocketProxy
        from websockify.token_plugins import TokenFile
        logging.basicConfig(level=logging.WARNING)
        WebSocketProxy(listen_fd=args.proxy_fd, web=str(args.web), token_plugin=TokenFile(str(args.token_file))).start_server()
        return
    if args.backend:
        if args.shared_root is None:
            parser.error("--shared-root is required for the shared backend")
        try:
            asyncio.run(backend(args))
        except Exception as error:
            with contextlib.suppress(BrokenPipeError):
                emit({"error": str(error)[:2000]})
            sys.exit(1)
        return
    if args.shared_root is None:
        parser.error("--shared-root is required")
    if args.profile is None or args.port is None:
        parser.error("--profile and --port are required for browser workers")
    args.profile.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        asyncio.run(run(args))
    except (KeyboardInterrupt, BrokenPipeError):
        pass
    except Exception as error:
        emit({"error": str(error)[:2000]})
        sys.exit(1)


if __name__ == "__main__":
    main()
