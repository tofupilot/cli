"""An orphaned plug service exits even when its instrument close hangs.

Stdlib-only (unittest + subprocess): run with
    python3 -m unittest discover crates/execution-engine/python/tests

The parent is a throwaway interpreter that spawns tp_plug.py and sleeps;
the test SIGKILLs it, the way a crashed CLI would go, and expects the
plug gone within the orphan cleanup budget plus the watchdog's poll.
Unix only: the Windows parent watchdog waits on a process handle the
test cannot stand in for without a Win32 harness.
"""

import json
import os
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

TP_PLUG = Path(__file__).resolve().parent.parent / "tp_plug.py"


def _alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


@unittest.skipIf(os.name == "nt", "Unix orphan detection only")
class OrphanExitTest(unittest.TestCase):
    def test_hanging_del_does_not_keep_the_orphan_alive(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "plug.py").write_text(textwrap.dedent(
                """
                import time

                class Plug:
                    def __del__(self):
                        time.sleep(60)
                """
            ))
            config = json.dumps(
                {"file": str(Path(tmp, "plug.py")), "class": "Plug", "config": {}}
            )
            parent_src = textwrap.dedent(
                f"""
                import subprocess, sys, time
                p = subprocess.Popen([sys.executable, {str(TP_PLUG)!r},
                    "--procedure-dir", {tmp!r}, "--plug-name", "psu",
                    "--display-name", "PSU", "--plug-config", {config!r}],
                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
                line = p.stdout.readline()
                assert line.startswith("NDJSON_PORT:"), line
                print(p.pid, flush=True)
                time.sleep(600)
                """
            )
            parent = subprocess.Popen(
                [sys.executable, "-c", parent_src], stdout=subprocess.PIPE, text=True
            )
            try:
                plug_pid = int(parent.stdout.readline().strip())
                self.assertTrue(_alive(plug_pid), "plug service did not start")

                os.kill(parent.pid, signal.SIGKILL)
                parent.wait()

                # 1 s ppid poll + 5 s cleanup budget, with slack for a loaded box.
                deadline = time.monotonic() + 8.0
                while _alive(plug_pid) and time.monotonic() < deadline:
                    time.sleep(0.1)
                survived = _alive(plug_pid)
                if survived:
                    os.kill(plug_pid, signal.SIGKILL)
                self.assertFalse(
                    survived,
                    "tp_plug.py outlived its parent: a hanging __del__ kept the orphan alive",
                )
            finally:
                if parent.poll() is None:
                    parent.kill()
                    parent.wait()
                parent.stdout.close()


if __name__ == "__main__":
    unittest.main()
