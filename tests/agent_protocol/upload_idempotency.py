#!/usr/bin/env python3
"""A retried upload must carry the SAME idempotency reference as the first try.

TP-1012: a run upload whose HTTP response never reached the station was
re-POSTed, and since `runs.create` had no idempotency the retry created a
second, complete run. 67 duplicate pairs at one customer over 45 days, every
POST answered 200 — the loss was on the response path.

The fix is a `client_run_ref` the CLI mints and persists BEFORE it POSTs, so
the retry replays it and the server recognises the upload it already stored.
This test pins the CLI half of that contract, which no unit test can reach:
the reference has to survive the upload failing and the process moving on.

What it asserts, against a fake dashboard that swallows the first response:

1. the first POST /v2/runs already carries a reference — minted before the
   request went out, not after a successful reply,
2. the retry POSTs the same reference byte for byte,
3. the reference is shaped `<credential_id>_<counter>`, i.e. namespaced by the
   server-issued credential rather than by anything the client invented.

Assertion 2 is the regression: without it a lost response means a duplicate
run. Run this against a build that predates the fix and it fails on 1, since
the field is absent entirely.

The server half — same reference means one run — is covered by
`apps/web/server/trpc/core/runs/create-idempotency.test.ts`. A fake dashboard
cannot deduplicate, so this side proves the reference travels; that side proves
it is honoured.

Unix only: it overrides HOME to keep the CLI out of the developer's real
`~/.tofupilot`, and `db::home_dir()` resolves through $HOME on unix. Once
`TOFUPILOT_HOME` exists this can run on Windows too.

Usage::

    python upload_idempotency.py <cli-binary> <procedure-dir>
"""
import json
import os
import re
import socket
import time
import subprocess
import sys
import tempfile
import threading
from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

RUN_DEADLINE_S = 300

if len(sys.argv) < 3:
    print("usage: upload_idempotency.py <cli-binary> <procedure-dir>", file=sys.stderr)
    sys.exit(2)

CLI = os.path.abspath(sys.argv[1])
PROCEDURE = os.path.abspath(sys.argv[2])
CREDENTIAL_ID = "cred_e2e_idempotency"
PROCEDURE_ID = "00000000-0000-4000-8000-000000000001"

# Every create POST the fake dashboard saw, in order.
creates: list[dict] = []
_lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass  # keep the test output readable

    def handle_error(self, *_args):
        # Cutting a connection mid-response is the scenario, not a fault:
        # socketserver would otherwise dump a traceback per dropped reply.
        pass

    def _json(self, status: int, payload: dict):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        # whoami / auth probes: answer plausibly so the CLI proceeds to upload.
        self._json(200, {
            "organization": {"id": "org_e2e", "slug": "e2e", "name": "E2E"},
            "user": {"id": "usr_e2e", "email": "e2e@example.com"},
        })

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b"{}"

        if "/runs" not in self.path:
            self._json(200, {"id": "00000000-0000-4000-8000-0000000000ff"})
            return

        try:
            payload = json.loads(raw)
        except json.JSONDecodeError:
            payload = {}

        with _lock:
            creates.append(payload)
            attempt = len(creates)

        if attempt == 1:
            # THE failure this whole feature exists for: the run IS created
            # server side, and the client never learns its id. Closing the
            # connection without a response is what a proxy dropping the reply
            # looks like from the CLI's side (their EMS runs behind a Squid
            # proxy, thread TH-1183 — that is the reference, not the task).
            print("fake-dashboard: create #1 accepted, response withheld", flush=True)
            self.close_connection = True
            try:
                # Shut the socket down without answering: from the CLI's side
                # this is indistinguishable from the proxy dropping the reply,
                # which is the production failure: their EMS runs the stations behind
                # a Squid proxy (thread TH-1183).
                self.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            return

        print(f"fake-dashboard: create #{attempt} answered", flush=True)
        self._json(200, {"id": "00000000-0000-4000-8000-000000000002"})


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
base_url = f"http://127.0.0.1:{server.server_port}"
threading.Thread(target=server.serve_forever, daemon=True).start()

failures: list[str] = []

with tempfile.TemporaryDirectory() as home:
    os.makedirs(os.path.join(home, ".tofupilot"), exist_ok=True)
    with open(os.path.join(home, ".tofupilot", "credentials.json"), "w") as fh:
        json.dump({
            "api_key": "tp_e2e_key",
            "base_url": base_url,
            "organization_slug": "e2e",
            "credential_id": CREDENTIAL_ID,
        }, fh)

    env = {
        **os.environ,
        "HOME": home,
        # The fixture is not linked to a procedure, and `run --upload` refuses
        # to guess one. Same escape hatch the CLI documents for CI.
        "TOFUPILOT_PROCEDURE_ID": PROCEDURE_ID,
    }

    def answer(evt):
        """Valid response for every input component, `default_value` first.

        The CLI injects an identify-unit prompt ahead of the first phase and
        aborts the run if nothing answers it, so a driver that only reads is a
        driver that tests nothing. Same shape as ci_smoke.py.
        """
        values = {}
        for c in evt.get("components", []):
            if not c.get("is_input"):
                continue
            t, key = c.get("type"), c["key"]
            opts = c.get("options") or []
            if c.get("default_value") is not None:
                values[key] = c["default_value"]
            elif t == "switch":
                values[key] = True
            elif t in ("radio", "select"):
                if opts:
                    values[key] = opts[0]["value"]
            elif t in ("multiselect", "checklist"):
                if opts:
                    values[key] = [opts[0]["value"]]
            elif t in ("number_input", "slider"):
                values[key] = 42
            elif t == "text_input":
                values[key] = "SN-IDEM-0001"
            elif t == "textarea":
                values[key] = "idempotency e2e"
            else:
                values[key] = "ok"
        return values

    proc = subprocess.Popen(
        [CLI, "run", PROCEDURE, "--upload", "--json", "--no-tui", "--no-kiosk",
         "--ui-timeout", "30"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1, env=env, cwd=home,
    )

    # Drain stderr on a thread: the CLI echoes the connector's stderr as it
    # arrives, and leaving it in the pipe deadlocks once the buffer fills.
    stderr_tail: deque = deque(maxlen=120)

    def _drain():
        if proc.stderr is not None:
            for line in proc.stderr:
                stderr_tail.append(line)

    threading.Thread(target=_drain, daemon=True).start()
    watchdog = threading.Timer(RUN_DEADLINE_S, proc.kill)
    watchdog.daemon = True
    watchdog.start()

    saw_finished = False
    try:
        for line in proc.stdout:
            try:
                evt = json.loads(line)
            except Exception:
                continue
            etype = evt.get("type")
            if etype in ("ui_request", "identify_request"):
                proc.stdin.write(json.dumps({
                    "type": "ui_response",
                    "request_id": evt["request_id"],
                    "values": answer(evt),
                }) + "\n")
                proc.stdin.flush()
            elif etype == "run_finished":
                saw_finished = True
            elif etype == "run_crashed":
                print(f"run_crashed: {evt}", file=sys.stderr)
    finally:
        watchdog.cancel()
        try:
            proc.wait(timeout=120)
        except subprocess.TimeoutExpired:
            proc.kill()

    print(f"run exit={proc.returncode} run_finished={saw_finished}")
    if not saw_finished:
        print("".join(stderr_tail)[-2000:], file=sys.stderr)

    # The upload lost its response, so the entry is still queued with
    # `run_id: null` and `last_error: network`. The queue holds it in backoff
    # (15 s for the first failure, `backoff_seconds` in queue.rs), so poll
    # `queue retry` past that deadline rather than assuming one call fires it.
    deadline = time.monotonic() + 90
    retry = None
    while time.monotonic() < deadline and len(creates) < 2:
        time.sleep(5)
        retry = subprocess.run(
            [CLI, "queue", "retry", "--json"],
            env=env, cwd=home, capture_output=True, text=True, timeout=180,
        )
    print(f"queue retry exit={retry.returncode if retry else 'n/a'} "
          f"{(retry.stdout.strip()[:120] if retry else '')}")
    if len(creates) < 2:
        ls = subprocess.run([CLI, "queue", "ls", "--json"], env=env, cwd=home,
                            capture_output=True, text=True, timeout=60)
        print(f"queue ls: {ls.stdout.strip()[:400]}")

server.shutdown()

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

if len(creates) < 2:
    failures.append(
        f"expected at least 2 create POSTs (first + retry), saw {len(creates)}. "
        "Without a retry there is nothing to compare and the regression is untested."
    )
else:
    refs = [c.get("client_run_ref") for c in creates]

    if refs[0] is None:
        failures.append(
            "the FIRST create carried no client_run_ref. The reference must be "
            "minted and persisted before the request goes out, or a lost "
            "response leaves nothing for the retry to reuse."
        )

    # `None` stays in the set on purpose: a retry that DROPS the reference is a
    # retry that changed it, and filtering nulls out here would let exactly the
    # regression this test exists for go green.
    if len(set(refs)) > 1:
        failures.append(
            f"the retry changed or dropped the reference: {refs}. Every attempt "
            "at one upload must send the same one, otherwise the server sees "
            "two different uploads and stores two runs."
        )

    if refs[0] is not None and not re.fullmatch(
        rf"{re.escape(CREDENTIAL_ID)}_\d+", refs[0]
    ):
        failures.append(
            f"reference {refs[0]!r} is not <credential_id>_<counter>. The "
            "credential id is what makes it unique by construction rather "
            "than by chance."
        )

for f in failures:
    print(f"FAIL: {f}", file=sys.stderr)

if failures:
    sys.exit(1)

print(f"OK: {len(creates)} creates, one reference throughout: {creates[0]['client_run_ref']}")
