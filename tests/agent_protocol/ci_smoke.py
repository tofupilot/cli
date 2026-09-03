#!/usr/bin/env python3
"""End-to-end smoke test for CI.

Drives `tofupilot run <procedure> --json` over the agent protocol, answers
every prompt with valid values (same generic strategy as drive_cli.py), and
asserts the run actually executed. Exits non-zero on any failure so it can
gate a CI job.

The assertions are deliberately stricter than "the stream started and ended".
Observing `run_started` and `run_finished` alone proves nothing: a procedure
whose interpreter dies at import time still produces both, wrapped around a
`run_crashed`, with `outcome: FAIL`. A gate built on that pair stays green
while executing zero phases. So we additionally require that no `run_crashed`
was emitted, that at least one phase reported an outcome, and that the run
ended on the expected outcome.

Answering `identify_request` is part of that: the CLI injects an identify-unit
prompt ahead of the first phase, and it is a distinct event type from
`ui_request`. Leaving it unanswered times out and aborts the run before any
phase executes.

Usage: ci_smoke.py <cli-binary> <procedure-dir>
"""
import json
import subprocess
import sys
import threading
import time
from collections import deque

if len(sys.argv) != 3:
    print("usage: ci_smoke.py <cli-binary> <procedure-dir>", file=sys.stderr)
    sys.exit(2)

CLI, PROCEDURE = sys.argv[1], sys.argv[2]

# Every leg asserts a passing run. Kept as a constant rather than a flag: an
# option no caller passes is one more thing to read and to keep working.
EXPECT = "PASS"

# Wall-clock bound for the whole run. The stdout loop below has no timeout of
# its own, and a child that hangs without printing would otherwise pin the job
# until GitHub's 6-hour default — on a Windows runner, for every leg.
DEADLINE_S = 600


def answer(evt):
    """Build a valid response for every input component in a prompt.

    A component's own `default_value` wins when present — the identify-unit
    prompt carries the part number resolved from the procedure, and inventing
    one instead would exercise a different code path than the operator's.
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

# Drain stderr on a thread. Holding it in the pipe until the stdout loop ends
# deadlocks as soon as the child fills the buffer — 64 KB on Linux, as little
# as 4 KB on a Windows pipe — and the CLI echoes every line of the connector's
# stderr as it arrives (`run/python.rs`), so that is a question of volume, not
# of failure. Only the tail is kept: that is what a failure report needs.
stderr_tail: deque = deque(maxlen=200)


def _drain_stderr():
    if proc.stderr is None:
        return
    for line in proc.stderr:
        stderr_tail.append(line)


threading.Thread(target=_drain_stderr, daemon=True).start()
watchdog = threading.Timer(DEADLINE_S, proc.kill)
watchdog.daemon = True
watchdog.start()

saw_run_started = False
saw_run_finished = False
run_outcome = None
slot_outcomes = {}
slots_seen = set()
crash = None
phase_outcomes = []

try:
    for line in proc.stdout:
        try:
            evt = json.loads(line)
        except Exception:
            continue
        etype = evt.get("type")
        # Lifecycle trace on stderr so a CI failure shows what the driver
        # saw and when, not only the final verdict.
        if etype in ("run_started", "ui_request", "identify_request", "ui_timeout",
                     "identify_timeout", "run_crashed", "run_finished"):
            print(f"[smoke {time.strftime('%H:%M:%S')}] {etype} {evt.get('request_id', '')}",
                  file=sys.stderr, flush=True)
        if etype == "run_started":
            saw_run_started = True
        elif etype in ("ui_request", "identify_request"):
            resp = {
                "type": "ui_response",
                "request_id": evt["request_id"],
                "values": answer(evt),
            }
            proc.stdin.write(json.dumps(resp) + "\n")
            proc.stdin.flush()
            print(f"[smoke {time.strftime('%H:%M:%S')}] answered {evt['request_id']}",
                  file=sys.stderr, flush=True)
        elif etype == "phase_finished":
            phase_outcomes.append((evt.get("phase_key"), evt.get("outcome")))
            if evt.get("slot_id") is not None:
                slots_seen.add(evt["slot_id"])
        elif etype == "run_crashed":
            crash = evt
        elif etype == "run_finished":
            saw_run_finished = True
            run_outcome = evt.get("outcome")
            slot_outcomes = evt.get("slot_outcomes") or {}
            break
finally:
    watchdog.cancel()
    try:
        proc.wait(timeout=90)
    except subprocess.TimeoutExpired:
        proc.kill()

errors = []
if not saw_run_started:
    errors.append("never observed a run_started event")
if not saw_run_finished:
    errors.append("never observed a run_finished event")
if crash is not None:
    errors.append(
        "run_crashed (exit {}): {}".format(
            crash.get("exit_code"), (crash.get("stderr_tail") or "").strip()[-1500:]
        )
    )
if not phase_outcomes:
    errors.append("no phase_finished event — the procedure executed nothing")
if run_outcome != EXPECT:
    errors.append(f"run outcome {run_outcome!r}, expected {EXPECT!r}")
# Multi-slot procedures: `run_finished.slot_outcomes` names every slot that
# ran a phase, each with the outcome its own uploaded run carries.
if len(slots_seen) > 1:
    missing = sorted(slots_seen - set(slot_outcomes))
    if missing:
        errors.append(f"run_finished.slot_outcomes lacks slots {missing}")
    bad = {k: v for k, v in slot_outcomes.items() if v != EXPECT}
    if bad:
        errors.append(f"slot outcomes {bad}, expected every slot {EXPECT!r}")
elif slot_outcomes:
    errors.append("single-slot run must not carry slot_outcomes")

if errors:
    print("SMOKE FAILED:", file=sys.stderr)
    for e in errors:
        print(f"  - {e}", file=sys.stderr)
    tail = "".join(stderr_tail)
    if tail:
        print("[stderr tail]", tail[-1000:], file=sys.stderr)
    sys.exit(1)

slot_note = f", slots={slot_outcomes}" if slot_outcomes else ""
print(
    f"SMOKE OK: {len(phase_outcomes)} phase(s) executed, outcome={run_outcome}{slot_note} "
    f"({', '.join(f'{k}={o}' for k, o in phase_outcomes)})"
)
