#!/usr/bin/env python3
"""The CLI must not upload a run twice when the test carries its own callback.

A procedure migrating off the self-managed Python integration still has
`test.add_output_callbacks(upload(...))` in main.py. That callback POSTs the
record over HTTP while the CLI uploads the same execution through its own
queue, so one test produces two runs on the dashboard. The connector drops it
and says so, rather than expecting every procedure to be edited before its
first `tofupilot run`.

Requires openhtf. `tofupilot` is NOT required: the test fakes a callback whose
module is `tofupilot.openhtf.upload`, which is what the connector matches on.

Usage::

    python openhtf_no_double_upload.py [python ...]
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

# `_FakeUpload.__module__` is forced to the real callback's module, so this
# exercises the connector's actual matching rule without installing the SDK.
# `_Unrelated` proves a third-party callback still runs.
PROBE = '''
import functools

import openhtf as htf
from openhtf import PhaseResult


class _FakeUpload:
    """Stands in for tofupilot.openhtf.upload.upload."""

    def __call__(self, record):
        print("TOFUPILOT_UPLOAD_FIRED", flush=True)


_FakeUpload.__module__ = "tofupilot.openhtf.upload"


class _Subclassed(_FakeUpload):
    """A user subclassing `upload` — defined in the user's own module."""


def _fake_upload_fn(record):
    print("TOFUPILOT_UPLOAD_FIRED", flush=True)


_fake_upload_fn.__module__ = "tofupilot.openhtf.upload"


class _Unrelated:
    """A user's own output callback, which must be left alone."""

    def __call__(self, record):
        print("USER_CALLBACK_FIRED", flush=True)


# The module-global an uploader would be bound to by
# `from tofupilot.openhtf import upload`.
upload = _FakeUpload


def _mentions_upload(record):
    """A user's own callback that merely REFERENCES `upload` by name.

    Dropping this would silently discard real data. Naming a symbol is not
    delegating to it.
    """
    if upload is None:  # never true; the point is the reference
        return
    print("USER_CALLBACK_FIRED", flush=True)


class _Holder:
    """Holds an uploader as an attribute — reached via `_holder.up(record)`."""

    def __init__(self):
        self.up = _FakeUpload()


class _Hostile:
    """Attribute access raises — a Mock, a lazy proxy, an ORM row.

    The filter reflects over arbitrary user objects, so it must not let one
    take down the run.
    """

    def __getattr__(self, name):
        raise RuntimeError("attribute access is not allowed on this object")

    def __call__(self, record):
        print("USER_CALLBACK_FIRED", flush=True)


@htf.measures(htf.Measurement("v"))
def phase(test):
    test.measurements.v = 1.0
    return PhaseResult.CONTINUE


test = htf.Test(phase, test_name="double upload probe")
test.add_output_callbacks(_FakeUpload())
test.add_output_callbacks(_Unrelated())

# Indirect shapes. A migrating user writes these: subclassing `upload` to
# change a timeout, functools.partial to bind an api_key, a lambda. Each
# reports the USER's module, not tofupilot's, so a check that only looks at
# the immediate object misses all three and the run uploads twice.
test.add_output_callbacks(_Subclassed())
test.add_output_callbacks(functools.partial(_fake_upload_fn))
_shared = _FakeUpload()
test.add_output_callbacks(lambda record: _shared(record))

# Must NOT be dropped: naming a symbol is not delegating to it, and a
# reflection failure must not cost the user their callback.
test.add_output_callbacks(_mentions_upload)
test.add_output_callbacks(_Hostile())

# KNOWN KEPT, deliberately. Matching an uploader reached through an attribute
# (`_holder.up`) or a multi-variable closure would mean guessing at intent, and
# guessing wrong drops a real callback. A duplicate run is visible and
# fixable; silently discarded data is not. Pinned here so a future attempt to
# widen the filter has to change this line and think about the trade-off.
_holder = _Holder()
test.add_output_callbacks(lambda record: _holder.up(record))
test.execute(test_start=lambda: "SN-DOUBLE")
'''


# Second probe: the REAL package shape, not a fake module name. A stand-in
# `tofupilot` package whose `upload.__init__` raises when TOFUPILOT_API_KEY is
# absent — exactly what the real client does, and the CLI strips that variable
# from the child env. Without source-neutralization this crashes at import
# time, before the registration filter ever runs. It also registers through
# `test.configure(output_callbacks=...)`, which bypasses
# `add_output_callbacks` entirely.
FAKE_TOFUPILOT_UPLOAD = '''
import os


class upload:  # noqa: N801 - mirrors the real class name
    def __init__(self, api_key=None, **kwargs):
        if api_key is None and not os.environ.get("TOFUPILOT_API_KEY"):
            raise Exception(
                "Please set TOFUPILOT_API_KEY or pass api_key to the client"
            )

    def __call__(self, record):
        print("TOFUPILOT_UPLOAD_FIRED", flush=True)
'''

PROBE_REAL_PACKAGE = '''
import openhtf as htf
from openhtf import PhaseResult

# Module-scope construction, the documented migration shape. The API key is
# stripped from the env, so the real class would raise right here.
from tofupilot.openhtf import upload

UPLOADER = upload()


@htf.measures(htf.Measurement("v"))
def phase(test):
    test.measurements.v = 1.0
    return PhaseResult.CONTINUE


test = htf.Test(phase, test_name="real package probe")
# Both registration paths: the filtered one and the configure() bypass.
test.add_output_callbacks(UPLOADER)
test.configure(output_callbacks=[upload()])
test.execute(test_start=lambda: "SN-REAL")
'''


def run(python, connector, probe, extra_env=None):
    env = dict(os.environ)
    env.pop("TOFUPILOT_API_KEY", None)  # the CLI strips it; so does the test
    if extra_env:
        env.update(extra_env)
    # Match production: `run/python.rs` spawns the connector with
    # PYTHONIOENCODING=utf-8, and a fidelity harness must not exercise it
    # under an encoding no station uses. Without both of these, parent and
    # child fall back to the locale codepage on Windows and the connector's
    # own notices round-trip by accident.
    env.setdefault("PYTHONIOENCODING", "utf-8")
    proc = subprocess.run(
        [python, connector, probe],
        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=300,
        encoding="utf-8", env=env,
    )
    events = []
    for line in proc.stdout.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except ValueError:
                pass
    return events, proc.stdout, proc.stderr


def main():
    pythons = [a for a in sys.argv[1:] if not a.startswith("-")] or [sys.executable]
    failures = []

    with tempfile.TemporaryDirectory() as tmp:
        probe = os.path.join(tmp, "probe_main.py")
        # Explicit UTF-8 everywhere below. Python's default text encoding
        # is the locale codepage on Windows (cp1252), so a probe containing
        # any non-ASCII character — an em dash in a docstring is enough — is
        # written in cp1252 and then read back as UTF-8 by the interpreter
        # running it, which fails with a SyntaxError before the connector is
        # reached.
        with open(probe, "w", encoding="utf-8") as fh:
            fh.write(PROBE)
        # Copied out of its source tree: Python puts a script's own directory
        # first on sys.path, so running connector/openhtf.py directly makes
        # `import openhtf` resolve to the connector itself.
        connector = os.path.join(tmp, "_tp_connector.py")
        with open(os.path.abspath(CONNECTOR), encoding="utf-8") as src, \
                open(connector, "w", encoding="utf-8") as dst:
            dst.write(src.read())

        for python in pythons:
            print(f"=== {python}")
            before = len(failures)
            events, stdout, stderr = run(python, connector, probe)

            test_ends = [e for e in events if e.get("type") == "test_end"]
            if len(test_ends) != 1:
                failures.append(f"[{python}] expected 1 test_end, got {len(test_ends)}")

            # Exactly one: the documented known-kept shape (an uploader
            # reached through an attribute). More means the filter regressed
            # and a migrated procedure uploads twice.
            fired = stdout.count("TOFUPILOT_UPLOAD_FIRED")
            if fired != 1:
                failures.append(
                    f"[{python}] expected 1 uploader call (the known-kept "
                    f"attribute-delegation lambda), got {fired}")

            # Three must survive: a plain user callback, one that merely names
            # `upload`, and one whose attribute access raises. Dropping any of
            # them is silent data loss.
            kept = stdout.count("USER_CALLBACK_FIRED")
            if kept != 3:
                failures.append(
                    f"[{python}] expected 3 surviving user callbacks "
                    f"(plain, mentions-upload, hostile-getattr), got {kept} — "
                    f"the filter is dropping real callbacks")

            warnings = [
                e["message"] for e in events
                if e.get("type") == "warning" and "upload callback" in e.get("message", "")
            ]
            # One per dropped uploader: direct, subclass, partial, lambda.
            # Asserting the count, not just presence, so catching three of the
            # four still fails.
            if len(warnings) != 4:
                failures.append(
                    f"[{python}] expected 4 dropped-callback notices "
                    f"(direct, subclass, partial, lambda), got {len(warnings)}")

            # A connector that dies at startup fails every count above with
            # a zero and explains none of them. Its traceback is on stderr,
            # which this harness used to drop on the floor.
            if len(failures) > before and stderr.strip():
                failures.append(
                    f"[{python}] connector stderr:\n{stderr[-2000:]}")

            print(f"    {len(events)} events, {len(warnings)} notice(s)")

            # --- Real-package probe: construction crash + configure() bypass.
            pkg_dir = os.path.join(tmp, "fake_site")
            up_dir = os.path.join(pkg_dir, "tofupilot", "openhtf")
            os.makedirs(up_dir, exist_ok=True)
            for rel in ("tofupilot/__init__.py",):
                with open(os.path.join(pkg_dir, rel), "w", encoding="utf-8") as fh:
                    fh.write("")
            with open(os.path.join(up_dir, "__init__.py"), "w", encoding="utf-8") as fh:
                fh.write("from .upload import upload\n")
            with open(os.path.join(up_dir, "upload.py"), "w", encoding="utf-8") as fh:
                fh.write(FAKE_TOFUPILOT_UPLOAD)
            probe_real = os.path.join(tmp, "probe_real.py")
            with open(probe_real, "w", encoding="utf-8") as fh:
                fh.write(PROBE_REAL_PACKAGE)

            before2 = len(failures)
            events2, stdout2, stderr2 = run(
                python, connector, probe_real,
                extra_env={"PYTHONPATH": pkg_dir},
            )

            test_ends2 = [e for e in events2 if e.get("type") == "test_end"]
            if len(test_ends2) != 1:
                failures.append(
                    f"[{python}] real-package probe: expected 1 test_end, got "
                    f"{len(test_ends2)} — construction of upload() without an "
                    f"API key killed the run before a phase ran")

            fired2 = stdout2.count("TOFUPILOT_UPLOAD_FIRED")
            if fired2 != 0:
                failures.append(
                    f"[{python}] real-package probe: uploader fired {fired2} "
                    f"time(s) — the configure() bypass or the registration "
                    f"filter let a real upload through")

            neutralized = [
                e["message"] for e in events2
                if e.get("type") == "warning"
                and "Neutralized the tofupilot upload callback" in e.get("message", "")
            ]
            if not neutralized:
                failures.append(
                    f"[{python}] real-package probe: no neutralization notice "
                    f"— the user was not told their callback is ignored")

            if len(failures) > before2 and stderr2.strip():
                failures.append(
                    f"[{python}] real-package probe, connector stderr:\n"
                    f"{stderr2[-2000:]}")

            print(f"    real-package probe: {len(events2)} events, "
                  f"{len(neutralized)} neutralization notice(s)")

    if failures:
        print("\nFAILED:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print(f"\nOK — {len(pythons)} interpreter(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
