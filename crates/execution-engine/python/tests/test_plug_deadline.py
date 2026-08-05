"""Deadline behavior of Plug RPC calls (tp_worker.Plug).

Stdlib-only (unittest + socket): run with
    python3 -m unittest discover crates/execution-engine/python/tests
No pytest, no venv — same zero-dependency contract as tp_worker itself.
"""

import importlib.util
import json
import socket
import threading
import time
import unittest
from pathlib import Path

_spec = importlib.util.spec_from_file_location(
    "tp_worker", Path(__file__).resolve().parent.parent / "tp_worker.py"
)
tp_worker = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tp_worker)


class _PlugServer:
    """Minimal NDJSON plug service double, one connection at a time."""

    def __init__(self, handler):
        self._handler = handler
        self._server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._server.bind(("127.0.0.1", 0))
        self._server.listen(1)
        self.address = "127.0.0.1:%d" % self._server.getsockname()[1]
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    def _serve(self):
        try:
            conn, _ = self._server.accept()
        except OSError:
            return
        with conn:
            self._handler(conn)

    def close(self):
        self._server.close()


class RemainingTest(unittest.TestCase):
    def test_no_deadline_is_unlimited(self):
        plug = tp_worker.Plug("dmm", "127.0.0.1:1")
        self.assertIsNone(plug._remaining("measure"))

    def test_future_deadline_returns_positive_remaining(self):
        plug = tp_worker.Plug("dmm", "127.0.0.1:1")
        plug.deadline = time.time() + 10
        remaining = plug._remaining("measure")
        self.assertGreater(remaining, 9)
        self.assertLessEqual(remaining, 10)

    def test_expired_deadline_raises_before_any_io(self):
        plug = tp_worker.Plug("dmm", "127.0.0.1:1")
        plug.deadline = time.time() - 1
        with self.assertRaisesRegex(Exception, "phase timed out"):
            # No server behind the address: proves expiry is checked
            # before the connection attempt.
            plug.measure()


class CallDeadlineTest(unittest.TestCase):
    def test_call_succeeds_within_deadline(self):
        def reply(conn):
            conn.makefile("rb").readline()
            conn.sendall(
                (json.dumps({"success": True, "result_json": "42"}) + "\n").encode()
            )

        server = _PlugServer(reply)
        self.addCleanup(server.close)
        plug = tp_worker.Plug("dmm", server.address)
        plug.deadline = time.time() + 5
        self.assertEqual(plug.measure(), 42)

    def test_silent_server_times_out_at_phase_deadline(self):
        release = threading.Event()

        def never_reply(conn):
            conn.makefile("rb").readline()
            release.wait(5)

        server = _PlugServer(never_reply)
        self.addCleanup(server.close)
        self.addCleanup(release.set)
        plug = tp_worker.Plug("dmm", server.address)
        plug.deadline = time.time() + 0.4

        start = time.time()
        with self.assertRaisesRegex(Exception, "phase timed out"):
            plug.measure()
        # Bounded by the phase deadline, not the old hard-coded 60s.
        self.assertLess(time.time() - start, 2)

    def test_slow_drip_response_cannot_outlive_deadline(self):
        release = threading.Event()

        def drip(conn):
            conn.makefile("rb").readline()
            # Steady traffic without a newline: each recv() succeeds, so
            # only per-chunk deadline re-derivation can stop the call.
            while not release.wait(0.1):
                try:
                    conn.sendall(b"x")
                except OSError:
                    return

        server = _PlugServer(drip)
        self.addCleanup(server.close)
        self.addCleanup(release.set)
        plug = tp_worker.Plug("dmm", server.address)
        plug.deadline = time.time() + 0.5

        start = time.time()
        with self.assertRaisesRegex(Exception, "phase timed out"):
            plug.measure()
        self.assertLess(time.time() - start, 2)

    def test_no_deadline_waits_past_former_60s_equivalent(self):
        # Scaled-down regression check for the customer bug: a reply
        # slower than the connect timeout would have died under the old
        # single 60s settimeout. With no deadline, I/O must have no
        # timeout of its own, so the call outwaits it.
        original = tp_worker.PLUG_CONNECT_TIMEOUT_S
        tp_worker.PLUG_CONNECT_TIMEOUT_S = 0.2
        self.addCleanup(setattr, tp_worker, "PLUG_CONNECT_TIMEOUT_S", original)

        def slow_reply(conn):
            conn.makefile("rb").readline()
            time.sleep(0.6)  # 3x the connect timeout
            conn.sendall(
                (json.dumps({"success": True, "result_json": "1"}) + "\n").encode()
            )

        server = _PlugServer(slow_reply)
        self.addCleanup(server.close)
        plug = tp_worker.Plug("dmm", server.address)
        self.assertEqual(plug.measure(), 1)


class SocketHygieneTest(unittest.TestCase):
    def test_socket_closed_when_connect_fails(self):
        created = []
        real_socket = tp_worker.socket.socket

        def tracking_socket(*args, **kwargs):
            sock = real_socket(*args, **kwargs)
            created.append(sock)
            return sock

        tp_worker.socket.socket = tracking_socket
        self.addCleanup(setattr, tp_worker.socket, "socket", real_socket)

        # Grab a port with no listener behind it.
        probe = real_socket(socket.AF_INET, socket.SOCK_STREAM)
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
        probe.close()

        plug = tp_worker.Plug("dmm", "127.0.0.1:%d" % port)
        with self.assertRaises(Exception):
            plug.measure()

        self.assertEqual(len(created), 1)
        self.assertEqual(created[0].fileno(), -1, "socket leaked after connect failure")


if __name__ == "__main__":
    unittest.main()
