"""Dependency-free tests for terminal browser health; never launch a live profile."""
import asyncio
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("browser_worker", Path(__file__).resolve().parents[1] / "scripts/browser-worker.py")
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class BrowserHealthTests(unittest.TestCase):
    def test_closed_context_or_disconnected_process_is_actionable(self):
        for closed, connected in [(True, True), (False, False), (True, False)]:
            self.assertIn("close and reopen", worker.browser_health_error(closed, connected))
        self.assertIsNone(worker.browser_health_error(False, True))

    def test_closing_only_a_tab_is_not_a_context_crash(self):
        class Page:
            def __init__(self, closed):
                self.closed = closed
            def is_closed(self):
                return self.closed
        group = worker.WindowGroup("isolated", 0)
        group.add(Page(True))
        group.add(Page(False))
        self.assertEqual(len(group.live()), 1)
        self.assertIsNone(worker.browser_health_error(False, True))


class BrowserTeardownTests(unittest.IsolatedAsyncioTestCase):
    async def test_deadline_includes_stalled_client_cancellation_cleanup(self):
        started, cleanup, release = asyncio.Event(), asyncio.Event(), asyncio.Event()

        async def connection():
            started.set()
            try:
                await asyncio.Future()
            finally:
                cleanup.set()
                await release.wait()

        task = asyncio.create_task(connection())
        await started.wait()
        try:
            # The former gather waited for release forever, before reaching
            # the server timeout. A hung cleanup must not stop browser teardown.
            with self.assertLogs(level="WARNING"):
                await asyncio.wait_for(worker.close_connections(None, {task}, timeout=0.05), 0.5)
            self.assertTrue(cleanup.is_set())
        finally:
            release.set()
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    async def test_stalled_server_closure_is_aborted_and_returns_at_deadline(self):
        class Server:
            closed, aborted = False, False
            def close(self):
                self.closed = True
            def abort_clients(self):
                self.aborted = True
            async def wait_closed(self):
                await asyncio.Future()

        server = Server()
        with self.assertLogs(level="WARNING"):
            await asyncio.wait_for(worker.close_connections(server, set(), timeout=0.05), 0.5)
        self.assertTrue(server.closed)
        self.assertTrue(server.aborted)

    async def test_server_closes_clients_before_waiting_for_server_closure(self):
        connections = set()
        accepted = asyncio.Event()

        async def connection(reader, writer):
            task = asyncio.current_task()
            connections.add(task)
            accepted.set()
            try:
                await reader.read()
            finally:
                writer.close()
                await writer.wait_closed()
                connections.discard(task)

        server = await asyncio.start_server(connection, "127.0.0.1", 0)
        reader, writer = await asyncio.open_connection(*server.sockets[0].getsockname())
        try:
            await accepted.wait()
            await asyncio.wait_for(worker.close_connections(server, connections), 1)
            self.assertEqual(await reader.read(), b"")
            self.assertFalse(connections)
        finally:
            writer.close()
            await writer.wait_closed()
            await worker.close_connections(server, connections)


if __name__ == "__main__":
    unittest.main()
