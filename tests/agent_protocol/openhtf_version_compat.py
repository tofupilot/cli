#!/usr/bin/env python3
"""Verify the OpenHTF connector against an old OpenHTF release.

The connector reaches into OpenHTF internals that moved between releases:

  * ``Attachment.size``               added in 1.5.2 (data moved to a temp file)
  * ``PhaseState.finalize``           added in 1.5 (``_set_phase_outcome`` before)
  * ``PhaseGroup``                    absent in 1.3.0, present from 1.4.4
  * ``AllInRangeValidator`` & friends added after 1.4.4
  * ``WithinPercent.marginal_percent`` added after 1.4.4

Every one of those is guarded, so a stock test running on OpenHTF 1.3.0 (2018)
emits the same event stream as one running on 1.6.1. This script asserts that
directly, without needing a built CLI binary.

Usage::

    python -m venv /tmp/htf13
    /tmp/htf13/bin/pip install 'setuptools<81' 'openhtf==1.3.0'
    /tmp/htf13/bin/python openhtf_version_compat.py

``setuptools<81`` is required because OpenHTF 1.3.0 imports ``pkg_resources``,
which setuptools 81 removed. That is an OpenHTF-era packaging detail, not a
TofuPilot constraint.

Interpreter ceilings come from OpenHTF itself, verified empirically:

  * 1.4.4 uses ``collections.Iterable``, moved to ``collections.abc`` in
    Python 3.10 — so 1.4.4 needs Python <= 3.9.
  * 1.3.0 and 1.4.4 use ``inspect.getargspec``, removed in Python 3.11.

Python 3.9 is therefore the only interpreter all four releases run on, which
is what CI pins. Both failures happen inside OpenHTF's own code before the
connector is reached, so a procedure pinning an old OpenHTF must also pin
``requires-python`` in its pyproject or the venv bootstrap picks a modern
interpreter and the run dies during import.
"""
import json
import os
import subprocess
import sys
import tempfile

CONNECTOR = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "..", "..", "src", "commands", "run", "connector", "openhtf.py",
)

# Reference event stream, version-independent by construction. Lets a single
# matrix job detect drift that otherwise needs two interpreters side by side.
GOLDEN = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "openhtf_version_compat.golden.json",
)

# A test exercising every construct the connector special-cases. Deliberately
# avoids PhaseGroup: it does not exist in 1.3.0, so a test using it fails in
# user code before the connector is involved.
PROBE = '''
import openhtf as htf
from openhtf import PhaseResult
from openhtf.plugs import BasePlug
from openhtf.util import units, validators as V

_tries = {"n": 0}


class Charger(BasePlug):
    def volts(self):
        return 231.4

    def tearDown(self):
        pass


@htf.plug(ch=Charger)
@htf.measures(
    htf.Measurement("v_in").with_units(units.VOLT).with_validator(V.in_range(200, 250)),
    htf.Measurement("lo_only").with_validator(V.in_range(minimum=1.0)),
    htf.Measurement("hi_only").with_validator(V.in_range(maximum=9.0)),
    htf.Measurement("fw").with_validator(V.matches_regex(r"^\\d+\\.\\d+$")),
    htf.Measurement("rev").with_validator(V.equals(2)),
    htf.Measurement("cur").with_units(units.AMPERE).with_validator(V.within_percent(32.0, 5.0)),
    htf.Measurement("s_val").with_validator(V.equals("OK")),
    htf.Measurement("b_val").with_validator(V.equals(True)),
    htf.Measurement("unset_one"),
)
def all_validators(test, ch):
    """Every validator kind the connector knows about."""
    test.logger.info("user log marker")
    test.measurements.v_in = ch.volts()
    test.measurements.lo_only = 5.0
    test.measurements.hi_only = 5.0
    test.measurements.fw = "3.4"
    test.measurements.rev = 2
    test.measurements.cur = 31.8
    test.measurements.s_val = "OK"
    test.measurements.b_val = True
    return PhaseResult.CONTINUE


@htf.measures(
    htf.Measurement("ramp").with_dimensions(units.SECOND).with_units(units.DEGREE_CELSIUS),
    htf.Measurement("grid").with_dimensions(units.VOLT, units.AMPERE).with_units(units.PERCENT),
)
def dimensioned(test):
    """1-D and 2-D dimensioned measurements."""
    for t in range(4):
        test.measurements.ramp[t] = 40.0 + t
    for v in (208, 230):
        for i in (8, 16):
            test.measurements.grid[v, i] = 90.0 + i
    return PhaseResult.CONTINUE


def attachments(test):
    """Attachments: exercises Attachment.size, absent before 1.5.2."""
    test.attach("cal", b"cal ok\\n", mimetype="text/plain")
    test.attach("blob", bytes(bytearray(range(64))), mimetype="application/octet-stream")
    return PhaseResult.CONTINUE


@htf.PhaseOptions(repeat_limit=4)
@htf.measures(htf.Measurement("tries").with_validator(V.in_range(maximum=5)))
def repeating(test):
    """PhaseResult.REPEAT -> retry_count on each attempt."""
    _tries["n"] += 1
    if _tries["n"] < 3:
        return PhaseResult.REPEAT
    test.measurements.tries = _tries["n"]
    return PhaseResult.CONTINUE


@htf.PhaseOptions(run_if=lambda: False)
def never_runs(test):
    """Skipped via run_if; must not appear in the stream."""
    return PhaseResult.CONTINUE


@htf.measures(
    htf.Measurement("leak").with_units(units.AMPERE).with_validator(V.in_range(maximum=0.003))
)
def failing(test):
    """Deliberate FAIL, with is_decisive on the breaching validator."""
    test.measurements.leak = 0.0074
    return PhaseResult.CONTINUE


test = htf.Test(
    all_validators, dimensioned, attachments, repeating, never_runs, failing,
    test_name="version compat probe",
)
test.execute(test_start=lambda: "SN-COMPAT")
'''

# Fields that legitimately differ run-to-run or across OpenHTF releases.
# `description` on an axis is populated by old OpenHTF and blank on new; it is
# additive, so it is normalized rather than treated as a difference.
#
# `logs` is scrubbed from the whole-event comparison because OpenHTF's own
# DEBUG chatter differs by release (24 records on 1.3.0 vs 35 on 1.6.1 for the
# probe below). Scrubbing it wholesale would also hide a genuine loss of USER
# log records, so check_logs() compares those separately.
VOLATILE = {
    "start_time_millis", "end_time_millis", "timestamp_millis", "timestamp",
    "path", "logs", "prompt_id", "duration_ms",
}

# Log records emitted by OpenHTF itself rather than by user phase code. Their
# volume and wording vary by release and carry no test data.
FRAMEWORK_LOG_SOURCES = (
    "phase_executor.py", "threads.py", "test_state.py", "test_descriptor.py",
    "phase_group.py", "phase_collections.py", "__init__.py",
)


def user_logs(events):
    """User-origin log records: (level, message), framework chatter dropped.

    Filters on DEBUG rather than source file: OpenHTF attributes its own
    "Executing phase ..." DEBUG lines to the *user's* module (and rewords them
    between releases), so a source-based filter cannot separate them. User
    phase code logs at INFO or above.
    """
    out = []
    for event in events:
        if event.get("type") != "test_end":
            continue
        for record in event.get("logs") or []:
            if record.get("level") == "DEBUG":
                continue
            source = record.get("source") or ""
            if source.endswith(FRAMEWORK_LOG_SOURCES):
                continue
            out.append((record.get("level"), record.get("message")))
    return out

EXPECTED_PHASES = [
    # FAIL, not PASS: `unset_one` is deliberately never assigned, and OpenHTF
    # fails a phase with an UNSET measurement. Keeps UNSET on the code path.
    ("all_validators", "FAIL", 0),
    ("dimensioned", "PASS", 0),
    ("attachments", "PASS", 0),
    ("repeating", "SKIP", 0),
    ("repeating", "SKIP", 1),
    ("repeating", "PASS", 2),
    ("failing", "FAIL", 0),
]


def scrub(o):
    if isinstance(o, dict):
        return {k: scrub(v) for k, v in o.items() if k not in VOLATILE}
    if isinstance(o, list):
        return [scrub(x) for x in o]
    return o


def normalize(event):
    event = scrub(json.loads(json.dumps(event)))
    if event.get("type") == "phase_end":
        for m in event.get("measurements") or []:
            axis = m.get("x_axis")
            if isinstance(axis, dict):
                axis.pop("description", None)
    return event


def run_connector(python, connector_path, probe_path):
    out = subprocess.run(
        [python, connector_path, probe_path],
        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=300,
    )
    events = []
    for line in out.stdout.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except ValueError:
                pass
    return events, out


def check(events, label, failures):
    def fail(msg):
        failures.append(f"[{label}] {msg}")

    warnings = [e["message"] for e in events if e.get("type") == "warning"]
    if warnings:
        fail(f"unexpected warnings: {warnings}")

    if not [e for e in events if e.get("type") == "test_end"]:
        fail("no test_end event — the run did not complete")
        return

    phase_ends = [e for e in events if e.get("type") == "phase_end"]
    got = [(e["name"], e["outcome"], e.get("retry_count")) for e in phase_ends]
    if got != EXPECTED_PHASES:
        fail(f"phase sequence mismatch:\n     got {got}\n     want {EXPECTED_PHASES}")

    if any(e["name"] == "never_runs" for e in phase_ends):
        fail("run_if=False phase was reported")

    # end_time_millis: absent before the fix on old OpenHTF, where the
    # connector hooks _set_phase_outcome, which runs one step before the
    # framework stamps the timestamp.
    missing = [e["name"] for e in phase_ends if not e.get("end_time_millis")]
    if missing:
        fail(f"phases missing end_time_millis: {missing}")

    # Attachment.size — the 1.5.2 rename this test exists for.
    sizes = {e["name"]: e.get("size") for e in events if e.get("type") == "attachment"}
    if sizes.get("cal") != 7 or sizes.get("blob") != 64:
        fail(f"attachment sizes wrong: {sizes}")

    by_phase = {}
    for e in phase_ends:
        by_phase.setdefault(e["name"], e)

    measurements = {m["name"]: m for m in by_phase["all_validators"]["measurements"]}
    for name, count in (("v_in", 2), ("lo_only", 1), ("hi_only", 1), ("fw", 1),
                        ("rev", 1), ("cur", 2), ("s_val", 1), ("b_val", 1)):
        vals = measurements.get(name, {}).get("validators")
        if not vals or len(vals) != count:
            fail(f"{name}: expected {count} validator(s), got {vals}")

    ramp = {m["name"]: m for m in by_phase["dimensioned"]["measurements"]}["ramp"]
    if not ramp.get("is_multidim") or ramp.get("x_axis", {}).get("data") != [0, 1, 2, 3]:
        fail(f"dimensioned x_axis wrong: {ramp.get('x_axis')}")
    if ramp.get("y_axis", [{}])[0].get("data") != [40.0, 41.0, 42.0, 43.0]:
        fail(f"dimensioned y_axis wrong: {ramp.get('y_axis')}")

    leak = {m["name"]: m for m in by_phase["failing"]["measurements"]}["leak"]
    if not any(v.get("is_decisive") for v in leak.get("validators") or []):
        fail(f"failing measurement lost is_decisive: {leak.get('validators')}")

    # User log records must survive on every release. OpenHTF's own DEBUG
    # volume differs, hence the framework-source filter.
    if ("INFO", "user log marker") not in user_logs(events):
        fail(f"user log record lost: {user_logs(events)}")


def main():
    write_golden = "--write-golden" in sys.argv
    pythons = [a for a in sys.argv[1:] if not a.startswith("-")] or [sys.executable]
    with tempfile.TemporaryDirectory() as tmp:
        probe = os.path.join(tmp, "probe_main.py")
        with open(probe, "w") as fh:
            fh.write(PROBE)

        # Copy the connector out of its source tree under a different name.
        # Python puts a script's own directory first on sys.path, so running
        # `python .../connector/openhtf.py` makes `import openhtf` resolve to
        # the connector itself. The CLI writes it to a temp dir, so this
        # mirrors production rather than working around it.
        connector = os.path.join(tmp, "_tp_connector.py")
        with open(os.path.abspath(CONNECTOR)) as src, open(connector, "w") as dst:
            dst.write(src.read())

        failures = []
        streams = {}
        logs = {}
        for python in pythons:
            # importlib.metadata first: pkg_resources is gone from
            # setuptools >= 81, and a silent "unknown" here would let a
            # matrix leg testing nothing still report success.
            version = subprocess.run(
                [python, "-c",
                 "import sys\n"
                 "try:\n"
                 "    from importlib.metadata import version as v\n"
                 "except ImportError:\n"
                 "    from pkg_resources import get_distribution\n"
                 "    v = lambda n: get_distribution(n).version\n"
                 "sys.stdout.write(v('openhtf'))"],
                capture_output=True, text=True,
            ).stdout.strip()
            if not version:
                failures.append(
                    f"[{python}] could not determine the installed OpenHTF "
                    f"version — refusing to report a pass for an unknown build")
                continue
            label = f"openhtf {version}"
            print(f"=== {label} ({python})")

            events, proc = run_connector(python, connector, probe)
            if not events:
                failures.append(f"[{label}] connector produced no events\n{proc.stderr[-2000:]}")
                continue
            check(events, label, failures)
            streams[label] = [normalize(e) for e in events]
            logs[label] = user_logs(events)
            print(f"    {len(events)} events")

        # Golden stream: CI runs one OpenHTF version per matrix job, so the
        # cross-version comparison below never fires there. Comparing each run
        # against a committed reference gives a single job the same power.
        # Regenerate deliberately with --write-golden when the event schema
        # changes on purpose.
        if streams:
            first_label = list(streams)[0]
            actual = {"events": streams[first_label], "user_logs": logs[first_label]}
            if write_golden:
                with open(GOLDEN, "w") as fh:
                    json.dump(actual, fh, indent=2, sort_keys=True)
                    fh.write("\n")
                print(f"    wrote golden from {first_label}")
            elif os.path.exists(GOLDEN):
                with open(GOLDEN) as fh:
                    golden = json.load(fh)
                for label in streams:
                    # Round-trip through JSON: tuples become lists on load, so
                    # an in-memory tuple would never compare equal to golden.
                    run = json.loads(json.dumps(
                        {"events": streams[label], "user_logs": logs[label]}))
                    if run == golden:
                        continue
                    if len(run["events"]) != len(golden["events"]):
                        failures.append(
                            f"[{label} vs golden] event count "
                            f"{len(run['events'])} != {len(golden['events'])}")
                        continue
                    if run["user_logs"] != golden["user_logs"]:
                        failures.append(
                            f"[{label} vs golden] user logs differ:\n"
                            f"       {run['user_logs']}\n"
                            f"       {golden['user_logs']}")
                    for i, (a, b) in enumerate(zip(run["events"], golden["events"])):
                        if a != b:
                            detail = "\n".join(
                                f"       {k}: {json.dumps(a.get(k))[:120]}"
                                f" != {json.dumps(b.get(k))[:120]}"
                                for k in sorted(set(a) | set(b)) if a.get(k) != b.get(k)
                            )
                            failures.append(
                                f"[{label} vs golden] event #{i} "
                                f"({a.get('type')}) differs:\n{detail}")

        # Cross-version: every run must produce the same stream.
        labels = list(streams)
        for other in labels[1:]:
            if logs.get(labels[0]) != logs.get(other):
                failures.append(
                    f"[{labels[0]} vs {other}] user log records differ:\n"
                    f"       {logs.get(labels[0])}\n"
                    f"       {logs.get(other)}")
            first = streams[labels[0]]
            second = streams[other]
            if len(first) != len(second):
                failures.append(
                    f"[{labels[0]} vs {other}] event count {len(first)} != {len(second)}")
                continue
            for i, (a, b) in enumerate(zip(first, second)):
                if a != b:
                    keys = sorted(set(a) | set(b))
                    detail = "\n".join(
                        f"       {k}: {json.dumps(a.get(k))[:120]}"
                        f" != {json.dumps(b.get(k))[:120]}"
                        for k in keys if a.get(k) != b.get(k)
                    )
                    failures.append(
                        f"[{labels[0]} vs {other}] event #{i} ({a.get('type')}) differs:\n{detail}")

        if failures:
            print("\nFAILED:")
            for f in failures:
                print(f"  - {f}")
            return 1
        print(f"\nOK — {len(labels)} interpreter(s), identical event streams")
        return 0


if __name__ == "__main__":
    sys.exit(main())
