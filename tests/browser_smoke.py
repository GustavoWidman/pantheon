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
import subprocess
import tempfile
import threading
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
PYTHON = os.environ["PANTHEON_BROWSER_PYTHON"]
WEB = os.environ["PANTHEON_NOVNC_WEB"]
WORKER = os.environ.get("PANTHEON_BROWSER_WORKER", str(ROOT / "scripts/browser-worker.py"))
TOKEN = "0" * 64
ATTACHMENT = b"%PDF-1.7\nlocal authenticated attachment\x00\xff\n"


class Site(http.server.BaseHTTPRequestHandler):
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
        color = "#e00000" if self.path == "/red" else "#00e000"
        cookie = self.headers.get("Cookie", "")
        body = f'''<html><head><title>{self.path}</title></head><body style="background:{color}">
<label>Name <input aria-label="Name"></label><button onclick="document.cookie='session=shared;max-age=86400';localStorage.setItem('login','shared');document.getElementById('result').textContent='saved'">Save</button>
<button onclick="document.cookie='session=;max-age=0';localStorage.removeItem('login')">Logout</button>
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


def main():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_port}"
    processes = []
    viewers = []
    with tempfile.TemporaryDirectory(prefix="pantheon-browser-test-") as state:
        state = Path(state)
        shared = state / "pantheon-shared"
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
            first_tabs = call(first, "tabs")["tabs"]
            second_tabs = call(second, "tabs")["tabs"]
            call(first, "new_tab")
            assert len(call(first, "tabs")["tabs"]) == 2
            assert len(call(second, "tabs")["tabs"]) == 1
            # Browser-local tab IDs cannot be used to steer another window.
            second.stdin.write(json.dumps({"action": "select_tab", "tab_id": first_tabs[0]["tab_id"]}) + "\n")
            second.stdin.flush()
            assert "error" in json.loads(second.stdout.readline())
            call(first, "select_tab", tab_id=first_tabs[0]["tab_id"])
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
            assert not view_only(first_dir, first_info["display"])
            assert view_only(second_dir, second_info["display"])
            call(second, "navigate", url=url + "/green")
            call(second, "snapshot")
            call(first, "resume")
            assert view_only(first_dir, first_info["display"])
            assert view_only(second_dir, second_info["display"])
            first_view.close()
            second_view.close()
            call(first, "close")
            assert first.wait(timeout=15) == 0
            assert len(call(second, "tabs")["tabs"]) == len(second_tabs)
            call(second, "click", role="button", name="Logout")
            failed = state / "unauthenticated"
            second.stdin.write(json.dumps({"action":"download","url":url + "/attachment","path":str(failed)}) + "\n")
            second.stdin.flush()
            assert "403" in json.loads(second.stdout.readline())["error"]
            assert not failed.exists()
            third, _, _ = window("third")
            call(third, "navigate", url=url + "/red")
            assert "session=shared" not in call(third, "snapshot")["snapshot"]
            call(second, "click", role="button", name="Save")
            image = call(second, "screenshot")
            assert Path(image["path"]).stat().st_size > 100
            for child in (second, third):
                call(child, "close")
                assert child.wait(timeout=15) == 0
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
            print("PASS: Full HD framebuffers, real-window viewport, separated window edges, shared live cookies/storage/logout, private viewer pixels, tab ownership, scoped handoff, independent close, screenshot, authenticated file download/upload bytes and confirmed receipt, restart durability")
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
    main()
