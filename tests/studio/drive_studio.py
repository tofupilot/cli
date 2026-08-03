#!/usr/bin/env python3
"""Drive `tofupilot studio` end to end: spawn the real binary against a
scratch project, parse the pairing (port + token) from its output, and
exercise the full RPC surface plus the WS auth boundary the dashboard
Studio page relies on.

Usage: drive_studio.py <path-to-tofupilot-binary>
Exit 0 = all checks passed.
"""
import base64
import http.client
import json
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

CLI = sys.argv[1]

PROCEDURE = """name: Demo
version: 1.0.0
main:
  - name: Check
    python: phases.main.check
"""

checks = 0


def check(name, cond, detail=""):
    global checks
    checks += 1
    status = "ok" if cond else "FAIL"
    print(f"[{status}] {name}" + (f" ({detail})" if detail and not cond else ""))
    if not cond:
        raise SystemExit(f"check failed: {name}: {detail}")


def rpc(port, token, body, expect_status=200):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    conn.request("POST", "/studio/rpc", json.dumps(body), headers)
    res = conn.getresponse()
    raw = res.read()
    check(f"rpc {body.get('op')} status {expect_status}", res.status == expect_status,
          f"got {res.status}: {raw[:200]!r}")
    conn.close()
    return json.loads(raw) if res.status == 200 else None


def ws_status(port, path, origin):
    """Raw WS upgrade handshake; returns the HTTP status code."""
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    key = base64.b64encode(b"drive-studio-nonce!!").decode()
    conn.request("GET", path, headers={
        "Origin": origin,
        "Connection": "Upgrade",
        "Upgrade": "websocket",
        "Sec-WebSocket-Version": "13",
        "Sec-WebSocket-Key": key,
    })
    status = conn.getresponse().status
    conn.close()
    return status


def main():
    project = Path(tempfile.mkdtemp(prefix="tp-studio-e2e-"))
    (project / "phases").mkdir()
    (project / "procedure.yaml").write_text(PROCEDURE)
    (project / "phases" / "main.py").write_text("def check():\n    return True\n")
    (project / ".env").write_text("SECRET=1\n")

    proc = subprocess.Popen(
        [CLI, "studio", str(project), "--no-open"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )
    port = token = None
    try:
        # Pairing appears either in the dashboard URL fragment
        # (#port=..&token=..) or the not-logged-in fallback line
        # (port=.. token=..).
        deadline = time.time() + 15
        for line in proc.stdout:
            print("cli:", line.rstrip())
            m = re.search(r"port=(\d+)[&\s]+token=([0-9a-f]{32})", line)
            if m:
                port, token = int(m.group(1)), m.group(2)
                break
            if time.time() > deadline:
                break
        check("pairing parsed from CLI output", port is not None and token is not None)

        # Auth boundary.
        rpc(port, None, {"op": "project_info"}, expect_status=403)
        rpc(port, "0" * 32, {"op": "project_info"}, expect_status=403)

        info = rpc(port, token, {"op": "project_info"})
        check("project_info name", info["name"] == project.name, str(info))
        check("project_info procedure", info["procedure_path"] == "procedure.yaml", str(info))

        listing = rpc(port, token, {"op": "list_files"})
        names = [e["name"] for e in listing["entries"]]
        check("listing hides dotfiles, dirs first", names == ["phases", "procedure.yaml"], str(names))

        read = rpc(port, token, {"op": "read_file", "path": "procedure.yaml"})
        check("read content", read["content"] == PROCEDURE)

        write = rpc(port, token, {
            "op": "write_file", "path": "procedure.yaml",
            "content": PROCEDURE.replace("Demo", "Demo2"),
            "expected_sha256": read["sha256"],
        })
        check("write ok", write["result"] == "written", str(write))

        stale = rpc(port, token, {
            "op": "write_file", "path": "procedure.yaml",
            "content": "x", "expected_sha256": read["sha256"],
        })
        check("stale baseline conflicts", stale["code"] == "conflict", str(stale))

        diags = rpc(port, token, {"op": "validate"})
        check("clean procedure validates", diags["diagnostics"] == [], str(diags))

        dotfile = rpc(port, token, {"op": "read_file", "path": ".env"})
        check("dotfile refused", dotfile["code"] == "forbidden", str(dotfile))

        escape = rpc(port, token, {"op": "read_file", "path": "../../etc/hosts"})
        check("traversal refused", escape["result"] == "error", str(escape))

        unknown = rpc(port, token, {"op": "future_thing"})
        check("unknown op typed as unsupported", unknown["code"] == "unsupported", str(unknown))

        # WS auth boundary.
        check("ws foreign origin no token 403", ws_status(port, "/ws", "https://evil.example") == 403)
        check("ws bad token 403", ws_status(port, "/ws?token=nope", "https://www.tofupilot.app") == 403)
        check("ws valid token 101",
              ws_status(port, f"/ws?token={token}", "https://www.tofupilot.app") == 101)
        check("ws loopback origin no token 101",
              ws_status(port, "/ws", f"http://127.0.0.1:{port}") == 101)

        print(f"\nall {checks} checks passed")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(project, ignore_errors=True)


if __name__ == "__main__":
    main()
