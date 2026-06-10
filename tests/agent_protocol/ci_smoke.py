#!/usr/bin/env python3
"""End-to-end smoke test for CI.

Drives `tofupilot run <procedure> --json` over the agent protocol, answering
every `ui_request` with valid values (same generic strategy as drive_cli.py),
and asserts the run reaches `run_finished`. Exits non-zero on any failure so it
can gate a CI job.

Usage: ci_smoke.py <cli-binary> <procedure-dir>
"""
import json
import subprocess
import sys

if len(sys.argv) != 3:
    print("usage: ci_smoke.py <cli-binary> <procedure-dir>", file=sys.stderr)
    sys.exit(2)

CLI, PROCEDURE = sys.argv[1], sys.argv[2]


def answer(evt):
    """Build a valid response for every input component in a ui_request."""
    values = {}
    for c in evt.get("components", []):
        if not c.get("is_input"):
            continue
        t, key = c.get("type"), c["key"]
        opts = c.get("options") or []
        if t == "switch":
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
            values[key] = "SN-0001"
        elif t == "textarea":
            values[key] = "ci note"
        else:
            values[key] = "ok"
    return values


proc = subprocess.Popen(
    [CLI, "run", PROCEDURE, "--json", "--ui-timeout", "30"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    bufsize=1,
)

saw_run_started = False
saw_run_finished = False
run_outcome = None

try:
    for line in proc.stdout:
        try:
            evt = json.loads(line)
        except Exception:
            continue
        etype = evt.get("type")
        if etype == "run_started":
            saw_run_started = True
        elif etype == "ui_request":
            resp = {
                "type": "ui_response",
                "request_id": evt["request_id"],
                "values": answer(evt),
            }
            proc.stdin.write(json.dumps(resp) + "\n")
            proc.stdin.flush()
        elif etype == "run_finished":
            saw_run_finished = True
            run_outcome = evt.get("outcome")
            break
finally:
    try:
        proc.wait(timeout=90)
    except subprocess.TimeoutExpired:
        proc.kill()

errors = []
if not saw_run_started:
    errors.append("never observed a run_started event")
if not saw_run_finished:
    errors.append("never observed a run_finished event")

if errors:
    print("SMOKE FAILED:", "; ".join(errors), file=sys.stderr)
    tail = proc.stderr.read()
    if tail:
        print("[stderr tail]", tail[-1000:], file=sys.stderr)
    sys.exit(1)

print(f"SMOKE OK: run_started -> run_finished (outcome={run_outcome})")
