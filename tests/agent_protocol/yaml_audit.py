#!/usr/bin/env python3
"""Audit the agent protocol against YAML procedures — happy + corner cases."""
import json
import subprocess
import sys

CLI = sys.argv[1]
RESULTS = []


def answer(component):
    """Produce a valid response for any input component type."""
    t = component["type"]
    opts = component.get("options") or []
    if t == "switch":
        return True
    if t in ("radio", "select", "image_choice") and opts:
        return opts[0]["value"]
    if t in ("multiselect", "checklist", "image_checklist") and opts:
        return [opts[0]["value"]]
    if t == "number_input":
        return 1
    if t == "slider":
        lo = component.get("min", 0)
        step = component.get("step", 1)
        return lo + step  # inside range
    if t == "text_input":
        pat = component.get("pattern")
        if pat == "^SN-[0-9]+$":
            return "SN-1234"
        return "hello"
    if t == "textarea":
        return "multi\nline"
    return "ok"


def drive(path, ui_timeout=5, ui_values=None):
    cmd = [CLI, "run", path, "--json", "--ui-timeout", str(ui_timeout)]
    if ui_values is not None:
        import tempfile, os
        fp = tempfile.NamedTemporaryFile(mode="w", suffix=".json", delete=False)
        json.dump(ui_values, fp); fp.close()
        cmd += ["--ui-values", fp.name]
    proc = subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1,
    )
    events = []
    for line in proc.stdout:
        line = line.rstrip("\n")
        if not line: continue
        try:
            evt = json.loads(line)
        except Exception:
            continue
        events.append(evt)
        if evt.get("type") == "ui_request":
            vals = {}
            for c in evt.get("components", []):
                if c.get("is_input"):
                    vals[c["key"]] = answer(c)
            proc.stdin.write(json.dumps({
                "type": "ui_response", "request_id": evt["request_id"], "values": vals
            }) + "\n"); proc.stdin.flush()
        if evt.get("type") == "run_finished":
            break
    try:
        proc.wait(timeout=30)
    except Exception:
        proc.kill()
    return events, proc.returncode


def check(name, events, rc, *, expect_pass=None, expect_crashed=False,
          expect_phase_error=False, expect_plan_phases=None):
    errs = []
    types = [e.get("type") for e in events]

    # Strict: starts with run_started, ends with run_finished, unique.
    if not events or types[0] != "run_started":
        errs.append(f"first event {types[:1]} != run_started")
    if not events or types[-1] != "run_finished":
        errs.append(f"last event {types[-1:]} != run_finished")

    # Paired phase_started/phase_finished per (phase_key, attempt, slot_id).
    # Multi-slot runs legitimately emit the same (key, attempt) in each slot,
    # distinguished only by slot_id.
    def pair_key(e):
        return (e["phase_key"], e.get("attempt", 1), e.get("slot_id"))
    seen_started = set()
    seen_finished = set()
    for e in events:
        if e.get("type") == "phase_started":
            key = pair_key(e)
            if key in seen_started: errs.append(f"dup phase_started {key}")
            seen_started.add(key)
        elif e.get("type") == "phase_finished":
            key = pair_key(e)
            if key not in seen_started:
                errs.append(f"phase_finished without started {key}")
            if key in seen_finished: errs.append(f"dup phase_finished {key}")
            seen_finished.add(key)
    missing_end = seen_started - seen_finished
    if missing_end: errs.append(f"phase_started without finished {missing_end}")

    # run_finished must be after all phase_finished events
    last_phase_fin_idx = -1
    for i, e in enumerate(events):
        if e.get("type") == "phase_finished":
            last_phase_fin_idx = i
    run_fin_idx = len(events) - 1
    if last_phase_fin_idx > run_fin_idx:
        errs.append("run_finished emitted before phase_finished")

    # Outcome vs exit code coherence
    last = events[-1] if events else {}
    if last.get("exit_code") != rc:
        errs.append(f"run_finished.exit_code={last.get('exit_code')} != process rc={rc}")

    # Expectations
    if expect_pass is True and (rc != 0 or last.get("outcome") != "PASS"):
        errs.append(f"expected PASS exit 0, got outcome={last.get('outcome')} exit={rc}")
    if expect_pass is False and (rc == 0 or last.get("outcome") == "PASS"):
        errs.append(f"expected FAIL, got outcome={last.get('outcome')} exit={rc}")
    if expect_crashed and not any(e.get("type") == "run_crashed" for e in events):
        errs.append("expected run_crashed event, none found")
    if expect_phase_error:
        errored = [e for e in events if e.get("type") == "phase_finished"
                   and e.get("outcome") == "ERROR" and e.get("error")]
        if not errored:
            errs.append("expected phase_finished ERROR with error field")
    if expect_plan_phases is not None:
        plans = [e for e in events if e.get("type") == "plan"]
        if len(plans) != 1:
            errs.append(f"expected 1 plan event, got {len(plans)}")
        elif [p["key"] for p in plans[0].get("phases", [])] != expect_plan_phases:
            errs.append(f"plan mismatch: got {[p['key'] for p in plans[0]['phases']]}, expected {expect_plan_phases}")

    ok = not errs
    RESULTS.append((name, ok, errs))
    status = "OK" if ok else "FAIL"
    print(f"[{status}] {name}  ({len(events)} events)")
    for e in errs:
        print(f"  - {e}")


# ------------------------------------------------------------ tests --------
events, rc = drive("/tmp/yaml_test1")
check("Y1 happy single phase", events, rc, expect_pass=True, expect_plan_phases=["hello"])

events, rc = drive("/tmp/yaml_test2")
check("Y2 measurement pass", events, rc, expect_pass=True)

events, rc = drive("/tmp/yaml_test3")
check("Y3 measurement out of range", events, rc, expect_pass=False)

events, rc = drive("/tmp/yaml_test4")
check("Y4 phase exception", events, rc, expect_pass=False, expect_phase_error=True)

events, rc = drive("/tmp/yaml_test5")
check("Y5 dependency order", events, rc, expect_pass=True,
      expect_plan_phases=["a", "b"])
# Verify a runs before b
idx_a_end = next(i for i, e in enumerate(events)
                 if e.get("type") == "phase_finished" and e.get("phase_key") == "a")
idx_b_start = next(i for i, e in enumerate(events)
                   if e.get("type") == "phase_started" and e.get("phase_key") == "b")
if idx_b_start < idx_a_end:
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + ["b started before a finished"])
    print(f"  - b started before a finished")

events, rc = drive("/tmp/yaml_test6")
check("Y6 parallel workers", events, rc, expect_pass=True,
      expect_plan_phases=["init", "v", "i", "t"])

events, rc = drive("/tmp/yaml_test7")
check("Y7 missing phase module", events, rc, expect_pass=False,
      expect_phase_error=True)

events, rc = drive("/tmp/yaml_test8")
check("Y8 YAML syntax error", events, rc, expect_pass=False,
      expect_crashed=True)

events, rc = drive("/tmp/yaml_test9")
check("Y9 empty main", events, rc, expect_pass=False, expect_crashed=True)

events, rc = drive("/tmp/yaml_test10")
check("Y10 python syntax error in phase", events, rc, expect_pass=False,
      expect_phase_error=True)

events, rc = drive("/tmp/yaml_test11")
check("Y11 python import error in phase", events, rc, expect_pass=False,
      expect_phase_error=True)

events, rc = drive("/tmp/yaml_test12")
check("Y12 unknown depends_on", events, rc, expect_pass=False,
      expect_crashed=True)

# ===== Y13-Y23: UI components + mixed scenarios =====

events, rc = drive("/tmp/yaml_test13")
check("Y13 all UI component types", events, rc, expect_pass=True)
# Confirm every component type appears in a ui_request
ui_reqs = [e for e in events if e.get("type") == "ui_request"]
if not ui_reqs:
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + ["no ui_request"])
else:
    seen_types = {c["type"] for c in ui_reqs[0]["components"]}
    expected = {"text_input", "textarea", "switch", "number_input", "slider",
                "radio", "select", "multiselect", "checklist"}
    missing = expected - seen_types
    if missing:
        RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + [f"missing component types: {missing}"])
        print(f"  - missing component types: {missing}")

events, rc = drive("/tmp/yaml_test14")
check("Y14 display-only auto-continue", events, rc, expect_pass=True)
auto = [e for e in events if e.get("type") == "ui_auto_continue" and e.get("source") == "display_only"]
if not auto:
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + ["no ui_auto_continue display_only"])
    print(f"  - no ui_auto_continue display_only event")

events, rc = drive("/tmp/yaml_test15")
check("Y15 parallel phases with UI prompts", events, rc, expect_pass=True)
# Both visual_a and visual_b should produce ui_request events, independent order.
prompt_keys = {e["phase_key"] for e in events if e.get("type") == "ui_request"}
if prompt_keys != {"visual_a", "visual_b"}:
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + [f"expected prompts for visual_a+visual_b, got {prompt_keys}"])
    print(f"  - prompt keys mismatch: {prompt_keys}")

events, rc = drive("/tmp/yaml_test16")
check("Y16 image_choice + image_checklist", events, rc, expect_pass=True)

events, rc = drive("/tmp/yaml_test17")
check("Y17 phase with attach.data", events, rc, expect_pass=True)

events, rc = drive("/tmp/yaml_test18")
check("Y18 UI bound to measurement", events, rc, expect_pass=True)

events, rc = drive("/tmp/yaml_test19")
check("Y19 text_input with pattern/length", events, rc, expect_pass=True)

events, rc = drive("/tmp/yaml_test20")
check("Y20 slider + number boundaries", events, rc, expect_pass=True)

events, rc = drive("/tmp/yaml_test21")
check("Y21 full journey (setup+parallel+report)", events, rc, expect_pass=True,
      expect_plan_phases=["setup", "voltage", "current", "report"])

events, rc = drive("/tmp/yaml_test22", ui_timeout=3)
# No stdin responses provided (we respond automatically actually, let's change)
# Actually we DO respond — so this should pass. Let me rename.
check("Y22 required UI answered", events, rc, expect_pass=True)

# Drive Y22 again with a bad driver: don't respond. Run without responding.
def drive_no_respond(path, ui_timeout=3):
    proc = subprocess.Popen(
        [CLI, "run", path, "--json", "--ui-timeout", str(ui_timeout)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, bufsize=1,
    )
    events = []
    for line in proc.stdout:
        line = line.rstrip("\n")
        if not line: continue
        try:
            evt = json.loads(line)
        except Exception:
            continue
        events.append(evt)
        if evt.get("type") == "run_finished":
            break
    try:
        proc.wait(timeout=30)
    except Exception:
        proc.kill()
    return events, proc.returncode

events, rc = drive_no_respond("/tmp/yaml_test22", ui_timeout=3)
check("Y22b required UI timed out", events, rc, expect_pass=False)
if not any(e.get("type") == "ui_timeout" for e in events):
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + ["no ui_timeout event"])
    print(f"  - expected ui_timeout event")

events, rc = drive("/tmp/yaml_test23")
check("Y23 unit metadata defaults", events, rc, expect_pass=True)

# ===== Y25+ plugs =====
events, rc = drive("/tmp/yaml_test24")
check("Y24p simple plug", events, rc, expect_pass=True)

events, rc = drive("/tmp/yaml_test25")
check("Y25p plug __init__ raises", events, rc, expect_pass=False)
# Plug init failure cancels the downstream phase before execution — should
# emit phase_skipped (not a fake phase_finished pair) with the plug error
# as reason.
sk = next((e for e in events if e.get("type") == "phase_skipped"), None)
if not sk or not sk.get("reason"):
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + ["expected phase_skipped with reason"])
    print("  - expected phase_skipped with reason")

events, rc = drive("/tmp/yaml_test26")
check("Y26p plug state persists", events, rc, expect_pass=True)

events, rc = drive("/tmp/yaml_test27")
check("Y27p plug missing module", events, rc, expect_pass=False, expect_phase_error=True)

# ===== Y28+ framework depth =====
events, rc = drive("/tmp/yaml_test28")
check("Y28 phase timeout → TIMEOUT", events, rc, expect_pass=False)
if not any(e.get("type") == "phase_finished" and e.get("outcome") == "TIMEOUT" for e in events):
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + ["no phase_finished outcome=TIMEOUT"])
    print("  - no phase_finished outcome=TIMEOUT")
# Confirm the error message has sane units
tmo = next((e for e in events if e.get("type") == "phase_finished" and e.get("outcome") == "TIMEOUT"), None)
if tmo and "1000 seconds" in (tmo.get("error") or ""):
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + ["timeout error says 1000 seconds (should be 1)"])
    print("  - timeout error says 1000 seconds")

events, rc = drive("/tmp/yaml_test30")
check("Y30 then.fail=stop halts downstream", events, rc, expect_pass=False)
# A fails, B must be emitted as a PhaseSkipped (not a fake finished pair)
b = next((e for e in events if e.get("type") == "phase_skipped" and e.get("phase_key") == "b"), None)
if not b:
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + ["phase b did not emit phase_skipped after A failed"])
    print("  - phase b did not emit phase_skipped")

events, rc = drive("/tmp/yaml_test31")
check("Y31 on_first_failure continue → phase B runs", events, rc, expect_pass=False)
# The failing phase must have an error field
f = next((e for e in events if e.get("type") == "phase_finished" and e.get("phase_key") == "fails"), None)
if not f or not f.get("error"):
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + ["fail phase has no error field"])
    print("  - fail phase has no error field")
# later phase must have run
later = next((e for e in events if e.get("type") == "phase_finished" and e.get("phase_key") == "later"), None)
if not later or later.get("outcome") != "PASS":
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + ["later phase did not run / pass"])
    print("  - later phase did not run/pass")

events, rc = drive("/tmp/yaml_test32")
check("Y32 enabled:false phase skipped from plan", events, rc, expect_pass=True,
      expect_plan_phases=["on_"])

events, rc = drive("/tmp/yaml_test33")
check("Y33 multi-slot carries slot_id", events, rc, expect_pass=True)
slot_ids = {e.get("slot_id") for e in events
            if e.get("type") in ("phase_started", "phase_finished")}
if slot_ids != {"slot_a", "slot_b"}:
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + [f"expected slot_ids slot_a+slot_b, got {slot_ids}"])
    print(f"  - slot ids mismatch: {slot_ids}")

events, rc = drive("/tmp/yaml_test34")
check("Y34 sub-units", events, rc, expect_pass=True)

events, rc = drive("/tmp/yaml_test35")
check("Y35 shell executable phase", events, rc, expect_pass=True)

events, rc = drive("/tmp/yaml_test37")
check("Y37 circular depends_on rejected", events, rc, expect_pass=False,
      expect_crashed=True)

events, rc = drive("/tmp/yaml_test38")
check("Y38 duplicate phase keys rejected", events, rc, expect_pass=False,
      expect_crashed=True)

events, rc = drive("/tmp/yaml_test40")
check("Y40 plug scope all vs each", events, rc, expect_pass=True)
slot_ids = {e.get("slot_id") for e in events if e.get("type") == "phase_finished"}
if slot_ids != {"slot_a", "slot_b"}:
    RESULTS[-1] = (RESULTS[-1][0], False, RESULTS[-1][2] + [f"expected slot_ids slot_a+slot_b, got {slot_ids}"])
    print(f"  - slot ids mismatch: {slot_ids}")

# Pre-baked UI values exercise
events, rc = drive("/tmp/yaml_test13", ui_values={
    "all_inputs": {
        "text_field": "pre-baked",
        "switch_field": True,
        "radio_field": "a",
    }
})
# Expect ui_auto_continue source=pre_baked (since required text + switch + radio covered)
if not any(e.get("type") == "ui_auto_continue" and e.get("source") == "pre_baked" for e in events):
    RESULTS.append(("Y24 prebake UI", False, ["no ui_auto_continue pre_baked"]))
    print("[FAIL] Y24 prebake UI: no pre_baked ui_auto_continue")
else:
    RESULTS.append(("Y24 prebake UI", True, []))
    print("[OK] Y24 prebake UI")

# Summary
ok = sum(1 for _, o, _ in RESULTS if o)
total = len(RESULTS)
print(f"\n{ok}/{total} passed")
sys.exit(0 if ok == total else 1)
