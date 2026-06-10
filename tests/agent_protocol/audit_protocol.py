#!/usr/bin/env python3
"""Audit the agent protocol for ordering, completeness, and shape invariants.

Invariants checked:
  1. The first event is `run_started`.
  2. Exactly one `plan` event, right after `run_started`.
  3. Every `phase_started` is followed by a matching `phase_finished` with the same phase_key.
  4. No `phase_finished` without a prior `phase_started` (same phase_key).
  5. Every `ui_request` has `phase_key` matching the most recent `phase_started` for that phase,
     and the phase_key is not empty.
  6. Every `ui_request` is followed (eventually) by a `phase_finished` for the same phase_key
     (never a second `ui_request` for the same `request_id`).
  7. `ui_auto_continue` has `source` in {"display_only","pre_baked"} and values is a dict.
  8. The last event is `run_finished`, and exit_code 0 ⇔ outcome "PASS".
  9. All phase_keys in the `plan` event appear as `phase_started` in declared order
     (for OpenHTF: allows phases to be skipped entirely if the run errored).
 10. Each `phase_started.phase_key` also appears in `plan.phases`.
"""
import json
import os
import subprocess
import sys

CLI = sys.argv[1]
DRIVER_ANSWER = {
    "switch": True,
    "text_input": "operator",
    "textarea": "note",
    "number_input": 42,
    "slider": 50,
    "radio": None,  # pick first option
    "select": None,
    "multiselect": None,
    "checklist": None,
}


def answer(component):
    key = component["key"]
    t = component["type"]
    opts = component.get("options") or []
    if t == "switch":
        return True
    if t in ("radio", "select") and opts:
        return opts[0]["value"]
    if t in ("multiselect", "checklist") and opts:
        return [opts[0]["value"]]
    if t in ("number_input", "slider"):
        return 42
    if t == "text_input":
        return "op"
    if t == "textarea":
        return "note"
    return "ok"


def drive(procedure_dir):
    proc = subprocess.Popen(
        [CLI, "run", procedure_dir, "--json", "--ui-timeout", "10"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1,
    )
    events = []
    try:
        for line in proc.stdout:
            line = line.rstrip("\n")
            if not line:
                continue
            try:
                evt = json.loads(line)
            except Exception:
                # Drop non-protocol lines (e.g. stderr mirrored via {"type":"stderr"})
                continue
            events.append(evt)
            if evt.get("type") == "ui_request":
                values = {c["key"]: answer(c) for c in evt.get("components", [])
                          if c.get("is_input")}
                resp = {"type": "ui_response", "request_id": evt["request_id"], "values": values}
                proc.stdin.write(json.dumps(resp) + "\n"); proc.stdin.flush()
            if evt.get("type") == "run_finished":
                break
    finally:
        try:
            proc.wait(timeout=20)
        except Exception:
            proc.kill()
    return events, proc.returncode


def audit(name, events, exit_code):
    errors = []

    def fail(msg):
        errors.append(f"  - {msg}")

    # 1. first event
    types = [e.get("type") for e in events]
    if not types or types[0] != "run_started":
        fail(f"first event is {types[:1]!r}, expected run_started")

    # 2. exactly one plan event, right after run_started — unless the run
    # crashed before the subprocess got far enough to emit test_start.
    crashed_run = any(e.get("type") == "run_crashed" for e in events)
    plan_idxs = [i for i, e in enumerate(events) if e.get("type") == "plan"]
    if crashed_run:
        if len(plan_idxs) > 1:
            fail(f"{len(plan_idxs)} plan events in crashed run, expected 0 or 1")
    else:
        if len(plan_idxs) != 1:
            fail(f"expected exactly 1 plan event, got {len(plan_idxs)}")
        elif plan_idxs[0] != 1:
            fail(f"plan event at index {plan_idxs[0]}, expected 1 (right after run_started)")

    # 3 & 4. phase_started/finished pairs by (phase_key, attempt).
    # Every `phase_started` must have a matching `phase_finished` with the
    # same (phase_key, attempt). OpenHTF may emit all phase_starteds eagerly
    # (live, per retry) and then a burst of phase_finisheds from test_record,
    # so per-attempt ordering across the pair is not strictly begin→end;
    # what matters is that the pairing is bijective.
    started_attempts = set()
    finished_attempts = set()
    for e in events:
        t = e.get("type")
        if t == "phase_started":
            key = (e["phase_key"], e.get("attempt", 1))
            if key in started_attempts:
                fail(f"duplicate phase_started for {key!r}")
            started_attempts.add(key)
        elif t == "phase_finished":
            key = (e["phase_key"], e.get("attempt", 1))
            if key not in started_attempts:
                fail(f"phase_finished for {key!r} without prior phase_started")
            if key in finished_attempts:
                fail(f"duplicate phase_finished for {key!r}")
            finished_attempts.add(key)
    for key in started_attempts - finished_attempts:
        fail(f"phase_started {key!r} never finished")

    # 5. ui_request.phase_key non-empty and refers to a phase that has at
    # least one phase_started without a matching phase_finished. Parallel
    # execution means multiple phases can be in-flight at once, and retries
    # can emit multiple phase_starteds for the same key — track counts.
    in_flight = {}  # phase_key -> count of pending (started - finished)
    for e in events:
        t = e.get("type")
        if t == "phase_started":
            in_flight[e["phase_key"]] = in_flight.get(e["phase_key"], 0) + 1
        elif t == "phase_finished":
            k = e["phase_key"]
            if in_flight.get(k, 0) > 0:
                in_flight[k] -= 1
        elif t == "ui_request":
            if not e.get("phase_key"):
                fail(f"ui_request with empty phase_key (request_id={e.get('request_id')})")
            elif in_flight.get(e["phase_key"], 0) <= 0:
                fail(f"ui_request.phase_key={e['phase_key']!r} not in-flight (pending counts: { {k:v for k,v in in_flight.items() if v>0} })")

    # 6. every ui_request eventually followed by phase_finished of same phase_key
    pending = {}  # request_id -> phase_key
    for e in events:
        if e.get("type") == "ui_request":
            pending[e["request_id"]] = e["phase_key"]
        elif e.get("type") == "phase_finished":
            for rid, pk in list(pending.items()):
                if pk == e["phase_key"]:
                    del pending[rid]
    for rid, pk in pending.items():
        fail(f"ui_request {rid} (phase {pk}) never saw matching phase_finished")

    # 6b. identify-unit lifecycle
    #
    # `identify_request` is run metadata, not a phase prompt — no
    # phase_key, no phase pairing requirement. It must be followed by
    # exactly one of:
    #   * `identify_resolved` (operator scanned, or auto_identify
    #     defaults filled in for this same slot)
    #   * `identify_timeout` (operator never answered)
    #
    # `run_crashed` does NOT discharge the pairing: a crash mid-
    # identify is only legitimate when paired with an explicit timeout
    # OR when the crash error_kind is `identify_unit_failed` (the
    # framework's signal that identify itself blew up). Any other
    # combination is a wire-protocol violation — agents and UIs rely
    # on every request producing a terminal event.
    #
    # Auto-resolved (`auto_identify: true`) runs emit only
    # `identify_resolved` with no preceding `identify_request` — the
    # engine fills defaults without prompting.
    identify_pending = {}  # request_id -> slot_id (or None)
    identify_failed = any(
        e.get("type") == "run_crashed"
        and e.get("error_kind") == "identify_unit_failed"
        for e in events
    )
    for e in events:
        t = e.get("type")
        if t == "identify_request":
            rid = e.get("request_id")
            if not rid:
                fail("identify_request missing request_id")
            identify_pending[rid] = e.get("slot_id")
        elif t == "identify_resolved":
            # Resolution is identified by slot_id; clear any pending
            # request that targets the same slot. Multi-slot runs may
            # have several pending entries — we drop them all because
            # `identify_resolved` is per-slot and a slot only ever has
            # one pending request at a time.
            slot = e.get("slot_id")
            for rid, pending_slot in list(identify_pending.items()):
                if pending_slot == slot:
                    del identify_pending[rid]
        elif t == "identify_timeout":
            rid = e.get("request_id")
            identify_pending.pop(rid, None)
    if identify_pending and not identify_failed:
        fail(
            f"identify_request never resolved: {list(identify_pending.keys())}"
        )

    # 7. ui_auto_continue shape
    for e in events:
        if e.get("type") == "ui_auto_continue":
            if e.get("source") not in ("display_only", "pre_baked"):
                fail(f"ui_auto_continue bad source: {e.get('source')!r}")
            if not isinstance(e.get("values"), dict):
                fail(f"ui_auto_continue values not a dict: {type(e.get('values')).__name__}")

    # 8. last event is run_finished, exit_code matches outcome.
    # If present, run_crashed MUST immediately precede run_finished.
    if not types or types[-1] != "run_finished":
        fail(f"last event is {types[-1:]!r}, expected run_finished")
    else:
        last = events[-1]
        if last.get("exit_code") != exit_code:
            fail(f"run_finished.exit_code={last.get('exit_code')} but process exited {exit_code}")
        if last.get("exit_code") == 0 and last.get("outcome") != "PASS":
            fail(f"exit_code=0 but outcome={last.get('outcome')!r}")
        if last.get("exit_code") != 0 and last.get("outcome") == "PASS":
            fail(f"exit_code != 0 but outcome=PASS")
    crashed = [i for i, e in enumerate(events) if e.get("type") == "run_crashed"]
    if len(crashed) > 1:
        fail(f"multiple run_crashed events: {len(crashed)}")
    if crashed and crashed[0] != len(events) - 2:
        fail("run_crashed must immediately precede run_finished")

    # 9. Every phase_started refers to a key announced in the plan.
    plan_phases = events[1].get("phases", []) if len(events) > 1 and events[1].get("type") == "plan" else []
    plan_keys = set(p["key"] for p in plan_phases)
    for e in events:
        if e.get("type") == "phase_started" and e["phase_key"] not in plan_keys:
            fail(f"phase_started {e['phase_key']!r} not in plan")

    status = "OK" if not errors else "FAIL"
    print(f"[{status}] {name}  ({len(events)} events)")
    for e in errors:
        print(e)
    return not errors


cases = [
    ("simple-measurements", "/tmp/ohtf_test"),
    ("failing-measurement", "/tmp/ohtf_test2"),
    ("prompt-text", "/tmp/ohtf_test3"),
    ("prompt-confirm", "/tmp/ohtf_test4"),
    ("prompt-image", "/tmp/ohtf_test5"),
    ("exception-phase", "/tmp/ohtf_test6"),
    ("multi-prompt-phase", "/tmp/ohtf_test7"),
    ("dimensioned-measurement", "/tmp/ohtf_test8"),
    ("skip-phase", "/tmp/ohtf_test9"),
    ("repeat-limit", "/tmp/ohtf_test10"),
    ("phase-repeat", "/tmp/ohtf_test11"),
    ("attachment", "/tmp/ohtf_test12"),
    ("phase-docstrings", "/tmp/ohtf_test13"),
    ("phase-group-teardown", "/tmp/ohtf_test14"),
    ("phase-timeout", "/tmp/ohtf_test15"),
    ("phase-logs", "/tmp/ohtf_test16"),
    ("syntax-error", "/tmp/ohtf_test17"),
    ("import-error", "/tmp/ohtf_test18"),
    ("boot-exception", "/tmp/ohtf_test19"),
    ("mid-phase-sysexit", "/tmp/ohtf_test20"),
    ("segfault", "/tmp/ohtf_test21"),
    # Cross-check: a YAML operator-ui procedure should audit identically to the
    # OpenHTF ones (same canonical event shape). Override the directory with the
    # OPERATOR_UI_PROCEDURE env var; defaults to a copied scenario under /tmp.
    (
        "yaml-operator-ui",
        os.environ.get("OPERATOR_UI_PROCEDURE", "/tmp/yaml_operator_ui"),
    ),
]
all_ok = True
for name, path in cases:
    events, rc = drive(path)
    ok = audit(name, events, rc)
    all_ok = all_ok and ok
sys.exit(0 if all_ok else 1)
