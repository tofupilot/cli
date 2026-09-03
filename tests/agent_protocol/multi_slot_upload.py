#!/usr/bin/env python3
"""One start on a multi-slot procedure must upload one run per slot.

Against a fake dashboard that accepts every upload, run the four-slot
fixture and read what reached POST /v2/runs:

1. exactly one create per slot, four in total,
2. all four share one `execution_id` (a UUID minted once per start) and
   carry their own `slot_key` and `slot_name`,
3. each carries its own unit (`SMOKE-<slot>` from the `{slot}` placeholder,
   `Nest N` from `{slot_name}`), its own idempotency reference, the PASS
   outcome, and the shared stages (`power_on`, `power_off`) next to its own
   phases,
4. run metadata written by a shared stage reaches every slot's run.

The server half (grouping, filters, uniqueness per execution and slot) is
covered by `apps/web/server/trpc/core/runs`; this side proves the CLI
produces the N uploads.

Unix only, same HOME override as upload_idempotency.py.

Usage::

    python multi_slot_upload.py <cli-binary> <procedure-dir>
"""

import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

RUN_DEADLINE_S = 300
EXPECTED_SLOTS = {"s1": "Nest 1", "s2": "Nest 2", "s3": "Nest 3", "s4": "Nest 4"}
SHARED_PHASES = {"Power on rack", "Power off rack"}
OWN_PHASES = {"Prepare nest", "Measure", "Release nest"}

if len(sys.argv) < 3:
    print("usage: multi_slot_upload.py <cli-binary> <procedure-dir>", file=sys.stderr)
    sys.exit(2)

CLI = os.path.abspath(sys.argv[1])
PROCEDURE = os.path.abspath(sys.argv[2])
CREDENTIAL_ID = "cred_e2e_multislot"
PROCEDURE_ID = "00000000-0000-4000-8000-000000000003"

creates: list[dict] = []
_lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def handle_error(self, *_args):
        # A client that vanished mid-response is the symptom under test,
        # not a fault of the fake dashboard: no traceback per connection.
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
            n = len(creates)
        self._json(200, {"id": f"00000000-0000-4000-8000-0000000000{n:02x}"})


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
    env = {**os.environ, "HOME": home, "TOFUPILOT_PROCEDURE_ID": PROCEDURE_ID}

    proc = subprocess.Popen(
        [CLI, "run", PROCEDURE, "--upload", "--json", "--no-tui", "--no-kiosk",
         "--ui-timeout", "30"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1, env=env, cwd=home,
    )
    stderr_tail: deque = deque(maxlen=120)

    def _drain():
        if proc.stderr is not None:
            for line in proc.stderr:
                stderr_tail.append(line)

    threading.Thread(target=_drain, daemon=True).start()
    watchdog = threading.Timer(RUN_DEADLINE_S, proc.kill)
    watchdog.daemon = True
    watchdog.start()

    started_execution_id = None
    finished = None
    try:
        for line in proc.stdout:
            try:
                evt = json.loads(line)
            except Exception:
                continue
            etype = evt.get("type")
            if etype == "run_started":
                started_execution_id = evt.get("execution_id")
            elif etype in ("ui_request", "identify_request"):
                # auto_identify: no prompt expected; answer defensively.
                values = {c["key"]: c.get("default_value") or "x"
                          for c in evt.get("components", []) if c.get("is_input")}
                proc.stdin.write(json.dumps({
                    "type": "ui_response", "request_id": evt["request_id"], "values": values,
                }) + "\n")
                proc.stdin.flush()
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

    print(f"run exit={proc.returncode} finished={finished is not None}")
    if finished is None:
        print("".join(stderr_tail)[-2000:], file=sys.stderr)

    # Uploads are spawned after run_finished; give the queue a moment, then
    # drain anything still pending.
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline and len(creates) < len(EXPECTED_SLOTS):
        time.sleep(2)
        subprocess.run([CLI, "queue", "retry", "--json"], env=env, cwd=home,
                       capture_output=True, text=True, timeout=120)

server.shutdown()

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

if finished is None:
    failures.append("never observed run_finished")
elif finished.get("outcome") != "PASS":
    failures.append(f"run outcome {finished.get('outcome')!r}, expected PASS")
elif set((finished.get("slot_outcomes") or {})) != set(EXPECTED_SLOTS):
    failures.append(f"run_finished.slot_outcomes {finished.get('slot_outcomes')!r} "
                    f"must name exactly {sorted(EXPECTED_SLOTS)}")

if len(creates) != len(EXPECTED_SLOTS):
    failures.append(f"expected {len(EXPECTED_SLOTS)} create POSTs (one per slot), saw {len(creates)}")
else:
    by_slot = {c.get("slot_key"): c for c in creates}
    if set(by_slot) != set(EXPECTED_SLOTS):
        failures.append(f"slot_keys {sorted(map(str, by_slot))} != {sorted(EXPECTED_SLOTS)}")
    exec_ids = {c.get("execution_id") for c in creates}
    if len(exec_ids) != 1 or None in exec_ids:
        failures.append(f"every create must share one execution_id, saw {exec_ids}")
    else:
        (eid,) = exec_ids
        if not re.fullmatch(r"[0-9a-f-]{36}", eid):
            failures.append(f"execution_id {eid!r} is not a UUID")
        if started_execution_id and eid != started_execution_id:
            failures.append(f"execution_id {eid} differs from run_started.execution_id {started_execution_id}")
    refs = [c.get("client_run_ref") for c in creates]
    if None in refs or len(set(refs)) != len(refs):
        failures.append(f"each slot's upload needs its own idempotency reference, saw {refs}")
    for slot, name in EXPECTED_SLOTS.items():
        c = by_slot.get(slot)
        if c is None:
            continue
        if c.get("slot_name") != name:
            failures.append(f"{slot}: slot_name {c.get('slot_name')!r} != {name!r}")
        if c.get("serial_number") != f"SMOKE-{slot}":
            failures.append(f"{slot}: serial_number {c.get('serial_number')!r}, "
                            f"expected SMOKE-{slot} from the {{slot}} placeholder")
        if c.get("batch_number") != name:
            failures.append(f"{slot}: batch_number {c.get('batch_number')!r}, "
                            f"expected {name!r} from the {{slot_name}} placeholder")
        if c.get("outcome") != "PASS":
            failures.append(f"{slot}: outcome {c.get('outcome')!r}")
        names = {p.get("name") for p in c.get("phases") or []}
        if not SHARED_PHASES <= names:
            failures.append(f"{slot}: shared stages missing from phases {sorted(names)}")
        if not OWN_PHASES <= names:
            failures.append(f"{slot}: own phases missing from {sorted(names)}")
        if len(c.get("phases") or []) != len(SHARED_PHASES | OWN_PHASES):
            failures.append(f"{slot}: {len(c.get('phases') or [])} phases, expected "
                            f"{len(SHARED_PHASES | OWN_PHASES)} (no sibling slot's phases)")
        if (c.get("metadata") or {}).get("rack") != "R-1":
            failures.append(f"{slot}: run metadata from the shared stage missing: {c.get('metadata')}")

for f in failures:
    print(f"FAIL: {f}", file=sys.stderr)
if failures:
    sys.exit(1)
print(f"OK: {len(creates)} uploads, one per slot, execution_id={creates[0]['execution_id']}")
