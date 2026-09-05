#!/usr/bin/env python3
"""A run stopped by the OS must still power the bench down and queue its runs.

An operator closing the console, a logoff, a supervisor's SIGTERM: every
one of them used to end `tofupilot run` with no teardown and nothing
queued, leaving the instruments in their last state and 11 hours of
burn-in data in memory. This drives the two-slot soak fixture, interrupts
it mid-soak, and asserts the contract the CLI now honours:

1. the execution-scoped teardown ran: `teardown.marker` exists,
2. the process exited 130, the signal ladder's code, on its own,
3. the upload queue holds one ABORTED run per slot (the fake dashboard
   answers 503, so both stay queued instead of uploading),
4. the stop took seconds, not the 30 s the soak still had to run: a
   signal interrupts the running phases instead of waiting them out,
5. `run_finished` was still emitted, and (Unix) no worker or plug
   interpreter outlived the CLI.

Signal: SIGTERM on Unix. On Windows a CTRL_BREAK_EVENT to the CLI's own
process group, which is the console event a test can generate (close /
logoff / shutdown cannot be raised by `GenerateConsoleCtrlEvent`; the CLI
routes all four through the same arm).

The queue is read through `tofupilot queue ls --json`, so the HOME
override matters on Unix (`db::home_dir()` resolves through $HOME). On
Windows the profile directory comes from the shell API, not an env var,
so the run shares the runner's real queue: entries are filtered by
procedure id and by their queued_at being after this run started.

Usage::

    python signal_teardown.py <cli-binary> <procedure-dir>
"""

import contextlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from collections import deque
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

RUN_DEADLINE_S = 180
EXIT_SIGNALLED = 130
EXPECTED_SLOTS = {"s1", "s2"}

if len(sys.argv) < 3:
    print("usage: signal_teardown.py <cli-binary> <procedure-dir>", file=sys.stderr)
    sys.exit(2)

CLI = os.path.abspath(sys.argv[1])
SCENARIO = os.path.abspath(sys.argv[2])
PROCEDURE_ID = "00000000-0000-4000-8000-000000000004"
WINDOWS = os.name == "nt"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def handle_error(self, *_args):
        pass

    def _json(self, status: int, payload: dict):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self._json(200, {
            "organization": {"id": "org_e2e", "slug": "e2e", "name": "E2E"},
            "user": {"id": "usr_e2e", "email": "e2e@example.com"},
        })

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        if length:
            self.rfile.read(length)
        # Transient failure: the run stays queued (backoff), which is what
        # lets the queue be inspected after the CLI exits.
        self._json(503, {"error": "unavailable"})


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
base_url = f"http://127.0.0.1:{server.server_port}"
threading.Thread(target=server.serve_forever, daemon=True).start()

failures: list[str] = []


def leftover_interpreters(procedure_dir: str) -> list[str]:
    """Unix: `ps` lines of tp_worker / tp_plug processes for this run."""
    if WINDOWS:
        return []
    out = subprocess.run(["ps", "-eo", "pid,args"], capture_output=True, text=True).stdout
    return [
        line.strip() for line in out.splitlines()
        if procedure_dir in line and ("tp_worker.py" in line or "tp_plug.py" in line)
    ]


def _home():
    """Where the CLI will look for ~/.tofupilot.

    On Unix HOME redirects it into a scratch dir. On Windows the CLI asks
    the shell for the profile folder (`directories::BaseDirs`), which no
    environment variable moves, so the runner's real profile is used and
    the queue assertions filter by procedure id and time instead.
    """
    if WINDOWS:
        return contextlib.nullcontext(str(Path.home()))
    return tempfile.TemporaryDirectory()


with _home() as home:
    procedure = os.path.join(home, "procedure")
    shutil.copytree(SCENARIO, procedure)
    marker = os.path.join(procedure, "teardown.marker")

    os.makedirs(os.path.join(home, ".tofupilot"), exist_ok=True)
    with open(os.path.join(home, ".tofupilot", "credentials.json"), "w") as fh:
        json.dump({
            "api_key": "tp_e2e_key",
            "base_url": base_url,
            "organization_slug": "e2e",
            "credential_id": "cred_e2e_signal",
        }, fh)
    env = {**os.environ, "HOME": home, "TOFUPILOT_PROCEDURE_ID": PROCEDURE_ID}

    started_at = datetime.now(timezone.utc)
    popen_kwargs = {}
    if WINDOWS:
        # A new group is the only target `GenerateConsoleCtrlEvent` can
        # address without also hitting this test's own interpreter.
        popen_kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    proc = subprocess.Popen(
        [CLI, "run", procedure, "--upload", "--json", "--no-tui", "--no-kiosk"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1, env=env, cwd=home, **popen_kwargs,
    )
    stderr_tail: deque = deque(maxlen=200)

    def _drain():
        if proc.stderr is not None:
            for line in proc.stderr:
                stderr_tail.append(line)

    threading.Thread(target=_drain, daemon=True).start()
    watchdog = threading.Timer(RUN_DEADLINE_S, proc.kill)
    watchdog.daemon = True
    watchdog.start()

    soaking: set = set()
    signalled_at = None
    finished = None
    try:
        for line in proc.stdout:
            try:
                evt = json.loads(line)
            except Exception:
                continue
            etype = evt.get("type")
            if etype in ("ui_request", "identify_request"):
                values = {c["key"]: c.get("default_value") or "x"
                          for c in evt.get("components", []) if c.get("is_input")}
                proc.stdin.write(json.dumps({
                    "type": "ui_response", "request_id": evt["request_id"], "values": values,
                }) + "\n")
                proc.stdin.flush()
            elif etype == "phase_started" and evt.get("phase_key") == "soak":
                soaking.add(evt.get("slot_id"))
                if soaking >= EXPECTED_SLOTS and signalled_at is None:
                    # Both slots are inside their 30 s sleep: interrupt now.
                    time.sleep(1)
                    signalled_at = time.monotonic()
                    if WINDOWS:
                        proc.send_signal(signal.CTRL_BREAK_EVENT)
                    else:
                        proc.send_signal(signal.SIGTERM)
                    print("signal sent", flush=True)
            elif etype == "run_finished":
                finished = evt
            elif etype == "run_crashed":
                print(f"run_crashed: {evt}", file=sys.stderr)
    finally:
        watchdog.cancel()
        try:
            proc.wait(timeout=120)
        except subprocess.TimeoutExpired:
            proc.kill()
    exited_at = time.monotonic()

    print(f"run exit={proc.returncode} finished={finished is not None} "
          f"stop_took={(exited_at - signalled_at) if signalled_at else None}")
    if finished is None or proc.returncode not in (0, 130):
        # The run did not reach its end on its own: show what the CLI said.
        print("--- cli stderr (tail) ---", file=sys.stderr)
        for line in stderr_tail:
            print(line.rstrip(), file=sys.stderr)
        print("--- end cli stderr ---", file=sys.stderr)

    # Interpreters are reaped by the CLI before it exits; give the kernel
    # a beat to reflect it, then look.
    time.sleep(1)
    leftovers = leftover_interpreters(procedure)

    ls = subprocess.run([CLI, "queue", "ls", "--json"], env=env, cwd=home,
                        capture_output=True, text=True, timeout=60)
    queued = []
    for line in ls.stdout.splitlines():
        try:
            entry = json.loads(line)
        except Exception:
            continue
        if entry.get("procedure_id") != PROCEDURE_ID:
            continue
        queued_at = entry.get("queued_at")
        if queued_at:
            try:
                when = datetime.fromisoformat(queued_at.replace("Z", "+00:00"))
                if when < started_at:
                    continue
            except ValueError:
                pass
        queued.append(entry)

    marker_present = os.path.exists(marker)
    if finished is None:
        print("".join(stderr_tail)[-3000:], file=sys.stderr)

server.shutdown()

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

if signalled_at is None:
    failures.append(f"never saw both slots soaking (saw {sorted(map(str, soaking))}); "
                    "nothing was signalled")

if not marker_present:
    failures.append("teardown.marker missing: the execution-scoped power_off did not run")

if proc.returncode != EXIT_SIGNALLED:
    failures.append(f"exit code {proc.returncode}, expected {EXIT_SIGNALLED}")

if finished is None:
    failures.append("never observed run_finished")

if signalled_at is not None and exited_at - signalled_at > 20:
    failures.append(f"stop took {exited_at - signalled_at:.1f}s: the running phases were "
                    "waited out instead of interrupted")

if len(queued) != len(EXPECTED_SLOTS):
    failures.append(f"expected {len(EXPECTED_SLOTS)} queued runs, found {len(queued)}: "
                    f"{[(q.get('serial_number'), q.get('outcome')) for q in queued]}")
else:
    wrong = [q for q in queued if q.get("outcome") != "ABORTED"]
    if wrong:
        failures.append(f"queued runs not ABORTED: "
                        f"{[(q.get('serial_number'), q.get('outcome')) for q in wrong]}")

if leftovers:
    failures.append("interpreters outlived the CLI:\n  " + "\n  ".join(leftovers))

if failures:
    print("FAIL", file=sys.stderr)
    for f in failures:
        print(f"  - {f}", file=sys.stderr)
    sys.exit(1)

print(f"OK: teardown ran, exit {EXIT_SIGNALLED}, {len(queued)} ABORTED runs queued")
