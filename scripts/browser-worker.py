#!/usr/bin/env python3
"""JSON-line RPC; one persistent Camoufox, private X display and authenticated noVNC.

Only the Rust supervisor starts this process. stdout is exclusively protocol frames.
Browser downloads, pip installs and Playwright installs never happen at runtime.
"""
import argparse
import asyncio
import contextlib
import errno
import json
import logging
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import uuid


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


async def run(args):
    from playwright.async_api import async_playwright

    children = []
    browser = None
    playwright = None
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
        # Xvfb atomically allocates a free display; no shared :99 or test-then-open race.
        display_server = start([os.environ.get("PANTHEON_XVFB", "Xvfb"), "-displayfd", "1", "-screen", "0", "1440x900x24", "-nolisten", "tcp"], stdout=subprocess.PIPE)
        children.append(display_server)
        display_number = await asyncio.wait_for(asyncio.to_thread(display_server.stdout.readline), 15)
        if not display_number.strip().isdigit():
            raise RuntimeError("Xvfb did not allocate a display")
        display = ":" + display_number.decode().strip()
        os.environ["DISPLAY"] = display
        # -autoport starts at 5900 and lets the OS choose; parse x11vnc's PORT frame.
        vnc = start([os.environ.get("PANTHEON_X11VNC", "x11vnc"), "-display", display, "-localhost", "-autoport", "5900", "-forever", "-shared", "-nopw", "-viewonly", "-quiet"], stdout=subprocess.PIPE)
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
        executable = os.environ.get("PANTHEON_CAMOUFOX")
        if not executable or not Path(executable).is_file():
            raise RuntimeError("PANTHEON_CAMOUFOX must point to the packaged Camoufox executable")
        playwright = await async_playwright().start()
        browser = await playwright.firefox.launch_persistent_context(
            str(args.profile / "profile"), executable_path=executable, headless=False,
            viewport={"width": 1400, "height": 820},
            firefox_user_prefs={"browser.shell.checkDefaultBrowser": False,
                "browser.startup.homepage_override.mstone": "ignore",
                "browser.cache.disk.enable": True,
                "browser.cache.disk.capacity": 262144},
        )
        page = browser.pages[0] if browser.pages else await browser.new_page()
        page.set_default_timeout(20000)
        human = False
        emit({"ready": True, "display": display, "port": args.port})
        while True:
            line = await asyncio.to_thread(sys.stdin.readline)
            if not line:
                break
            try:
                request = json.loads(line)
                action = request.get("action")
                if any(child.poll() is not None for child in children):
                    raise RuntimeError("a browser desktop component exited; close and reopen this browser")
                if action == "close":
                    await browser.close()
                    browser = None
                    emit({"closed": True})
                    break
                if action in ("handoff", "resume"):
                    if action == "handoff":
                        human = True  # A partially applied VNC command fails closed.
                    control = "noviewonly" if action == "handoff" else "viewonly"
                    result = await asyncio.create_subprocess_exec(os.environ.get("PANTHEON_X11VNC", "x11vnc"), "-display", display, "-sync", "-R", control, stdout=sys.stderr, stderr=sys.stderr)
                    if await result.wait() != 0:
                        raise RuntimeError("failed to change browser viewer ownership")
                    human = action == "handoff"
                    emit({"state": "human" if human else "agent"})
                    continue
                if human:
                    raise RuntimeError("automation paused while human owns browser")
                # Human interaction may close the active tab or open another.
                if page.is_closed():
                    page = browser.pages[-1] if browser.pages else await browser.new_page()
                if action == "navigate":
                    url = request["url"]
                    if not isinstance(url, str) or not url.startswith(("http://", "https://", "about:blank")):
                        raise ValueError("navigate requires an http(s) URL")
                    await page.goto(url, wait_until="domcontentloaded", timeout=30000)
                    emit({"url": page.url, "title": await page.title()})
                elif action == "snapshot":
                    # Semantic snapshot supplies stable role/name locators and bounded text.
                    snapshot = await page.locator("body").aria_snapshot(timeout=20000)
                    emit({"url": page.url, "title": await page.title(), "snapshot": snapshot[:50000], "truncated": len(snapshot) > 50000,
                          "tabs": [{"index": i, "url": p.url} for i, p in enumerate(browser.pages)]})
                elif action in ("click", "type"):
                    if request.get("role"):
                        locator = page.get_by_role(request["role"], name=request.get("name", ""), exact=True)
                    elif request.get("selector"):
                        locator = page.locator(request["selector"])
                    else:
                        raise ValueError("provide role and name from snapshot, or a CSS selector")
                    if action == "click":
                        await locator.click()
                    else:
                        await locator.fill(request["text"])
                    emit({"url": page.url, "done": True})
                elif action == "screenshot":
                    destination = args.profile / ("screenshot-" + uuid.uuid4().hex + ".png")
                    await page.screenshot(path=str(destination), full_page=False)
                    emit({"path": str(destination), "mime_type": "image/png", "url": page.url})
                else:
                    raise ValueError("unknown browser action")
            except Exception as error:
                emit({"error": str(error)[:2000]})
    finally:
        listener.close()
        if browser:
            with contextlib.suppress(Exception):
                await asyncio.wait_for(browser.close(), 5)
        if playwright:
            with contextlib.suppress(Exception):
                await playwright.stop()
        for child in reversed(children):
            with contextlib.suppress(ProcessLookupError):
                child.terminate()
        for child in reversed(children):
            try:
                await asyncio.wait_for(asyncio.to_thread(child.wait), 3)
            except asyncio.TimeoutError:
                child.kill()
                await asyncio.to_thread(child.wait)
        token_file.unlink(missing_ok=True)


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", type=Path)
    parser.add_argument("--port", type=int)
    parser.add_argument("--port-end", type=int)
    parser.add_argument("--reserved-ports", default="")
    parser.add_argument("--web", type=Path, required=True)
    parser.add_argument("--proxy-fd", type=int)
    parser.add_argument("--token-file", type=Path)
    args = parser.parse_args()
    if args.proxy_fd is not None:
        from websockify.websocketproxy import WebSocketProxy
        from websockify.token_plugins import TokenFile
        logging.basicConfig(level=logging.WARNING)
        WebSocketProxy(listen_fd=args.proxy_fd, web=str(args.web), token_plugin=TokenFile(str(args.token_file))).start_server()
        return
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
