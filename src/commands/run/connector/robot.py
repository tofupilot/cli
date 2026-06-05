"""Robot Framework connector for TofuPilot CLI.

Drives Robot's Listener API v3 from inside the user's `.robot` suite:
each test case becomes a wire phase, `Measure *` keywords from the
embedded `tofupilot_robot` library bundle measurements onto
`phase_end`, and the same identify-unit handshake the pytest connector
uses gates the run on a unit reply from the parent CLI.

Why a listener and not a pytest-style plugin: Robot's only public
extension surface for cross-test telemetry is the listener API.
Listener v3 receives mutable `data` / `result` objects on every
suite/test/keyword event, which is enough to drive the wire shape
without modifying the user's suite.

Why a keyword library for measurements (not `Should Be Within Range`
introspection): Robot's BuiltIn library expands variables and resolves
expressions before the listener sees the keyword call, so the literal
limit values would already be lost. Even when literals survive, the
matrix of comparison keywords (Should Be True, Should Be Equal As
Numbers, …) is wide and fragile. Shipping a TofuPilot keyword
(`Measure Numeric` / `Measure String` / `Measure Boolean`) is the
narrow, public contract that doesn't break under variable expansion.

Usage: python robot.py <test_path>
       <test_path> is the package dir containing `.robot` files.
       Robot's own test selection (-t / --test) is not supported here;
       the connector hands the dir to Robot and lets it discover all
       suites underneath.
"""

from __future__ import annotations

import json
import os
import select
import signal
import sys
import time
import traceback
from typing import Any


# Robot does not redirect fd 0 at suite start the way pytest's
# `--capture=fd` does; reading from fd 0 directly is enough. We keep the
# same readline helper shape as the pytest / openhtf connectors for
# consistency, but the dup that pytest needs is unnecessary here.
_STDIN_FD = 0
_STDOUT_FD = os.dup(1)


def _readline_interruptible() -> str:
    """Read one line from stdin in a way that lets SIGTERM/SIGINT fire
    promptly. Mirror of the openhtf / pytest connector helper.
    """
    fd = _STDIN_FD
    buf = bytearray()
    while True:
        ready, _, _ = select.select([fd], [], [], 0.2)
        if ready:
            chunk = os.read(fd, 4096)
            if not chunk:
                return buf.decode("utf-8", errors="replace")
            buf.extend(chunk)
            if b"\n" in chunk:
                return buf.decode("utf-8", errors="replace")


def _exit_on_term(_signum, _frame):
    os._exit(130)


def _install_signal_handlers() -> None:
    try:
        signal.signal(signal.SIGTERM, _exit_on_term)
        signal.signal(signal.SIGINT, _exit_on_term)
        signal.siginterrupt(signal.SIGTERM, True)
        signal.siginterrupt(signal.SIGINT, True)
    except (ValueError, OSError):
        pass


# ---------------------------------------------------------------------------
# Wire protocol — mirror of `connector/events.rs::PythonEvent`
# ---------------------------------------------------------------------------


class Event:
    BRIDGE_READY = "bridge_ready"
    TEST_START = "test_start"
    TEST_END = "test_end"
    PHASE_BEGIN = "phase_begin"
    PHASE_END = "phase_end"
    MEASUREMENT = "measurement"
    PHASE_LOG = "phase_log"
    WARNING = "warning"
    ATTACHMENT = "attachment"


class Outcome:
    PASS = "PASS"
    FAIL = "FAIL"
    ERROR = "ERROR"
    SKIP = "SKIP"
    ABORTED = "ABORTED"


def _emit(event: dict[str, Any]) -> None:
    line = (json.dumps(event, default=str) + "\n").encode("utf-8")
    try:
        os.write(_STDOUT_FD, line)
    except OSError:
        try:
            print(json.dumps(event, default=str), flush=True)
        except OSError:
            pass


def _iso_timestamp(epoch_seconds: float) -> str:
    import datetime as _dt
    return _dt.datetime.fromtimestamp(epoch_seconds, _dt.timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%S.%fZ"
    )


# ---------------------------------------------------------------------------
# Identify-unit handshake (mirrors openhtf / pytest path)
# ---------------------------------------------------------------------------


def _await_unit_resolution() -> dict[str, Any]:
    try:
        line = _readline_interruptible()
        if not line:
            return {}
        msg = json.loads(line)
        if isinstance(msg, dict) and msg.get("type") == "set_unit_resolved":
            return msg
        return {}
    except Exception:
        traceback.print_exc()
        return {}


def _build_pyproject_unit_kwargs(test_root: str) -> tuple[dict[str, str], bool]:
    """Read `[tool.tofupilot]` from `pyproject.toml` next to the suite
    so users can declare `serial_number = "SN-1"` etc. without modifying
    their `.robot` files. Returns `(unit_kwargs, auto_identify)`.
    """
    candidates = [
        os.path.join(test_root, "pyproject.toml"),
        os.path.join(os.path.dirname(test_root), "pyproject.toml"),
    ]
    for path in candidates:
        if not os.path.isfile(path):
            continue
        try:
            try:
                import tomllib  # py311+
            except ImportError:
                try:
                    import tomli as tomllib  # type: ignore
                except ImportError:
                    return {}, False
            with open(path, "rb") as f:
                data = tomllib.load(f)
            tp_cfg = data.get("tool", {}).get("tofupilot", {})
            kwargs = {
                "serial_number": str(tp_cfg.get("serial_number", "") or ""),
                "part_number": str(tp_cfg.get("part_number", "") or ""),
                "revision_number": str(tp_cfg.get("revision_number", "") or ""),
                "batch_number": str(tp_cfg.get("batch_number", "") or ""),
            }
            auto_identify = bool(tp_cfg.get("auto_identify", False))
            return kwargs, auto_identify
        except Exception:
            pass
    return {}, False


# ---------------------------------------------------------------------------
# Listener
# ---------------------------------------------------------------------------


class _Listener:
    """Robot Framework listener (API v3).

    Hooks:
      * `start_suite`  — for the top-level suite, emit `test_start`
                         with a per-test phase plan and run the
                         identify-unit handshake.
      * `start_test`   — emit `phase_begin`.
      * `end_test`     — read measurements stashed by `tofupilot_robot`
                         and emit `phase_end`.
      * `end_suite`    — for the top-level suite, emit `test_end`.
      * `log_message`  — forward to `phase_log`.

    Robot creates one listener instance per `--listener` argument; we
    carry per-instance state on `self`.
    """

    # Listener API version. Robot 7+ supports v3. v2 is also widely
    # supported but uses positional-arg signatures (string-typed) that
    # would force a second translation pass; v3 hands us first-class
    # `data` / `result` objects.
    ROBOT_LISTENER_API_VERSION = 3

    def __init__(self) -> None:
        self.identify_enabled = True
        self.unit_kwargs: dict[str, str] = {}
        self.auto_identify = False
        self.resolved_unit: dict[str, Any] = {}
        self.test_name = ""
        self.session_start_ms: int | None = None
        # Phase plan order: top-level suite's recursive list of tests.
        # Built lazily on first `start_suite` (the top-level call).
        self.phase_plan: list[str] = []
        self._top_suite_id: str | None = None
        self._phase_start_ms: dict[str, int] = {}
        self._observed_outcomes: list[str] = []
        self._current_phase: str | None = None
        # Lazy import so this listener module remains importable even
        # in environments without the keyword library (the embedded
        # copy is written next to it at run time, but defending here
        # keeps unit testing simple). `_tofupilot_robot_import_failed`
        # caches the failure so the warning fires once per run instead
        # of once per failed test.
        self._tofupilot_robot = None
        self._tofupilot_robot_import_failed = False

    # --- helpers ---------------------------------------------------------

    def _phase_name(self, test) -> str:
        """Robot test names are unique within a suite; suite source
        plus name keeps duplicates across suites disambiguated.
        Mirrors `tofupilot_robot._current_test_id` so the keyword's
        stash key matches what we look up in `end_test`.
        """
        suite_source = getattr(test.parent, "source", "") if test.parent else ""
        name = test.name
        if suite_source:
            return f"{suite_source}::{name}"
        return name

    def _enumerate_tests(self, suite) -> list[str]:
        """Walk the suite tree and return the recursive test-id list,
        in execution order. Mirrors what Robot itself runs.
        """
        ids: list[str] = []
        for t in suite.tests:
            ids.append(self._phase_name(t))
        for child in suite.suites:
            ids.extend(self._enumerate_tests(child))
        return ids

    def _wire_outcome(self, status: str, measurement_failed: bool) -> str:
        # Robot status strings: PASS / FAIL / SKIP / NOT RUN.
        # FAIL with a Measure assertion → wire FAIL (limit violation).
        # FAIL without one → wire ERROR (Python crash, setup failure,
        # framework error). The pytest connector makes the same
        # distinction so the wire stays consistent.
        if status == "PASS":
            return Outcome.PASS
        if status == "FAIL":
            return Outcome.FAIL if measurement_failed else Outcome.ERROR
        if status in ("SKIP", "NOT RUN"):
            return Outcome.SKIP
        return Outcome.ERROR

    # --- listener hooks --------------------------------------------------

    def start_suite(self, data, result) -> None:
        # Robot fires `start_suite` for every nested suite; only the
        # top-level call (the one we see first) gates the run-level
        # test_start emit.
        if self._top_suite_id is not None:
            return
        self._top_suite_id = data.id
        self.session_start_ms = int(time.time() * 1000)
        self.phase_plan = self._enumerate_tests(data)
        self.test_name = data.name or ""

        _emit({
            "type": Event.TEST_START,
            "test_name": self.test_name,
            "phases": self.phase_plan,
            "identify": self.identify_enabled,
            "auto_identify": self.auto_identify,
            "unit_kwargs": self.unit_kwargs,
        })
        if self.identify_enabled:
            self.resolved_unit = _await_unit_resolution()
        else:
            self.resolved_unit = {}

    def start_test(self, data, result) -> None:
        phase_name = self._phase_name(data)
        self._current_phase = phase_name
        self._phase_start_ms[phase_name] = int(time.time() * 1000)
        _emit({"type": Event.PHASE_BEGIN, "name": phase_name})

    def end_test(self, data, result) -> None:
        phase_name = self._phase_name(data)

        # Pull measurements + read the per-test measurement-failure
        # flag stashed by the keyword library. The flag drives the
        # FAIL-vs-ERROR distinction for Robot's coarse `FAIL` status.
        measurements: list[dict[str, Any]] = []
        measurement_failed = False
        if not self._tofupilot_robot_import_failed:
            try:
                if self._tofupilot_robot is None:
                    import tofupilot_robot as _tp_robot
                    self._tofupilot_robot = _tp_robot
                measurements = self._tofupilot_robot.consume_measurements(phase_name)
                measurement_failed = self._tofupilot_robot.consume_measurement_failure(phase_name)
            except Exception:
                # Library missing or import failed — emit a warning
                # once and stop trying. The phase still reports its
                # outcome; the user just won't see structured
                # measurement payloads.
                self._tofupilot_robot_import_failed = True
                _emit({
                    "type": Event.WARNING,
                    "message": "tofupilot_robot library unavailable — measurements will not be captured",
                })

        outcome = self._wire_outcome(result.status, measurement_failed)
        self._observed_outcomes.append(outcome)

        # Stream live `measurement` events too, mirroring the pytest
        # connector. Live consumers (operator UI agent_proto) display
        # these as soon as they happen; the bundled list rides on
        # phase_end for upload-time persistence.
        for m in measurements:
            live = {
                "type": Event.MEASUREMENT,
                "name": m.get("name"),
                "value": m.get("measured_value"),
                "phase_name": phase_name,
            }
            if m.get("units"):
                live["unit"] = m["units"]
            _emit(live)

        # Surface the failure message on FAIL / ERROR as a phase_log so
        # the operator sees what went wrong without parsing stderr.
        if outcome in (Outcome.FAIL, Outcome.ERROR) and getattr(result, "message", ""):
            _emit({
                "type": Event.PHASE_LOG,
                "level": "ERROR",
                "message": str(result.message),
                "timestamp": _iso_timestamp(time.time()),
                "phase_name": phase_name,
            })

        start_ms = self._phase_start_ms.get(phase_name, int(time.time() * 1000))
        end_ms = int(time.time() * 1000)
        # Robot exposes `result.elapsedtime` in ms when available.
        elapsed = getattr(result, "elapsedtime", None)
        if isinstance(elapsed, (int, float)) and elapsed > 0:
            start_ms = end_ms - int(elapsed)

        _emit({
            "type": Event.PHASE_END,
            "name": phase_name,
            "outcome": outcome,
            "start_time_millis": start_ms,
            "end_time_millis": end_ms,
            "measurements": measurements,
            "retry_count": 0,
            "docstring": getattr(data, "doc", None) or None,
        })
        self._current_phase = None

    def end_suite(self, data, result) -> None:
        # Only emit `test_end` once the top-level suite finishes.
        if data.id != self._top_suite_id:
            return

        # Aggregate outcome: any FAIL drops the run to FAIL, any ERROR
        # to ERROR (ERROR wins over FAIL since a crash is more severe
        # than a measurement failure). Empty suite is ERROR — matches
        # pytest's empty-collection handling: a procedure declared as
        # Robot but shipping no `.robot` cases is a procedure-config
        # bug, not a successful no-op.
        #
        # Suite-level setup/teardown failures don't show up in
        # `_observed_outcomes` (those are per-test). Robot exposes
        # them via `result.setup.status` / `result.teardown.status` in
        # listener API v3; fold those into the rollup so a Suite
        # Teardown failure with all tests passing doesn't get reported
        # as PASS. We deliberately do NOT use the top-level
        # `result.status`: that's always FAIL when any child fails,
        # which would double-report the per-test failures as a
        # suite-level event.
        setup_status = getattr(getattr(result, "setup", None), "status", None)
        teardown_status = getattr(
            getattr(result, "teardown", None), "status", None
        )
        suite_failed = setup_status == "FAIL" or teardown_status == "FAIL"
        if not self._observed_outcomes:
            outcome = Outcome.ERROR
            _emit({
                "type": Event.WARNING,
                "message": "No Robot Framework tests executed — pyproject declared robot but no test cases were found.",
            })
        elif Outcome.ERROR in self._observed_outcomes:
            outcome = Outcome.ERROR
        elif Outcome.FAIL in self._observed_outcomes or suite_failed:
            outcome = Outcome.FAIL
        else:
            # Every observed outcome is PASS or SKIP. Matches pytest's
            # exit-0 on an all-passing or all-skipped run.
            outcome = Outcome.PASS

        # Surface suite-level setup/teardown failure messages. They
        # never reach `end_test`, so without this the operator sees a
        # FAIL run with no explanation. We only fire on actual setup
        # or teardown failure (not the generic suite rollup, which is
        # always FAIL when any child test failed and would otherwise
        # spam every failed run with "1 test, 1 failed" noise). Robot
        # exposes the per-step status on `result.setup` / `result.teardown`
        # in API v3.
        setup_failed = (
            getattr(getattr(result, "setup", None), "status", None) == "FAIL"
        )
        teardown_failed = (
            getattr(getattr(result, "teardown", None), "status", None) == "FAIL"
        )
        setup_msg = getattr(getattr(result, "setup", None), "message", "") or ""
        teardown_msg = (
            getattr(getattr(result, "teardown", None), "message", "") or ""
        )
        if setup_failed and setup_msg:
            _emit({
                "type": Event.PHASE_LOG,
                "level": "ERROR",
                "message": f"Suite Setup failed: {setup_msg}",
                "timestamp": _iso_timestamp(time.time()),
                "phase_name": None,
            })
        if teardown_failed and teardown_msg:
            _emit({
                "type": Event.PHASE_LOG,
                "level": "ERROR",
                "message": f"Suite Teardown failed: {teardown_msg}",
                "timestamp": _iso_timestamp(time.time()),
                "phase_name": None,
            })

        end_ms = int(time.time() * 1000)
        metadata: dict[str, Any] = {}
        for k in ("serial_number", "part_number", "revision_number", "batch_number"):
            v = self.resolved_unit.get(k)
            if isinstance(v, str) and v:
                metadata[k] = v
        sub_units = self.resolved_unit.get("sub_units")
        if isinstance(sub_units, list) and sub_units:
            metadata["sub_units"] = sub_units

        _emit({
            "type": Event.TEST_END,
            "outcome": outcome,
            "dut_id": metadata.get("serial_number", ""),
            "test_name": self.test_name,
            "start_time_millis": self.session_start_ms or end_ms,
            "end_time_millis": end_ms,
            "logs": [],
            "docstring": None,
            "metadata": metadata,
        })

    def log_message(self, message) -> None:
        # Robot levels: TRACE, DEBUG, INFO, WARN, ERROR, FAIL.
        # Map FAIL to ERROR on the wire — wire log levels follow
        # logging-module conventions, and FAIL is Robot-specific.
        level = (message.level or "INFO").upper()
        if level == "FAIL":
            level = "ERROR"
        _emit({
            "type": Event.PHASE_LOG,
            "level": level,
            "message": message.message or "",
            "timestamp": _iso_timestamp(time.time()),
            "phase_name": self._current_phase,
            "file": None,
            "line": None,
        })


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> int:
    _install_signal_handlers()
    _emit({"type": Event.BRIDGE_READY})

    test_path = sys.argv[1] if len(sys.argv) >= 2 else os.getcwd()
    test_root = test_path if os.path.isdir(test_path) else os.path.dirname(
        os.path.abspath(test_path)
    ) or os.getcwd()

    unit_kwargs, auto_identify = _build_pyproject_unit_kwargs(test_root)
    if os.environ.get("TOFUPILOT_AUTO_IDENTIFY"):
        auto_identify = True

    # Make the embedded `tofupilot_robot.py` importable regardless of
    # what Robot's default module search resolves. The Rust side writes
    # `tofupilot_robot.py` next to `.tofupilot_robot.py`, so the
    # connector's own dir is the safe place to add.
    here = os.path.dirname(os.path.abspath(__file__))
    if here and here not in sys.path:
        sys.path.insert(0, here)

    try:
        from robot import run as robot_run
    except ImportError:
        _emit({"type": Event.WARNING, "message": "robotframework not installed in venv"})
        _emit({
            "type": Event.TEST_START,
            "test_name": "",
            "phases": [],
            "identify": False,
            "auto_identify": False,
            "unit_kwargs": unit_kwargs,
        })
        _emit({
            "type": Event.TEST_END,
            "outcome": Outcome.ERROR,
            "dut_id": "",
            "test_name": "",
            "start_time_millis": int(time.time() * 1000),
            "end_time_millis": int(time.time() * 1000),
            "logs": [],
            "metadata": {},
        })
        return 1

    listener = _Listener()
    listener.identify_enabled = True
    listener.unit_kwargs = unit_kwargs
    listener.auto_identify = auto_identify

    # Robot writes `output.xml` / `log.html` / `report.html` to cwd by
    # default, which clutters the user's repo. Point them at a tmpdir
    # under the test root and let Robot manage cleanup. We don't use
    # the artifacts ourselves — the listener emits everything we need.
    output_dir = os.path.join(test_root, ".tofupilot_robot_output")
    os.makedirs(output_dir, exist_ok=True)

    # robot.run.run() returns the integer exit code (0 = all pass,
    # 1+ = number of failed critical tests, capped at 250). We open
    # devnull via `with` so the file descriptors close on exit
    # regardless of how `robot.run` returns (success, SystemExit,
    # exception). Long-running parents would otherwise leak two fds
    # per call.
    output_xml = os.path.join(output_dir, "output.xml")
    try:
        with open(os.devnull, "w") as _devnull_out, open(os.devnull, "w") as _devnull_err:
            rc = robot_run(
                test_path,
                listener=listener,
                outputdir=output_dir,
                stdout=_devnull_out,
                stderr=_devnull_err,
                console="none",
                output="output.xml",
                log=None,
                report=None,
            )
    except SystemExit as e:
        rc = int(e.code) if e.code is not None else 1
    except Exception:
        traceback.print_exc()
        rc = 1
    finally:
        # Persist `output.xml` outside the run-temp dir before we reap.
        # The CLI's queue + retry pipeline reads the file lazily (could
        # be minutes later, after a network failure) — pointing the
        # attachment path inside `output_dir` would race with the
        # `shutil.rmtree` below. Mirror the openhtf attachment path:
        # copy into `~/.tofupilot/attachments/<queue_id>/output.xml`
        # so the Rust upload path finds it whenever it gets around to
        # uploading.
        try:
            if os.path.isfile(output_xml):
                queue_id = os.environ.get("TOFUPILOT_QUEUE_ID", "") or "tmp"
                att_dir = os.path.join(
                    os.path.expanduser("~"),
                    ".tofupilot",
                    "attachments",
                    queue_id,
                )
                os.makedirs(att_dir, exist_ok=True)
                persisted = os.path.join(att_dir, "output.xml")
                import shutil as _shutil
                _shutil.copyfile(output_xml, persisted)
                _emit({
                    "type": Event.ATTACHMENT,
                    "phase_name": None,
                    "name": "output.xml",
                    "path": persisted,
                    "mimetype": "application/xml",
                    "live": False,
                })
        except Exception:
            pass
        # Reap the Robot run-temp dir. Safe now: the persisted copy
        # under `~/.tofupilot/attachments/` is what the upload pipeline
        # reads.
        try:
            import shutil
            shutil.rmtree(output_dir, ignore_errors=True)
        except Exception:
            pass

    return int(rc) if rc is not None else 0


if __name__ == "__main__":
    sys.exit(main())
