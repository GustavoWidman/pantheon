"""Real Unix-socket tests; no browsers, credentials or live state are used."""
import asyncio
import importlib.util
import os
from pathlib import Path
import socket
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("browser_worker", Path(__file__).resolve().parents[1] / "scripts/browser-worker.py")
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class SocketPathTests(unittest.TestCase):
    def test_paths_are_short_isolated_canonical_and_independent_of_tmpdir(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            source = root / ("é" * 50) / ("nested-" * 20)
            sources = [source / "backend.sock", source / "window-a" / "clipboard-disconnect.sock", source / "window-b" / "clipboard-disconnect.sock"]
            with mock.patch.dict(os.environ, {"TMPDIR": str(source)}):
                paths = [worker.unix_socket_path(path) for path in sources]
            self.assertEqual(len(set(paths)), 3)
            self.assertTrue(all(len(os.fsencode(path)) <= 107 for path in paths))
            self.assertTrue(all(path.parent.parent == Path("/tmp") for path in paths))
            self.assertEqual(worker.unix_socket_path(source / "backend.sock"), worker.unix_socket_path(source / "extra" / ".." / "backend.sock"))
            alias = root / "alias"
            alias.symlink_to(root, target_is_directory=True)
            self.assertEqual(worker.unix_socket_path(root / "backend.sock"), worker.unix_socket_path(alias / "backend.sock"))
            with mock.patch.object(worker.os, "getuid", return_value=os.getuid() + 1):
                self.assertNotEqual(worker.unix_socket_path(sources[0]), paths[0])

    def test_multibyte_limit_is_bytes_not_characters(self):
        source = Path("/tmp") / ("é" * 50) / "rpc.sock"
        self.assertLess(len(str(source)), 107)
        self.assertGreater(len(os.fsencode(source)), 107)
        with socket.socket(socket.AF_UNIX) as server:
            with self.assertRaisesRegex(OSError, "AF_UNIX path too long"):
                server.bind(str(source))
        self.assertLessEqual(len(os.fsencode(worker.unix_socket_path(source))), 107)

    def test_stale_socket_is_removed_inside_private_directory(self):
        with tempfile.TemporaryDirectory() as root:
            source = Path(root) / "backend.sock"
            path = worker.prepare_unix_socket(source)
            try:
                self.assertEqual(stat.S_IMODE(path.parent.stat().st_mode), 0o700)
                with socket.socket(socket.AF_UNIX) as server:
                    server.bind(str(path))
                self.assertTrue(path.exists())
                self.assertEqual(worker.prepare_unix_socket(source), path)
                self.assertFalse(path.exists())
                with socket.socket(socket.AF_UNIX) as server:
                    server.bind(str(path))
            finally:
                worker.remove_unix_socket(path)
            self.assertFalse(path.parent.exists())

    def test_symlink_or_nonprivate_directory_is_rejected_without_touching_contents(self):
        with tempfile.TemporaryDirectory() as root:
            source = Path(root) / "backend.sock"
            path = worker.unix_socket_path(source)
            target = Path(root) / "target"
            target.mkdir(mode=0o700)
            sentinel = target / "rpc.sock"
            sentinel.write_text("untouched")
            path.parent.symlink_to(target, target_is_directory=True)
            try:
                with self.assertRaises(PermissionError):
                    worker.prepare_unix_socket(source)
                self.assertEqual(sentinel.read_text(), "untouched")
            finally:
                path.parent.unlink()
            path.parent.mkdir(mode=0o700)
            path.parent.chmod(0o755)
            path.write_text("untouched")
            try:
                with self.assertRaises(PermissionError):
                    worker.prepare_unix_socket(source)
                self.assertEqual(path.read_text(), "untouched")
            finally:
                path.unlink()
                path.parent.rmdir()

    def test_cleanup_does_not_delete_unexpected_contents_or_interrupt_teardown(self):
        with tempfile.TemporaryDirectory() as root:
            path = worker.prepare_unix_socket(Path(root) / "backend.sock")
            unexpected = path.parent / "unexpected"
            unexpected.write_text("untouched")
            try:
                with self.assertLogs(level="WARNING"):
                    worker.remove_unix_socket(path)
                self.assertEqual(unexpected.read_text(), "untouched")
            finally:
                unexpected.unlink()
                worker.remove_unix_socket(path)
            worker.remove_unix_socket(path)  # Already removed is harmless.
            worker.remove_unix_socket(None)  # Failed preparation is harmless.

    def test_other_uid_directory_is_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            source = Path(root) / "backend.sock"
            path = worker.unix_socket_path(source)
            path.parent.mkdir(mode=0o700)
            real_lstat = Path.lstat
            def wrong_owner(candidate):
                info = real_lstat(candidate)
                if candidate == path.parent:
                    fields = list(info)
                    fields[4] = os.getuid() + 1
                    return os.stat_result(fields)
                return info
            try:
                with mock.patch.object(Path, "lstat", wrong_owner):
                    with self.assertRaises(PermissionError):
                        worker.prepare_unix_socket(source)
            finally:
                path.parent.rmdir()


class SocketConnectionTests(unittest.IsolatedAsyncioTestCase):
    async def test_long_backend_and_disconnect_paths_bind_connect_and_cleanup(self):
        with tempfile.TemporaryDirectory() as root:
            source = Path(root) / ("nested-" * 20) / ("é" * 50)
            for suffix in [Path("pantheon-shared/backend.sock"), Path("00000000-0000-0000-0000-000000000000/clipboard-disconnect.sock")]:
                original = source / suffix
                self.assertGreater(len(os.fsencode(original)), 107)
                with socket.socket(socket.AF_UNIX) as server:
                    with self.assertRaisesRegex(OSError, "AF_UNIX path too long"):
                        server.bind(str(original))
                path = worker.prepare_unix_socket(original)
                completed = asyncio.Event()
                messages = []
                async def echo(reader, writer):
                    try:
                        messages.append(await reader.readline())
                        if suffix.name == "backend.sock":
                            writer.write(b"ok\n")
                            await writer.drain()
                    finally:
                        writer.close()
                        await writer.wait_closed()
                        completed.set()
                server = await asyncio.start_unix_server(echo, path=str(path))
                path.chmod(0o600)
                try:
                    self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
                    if suffix.name == "backend.sock":
                        reader, writer = await asyncio.open_unix_connection(str(worker.unix_socket_path(original)))
                        writer.write(b"health\n")
                        await writer.drain()
                        self.assertEqual(await asyncio.wait_for(reader.readline(), 2), b"ok\n")
                        writer.close()
                        await writer.wait_closed()
                        expected = b"health\n"
                    else:
                        # The exact hook argv x11vnc receives still works, even
                        # though its socket has moved outside the state root.
                        hook = await asyncio.create_subprocess_exec(sys.executable, worker.__file__, "--clipboard-disconnect", str(path), "--web", root, stderr=subprocess.PIPE)
                        _, error = await asyncio.wait_for(hook.communicate(), 3)
                        self.assertEqual(hook.returncode, 0, error)
                        expected = b"clear\n"
                    await asyncio.wait_for(completed.wait(), 2)
                    self.assertEqual(messages, [expected])
                finally:
                    server.close()
                    await server.wait_closed()
                    worker.remove_unix_socket(path)
                self.assertFalse(path.exists())
                self.assertFalse(path.parent.exists())


if __name__ == "__main__":
    unittest.main()
