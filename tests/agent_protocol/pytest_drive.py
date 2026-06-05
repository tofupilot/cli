#!/usr/bin/env python3
"""Drive every pytest_test* scenario through the CLI in agent-proto mode
and report which pass and which fail.

This is the pytest analogue of `audit_protocol.py` for OpenHTF: it runs
each scenario, applies a per-scenario expectation set (phase outcome
distribution, presence of identify-resolved, etc.), and prints a
PASS/FAIL summary at the end.
"""
from __future__ import annotations

import json
import os
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
        # Identify-unit prompts use field keys like serial_number /
        # part_number — return scenario-meaningful values.
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
            # Hard-cap on runaway scenarios so the suite never hangs.
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
    counts = {"PASS": 0, "FAIL": 0, "SKIP": 0, "ERROR": 0, "XFAIL": 0}
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
        "name": "pytest_test1: simple_pass",
        "dir": "pytest_test1",
        "expect_outcome": "PASS",
        "expect_phase_count": 3,
        "expect_pass": 3,
    },
    {
        "name": "pytest_test2: simple_fail",
        "dir": "pytest_test2",
        "expect_outcome": "FAIL",
        "expect_phase_count": 1,
        "expect_fail": 1,
    },
    {
        "name": "pytest_test3: mixed pass/fail/skip/xfail",
        "dir": "pytest_test3",
        "expect_outcome": "FAIL",
        "expect_phase_count": 4,
        "expect_xfail": 1,
    },
    {
        "name": "pytest_test4: parametrize",
        "dir": "pytest_test4",
        # 3 equal cases (PASS) + 2 under_100 cases (PASS) including 1 xfail (SKIP)
        "expect_phase_count": 5,
    },
    {
        "name": "pytest_test5: fixtures + autouse + teardown",
        "dir": "pytest_test5",
        "expect_outcome": "PASS",
        "expect_phase_count": 2,
    },
    {
        "name": "pytest_test6: print + logging + pytest.fail",
        "dir": "pytest_test6",
        "expect_outcome": "FAIL",
        "expect_phase_count": 3,
    },
    {
        "name": "pytest_test7: conftest shared fixture",
        "dir": "pytest_test7",
        "expect_outcome": "PASS",
        "expect_phase_count": 2,
    },
    {
        "name": "pytest_test8: AST numeric range",
        "dir": "pytest_test8",
        "expect_outcome": "PASS",
        "expect_phase_count": 1,
        "expect_measurements": True,
        "expect_measurement": {
            "name": "v",
            "min_value": 4.8,
            "max_value": 5.2,
            "unit": "V",
        },
    },
    {
        "name": "pytest_test9: AST single bound + pytest.approx",
        "dir": "pytest_test9",
        "expect_outcome": "PASS",
        "expect_phase_count": 2,
        "expect_measurements": True,
        "expect_measurement_count": 2,
    },
    {
        "name": "pytest_test10: identify_unit handshake",
        "dir": "pytest_test10",
        "expect_outcome": "PASS",
        "expect_phase_count": 1,
        "expect_identify_request": True,
    },
    {
        "name": "pytest_test11: collection error → ERROR",
        "dir": "pytest_test11",
        "expect_outcome_in": ("ERROR", "FAIL"),
    },
    {
        "name": "pytest_test12: slow test (live phase events)",
        "dir": "pytest_test12",
        "expect_outcome": "PASS",
        "expect_phase_count": 3,
        "expect_live_streaming": True,
    },
    {
        "name": "pytest_test13: conftest-only (no test_*.py) → ERROR",
        "dir": "pytest_test13",
        "expect_outcome": "ERROR",
        "expect_phase_count": 0,
    },
    {
        "name": "pytest_test14: AST string equality + membership",
        "dir": "pytest_test14",
        "expect_outcome": "PASS",
        "expect_phase_count": 2,
        "expect_measurements": True,
        "expect_measurement_count": 2,
    },
    {
        "name": "pytest_test15: multi-assert test emits one measurement per recognized assert",
        "dir": "pytest_test15",
        "expect_outcome": "PASS",
        "expect_phase_count": 1,
        "expect_measurements": True,
        "expect_measurement_count": 2,
    },
    {
        "name": "pytest_test16: variable-bound assert rejects measurement",
        "dir": "pytest_test16",
        "expect_outcome": "PASS",
        "expect_phase_count": 1,
        "expect_no_measurements": True,
    },
    {
        "name": "pytest_test17: failing range still emits measurement",
        "dir": "pytest_test17",
        "expect_outcome": "FAIL",
        "expect_phase_count": 1,
        "expect_fail": 1,
        "expect_measurements": True,
        "expect_measurement_count": 1,
        "expect_measurement": {
            "name": "voltage",
            "unit": "V",
            "value": 5.5,
        },
    },
    {
        "name": "pytest_test18: same-identifier asserts stack as validators on one measurement",
        "dir": "pytest_test18",
        "expect_outcome": "PASS",
        "expect_phase_count": 1,
        "expect_measurements": True,
        "expect_measurement_count": 1,
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
        issues.append(
            f"outcome={outcome!r}, expected {expected_outcome!r}"
        )
    expected_set = scenario.get("expect_outcome_in")
    if expected_set and outcome not in expected_set:
        issues.append(
            f"outcome={outcome!r}, expected one of {expected_set!r}"
        )
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
    if scenario.get("expect_xfail") is not None:
        if counts["XFAIL"] != scenario["expect_xfail"]:
            issues.append(
                f"XFAIL phase count {counts['XFAIL']} != expected {scenario['expect_xfail']}"
            )
    if scenario.get("expect_identify_request") and not has(events, "identify_request"):
        issues.append("expected identify_request, none observed")
    if scenario.get("expect_measurements"):
        if not has(events, "measurement_recorded"):
            issues.append("expected measurement_recorded events")
    if scenario.get("expect_no_measurements"):
        if has(events, "measurement_recorded"):
            issues.append("expected no measurement_recorded events")
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
    if scenario.get("expect_live_streaming"):
        # Verify phase_started for the slow test arrives before the
        # phase_finished — i.e. events stream live, not all batched at
        # the end. The simplest check: total events span at least 1.5s
        # of wall clock between the first phase_started and the last
        # phase_finished. We'd need timestamps to verify properly; for
        # this audit, ensure that not all phase_started events arrive
        # before all phase_finished events.
        phase_events = [e for e in events
                        if e.get("type") in ("phase_started", "phase_finished")]
        if phase_events:
            # Indices of last phase_started vs first phase_finished.
            last_started = max(
                (i for i, e in enumerate(phase_events) if e["type"] == "phase_started"),
                default=-1,
            )
            first_finished = min(
                (i for i, e in enumerate(phase_events) if e["type"] == "phase_finished"),
                default=len(phase_events),
            )
            if last_started < first_finished:
                # All starteds bunched up before all finisheds — that's
                # NOT live streaming.
                issues.append(
                    "phase_started events all batched before any phase_finished — live streaming may have regressed"
                )
    return issues


def main():
    print(f"=== pytest connector battery — {len(SCENARIOS)} scenarios ===\n")
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
