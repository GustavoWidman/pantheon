#!/usr/bin/env python3
"""Live test against the bundled browser; no external network or credentials."""
import contextlib
import http.server
import json
import os
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
TOKEN = "0" * 64


class Site(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        color = "#e00000" if self.path == "/red" else "#00e000"
        cookie = self.headers.get("Cookie", "")
        body = f'''<html><head><title>{self.path}</title></head><body style="background:{color}">
<label>Name <input aria-label="Name"></label><button onclick="document.cookie='session=shared;max-age=86400';localStorage.setItem('login','shared');document.getElementById('result').textContent='saved'">Save</button>
<button onclick="document.cookie='session=;max-age=0';localStorage.removeItem('login')">Logout</button>
<a href="/popup" target="_blank">Popup</a><p id="result">{cookie}</p>
<script>document.getElementById('result').textContent += ' storage=' + localStorage.getItem('login');</script></body></html>'''.encode()
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
        assert (self.width, self.height) == (1440, 900)
        self.connection.sendall(struct.pack(">BBBBBBBBHHHBBBxxx", 0, 0, 0, 0, 32, 24, 0, 1, 255, 255, 255, 16, 8, 0))
        self.connection.sendall(struct.pack(">BBHi", 2, 0, 1, 0))

    def pixel(self, x=200, y=200):
        self.connection.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, self.width, self.height))
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
            child = subprocess.Popen([PYTHON, str(ROOT / "scripts/browser-worker.py"), *arguments, "--web", WEB], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True, start_new_session=True, env=dict(os.environ, PANTHEON_BROWSER_TOKEN=TOKEN))
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
            backend, _ = process(["--backend", "--shared-root", str(shared), "--capacity", "4"])
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
            first_view, second_view = Viewer(first_dir), Viewer(second_dir)
            viewers.extend([first_view, second_view])
            time.sleep(0.3)
            assert first_view.pixel() == (224, 0, 0)
            assert second_view.pixel() == (0, 224, 0)
            first_view.close()
            second_view.close()
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
            assert not view_only(first_dir, first_info["display"])
            assert view_only(second_dir, second_info["display"])
            call(second, "navigate", url=url + "/green")
            call(second, "snapshot")
            call(first, "resume")
            assert view_only(first_dir, first_info["display"])
            assert view_only(second_dir, second_info["display"])
            call(first, "close")
            assert first.wait(timeout=15) == 0
            assert len(call(second, "tabs")["tabs"]) == len(second_tabs)
            call(second, "click", role="button", name="Logout")
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
            backend, _ = process(["--backend", "--shared-root", str(shared), "--capacity", "4"])
            reopened, _, _ = window("first")
            call(reopened, "navigate", url=url + "/red")
            snapshot = call(reopened, "snapshot")["snapshot"]
            assert "session=shared" in snapshot and "storage=shared" in snapshot
            call(reopened, "close")
            assert reopened.wait(timeout=15) == 0
            backend.stdin.write("shutdown\n")
            backend.stdin.flush()
            assert backend.wait(timeout=20) == 0
            print("PASS: shared live cookies/storage/logout, private viewer pixels, tab ownership, scoped handoff, independent close, screenshot, restart durability")
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
