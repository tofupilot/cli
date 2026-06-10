#!/usr/bin/env python3
"""Stress-test the CLI agent protocol across every component type and edge case.

Uses the YAML demo-operator-ui procedure as the test bed (covers all 11
YAML component types). Validates:
  - Happy path per component type (correct value accepted)
  - Edge cases (empty array, boolean as string, number as string, etc.)
  - Error paths per type (wrong type, out-of-enum, non-numeric)
  - Protocol-level errors (malformed stdin, unknown request_id, duplicate response)
  - Concurrency: rapid response submission, partial prebake, timeout + recovery
  - Large payloads, unicode, empty strings
"""
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from collections import defaultdict

# Usage: stress_test.py <cli-binary> <procedure-dir>
CLI = sys.argv[1]
PROC_YAML = sys.argv[2] if len(sys.argv) > 2 else "./procedure"

RESULTS = []

def record(name, ok, detail=""):
    RESULTS.append((name, ok, detail))
    status = "OK" if ok else "FAIL"
    print(f"[{status}] {name}{': ' + detail if detail else ''}")


def run(args, timeout=30, ui_values=None, stdin_feed=None, responses=None):
    """Run the CLI. Returns (events, returncode, errors_by_type_count)."""
    ui_values_path = None
    cmd = [CLI, "run", PROC_YAML, "--json", "--ui-timeout", "10"]
    if ui_values is not None:
        ui_values_path = tempfile.NamedTemporaryFile(
            mode="w", suffix=".json", delete=False).name
        with open(ui_values_path, "w") as f:
            json.dump(ui_values, f)
        cmd += ["--ui-values", ui_values_path]
    cmd += args

    proc = subprocess.Popen(
        cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, text=True, bufsize=1,
    )
    events = []
    err_counts = defaultdict(int)
    try:
        if stdin_feed is not None:
            for line in stdin_feed:
                try:
                    proc.stdin.write(line + "\n")
                    proc.stdin.flush()
                except Exception:
                    break
        for line in proc.stdout:
            line = line.rstrip("\n")
            if not line:
                continue
            try:
                evt = json.loads(line)
            except Exception:
                continue
            events.append(evt)
            if evt.get("type") == "ui_error":
                err_counts[evt.get("reason", "?")] += 1
            if evt.get("type") == "ui_request" and responses is not None:
                resp = responses(evt)
                if resp is not None:
                    proc.stdin.write(json.dumps(resp) + "\n"); proc.stdin.flush()
            if evt.get("type") == "run_finished":
                break
    finally:
        try:
            proc.wait(timeout=timeout)
        except Exception:
            proc.kill()
        if ui_values_path:
            os.unlink(ui_values_path)
    return events, proc.returncode, dict(err_counts)


def good_response(evt):
    """Answer every input with a valid value per its component type."""
    values = {}
    for c in evt.get("components", []):
        if not c.get("is_input"):
            continue
        t = c["type"]
        opts = c.get("options") or []
        if t == "switch":
            values[c["key"]] = True
        elif t in ("radio", "select") and opts:
            values[c["key"]] = opts[0]["value"]
        elif t in ("multiselect", "checklist") and opts:
            values[c["key"]] = [opts[0]["value"]]
        elif t in ("number_input", "slider"):
            values[c["key"]] = 42
        elif t == "text_input":
            values[c["key"]] = "hello"
        elif t == "textarea":
            values[c["key"]] = "a\nmultiline\nnote"
        else:
            values[c["key"]] = "ok"
    return {"type": "ui_response", "request_id": evt["request_id"], "values": values}


# ---------------------------------------------------------------------------
# 1. Happy path with every component type answered correctly
# ---------------------------------------------------------------------------
events, rc, _ = run([], responses=good_response)
ok = rc == 0 and events and events[-1].get("type") == "run_finished" and events[-1].get("outcome") == "PASS"
record("happy-path-all-types", ok, f"exit={rc} events={len(events)}")


# ---------------------------------------------------------------------------
# 2. Each component type answered with wrong shape → ui_error invalid_value
# ---------------------------------------------------------------------------
def bad_type_response(evt):
    """Reply with deliberately wrong types to trigger invalid_value errors."""
    vals = {}
    for c in evt.get("components", []):
        if not c.get("is_input"):
            continue
        t = c["type"]
        if t == "switch":
            vals[c["key"]] = "yes"  # not true/false
        elif t == "number_input":
            vals[c["key"]] = "abc"  # not numeric
        elif t == "slider":
            vals[c["key"]] = [1, 2]  # array for scalar
        elif t in ("radio", "select"):
            vals[c["key"]] = "NOT-A-VALID-OPTION"
        elif t in ("multiselect", "checklist"):
            vals[c["key"]] = ["bad-option"]
        else:
            vals[c["key"]] = {"oops": "object"}
    # If all are bad, retry with good values so the phase eventually continues
    return {"type": "ui_response", "request_id": evt["request_id"], "values": vals}

retried = {}
def bad_then_good(evt):
    rid = evt["request_id"]
    if rid not in retried:
        retried[rid] = 1
        return bad_type_response(evt)
    return good_response(evt)

events, rc, errs = run([], responses=bad_then_good, timeout=60)
ok = errs.get("invalid_value", 0) >= 1
record("invalid-value-per-type", ok, f"invalid_value events={errs.get('invalid_value', 0)}")


# ---------------------------------------------------------------------------
# 3. Missing required: answer only non-required fields
# ---------------------------------------------------------------------------
missing_tried = set()
def skip_required(evt):
    rid = evt["request_id"]
    if rid not in missing_tried:
        missing_tried.add(rid)
        vals = {}
        for c in evt.get("components", []):
            if c.get("is_input") and not c.get("required"):
                # answer one non-required field
                vals[c["key"]] = good_response(evt)["values"].get(c["key"])
                break
        return {"type": "ui_response", "request_id": rid, "values": vals}
    return good_response(evt)

events, rc, errs = run([], responses=skip_required, timeout=60)
ok = errs.get("missing_required", 0) >= 1
record("missing-required", ok, f"missing_required events={errs.get('missing_required', 0)}")


# ---------------------------------------------------------------------------
# 4. Unknown field
# ---------------------------------------------------------------------------
unknown_tried = set()
def unknown_field(evt):
    rid = evt["request_id"]
    if rid not in unknown_tried:
        unknown_tried.add(rid)
        return {"type": "ui_response", "request_id": rid, "values": {"__does_not_exist__": 1}}
    return good_response(evt)

events, rc, errs = run([], responses=unknown_field, timeout=60)
ok = errs.get("unknown_field", 0) >= 1
record("unknown-field", ok, f"unknown_field events={errs.get('unknown_field', 0)}")


# ---------------------------------------------------------------------------
# 5. Malformed JSON on stdin
# ---------------------------------------------------------------------------
# Can't mix with normal responses; just send garbage + valid. Cancel via timeout.
events, rc, errs = run([], stdin_feed=["not-json", '{"oops":true}', 'completely broken'], timeout=30)
ok = errs.get("parse_error", 0) >= 1
record("parse-error", ok, f"parse_error events={errs.get('parse_error', 0)}")


# ---------------------------------------------------------------------------
# 6. Unknown request_id
# ---------------------------------------------------------------------------
events, rc, errs = run([], stdin_feed=[
    json.dumps({"type": "ui_response", "request_id": "ghost", "values": {}}),
], timeout=30)
ok = errs.get("unknown_request", 0) >= 1
record("unknown-request-id", ok, f"unknown_request events={errs.get('unknown_request', 0)}")


# ---------------------------------------------------------------------------
# 7. Duplicate response for same request_id
# ---------------------------------------------------------------------------
sent_twice = set()
def duplicate_send(evt):
    rid = evt["request_id"]
    if rid in sent_twice:
        return None  # already sent, don't re-send
    resp = good_response(evt)
    # Send twice
    sent_twice.add(rid)
    return resp  # first send goes via normal path

# Custom drive to send duplicates:
def drive_duplicate():
    proc = subprocess.Popen(
        [CLI, "run", PROC_YAML, "--json", "--ui-timeout", "10"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, text=True, bufsize=1)
    events = []
    errs = defaultdict(int)
    for line in proc.stdout:
        line = line.rstrip("\n")
        if not line:
            continue
        try:
            e = json.loads(line)
        except Exception:
            continue
        events.append(e)
        if e.get("type") == "ui_error":
            errs[e.get("reason", "?")] += 1
        if e.get("type") == "ui_request":
            resp = good_response(e)
            proc.stdin.write(json.dumps(resp) + "\n"); proc.stdin.flush()
            # now send again — should error with unknown_request (the first one resolved it)
            proc.stdin.write(json.dumps(resp) + "\n"); proc.stdin.flush()
        if e.get("type") == "run_finished":
            break
    proc.wait(timeout=30)
    return events, proc.returncode, dict(errs)

events, rc, errs = drive_duplicate()
ok = errs.get("unknown_request", 0) >= 1
record("duplicate-response", ok, f"unknown_request (for 2nd send)={errs.get('unknown_request', 0)}")


# ---------------------------------------------------------------------------
# 8. Large textarea payload (10KB)
# ---------------------------------------------------------------------------
def large_response(evt):
    vals = good_response(evt)["values"]
    # Inflate any textarea value
    for c in evt.get("components", []):
        if c.get("type") == "textarea" and c.get("is_input"):
            vals[c["key"]] = "x" * 10240
    return {"type": "ui_response", "request_id": evt["request_id"], "values": vals}

events, rc, errs = run([], responses=large_response, timeout=60)
ok = rc == 0 and not errs
record("large-textarea-10kb", ok, f"exit={rc} errors={errs}")


# ---------------------------------------------------------------------------
# 9. Unicode response (emoji + CJK + combining marks)
# ---------------------------------------------------------------------------
def unicode_response(evt):
    vals = good_response(evt)["values"]
    for c in evt.get("components", []):
        if c.get("type") in ("text_input", "textarea") and c.get("is_input"):
            vals[c["key"]] = "🎉 テスト ñéè ☕"
    return {"type": "ui_response", "request_id": evt["request_id"], "values": vals}

events, rc, errs = run([], responses=unicode_response, timeout=60)
ok = rc == 0 and not errs
record("unicode-response", ok, f"exit={rc} errors={errs}")


# ---------------------------------------------------------------------------
# 10. Empty string for text_input (non-required)
# ---------------------------------------------------------------------------
def empty_string(evt):
    vals = {}
    for c in evt.get("components", []):
        if not c.get("is_input"):
            continue
        t = c["type"]
        if t == "text_input" and not c.get("required"):
            vals[c["key"]] = ""
        else:
            # Fall back to valid values for required
            vals.update({k: v for k, v in good_response(evt)["values"].items()
                         if k == c["key"]})
    return {"type": "ui_response", "request_id": evt["request_id"], "values": vals}

events, rc, errs = run([], responses=empty_string, timeout=60)
ok = rc == 0
record("empty-string-optional", ok, f"exit={rc} errors={errs}")


# ---------------------------------------------------------------------------
# 11. Multiselect with multiple valid values (not just first)
# ---------------------------------------------------------------------------
def multi_values(evt):
    vals = good_response(evt)["values"]
    for c in evt.get("components", []):
        if c.get("type") in ("multiselect", "checklist") and c.get("is_input"):
            opts = c.get("options") or []
            if len(opts) >= 2:
                vals[c["key"]] = [o["value"] for o in opts[:2]]
    return {"type": "ui_response", "request_id": evt["request_id"], "values": vals}

events, rc, errs = run([], responses=multi_values, timeout=60)
ok = rc == 0 and not errs
record("multiselect-multiple", ok, f"exit={rc} errors={errs}")


# ---------------------------------------------------------------------------
# 12. Multiselect with empty array (no selections)
# ---------------------------------------------------------------------------
def empty_array(evt):
    vals = good_response(evt)["values"]
    for c in evt.get("components", []):
        if c.get("type") in ("multiselect", "checklist") \
                and c.get("is_input") and not c.get("required"):
            vals[c["key"]] = []
    return {"type": "ui_response", "request_id": evt["request_id"], "values": vals}

events, rc, errs = run([], responses=empty_array, timeout=60)
ok = rc == 0
record("empty-array-optional-multi", ok, f"exit={rc} errors={errs}")


# ---------------------------------------------------------------------------
# 13. Numeric boundary values
# ---------------------------------------------------------------------------
def numeric_edges(evt):
    vals = good_response(evt)["values"]
    for c in evt.get("components", []):
        if not c.get("is_input"):
            continue
        t = c["type"]
        if t == "number_input":
            vals[c["key"]] = -99999.9999
        elif t == "slider":
            vals[c["key"]] = c.get("min", 0)  # minimum boundary
    return {"type": "ui_response", "request_id": evt["request_id"], "values": vals}

events, rc, errs = run([], responses=numeric_edges, timeout=60)
ok = rc == 0
record("numeric-boundaries", ok, f"exit={rc} errors={errs}")


# ---------------------------------------------------------------------------
# 14. Numeric as string (should coerce)
# ---------------------------------------------------------------------------
def numeric_as_string(evt):
    vals = good_response(evt)["values"]
    for c in evt.get("components", []):
        if not c.get("is_input"):
            continue
        t = c["type"]
        if t == "number_input":
            vals[c["key"]] = "3.14"
        elif t == "slider":
            vals[c["key"]] = "50"
    return {"type": "ui_response", "request_id": evt["request_id"], "values": vals}

events, rc, errs = run([], responses=numeric_as_string, timeout=60)
ok = rc == 0
record("numeric-as-string-coercion", ok, f"exit={rc} errors={errs}")


# ---------------------------------------------------------------------------
# 15. Switch as string ("true"/"false")
# ---------------------------------------------------------------------------
def switch_as_string(evt):
    vals = good_response(evt)["values"]
    for c in evt.get("components", []):
        if c.get("type") == "switch" and c.get("is_input"):
            vals[c["key"]] = "true"
    return {"type": "ui_response", "request_id": evt["request_id"], "values": vals}

events, rc, errs = run([], responses=switch_as_string, timeout=60)
ok = rc == 0
record("switch-as-string", ok, f"exit={rc} errors={errs}")


# ---------------------------------------------------------------------------
# 16. Full pre-baked values for all phases (agent never needs to respond)
# ---------------------------------------------------------------------------
# Run with a driver that *records* ui_requests but never answers — relies on
# prebaked map.
prebaked = {
    "basic_input_components": {"number_input": 1},
    "selection_components": {"radio_demo": "a"},
    "slider_component": {},
    "progress_monitoring": {},
    "image_selection_components": {"board_variant": "rev_a"},
}
events, rc, errs = run([], ui_values=prebaked, timeout=60)
prebaked_auto = sum(1 for e in events if e.get("type") == "ui_auto_continue" and e.get("source") == "pre_baked")
ok = rc == 0 and prebaked_auto >= 3
record("prebake-all", ok, f"pre_baked auto-continues={prebaked_auto} exit={rc}")


# ---------------------------------------------------------------------------
# 17. Partial pre-bake — agent answers the rest
# ---------------------------------------------------------------------------
partial = {"basic_input_components": {"number_input": 7}}
events, rc, errs = run([], ui_values=partial, responses=good_response, timeout=60)
ok = rc == 0
record("prebake-partial+agent", ok, f"exit={rc}")


# ---------------------------------------------------------------------------
# 18. UI timeout triggers, run fails cleanly (no agent answers)
# ---------------------------------------------------------------------------
# Drive with no responses at all; --ui-timeout 3.
proc = subprocess.Popen(
    [CLI, "run", PROC_YAML, "--json", "--ui-timeout", "3"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    text=True, bufsize=1)
timeouts = 0
events = []
for line in proc.stdout:
    line = line.rstrip("\n")
    if not line: continue
    try:
        e = json.loads(line)
    except Exception:
        continue
    events.append(e)
    if e.get("type") == "ui_timeout":
        timeouts += 1
    if e.get("type") == "run_finished":
        break
proc.wait(timeout=30)
ok = timeouts >= 1 and proc.returncode != 0
record("ui-timeout-fires", ok, f"timeouts={timeouts} exit={proc.returncode}")


# ---------------------------------------------------------------------------
# 19. Rapid fire responses (pipeline many)
# ---------------------------------------------------------------------------
# Send a bunch of ghost responses immediately, then real ones; protocol must
# not lose real responses.
def rapid_fire():
    proc = subprocess.Popen(
        [CLI, "run", PROC_YAML, "--json", "--ui-timeout", "10"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, text=True, bufsize=1)
    # Pre-seed the channel with 50 junk ui_response messages
    for i in range(50):
        proc.stdin.write(json.dumps({"type": "ui_response", "request_id": f"junk-{i}", "values": {}}) + "\n")
    proc.stdin.flush()
    events = []
    errs = defaultdict(int)
    for line in proc.stdout:
        line = line.rstrip("\n")
        if not line: continue
        try:
            e = json.loads(line)
        except Exception:
            continue
        events.append(e)
        if e.get("type") == "ui_error":
            errs[e.get("reason", "?")] += 1
        if e.get("type") == "ui_request":
            proc.stdin.write(json.dumps(good_response(e)) + "\n")
            proc.stdin.flush()
        if e.get("type") == "run_finished":
            break
    proc.wait(timeout=60)
    return events, proc.returncode, dict(errs)

events, rc, errs = rapid_fire()
ok = rc == 0 and errs.get("unknown_request", 0) == 50
record("rapid-junk-then-real", ok, f"junk unknown_request={errs.get('unknown_request', 0)} exit={rc}")


# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
print()
ok = sum(1 for _, o, _ in RESULTS if o)
total = len(RESULTS)
print(f"{ok}/{total} passed")
sys.exit(0 if ok == total else 1)
