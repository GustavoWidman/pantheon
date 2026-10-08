#!/usr/bin/env python3
"""Live test against the bundled browser; no external network or credentials."""
import contextlib
import http.server
import json
import os
import re
from pathlib import Path
import socket
import struct
import sys
import subprocess
import tempfile
import threading
import time
import urllib.request
import urllib.parse
import ctypes
import ctypes.util

ROOT = Path(__file__).resolve().parents[1]
PYTHON = os.environ["PANTHEON_BROWSER_PYTHON"]
WEB = os.environ["PANTHEON_NOVNC_WEB"]
WORKER = os.environ.get("PANTHEON_BROWSER_WORKER", str(ROOT / "scripts/browser-worker.py"))
TOKEN = "0" * 64
ATTACHMENT = b"%PDF-1.7\nlocal authenticated attachment\x00\xff\n"


class Site(http.server.BaseHTTPRequestHandler):
    entered = ""

    uploaded = None

    def do_POST(self):
        if self.path != "/uploaded" or "session=shared" not in self.headers.get("Cookie", ""):
            self.send_error(403)
            return
        Site.uploaded = self.rfile.read(int(self.headers["Content-Length"]))
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"verified receipt")

    def do_GET(self):
        if self.path.startswith("/attachment"):
            if "session=shared" not in self.headers.get("Cookie", ""):
                self.send_error(403)
                return
            self.send_response(200)
            self.send_header("Content-Type", "application/pdf")
            self.send_header("Content-Disposition", 'attachment; filename="../../untrusted.pdf"')
            self.send_header("Content-Length", str(len(ATTACHMENT)))
            self.end_headers()
            self.wfile.write(ATTACHMENT)
            return
        if self.path.startswith("/entered?"):
            Site.entered = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query, keep_blank_values=True)["value"][0]
            self.send_response(204)
            self.end_headers()
            return
        color = "#e00000" if self.path == "/red" else "#00e000"
        cookie = self.headers.get("Cookie", "")
        body = f'''<html><head><title>{self.path}</title></head><body style="background:{color}">
<label>Name <input aria-label="Name"></label><button onclick="document.cookie='session=shared;max-age=86400';localStorage.setItem('login','shared');document.getElementById('result').textContent='saved'">Save</button>
<button onclick="document.cookie='session=;max-age=0';localStorage.removeItem('login')">Logout</button>
<label>Dummy password <input type="password" aria-label="Dummy password" oninput="fetch('/entered?value='+encodeURIComponent(this.value))"></label>
<label>Dummy copy <input aria-label="Dummy copy" value="browser-local-copy"></label>
<a href="/popup" target="_blank">Popup</a><a href="/attachment">Download attachment</a>
<input type="file" id="attachment" style="display:none" multiple>
<button onclick="document.getElementById('confirmation').hidden=false">Submit request</button>
<div id="confirmation" hidden><button onclick="fetch('/uploaded',{{method:'POST',body:document.getElementById('attachment').files[0]}}).then(r=>r.text()).then(t=>document.getElementById('receipt').textContent=t)">Confirm request</button></div><p id="receipt"></p><p id="result">{cookie}</p><p id="geometry"></p>
<script>document.getElementById('geometry').textContent = 'viewport=' + innerWidth + 'x' + innerHeight;document.getElementById('result').textContent += ' storage=' + localStorage.getItem('login');</script></body></html>'''.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def read_exact(connection, size):
    output = bytearray()
    while len(output) < size:
        chunk = connection.recv(size - len(output))
        if not chunk:
            raise AssertionError("RFB connection closed")
        output.extend(chunk)
    return bytes(output)


class Viewer:
    def __init__(self, directory):
        port = int((directory / "viewer.tokens").read_text().strip().rsplit(":", 1)[1])
        self.connection = socket.create_connection(("127.0.0.1", port), timeout=10)
        assert read_exact(self.connection, 12).startswith(b"RFB ")
        self.connection.sendall(b"RFB 003.008\n")
        count = read_exact(self.connection, 1)[0]
        assert 1 in read_exact(self.connection, count)
        self.connection.sendall(b"\1")
        assert read_exact(self.connection, 4) == b"\0" * 4
        self.connection.sendall(b"\1")
        header = read_exact(self.connection, 24)
        self.width, self.height = struct.unpack(">HH", header[:4])
        read_exact(self.connection, struct.unpack(">I", header[20:])[0])
        assert (self.width, self.height) == (1920, 1080)
        self.connection.sendall(struct.pack(">BBBBBBBBHHHBBBxxx", 0, 0, 0, 0, 32, 24, 0, 1, 255, 255, 255, 16, 8, 0))
        self.connection.sendall(struct.pack(">BBHi", 2, 0, 1, 0))

    def clipboard(self, text):
        data = text.encode("latin-1")
        self.connection.sendall(struct.pack(">BxxxI", 6, len(data)) + data)

    def key(self, key, down):
        self.connection.sendall(struct.pack(">BBxxI", 4, int(down), key))

    def click(self, x, y):
        self.connection.sendall(struct.pack(">BBHH", 5, 0, x, y))
        self.connection.sendall(struct.pack(">BBHH", 5, 1, x, y))
        self.connection.sendall(struct.pack(">BBHH", 5, 0, x, y))

    def chord(self, key):
        self.key(0xffe3, True)  # Linux Ctrl, including when the client is a Mac.
        self.key(ord(key), True)
        self.key(ord(key), False)
        self.key(0xffe3, False)

    def no_clipboard_output(self):
        # No framebuffer request is pending on these fresh viewer connections.
        self.connection.settimeout(0.3)
        try:
            data = self.connection.recv(1)
            raise AssertionError(f"unexpected server message or disconnect: {data!r}")
        except socket.timeout:
            pass
        finally:
            self.connection.settimeout(10)

    def pixel(self, x=200, y=200):
        self.connection.sendall(struct.pack(">BBHHHH", 3, 0, x, y, 1, 1))
        while True:
            kind = read_exact(self.connection, 1)[0]
            if kind == 2:  # Bell.
                continue
            if kind == 3:
                length = struct.unpack(">I", read_exact(self.connection, 7)[3:])[0]
                read_exact(self.connection, length)
                continue
            assert kind == 0, kind
            count = struct.unpack(">H", read_exact(self.connection, 3)[1:])[0]
            result = None
            for _ in range(count):
                rx, ry, width, height, encoding = struct.unpack(">HHHHi", read_exact(self.connection, 12))
                assert encoding == 0, encoding
                pixels = read_exact(self.connection, width * height * 4)
                if rx <= x < rx + width and ry <= y < ry + height:
                    offset = ((y - ry) * width + x - rx) * 4
                    blue, green, red = pixels[offset:offset + 3]
                    result = red, green, blue
            if result is not None:
                return result

    def close(self):
        self.connection.close()


def websocket(port, token):
    with socket.create_connection(("127.0.0.1", port), timeout=10) as connection:
        path = "/websockify" + ("?token=" + token if token else "")
        connection.sendall((f"GET {path} HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        return connection.recv(8192)


def assert_clipboard_empty(display):
    library = ctypes.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")
    library.XOpenDisplay.argtypes = [ctypes.c_char_p]
    library.XOpenDisplay.restype = ctypes.c_void_p
    library.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
    library.XInternAtom.restype = ctypes.c_ulong
    library.XGetSelectionOwner.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    library.XGetSelectionOwner.restype = ctypes.c_ulong
    library.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    library.XDefaultRootWindow.restype = ctypes.c_ulong
    library.XGetWindowProperty.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_long, ctypes.c_long, ctypes.c_int, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_void_p)]
    library.XGetWindowProperty.restype = ctypes.c_int
    library.XFree.argtypes = [ctypes.c_void_p]
    library.XCloseDisplay.argtypes = [ctypes.c_void_p]
    connection = library.XOpenDisplay(display.encode())
    assert connection
    try:
        for name in (b"CLIPBOARD", b"PRIMARY", b"SECONDARY"):
            atom = library.XInternAtom(connection, name, 0)
            assert library.XGetSelectionOwner(connection, atom) == 0, name
        for index in range(8):
            atom = library.XInternAtom(connection, f"CUT_BUFFER{index}".encode(), 0)
            actual_type, format_, count, remaining, data = ctypes.c_ulong(), ctypes.c_int(), ctypes.c_ulong(), ctypes.c_ulong(), ctypes.c_void_p()
            assert library.XGetWindowProperty(connection, library.XDefaultRootWindow(connection), atom, 0, 1, 0, 0, ctypes.byref(actual_type), ctypes.byref(format_), ctypes.byref(count), ctypes.byref(remaining), ctypes.byref(data)) == 0
            try:
                assert actual_type.value == 0, (index, actual_type.value)
            finally:
                if data:
                    library.XFree(data)
    finally:
        library.XCloseDisplay(connection)

def main():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_port}"
    processes = []
    viewers = []
    with tempfile.TemporaryDirectory(prefix="pantheon-browser-test-") as state:
        # Exercise both backend and clipboard sockets beyond sockaddr_un's
        # byte limit, including a multibyte state-root component.
        state = Path(state) / ("long-state-" + "é" * 48) / ("nested-" + "x" * 48)
        state.mkdir(parents=True)
        shared = state / "pantheon-shared"
        assert len(os.fsencode(shared / "backend.sock")) > 107
        log = (state / "worker.log").open("w+")

        def process(arguments):
            child = subprocess.Popen([PYTHON, WORKER, *arguments, "--web", WEB], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True, start_new_session=True, env=dict(os.environ, PANTHEON_BROWSER_TOKEN=TOKEN))
            processes.append(child)
            ready = json.loads(child.stdout.readline())
            assert ready.get("ready"), ready
            return child, ready

        def call(child, action, **arguments):
            child.stdin.write(json.dumps(dict(action=action, **arguments)) + "\n")
            child.stdin.flush()
            response = json.loads(child.stdout.readline())
            assert "error" not in response, response
            return response

        def window(identity):
            directory = state / identity
            child, ready = process(["--profile", str(directory), "--shared-root", str(shared), "--port", "16080", "--port-end", "16100"])
            return child, ready, directory

        def view_only(directory, display):
            result = subprocess.run([os.environ.get("PANTHEON_X11VNC", "x11vnc"), "-display", display, "-connect", str(directory / "viewer.control"), "-Q", "viewonly"], capture_output=True, text=True, timeout=15)
            assert result.returncode == 0, result.stderr
            return "viewonly:1" in result.stdout

        def entered(expected):
            deadline = time.monotonic() + 5
            while Site.entered != expected and time.monotonic() < deadline:
                time.sleep(0.05)
            assert Site.entered == expected, (Site.entered, expected)

        def clipboard_empty(display):
            subprocess.run([PYTHON, str(Path(__file__).resolve()), "--clipboard-empty", display], check=True, timeout=10)

        try:
            backend, _ = process(["--backend", "--workspace", str(state), "--shared-root", str(shared), "--capacity", "4"])
            first, first_info, first_dir = window("first")
            second, second_info, second_dir = window("second")
            assert first_info["display"] == second_info["display"]
            assert first_info["port"] != second_info["port"]
            for info in (first_info, second_info):
                port = info["port"]
                assert urllib.request.urlopen(f"http://127.0.0.1:{port}/vnc.html").status == 200
                assert b" 101 " not in websocket(port, "wrong")
                assert b" 101 " not in websocket(port, None)
                assert b" 101 " in websocket(port, TOKEN)
            call(first, "navigate", url=url + "/red")
            call(first, "click", role="button", name="Save")
            call(second, "navigate", url=url + "/green")
            snapshot = call(second, "snapshot")
            assert "session=shared" in snapshot["snapshot"] and "storage=shared" in snapshot["snapshot"], snapshot
            # Cookie-authenticated URL downloads keep the form's current page,
            # while event-first click downloads handle Content-Disposition.
            artifact = state / "download-url"
            downloaded = call(second, "download", url=url + "/attachment", path=str(artifact))
            assert artifact.read_bytes() == ATTACHMENT
            assert downloaded["size"] == len(ATTACHMENT)
            assert downloaded["path"] == str(artifact)
            assert downloaded["suggested_filename"] == "../../untrusted.pdf"
            assert call(second, "snapshot")["url"] == url + "/green"
            clicked = state / "download-click"
            downloaded = call(second, "download", role="link", name="Download attachment", path=str(clicked))
            assert clicked.read_bytes() == ATTACHMENT and downloaded["size"] == len(ATTACHMENT)
            call(second, "upload", selector="#attachment", paths=[str(artifact), str(clicked)])
            call(second, "click", role="button", name="Submit request")
            assert Site.uploaded is None  # A confirmation modal is not a receipt.
            call(second, "click", role="button", name="Confirm request")
            for _ in range(100):
                if "verified receipt" in call(second, "snapshot")["snapshot"]:
                    break
            else:
                raise AssertionError("upload form receipt missing")
            assert Site.uploaded == ATTACHMENT
            for request in [
                {"action":"download", "url":url + "/attachment", "path":str(artifact)},
                {"action":"upload", "selector":"#attachment", "paths":["/etc/passwd"]},
                {"action":"download", "url":url + "/attachment", "path":"/tmp/outside-pantheon-artifact"},
            ]:
                second.stdin.write(json.dumps(request) + "\n")
                second.stdin.flush()
                assert "error" in json.loads(second.stdout.readline())
            assert artifact.read_bytes() == ATTACHMENT
            first_view, second_view = Viewer(first_dir), Viewer(second_dir)
            viewers.extend([first_view, second_view])
            time.sleep(0.3)
            assert first_view.pixel() == (224, 0, 0)
            assert second_view.pixel() == (0, 224, 0)
            # Content reaches the far side of the Full HD window (not the old
            # 1280x720 emulated viewport), but stays out of each tile's margin.
            geometry = re.search(r"viewport=(\d+)x(\d+)", snapshot["snapshot"])
            assert geometry is not None, snapshot
            width, height = map(int, geometry.groups())
            # Camoufox/browser chrome may reserve or report additional pixels.
            # Verify real-window layout expands beyond the old emulated viewport.
            assert 1750 <= width <= 1888 and 850 <= height <= 1048, snapshot
            for viewer, color in ((first_view, (224, 0, 0)), (second_view, (0, 224, 0))):
                assert viewer.pixel(1850, 950) == color
                assert viewer.pixel(1918, 950) == (0, 0, 0)
                assert viewer.pixel(1850, 1078) == (0, 0, 0)
            first_view.close()
            second_view.close()
            first_tabs = call(first, "tabs")["tabs"]
            second_tabs = call(second, "tabs")["tabs"]
            temporary_tab = call(first, "new_tab")["tab_id"]
            assert len(call(first, "tabs")["tabs"]) == 2
            assert len(call(second, "tabs")["tabs"]) == 1
            # Browser-local tab IDs cannot be used to steer another window.
            second.stdin.write(json.dumps({"action": "select_tab", "tab_id": first_tabs[0]["tab_id"]}) + "\n")
            second.stdin.flush()
            assert "error" in json.loads(second.stdout.readline())
            call(first, "close_tab", tab_id=temporary_tab)
            call(first, "select_tab", tab_id=first_tabs[0]["tab_id"])
            call(first, "click", role="textbox", name="Dummy password")
            clipboard_view, other_view = Viewer(first_dir), Viewer(second_dir)
            viewers.extend([clipboard_view, other_view])
            clipboard_view.clipboard("view-only-must-not-paste")
            time.sleep(0.3)
            clipboard_empty(first_info["display"])
            call(first, "handoff")
            for request in [
                {"action":"upload","selector":"#attachment","paths":[str(artifact)]},
                {"action":"download","url":url + "/attachment","path":str(state / "leased")},
            ]:
                first.stdin.write(json.dumps(request) + "\n")
                first.stdin.flush()
                assert "error" in json.loads(first.stdout.readline())
                second.stdin.write(json.dumps(request) + "\n")
                second.stdin.flush()
                assert "error" in json.loads(second.stdout.readline())
            # Genuine ClientCutText, followed by VNC key events into Firefox.
            clipboard_view.clipboard("dummy-password-123")
            time.sleep(0.3)
            clipboard_view.chord("v")
            entered("dummy-password-123")
            other_view.clipboard("other-window-must-not-overwrite")
            time.sleep(0.3)
            clipboard_view.chord("a")
            clipboard_view.chord("v")
            entered("dummy-password-123")
            clipboard_view.no_clipboard_output()
            other_view.no_clipboard_output()
            # Password fields cannot be copied; Tab to the following normal input.
            clipboard_view.key(0xff09, True)
            clipboard_view.key(0xff09, False)
            # Browser-local copy must not export data through either VNC server.
            clipboard_view.chord("a")
            clipboard_view.chord("c")
            time.sleep(0.3)
            clipboard_view.no_clipboard_output()
            other_view.no_clipboard_output()
            clipboard_view.close()
            time.sleep(0.5)
            clipboard_empty(first_info["display"])
            clipboard_view = Viewer(first_dir)
            viewers.append(clipboard_view)
            clipboard_view.clipboard("lease-secret")
            time.sleep(0.3)
            assert not view_only(first_dir, first_info["display"])
            assert view_only(second_dir, second_info["display"])
            call(second, "navigate", url=url + "/green")
            call(second, "snapshot")
            call(first, "resume")
            clipboard_empty(first_info["display"])
            call(second, "click", role="textbox", name="Dummy password")
            Site.entered = "not-empty"
            call(second, "handoff")
            other_view.chord("v")
            time.sleep(0.4)
            assert Site.entered == "not-empty", Site.entered  # Empty field receives nothing.
            other_view.clipboard("second-lease-secret")
            time.sleep(0.3)
            other_view.chord("v")
            entered("second-lease-secret")
            call(second, "resume")
            clipboard_empty(second_info["display"])
            clipboard_view.close()
            other_view.close()
            assert view_only(first_dir, first_info["display"])
            assert view_only(second_dir, second_info["display"])
            call(first, "click", role="textbox", name="Dummy password")
            call(first, "handoff")
            close_view = Viewer(first_dir)
            viewers.append(close_view)
            close_view.clipboard("close-secret")
            time.sleep(0.3)
            call(first, "close")
            assert first.wait(timeout=15) == 0
            clipboard_empty(first_info["display"])
            assert len(call(second, "tabs")["tabs"]) == len(second_tabs)
            call(second, "click", role="button", name="Logout")
            failed = state / "unauthenticated"
            second.stdin.write(json.dumps({"action":"download","url":url + "/attachment","path":str(failed)}) + "\n")
            second.stdin.flush()
            assert "403" in json.loads(second.stdout.readline())["error"]
            assert not failed.exists()
            third, third_info, third_dir = window("third")
            call(third, "navigate", url=url + "/red")
            assert "session=shared" not in call(third, "snapshot")["snapshot"]
            call(second, "click", role="button", name="Save")
            image = call(second, "screenshot")
            assert Path(image["path"]).stat().st_size > 100
            call(third, "click", role="textbox", name="Dummy password")
            call(third, "handoff")
            disconnected_view = Viewer(third_dir)
            viewers.append(disconnected_view)
            disconnected_view.clipboard("worker-disconnect-secret")
            time.sleep(0.3)
            third.stdin.close()
            assert third.wait(timeout=15) == 0
            clipboard_empty(third_info["display"])
            call(second, "close")
            assert second.wait(timeout=15) == 0
            backend.stdin.write("shutdown\n")
            backend.stdin.flush()
            assert backend.wait(timeout=20) == 0
            backend, _ = process(["--backend", "--workspace", str(state), "--shared-root", str(shared), "--capacity", "4"])
            reopened, _, _ = window("first")
            call(reopened, "navigate", url=url + "/red")
            snapshot = call(reopened, "snapshot")["snapshot"]
            assert "session=shared" in snapshot and "storage=shared" in snapshot
            call(reopened, "close")
            assert reopened.wait(timeout=15) == 0
            backend.stdin.write("shutdown\n")
            backend.stdin.flush()
            assert backend.wait(timeout=20) == 0
            # Kill only the Firefox main process descended from this test's
            # isolated backend. No live harness services/profiles are touched.
            backend, _ = process(["--backend", "--workspace", str(state), "--shared-root", str(shared), "--capacity", "4"])
            crashed, _, _ = window("crashed")
            leased, _, _ = window("leased")
            call(leased, "handoff")
            assert call(leased, "health")["healthy"]
            parents = {}
            for entry in Path("/proc").iterdir():
                if entry.name.isdigit():
                    with contextlib.suppress(OSError, ValueError):
                        fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
                        parents[int(entry.name)] = int(fields[1])
            descendants = {backend.pid}
            while True:
                expanded = descendants | {pid for pid, parent in parents.items() if parent in descendants}
                if expanded == descendants:
                    break
                descendants = expanded
            firefox = []
            for pid in descendants - {backend.pid}:
                with contextlib.suppress(OSError):
                    argv = (Path("/proc") / str(pid) / "cmdline").read_bytes().split(b"\0")
                    if argv[0].decode() == os.environ["PANTHEON_CAMOUFOX"] and b"-juggler-pipe" in argv:
                        firefox.append(pid)
            assert len(firefox) == 1, firefox
            os.kill(firefox[0], 9)
            # Teardown must finish even while client RPC sockets remain open.
            # Waiting for server closure before closing clients deadlocks here.
            backend.wait(timeout=30)
            for child in (crashed, leased):
                child.stdin.write(json.dumps({"action": "health"}) + "\n")
                child.stdin.flush()
                failure = json.loads(child.stdout.readline())
                assert failure.get("fatal") and "close and reopen" in failure["error"], failure
                child.stdin.close()
                assert child.wait(timeout=15) == 0
            backend, _ = process(["--backend", "--workspace", str(state), "--shared-root", str(shared), "--capacity", "4"])
            recovered, _, _ = window("crashed")
            call(recovered, "navigate", url=url + "/red")
            assert "session=shared" in call(recovered, "snapshot")["snapshot"]
            call(recovered, "close")
            assert recovered.wait(timeout=15) == 0
            backend.stdin.write("shutdown\n")
            backend.stdin.flush()
            assert backend.wait(timeout=20) == 0
            print("PASS: long/multibyte state-root Unix sockets, Full HD framebuffers, real-window viewport, separated window edges, shared live cookies/storage/logout, private viewer pixels, tab ownership, scoped handoff and receive-only lease clipboard/paste/disconnect cleanup, independent close, screenshot, authenticated file download/upload bytes and confirmed receipt, restart durability, isolated Firefox death, fatal health under handoff, explicit recovery")
        except Exception:
            log.flush()
            log.seek(0)
            print(log.read()[-6000:])
            raise
        finally:
            for viewer in viewers:
                viewer.close()
            for child in processes:
                if child.poll() is None:
                    with contextlib.suppress(ProcessLookupError):
                        os.killpg(child.pid, 9)
                    child.wait(timeout=15)
            log.close()
            server.shutdown()


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--clipboard-empty":
        assert_clipboard_empty(sys.argv[2])
    else:
        main()
