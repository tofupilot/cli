"""Stdlib logging bridge (tp_worker.StdlibLoggingBridge).

Stdlib-only (unittest): run with
    python3 -m unittest discover crates/execution-engine/python/tests
No pytest, no venv — same zero-dependency contract as tp_worker itself.
"""

import importlib.util
import logging
import sys
import unittest
from pathlib import Path

_spec = importlib.util.spec_from_file_location(
    "tp_worker", Path(__file__).resolve().parent.parent / "tp_worker.py"
)
tp_worker = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tp_worker)


class LoggingBridgeTest(unittest.TestCase):
    def setUp(self):
        self.logs = tp_worker.Logs(job_id="job-1")
        self.bridge = tp_worker.StdlibLoggingBridge(self.logs)
        self.root = logging.getLogger()
        self._prev_level = self.root.level
        self.root.addHandler(self.bridge)
        self.root.setLevel(logging.DEBUG)
        self.addCleanup(self.root.setLevel, self._prev_level)
        self.addCleanup(self.root.removeHandler, self.bridge)

    def test_levels_are_preserved(self):
        log = logging.getLogger("phases.warmup")
        log.debug("debug")
        log.info("info")
        log.warning("warn")
        log.error("error")
        log.critical("critical")

        self.assertEqual(
            [(e["level"], e["message"]) for e in self.logs.entries],
            [
                ("DEBUG", "debug"),
                ("INFO", "info"),
                ("WARNING", "warn"),
                ("ERROR", "error"),
                ("CRITICAL", "critical"),
            ],
        )

    def test_no_duplicate_via_stderr_last_resort(self):
        # With a real handler attached, logging.lastResort must stay silent —
        # otherwise the stderr capture would add a second WARNING-tagged copy.
        original_stderr = sys.stderr
        sys.stderr = tp_worker.LogCapturingStream(self.logs, level="WARNING")
        self.addCleanup(setattr, sys, "stderr", original_stderr)

        logging.getLogger("phases.warmup").error("only once")
        sys.stderr.flush()

        matches = [e for e in self.logs.entries if e["message"] == "only once"]
        self.assertEqual(len(matches), 1)
        self.assertEqual(matches[0]["level"], "ERROR")

    def test_record_origin_is_captured(self):
        logging.getLogger("phases.warmup").info("where am I")

        entry = self.logs.entries[-1]
        self.assertTrue(entry["file"].endswith("test_logging_bridge.py"))
        self.assertIsInstance(entry["line"], int)

    def test_exception_includes_traceback(self):
        try:
            raise ValueError("boom")
        except ValueError:
            logging.getLogger("phases.warmup").exception("failed")

        entry = self.logs.entries[-1]
        self.assertEqual(entry["level"], "ERROR")
        self.assertIn("failed", entry["message"])
        self.assertIn("ValueError: boom", entry["message"])

    def test_direct_logs_methods_still_introspect_caller(self):
        self.logs.warning("direct call")

        entry = self.logs.entries[-1]
        self.assertEqual(entry["level"], "WARNING")
        self.assertTrue(entry["file"].endswith("test_logging_bridge.py"))


if __name__ == "__main__":
    unittest.main()
