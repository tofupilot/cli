#!/usr/bin/env python3
"""Drive tofupilot run --json: read stdout events, answer ui_request on stdin."""
import json
import subprocess
import sys

CLI = sys.argv[1]
PROCEDURE = sys.argv[2]

proc = subprocess.Popen(
    [CLI, "run", PROCEDURE, "--json", "--ui-timeout", "20"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    bufsize=1,
)

try:
    for line in proc.stdout:
        line = line.rstrip("\n")
        print(line)  # echo all events
        try:
            evt = json.loads(line)
        except Exception:
            continue
        if evt.get("type") != "ui_request":
            continue

        values = {}
        for c in evt.get("components", []):
            if not c.get("is_input"):
                continue
            t = c.get("type")
            key = c["key"]
            opts = c.get("options") or []
            if t == "switch":
                values[key] = True
            elif t in ("radio", "select", "image_choice"):
                if opts:
                    values[key] = opts[0]["value"]
            elif t in ("multiselect", "checklist", "image_checklist"):
                if opts:
                    values[key] = [opts[0]["value"]]
            elif t in ("number_input", "slider"):
                values[key] = 42
            elif t == "text_input":
                values[key] = "SN-0001"
            elif t == "textarea":
                values[key] = "driver note"
            else:
                values[key] = "ok"

        resp = {"type": "ui_response", "request_id": evt["request_id"], "values": values}
        proc.stdin.write(json.dumps(resp) + "\n")
        proc.stdin.flush()
finally:
    proc.wait(timeout=60)
    print(f"[driver] CLI exited {proc.returncode}")
    err = proc.stderr.read()
    if err:
        print("[stderr]", err[:500])
