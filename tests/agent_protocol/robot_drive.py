#!/usr/bin/env python3
"""Drive every robot_test* scenario through the CLI in agent-proto mode
and report which pass and which fail.

The robot analogue of `pytest_drive.py`. Each scenario ships a
`pyproject.toml` (declares the `robotframework` dependency and
optionally `[tool.tofupilot]` defaults) plus one or more `.robot`
files. The shared `/tmp/ohtf_test/.venv` is expected to have
`robotframework` installed; symlink each scenario's `.venv` to that
shared venv (see `apps/cli/tests/agent_protocol/README.md`).
"""
from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path

CLI = sys.argv[1] if len(sys.argv) >= 2 else None
if not CLI or not Path(CLI).exists():
    print(f"usage: {sys.argv[0]} <path-to-tofupilot-cli>", file=sys.stderr)
    sys.exit(2)

SCENARIO_ROOT = Path(__file__).parent / "scenarios"


def answer(component):
    t = component.get("type")
    opts = component.get("options") or []
    if t == "switch":
        return True
    if t in ("radio", "select", "image_choice") and opts:
        return opts[0]["value"]
    if t in ("multiselect", "checklist", "image_checklist") and opts:
        return [opts[0]["value"]]
    if t in ("number_input", "slider"):
        return 42
    if t == "text_input":
        if component.get("key") == "serial_number":
            return "SN-OPERATOR"
        if component.get("key") == "part_number":
            return "PCB-OPERATOR"
        return "value"
    if t == "textarea":
        return "note"
    return "ok"


def drive(scenario_dir):
    proc = subprocess.Popen(
        [CLI, "run", str(scenario_dir), "--json", "--ui-timeout", "8"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    events = []
    started = time.monotonic()
    try:
        for line in proc.stdout:
            line = line.rstrip("\n")
            if not line:
                continue
            try:
                evt = json.loads(line)
            except Exception:
                continue
            events.append(evt)
            if evt.get("type") in ("ui_request", "identify_request"):
                values = {
                    c["key"]: answer(c)
                    for c in evt.get("components", [])
                    if c.get("is_input")
                }
                resp = {
                    "type": "ui_response",
                    "request_id": evt["request_id"],
                    "values": values,
                }
                proc.stdin.write(json.dumps(resp) + "\n")
                proc.stdin.flush()
            if evt.get("type") == "run_finished":
                break
            if time.monotonic() - started > 60:
                break
    finally:
        try:
            proc.wait(timeout=15)
        except Exception:
            proc.kill()
            proc.wait(timeout=5)
    return events, proc.returncode, proc.stderr.read()


def phase_outcome_counts(events):
    counts = {"PASS": 0, "FAIL": 0, "SKIP": 0, "ERROR": 0}
    for e in events:
        if e.get("type") == "phase_finished":
            counts[e.get("outcome", "")] = counts.get(e.get("outcome", ""), 0) + 1
    return counts


def has(events, type_):
    return any(e.get("type") == type_ for e in events)


def find(events, type_):
    return [e for e in events if e.get("type") == type_]


SCENARIOS = [
    {
        "name": "robot_test1: simple_pass (3 cases)",
        "dir": "robot_test1",
        "expect_outcome": "PASS",
        "expect_phase_count": 3,
        "expect_pass": 3,
    },
    {
        "name": "robot_test2: simple_fail",
        "dir": "robot_test2",
        "expect_outcome": "FAIL",
        "expect_phase_count": 1,
        "expect_fail": 1,
    },
    {
        "name": "robot_test3: identify_unit handshake",
        "dir": "robot_test3",
        "expect_outcome": "PASS",
        "expect_phase_count": 1,
        "expect_identify_request": True,
    },
    {
        "name": "robot_test4: numeric + string + boolean measurements",
        "dir": "robot_test4",
        "expect_outcome": "PASS",
        "expect_phase_count": 3,
        "expect_measurements": True,
        "expect_measurement_count": 3,
        "expect_measurement": {
            "name": "voltage",
            "unit": "V",
            "value": 5.01,
        },
    },
    {
        "name": "robot_test5: keyword reused across cases (parametrize-equivalent)",
        "dir": "robot_test5",
        "expect_outcome": "PASS",
        "expect_phase_count": 3,
        "expect_measurements": True,
        "expect_measurement_count": 3,
    },
    {
        "name": "robot_test6: empty suite (zero test cases) -> ERROR",
        "dir": "robot_test6",
        # Empty Robot suite mirrors pytest's empty-collection: wire ERROR.
        # No phases ran, so no phase_finished counts.
        "expect_outcome": "ERROR",
        "expect_phase_count": 0,
    },
    {
        "name": "robot_test7: suite setup failure -> ERROR",
        "dir": "robot_test7",
        # `Suite Setup    Fail` aborts before any test body executes.
        # Robot still fires end_test for declared tests with status=FAIL
        # but with no Measure assertion -> wire ERROR.
        "expect_outcome": "ERROR",
        "expect_phase_count": 1,
    },
    {
        "name": "robot_test8: keyword runtime exception (non-Measure) -> ERROR",
        "dir": "robot_test8",
        # An out-of-range list index raises a Python-side error that
        # surfaces as Robot status=FAIL. Without a Measure assertion
        # marker the connector wires ERROR (the FAIL/ERROR split that
        # makes the dashboard distinguish limit failures from crashes).
        "expect_outcome": "ERROR",
        "expect_phase_count": 1,
    },
]


def evaluate(scenario, events, exit_code):
    issues = []
    finished = [e for e in events if e.get("type") == "run_finished"]
    if not finished:
        issues.append("missing run_finished")
        return issues
    last = finished[-1]
    outcome = last.get("outcome")
    expected_outcome = scenario.get("expect_outcome")
    if expected_outcome and outcome != expected_outcome:
        issues.append(f"outcome={outcome!r}, expected {expected_outcome!r}")
    counts = phase_outcome_counts(events)
    expected_phase_count = scenario.get("expect_phase_count")
    if expected_phase_count is not None:
        total = sum(counts.values())
        if total != expected_phase_count:
            issues.append(
                f"phase count {total} != expected {expected_phase_count} (counts={counts})"
            )
    if scenario.get("expect_pass") is not None:
        if counts["PASS"] != scenario["expect_pass"]:
            issues.append(
                f"PASS phase count {counts['PASS']} != expected {scenario['expect_pass']}"
            )
    if scenario.get("expect_fail") is not None:
        if counts["FAIL"] != scenario["expect_fail"]:
            issues.append(
                f"FAIL phase count {counts['FAIL']} != expected {scenario['expect_fail']}"
            )
    if scenario.get("expect_identify_request") and not has(events, "identify_request"):
        issues.append("expected identify_request, none observed")
    if scenario.get("expect_measurements"):
        if not has(events, "measurement_recorded"):
            issues.append("expected measurement_recorded events")
    if scenario.get("expect_measurement_count") is not None:
        n = sum(1 for e in events if e.get("type") == "measurement_recorded")
        if n != scenario["expect_measurement_count"]:
            issues.append(
                f"measurement_recorded count {n} != expected {scenario['expect_measurement_count']}"
            )
    expected_m = scenario.get("expect_measurement")
    if expected_m:
        records = find(events, "measurement_recorded")
        target = next(
            (r for r in records if r.get("name") == expected_m["name"]),
            None,
        )
        if target is None:
            issues.append(
                f"no measurement_recorded with name={expected_m['name']!r}"
            )
        else:
            if "unit" in expected_m and target.get("unit") != expected_m["unit"]:
                issues.append(
                    f"measurement {expected_m['name']!r}: unit {target.get('unit')!r} != {expected_m['unit']!r}"
                )
            if "value" in expected_m and target.get("value") != expected_m["value"]:
                issues.append(
                    f"measurement {expected_m['name']!r}: value {target.get('value')!r} != {expected_m['value']!r}"
                )
    return issues


def main():
    print(f"=== robot connector battery — {len(SCENARIOS)} scenarios ===\n")
    results = []
    for scenario in SCENARIOS:
        scenario_dir = SCENARIO_ROOT / scenario["dir"]
        if not scenario_dir.is_dir():
            print(f"[SKIP] {scenario['name']}: directory missing")
            results.append((scenario["name"], "SKIP", []))
            continue
        events, rc, err = drive(scenario_dir)
        issues = evaluate(scenario, events, rc)
        status = "OK" if not issues else "FAIL"
        print(f"[{status}] {scenario['name']}  ({len(events)} events, exit={rc})")
        for issue in issues:
            print(f"   - {issue}")
        if status == "FAIL" and err:
            tail = err.strip().splitlines()[-3:] if err.strip() else []
            for line in tail:
                print(f"   stderr: {line}")
        results.append((scenario["name"], status, issues))

    print()
    ok = sum(1 for _, s, _ in results if s == "OK")
    fail = sum(1 for _, s, _ in results if s == "FAIL")
    skip = sum(1 for _, s, _ in results if s == "SKIP")
    print(f"=== {ok} ok / {fail} fail / {skip} skipped ===")
    sys.exit(0 if fail == 0 else 1)


if __name__ == "__main__":
    main()
