"""Concurrency behaviour of the plug service (tp_plug.py) under a burst of
callers, plus the worker-side connect retry (tp_worker.Plug._connect).

Stdlib-only (unittest + socket + subprocess): run with
    python3 -m unittest discover crates/execution-engine/python/tests

The service is started as a real subprocess with a throwaway plug class,
exactly as the engine launches it, so the accept loop, the call lock and
the shutdown path are the production ones.

Which tests fail on the pre-fix service depends on the kernel: macOS and
Windows reset connections that overflow the backlog, Linux drops the SYN
and retransmits, so the 100-caller burst passes (slowly) there. The
control-request, shutdown and abandoned-caller tests fail everywhere.
"""

import importlib.util
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

PYTHON_DIR = Path(__file__).resolve().parent.parent
# TP_PLUG_PATH lets the suite run against another copy of the service
# (e.g. the pre-fix one) to prove the tests catch the regression.
TP_PLUG = Path(os.environ.get("TP_PLUG_PATH", PYTHON_DIR / "tp_plug.py"))

_spec = importlib.util.spec_from_file_location("tp_worker", PYTHON_DIR / "tp_worker.py")
tp_worker = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tp_worker)


PLUG_SOURCE = '''
import threading, time

class Counter:
    def __init__(self):
        self._lock = threading.Lock()
        self.calls = 0
        self.in_flight = 0
        self.max_in_flight = 0

    def work(self, seconds):
        with self._lock:
            self.in_flight += 1
            self.max_in_flight = max(self.max_in_flight, self.in_flight)
        time.sleep(seconds)
        with self._lock:
            self.in_flight -= 1
            self.calls += 1
        return self.calls

    def stats(self):
        return {"calls": self.calls, "max_in_flight": self.max_in_flight}
'''


def _request(address, payload, timeout=10.0):
    host, port = address.split(":")
    with socket.create_connection((host, int(port)), timeout=timeout) as sock:
        sock.sendall((json.dumps(payload) + "\n").encode())
        buf = b""
        while b"\n" not in buf:
            chunk = sock.recv(65536)
            if not chunk:
                break
            buf += chunk
    return json.loads(buf.split(b"\n", 1)[0].decode())


def _call(address, method, *args, timeout=10.0):
    return _request(
        address,
        {"type": "CallMethod", "method": method, "args_json": json.dumps(list(args))},
        timeout=timeout,
    )


def _call_in_background(address, method, *args):
    """Fire a call from a daemon thread; the service may exit before it
    answers (Shutdown tests), which is expected and not a failure."""

    def run():
        try:
            _call(address, method, *args)
        except Exception:  # noqa: BLE001
            pass

    threading.Thread(target=run, daemon=True).start()


class _PlugService:
    """A real tp_plug.py subprocess serving the Counter plug."""

    def __init__(self):
        self.dir = Path(tempfile.mkdtemp(prefix="tp-plug-fanout-"))
        plug_file = self.dir / "counter_plug.py"
        plug_file.write_text(PLUG_SOURCE)
        config = json.dumps({"file": str(plug_file), "class": "Counter", "config": {}})
        self.proc = subprocess.Popen(
            [
                sys.executable,
                str(TP_PLUG),
                "--procedure-dir",
                str(self.dir),
                "--plug-name",
                "counter",
                "--display-name",
                "Counter",
                "--plug-config",
                config,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        try:
            line = self.proc.stdout.readline().strip()
            assert line.startswith("NDJSON_PORT:"), f"unexpected handshake: {line!r}"
            self.address = "127.0.0.1:" + line.split(":", 1)[1]
            # Wait for background init so the first call does not pay the join.
            deadline = time.time() + 10
            while time.time() < deadline:
                if _request(self.address, {"type": "GetStatus"}).get("success"):
                    return
                time.sleep(0.05)
            raise AssertionError("plug never initialized")
        except BaseException:
            self.close()
            raise

    def exited_within(self, seconds):
        try:
            self.proc.wait(timeout=seconds)
            return True
        except subprocess.TimeoutExpired:
            return False

    def close(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        self.proc.stdout.close()
        shutil.rmtree(self.dir, ignore_errors=True)


class FanOutTest(unittest.TestCase):
    def setUp(self):
        self.svc = _PlugService()
        self.addCleanup(self.svc.close)

    def _wait_until_lock_held(self, timeout=5.0):
        """GetStatus reports an empty state while a method holds the lock."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if _request(self.svc.address, {"type": "GetStatus"}).get("state") == {}:
                return
            time.sleep(0.02)
        raise AssertionError("no method took the call lock in time")

    def test_100_simultaneous_callers_all_succeed_and_calls_serialize(self):
        n, work = 100, 0.02
        results = {}
        barrier = threading.Barrier(n)

        def client(i):
            barrier.wait()
            try:
                results[i] = _call(self.svc.address, "work", work)
            except Exception as e:  # noqa: BLE001 - we want the raw failure
                results[i] = {"success": False, "error": repr(e)}

        threads = [threading.Thread(target=client, args=(i,)) for i in range(n)]
        start = time.time()
        for t in threads:
            t.start()
        for t in threads:
            t.join(30)
        elapsed = time.time() - start

        failures = {i: r for i, r in results.items() if not r.get("success")}
        self.assertEqual(failures, {}, f"{len(failures)} of {n} calls failed")
        stats = json.loads(_call(self.svc.address, "stats")["result_json"])
        self.assertEqual(stats["calls"], n)
        self.assertEqual(stats["max_in_flight"], 1, "method calls must serialize")
        self.assertGreaterEqual(elapsed, n * work * 0.9)

    def test_control_requests_are_not_blocked_by_a_running_method(self):
        done = threading.Event()

        def long_call():
            try:
                _call(self.svc.address, "work", 2.0, timeout=10)
            except Exception:  # noqa: BLE001 - service may be killed at cleanup
                pass
            finally:
                done.set()

        threading.Thread(target=long_call, daemon=True).start()
        time.sleep(0.3)  # the long call holds the call lock now

        start = time.time()
        status = _request(self.svc.address, {"type": "GetStatus"}, timeout=5)
        self.assertTrue(status.get("success"), status)
        self.assertLess(time.time() - start, 0.5, "GetStatus waited on the call lock")
        self.assertEqual(status.get("state"), {}, "state must be skipped while a method runs")
        self.assertFalse(done.is_set(), "long call should still be running")

    def test_abandoned_request_is_not_executed_after_the_lock_frees(self):
        # Slot A holds the lock for 1.5 s. Slot B queues a call and then
        # hits its phase deadline: the worker closes the socket. B's
        # request must not run on the instrument once A is done.
        _call_in_background(self.svc.address, "work", 1.5)
        self._wait_until_lock_held()
        host, port = self.svc.address.split(":")
        sock = socket.create_connection((host, int(port)), timeout=5)
        sock.sendall(
            (json.dumps({"type": "CallMethod", "method": "work", "args_json": "[0.01]"}) + "\n").encode()
        )
        time.sleep(0.1)  # request parsed, thread now waiting on the lock
        sock.close()  # phase deadline: caller gone

        time.sleep(2.0)  # A finishes, B's turn would come here
        stats = json.loads(_call(self.svc.address, "stats")["result_json"])
        self.assertEqual(stats["calls"], 1, "abandoned request was executed")

    def test_shutdown_during_a_long_method_exits_the_process(self):
        _call_in_background(self.svc.address, "work", 5.0)
        time.sleep(0.3)
        reply = _request(self.svc.address, {"type": "Shutdown"}, timeout=5)
        self.assertTrue(reply.get("success"), reply)
        self.assertTrue(self.svc.exited_within(3), "service kept running after Shutdown")

    def test_cleanup_exits_the_process(self):
        reply = _request(self.svc.address, {"type": "Cleanup"}, timeout=5)
        self.assertTrue(reply.get("success"), reply)
        self.assertTrue(self.svc.exited_within(3), "service kept running after Cleanup")


class ConnectRetryTest(unittest.TestCase):
    def _reserve_port(self):
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        return port

    def test_refused_connect_is_retried_until_the_service_listens(self):
        port = self._reserve_port()

        def late_server():
            time.sleep(0.06)  # first attempt is refused, a retry lands
            srv = socket.socket()
            srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            srv.bind(("127.0.0.1", port))
            srv.listen(8)
            conn, _ = srv.accept()
            with conn:
                conn.makefile("rb").readline()
                conn.sendall((json.dumps({"success": True, "result_json": "7"}) + "\n").encode())
            srv.close()

        threading.Thread(target=late_server, daemon=True).start()
        plug = tp_worker.Plug("dmm", f"127.0.0.1:{port}")
        self.assertEqual(plug.measure(), 7)

    def test_gives_up_after_the_configured_attempts(self):
        port = self._reserve_port()
        plug = tp_worker.Plug("dmm", f"127.0.0.1:{port}")
        start = time.time()
        with self.assertRaisesRegex(Exception, "Connection refused|refused"):
            plug.measure()
        # 3 attempts, backoff 50-100 ms + 100-150 ms: well under a second.
        self.assertLess(time.time() - start, 2.0)

    def test_retry_never_sleeps_past_the_phase_deadline(self):
        port = self._reserve_port()
        plug = tp_worker.Plug("dmm", f"127.0.0.1:{port}")
        plug.deadline = time.monotonic() + 0.03
        start = time.time()
        with self.assertRaises(Exception):
            plug.measure()
        self.assertLess(time.time() - start, 0.5)


if __name__ == "__main__":
    unittest.main()
