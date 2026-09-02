#!/usr/bin/env python3
"""A credential file that predates `credential_id` must be filled in, and the
run that filled it in must ALREADY upload with a reference.

TP-1012 shipped run-upload idempotency keyed on `<credential_id>_<counter>`.
`credential_id` is only issued at login, and a production station never re-runs
`tofupilot login --token`, so without a backfill every station enrolled before
that release would upload without a reference for the rest of its life. The
installed base is the entire point of the feature, and it is the one part
`upload_idempotency.py` cannot reach: that test writes `credential_id` into the
credential file up front, so it always starts from an already-migrated install.

The backfill runs in `main.rs` after credential resolution and before
`run_cmd`, and the reference is minted later, inside `enqueue()`. That ordering
is the whole contract. Move the backfill after the upload, or drop it into an
arm that a deployment run does not take, and every assertion below still passes
except the third — which is why the third one exists.

What it asserts, against a fake dashboard that answers whoami like a current
one and accepts the upload:

1. the CLI probed `GET /api/cli/whoami` at all — a credential file missing the
   field must trigger exactly one round-trip, not zero,
2. `credentials.json` on disk carries the server's `credential_id` afterwards,
   so the next process starts already migrated and never probes again,
3. the create POST of THIS run already carries a reference namespaced by that
   id. A backfill that only takes effect from the next run leaves the upload
   that triggered it unprotected, which for a station that runs one long
   session per boot is most of them.

`credential_id_tests` in `commands/auth/mod.rs` covers the parser and the
one-hour retry window of a failed probe. Neither can see the ordering, because
neither runs the binary.

Unix only, same reason as `upload_idempotency.py`: it overrides HOME to keep
the CLI out of the developer's real `~/.tofupilot`, and `db::home_dir()`
resolves through $HOME on unix.

Usage::

    python credential_backfill.py <cli-binary> <procedure-dir>
"""
import json
import os
import re
import subprocess
import sys
import time
import tempfile
import threading
from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

RUN_DEADLINE_S = 300

if len(sys.argv) < 3:
    print("usage: credential_backfill.py <cli-binary> <procedure-dir>", file=sys.stderr)
    sys.exit(2)

CLI = os.path.abspath(sys.argv[1])
PROCEDURE = os.path.abspath(sys.argv[2])
# Deliberately not a plausible-looking id: if an assertion passes it is because
# this exact string travelled from the whoami body to the credential file to
# the upload, not because something reconstructed a similar-looking one.
CREDENTIAL_ID = "cred_e2e_backfill_9Qx"
PROCEDURE_ID = "00000000-0000-4000-8000-000000000001"

creates: list[dict] = []
whoami_hits: list[str] = []
# Every request the fake dashboard saw, so a failure says which endpoints the
# CLI actually reached instead of only which one it did not.
seen: list[str] = []
_lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass  # keep the test output readable

    def _json(self, status: int, payload: dict):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        with _lock:
            seen.append(f"GET {self.path}")
        identity = {
            "organization": {"id": "org_e2e", "slug": "e2e", "name": "E2E"},
            "user": {"id": "usr_e2e", "email": "e2e@example.com"},
        }
        # Only whoami carries the id, exactly as the dashboard does. Answering
        # it on every GET would let a CLI that read it from some other probe
        # pass a test meant to pin the whoami contract.
        if "/api/cli/whoami" in self.path:
            with _lock:
                whoami_hits.append(self.path)
            identity["credential_id"] = CREDENTIAL_ID
        self._json(200, identity)

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        with _lock:
            seen.append(f"POST {self.path}")

        if "/runs" not in self.path:
            self._json(200, {"id": "00000000-0000-4000-8000-0000000000ff"})
            return

        try:
            payload = json.loads(raw)
        except json.JSONDecodeError:
            payload = {}

        with _lock:
            creates.append(payload)

        # Unlike upload_idempotency.py this dashboard answers: the retry path
        # is that test's subject, and leaving the upload queued here would only
        # add a 15 s backoff between us and the assertions.
        self._json(200, {"id": "00000000-0000-4000-8000-000000000002"})


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
base_url = f"http://127.0.0.1:{server.server_port}"
threading.Thread(target=server.serve_forever, daemon=True).start()

failures: list[str] = []
saved: dict = {}

with tempfile.TemporaryDirectory() as home:
    creds_path = os.path.join(home, ".tofupilot", "credentials.json")
    os.makedirs(os.path.dirname(creds_path), exist_ok=True)
    # The pre-migration shape: no `credential_id` key at all, which is what a
    # file written by any CLI before 1.5.0 looks like. Not `null` — absent.
    with open(creds_path, "w") as fh:
        json.dump({
            "api_key": "tp_e2e_key",
            "base_url": base_url,
            "organization_slug": "e2e",
        }, fh)

    env = {
        **os.environ,
        "HOME": home,
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
                values[key] = "SN-BACKFILL-0001"
            elif t == "textarea":
                values[key] = "credential backfill e2e"
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

    # `run --upload` ENQUEUES; the POST is the queue's job and does not happen
    # before the process exits (`queue ls` shows attempts: 0 right here). So
    # drain it explicitly, exactly as upload_idempotency.py does. Asserting on
    # the creates without this step passes vacuously on a build where the
    # reference was never minted, because there is no create to inspect at all.
    # No backoff to wait out, unlike that test: this dashboard answered, so
    # nothing has failed yet and the first retry fires immediately.
    deadline = time.monotonic() + 60
    drain = None
    while time.monotonic() < deadline and not creates:
        drain = subprocess.run(
            [CLI, "queue", "retry", "--json"],
            env=env, cwd=home, capture_output=True, text=True, timeout=180,
        )
        if creates:
            break
        time.sleep(2)
    print(f"queue retry exit={drain.returncode if drain else 'n/a'} "
          f"{(drain.stdout.strip()[:120] if drain else '')}")
    if not creates:
        ls = subprocess.run([CLI, "queue", "ls", "--json"], env=env, cwd=home,
                            capture_output=True, text=True, timeout=60)
        print(f"queue ls: {ls.stdout.strip()[:400]}")

    # Read the credential file back INSIDE the temp dir's lifetime.
    try:
        with open(creds_path) as fh:
            saved = json.load(fh)
    except Exception as e:  # noqa: BLE001 - reported as a failure below
        failures.append(f"could not read {creds_path} back after the run: {e}")

server.shutdown()

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

if not whoami_hits:
    failures.append(
        "the CLI never called GET /api/cli/whoami. A credential file with no "
        "credential_id must probe for it once, or the installed base never "
        "gets an idempotency reference."
    )

if saved and saved.get("credential_id") != CREDENTIAL_ID:
    failures.append(
        f"credentials.json carries credential_id={saved.get('credential_id')!r} "
        f"after the run, expected {CREDENTIAL_ID!r}. Without the write the "
        "probe repeats forever and nothing is migrated."
    )

# The api_key must survive the rewrite: `save` serialises the whole struct, so
# a backfill that dropped a field would log the operator out on the next run.
if saved and saved.get("api_key") != "tp_e2e_key":
    failures.append(
        f"credentials.json lost its api_key (now {saved.get('api_key')!r}). "
        "The backfill rewrites the whole file and must preserve every field."
    )

if not creates:
    failures.append(
        "no create POST reached the dashboard, so the run never uploaded and "
        f"assertion 3 could not be evaluated. Requests seen: {seen}"
    )
else:
    ref = creates[0].get("client_run_ref")
    if ref is None:
        failures.append(
            "the run that triggered the backfill uploaded with no "
            "client_run_ref. The backfill must land before enqueue() mints "
            "the reference, otherwise the upload that migrated the file is "
            "itself unprotected."
        )
    elif not re.fullmatch(rf"{re.escape(CREDENTIAL_ID)}_\d+", ref):
        failures.append(
            f"reference {ref!r} is not <credential_id>_<counter> built from "
            f"the backfilled {CREDENTIAL_ID!r}. The id the server issued is "
            "what makes the reference unique by construction."
        )

for f in failures:
    print(f"FAIL: {f}", file=sys.stderr)

if failures:
    sys.exit(1)

print(
    f"OK: whoami probed {len(whoami_hits)}x, credentials.json migrated to "
    f"{saved.get('credential_id')!r}, this run uploaded as "
    f"{creates[0]['client_run_ref']!r}"
)
