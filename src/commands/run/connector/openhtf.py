"""OpenHTF connector for TofuPilot CLI.

Wraps an OpenHTF test with a JSON-line output callback. Monkey-patches
htf.Test.__init__ to inject the callback and capture phase metadata. When a
phase uses the UserInput plug, emits a `prompt` event with the currently
running phase name so the CLI can route responses back to the right phase.

Usage: python openhtf.py <main.py>
"""
import importlib.util
import json
import os
import select
import signal
import sys
import uuid

import openhtf as htf


def _readline_interruptible():
    """Read one line from stdin, releasing the GIL often enough for the
    SIGTERM/SIGINT handlers (`_exit_on_term`) to fire promptly.

    `sys.stdin.readline()` blocks in a `read(2)` that, on macOS,
    doesn't surface the signal until the syscall returns. Polling via
    `select` with a short timeout lets the interpreter run signal
    handlers between iterations.

    Reads via `os.read` on the underlying fd, NOT `sys.stdin.read`:
    the TextIOWrapper layer can buffer multiple bytes internally even
    when `select` says only one is ready, desyncing the next `select`
    with bytes already drained at the syscall layer.
    """
    fd = sys.stdin.fileno()
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


# Hard-exit on SIGTERM/SIGINT. `os._exit` skips Python finalizers — we
# can't risk OpenHTF catching `KeyboardInterrupt` and turning it into
# an ABORTED test_state, which deadlocks on plug teardown. The CLI
# parent already has everything from the event stream by the time it
# sends SIGTERM (5s before SIGKILL).
def _exit_on_term(_signum, _frame):
    os._exit(130)


def _install_signal_handlers():
    try:
        signal.signal(signal.SIGTERM, _exit_on_term)
        signal.signal(signal.SIGINT, _exit_on_term)
        # Don't auto-restart blocking syscalls on signal — we want
        # `select` to abort with EINTR.
        signal.siginterrupt(signal.SIGTERM, True)
        signal.siginterrupt(signal.SIGINT, True)
    except (ValueError, OSError):
        # Non-main thread or unsupported platform: best-effort only.
        pass
from openhtf import Test
from openhtf.core.measurements import DimensionedMeasuredValue
from openhtf.util import data


class Event:
    """Single source of truth for the connector ↔ Rust NDJSON event types.

    Keep in lockstep with `PythonEvent` in `connector/events.rs`. A rename
    or addition here without a matching Rust-side change surfaces at
    runtime as a serde deserialization error rather than silently
    disappearing.
    """

    # Handshake + lifecycle
    BRIDGE_READY = "bridge_ready"
    TEST_START = "test_start"
    TEST_END = "test_end"

    # Per-phase
    PHASE_BEGIN = "phase_begin"
    PHASE_END = "phase_end"

    # Side-channel
    PROMPT = "prompt"
    ATTACHMENT = "attachment"
    WARNING = "warning"
    MEASUREMENT = "measurement"
    PHASE_LOG = "phase_log"


class Outcome:
    """Canonical outcome string constants. Mirror of `run/outcomes.rs`.

    Every string that shows up in a `phase_end` / `test_end` / measurement
    payload MUST come from here. Keep in lockstep with the Rust side; a
    string drift would be invisible until a consumer acted on a specific
    value.
    """

    PASS = "PASS"
    FAIL = "FAIL"
    ERROR = "ERROR"
    SKIP = "SKIP"
    TIMEOUT = "TIMEOUT"
    ABORTED = "ABORTED"
    # Full set mirrors run/outcomes.rs. OpenHTF emits RETRY when a phase
    # requests re-execution (PhaseResult.REPEAT / retry_limit) and STOP
    # when the orchestrator short-circuits downstream phases after a
    # terminal failure. Missing these constants would mean a string
    # compared against Outcome.PASS / Outcome.FAIL would silently miss
    # a real OpenHTF outcome value.
    RETRY = "RETRY"
    STOP = "STOP"


def _emit(event):
    print(json.dumps(event, default=str), flush=True)


# Server enforces `log.source_file` length 1..=200 (drizzle constraint
# `log_source_file_len`). Sanitize once at the edge so both the live
# `phase_log` stream and the post-hoc `phase_end` batch produce readable,
# bounded paths.
#
# Steps (in order):
# 1. Strip Windows long-path prefix (`\\?\`).
# 2. If the path lives inside `procedure_dir`, make it relative — the
#    deployment dir prefix (`C:\Users\...\.tofupilot\deployments\<uuid>`)
#    is noise; the user wants `phases/binaries.py`. Paths outside
#    (stdlib, site-packages traceback frames) keep their absolute form.
# 3. Trim, fall back to "unknown" on empty.
# 4. Clamp to the trailing 200 chars.
_SOURCE_FILE_MAX = 200


def _strip_long_path_prefix(s):
    return s[4:] if s.startswith("\\\\?\\") else s


def _sanitize_source_file(raw, procedure_dir=None):
    if not raw:
        return "unknown"
    s = _strip_long_path_prefix(str(raw))

    if procedure_dir:
        dir_clean = _strip_long_path_prefix(str(procedure_dir)).rstrip("/\\")
        if dir_clean and s.startswith(dir_clean):
            s = s[len(dir_clean):].lstrip("/\\")

    s = s.strip()
    if not s:
        return "unknown"
    if len(s) > _SOURCE_FILE_MAX:
        s = s[-_SOURCE_FILE_MAX:]
    return s


# Phases whose `phase_end` we've already emitted live from the `finalize`
# wrapper, keyed by (phase_name, retry_count). The post-hoc output
# callback checks this set so it doesn't re-emit what the live wrapper
# already sent. Without dedup, downstream (Rust event_router) would see
# two `PhaseComplete` events per phase and the agent-protocol audit
# fails on duplicate phase_finished.
_phase_end_emitted = set()


# Procedure dir captured at test install time so `_extract_logs` and the
# live `_LiveLogHandler` share the same relativization base. Set once in
# `_install_test_patches`; both consumers read it via _get_procedure_dir.
_procedure_dir = None


def _set_procedure_dir(path):
    global _procedure_dir
    _procedure_dir = path


def _get_procedure_dir():
    return _procedure_dir


def _patch_once(owner, attr, wrapper):
    """Install `wrapper` as `owner.attr` exactly once.

    Idempotent across repeated imports of this connector in the same
    interpreter (e.g. when a user test re-imports `openhtf` lazily).
    The sentinel lives on the current value of `owner.attr`, not on
    `owner` itself — that way it survives even if something else in the
    interpreter has touched `owner` (classes, modules) and lets multiple
    attributes of the same owner be patched independently.
    Returns True if patched this call, False if it was already patched.
    """
    current = getattr(owner, attr, None)
    if current is not None and getattr(current, "_tofupilot_patched", False):
        return False
    setattr(wrapper, "_tofupilot_patched", True)
    setattr(owner, attr, wrapper)
    return True


# ---------------------------------------------------------------------------
# Test state access — used by CliUserInput to annotate prompts with the
# currently-running phase name.
# ---------------------------------------------------------------------------

def _get_executing_test():
    tests = list(Test.TEST_INSTANCES.values())
    if not tests:
        return None
    test = tests[0]
    return test.state if test.state is not None else None


# ---------------------------------------------------------------------------
# Output callback (called by OpenHTF when test completes)
# ---------------------------------------------------------------------------

def _emit_phase_end(phase, phase_docstrings=None, retry_count=0):
    """Emit a single `phase_end` event from a finalized PhaseRecord.

    Shared by the live `PhaseState.finalize` wrapper and the post-hoc
    `_output_callback`. The live path runs first for each phase; the
    callback path checks `_phase_end_emitted` to avoid double-send.

    Returns the `(phase_name, retry_count)` key used for dedup, or None
    when the phase was a trigger_phase we deliberately skip.
    """
    phase_docstrings = phase_docstrings or {}
    phase_name = phase.name
    if hasattr(phase, "descriptor") and hasattr(phase.descriptor, "func"):
        phase_name = phase.descriptor.func.__name__

    # Skip framework-internal trigger phases. `trigger_phase` is openhtf's
    # default name; `_identify_trigger_phase` is the wrapper this connector
    # injects when a user supplies a PhaseDescriptor as `test_start` (so we
    # can run the identify-unit handshake before their phase). Both are
    # infrastructure, not user code — emitting them as run phases would
    # show framework plumbing in the operator-UI table.
    if phase_name in ("trigger_phase", "_identify_trigger_phase"):
        return None

    key = (phase_name, retry_count)
    if key in _phase_end_emitted:
        return key
    _phase_end_emitted.add(key)

    measurements = _extract_measurements(phase)
    if phase.measurements:
        for m in measurements:
            validators = _extract_validators(phase.measurements.get(m["name"]))
            if validators:
                m["validators"] = validators

    phase_outcome = phase.outcome.name if phase.outcome else (
        Outcome.FAIL if any(m.get("outcome") == Outcome.FAIL for m in measurements) else Outcome.PASS
    )

    docstring = (
        (phase.codeinfo.docstring if phase.codeinfo else None)
        or phase_docstrings.get(phase_name)
    )

    _emit({
        "type": Event.PHASE_END,
        "name": phase_name,
        "outcome": phase_outcome,
        "start_time_millis": phase.start_time_millis,
        "end_time_millis": phase.end_time_millis,
        "measurements": measurements,
        "retry_count": retry_count,
        "docstring": docstring,
    })
    return key


def _output_callback(test_record, phase_docstrings=None):
    outcome = test_record.outcome.name if test_record.outcome else Outcome.ERROR
    phase_docstrings = phase_docstrings or {}

    # Attachment dir from Rust-provided queue ID
    queue_id = os.environ.get("TOFUPILOT_QUEUE_ID", "")
    att_dir = os.path.join(
        os.path.expanduser("~"), ".tofupilot", "attachments",
        queue_id if queue_id else "tmp",
    )
    used_names = set()

    # Single pass over phases: emit phase_end (deduped against the live
    # `finalize` wrapper) + save attachments. When the live wrapper fired
    # first (normal case) the `_emit_phase_end` call is a no-op and we
    # only hit the attachment branch below. When the live wrapper missed
    # (teardown phase that never finalized, early test abort, etc.) this
    # is the safety net.
    phase_name_count = {}
    for phase in test_record.phases:
        # Skip framework-internal trigger phases (see `_emit_phase_end`).
        # The `phases[0]` guard preserves the original intent — a user phase
        # that happens to be named "trigger_phase" mid-test is still emitted.
        # `_identify_trigger_phase` is our injected wrapper and is always
        # the test_start, so we drop it unconditionally.
        if phase.name == "trigger_phase" and phase == test_record.phases[0]:
            continue
        if phase.name == "_identify_trigger_phase":
            continue

        # Phase name resolution (mirror of _emit_phase_end).
        phase_name = phase.name
        if hasattr(phase, "descriptor") and hasattr(phase.descriptor, "func"):
            phase_name = phase.descriptor.func.__name__

        # Retry tracking — index within this test's retries of this phase.
        retry_count = phase_name_count.get(phase_name, 0)
        phase_name_count[phase_name] = retry_count + 1

        _emit_phase_end(phase, phase_docstrings, retry_count=retry_count)

        # Attachments
        for att_name, att in (phase.attachments or {}).items():
            try:
                os.makedirs(att_dir, exist_ok=True)
                safe = att_name.replace("/", "_").replace("\\", "_")
                if safe in used_names:
                    base, ext = os.path.splitext(safe)
                    n = 1
                    while f"{base}_{n}{ext}" in used_names:
                        n += 1
                    safe = f"{base}_{n}{ext}"
                used_names.add(safe)
                path = os.path.join(att_dir, safe)
                with open(path, "wb") as f:
                    f.write(att.data)
                _emit({
                    "type": Event.ATTACHMENT,
                    "name": att_name,
                    "path": path,
                    "mimetype": att.mimetype or "application/octet-stream",
                    "size": att.size,
                })
            except Exception as e:
                _emit({"type": Event.WARNING, "message": f"Attachment {att_name}: {e}"})

    # Capture final test_record.metadata so user-phase mutations
    # (e.g. `test.metadata["serial_number"] = read_from_eeprom()`)
    # win over the CLI-injected values. Filter to v2 API keys; other
    # user metadata stays on the Python side, not promoted to run
    # fields.
    metadata = test_record.metadata or {}
    final_metadata = {}
    for k in ("serial_number", "part_number", "revision_number",
              "batch_number"):
        v = metadata.get(k)
        if isinstance(v, str) and v:
            final_metadata[k] = v
    sub_units_raw = metadata.get("sub_units")
    if isinstance(sub_units_raw, list) and sub_units_raw:
        if isinstance(sub_units_raw[0], str):
            final_metadata["sub_units"] = sub_units_raw
        elif isinstance(sub_units_raw[0], dict):
            extracted = [
                item.get("serial_number")
                for item in sub_units_raw
                if isinstance(item, dict) and item.get("serial_number")
            ]
            if extracted:
                final_metadata["sub_units"] = extracted

    # Test-level event
    _emit({
        "type": Event.TEST_END,
        "outcome": outcome,
        "dut_id": test_record.dut_id,
        "test_name": metadata.get("test_name", ""),
        "start_time_millis": test_record.start_time_millis,
        "end_time_millis": test_record.end_time_millis,
        "logs": _extract_logs(test_record),
        "docstring": test_record.code_info.docstring if test_record.code_info else None,
        "metadata": final_metadata,
    })


# ---------------------------------------------------------------------------
# Measurement extraction
# ---------------------------------------------------------------------------

def _unit_suffix(unit):
    if unit is None:
        return None
    return unit.suffix if hasattr(unit, "suffix") else str(unit)


def _extract_measurements(phase):
    if not phase.measurements:
        return []

    result = []
    for name, meas in phase.measurements.items():
        m = {"name": name}

        if meas.outcome is not None:
            m["outcome"] = meas.outcome.name

        units = _unit_suffix(meas.units)
        if units:
            m["units"] = units

        mv = meas.measured_value
        if mv is not None and mv.is_value_set:
            if isinstance(mv, DimensionedMeasuredValue):
                _extract_multidim(m, mv, meas)
            else:
                val = mv.value
                if isinstance(val, bool):
                    m["measured_value"] = val
                else:
                    try:
                        m["measured_value"] = float(val)
                    except (TypeError, ValueError):
                        m["measured_value"] = str(val)

        if meas.docstring:
            m["docstring"] = meas.docstring

        result.append(m)
    return result


def _extract_multidim(m, mv, meas):
    rows = [list(row) for row in mv.value]
    dims = meas.dimensions or []

    if len(dims) == 1 and rows:
        m["is_multidim"] = True
        x_axis = {"data": [row[0] for row in rows]}
        x_unit = _unit_suffix(dims[0])
        if x_unit:
            x_axis["units"] = x_unit
        # Prefer an explicit Dimension(description=..., unit=...) over the
        # raw UnitDescriptor name (e.g. "second [unit of time]"), which is
        # OpenHTF's own metadata label and reads as noise on a chart axis.
        # Fall back to the measurement-side dim object's name only if it
        # differs from the unit's verbose name.
        x_desc = getattr(dims[0], "description", None) or None
        if not x_desc:
            x_name = getattr(dims[0], "name", None)
            unit_name = getattr(getattr(dims[0], "_unit", None), "name", None)
            if x_name and x_name != unit_name:
                x_desc = x_name
        if x_desc:
            x_axis["description"] = x_desc
        m["x_axis"] = x_axis

        y_axis = {"data": [row[-1] for row in rows]}
        y_unit = _unit_suffix(meas.units)
        if y_unit:
            y_axis["units"] = y_unit
        # Y has no separate dim object in OpenHTF, so its "label" is the
        # measurement name itself. Mirror it onto the y axis description
        # so the chart legend shows the user's chosen name.
        if getattr(meas, "name", None):
            y_axis["description"] = meas.name
        m["y_axis"] = [y_axis]
        m.pop("units", None)
    else:
        m["measured_value"] = rows
        m["is_multidim"] = True
        if dims:
            dim_units = [_unit_suffix(d) for d in dims]
            val_unit = _unit_suffix(meas.units)
            if val_unit:
                dim_units.append(val_unit)
            m["units"] = [u for u in dim_units if u]


# ---------------------------------------------------------------------------
# Validator extraction
# Uses private attrs (_minimum, _maximum, _expected) because OpenHTF has
# no public API for validator internals. All known OpenHTF tools do this.
# ---------------------------------------------------------------------------

def _eval_outcome(validator, value):
    """Evaluate a single validator against a value."""
    if value is None:
        return None
    try:
        return Outcome.PASS if validator(value) else Outcome.FAIL
    except Exception:
        return Outcome.FAIL


def _compare(op, value, expected):
    """Evaluate a comparison operator against a scalar value."""
    if value is None or expected is None:
        return None
    try:
        if op == ">=":
            return Outcome.PASS if value >= expected else Outcome.FAIL
        if op == "<=":
            return Outcome.PASS if value <= expected else Outcome.FAIL
        if op == "==":
            return Outcome.PASS if value == expected else Outcome.FAIL
        if op == "matches":
            import re
            return Outcome.PASS if re.search(str(expected), str(value)) else Outcome.FAIL
    except Exception:
        pass
    return Outcome.FAIL


def _val_entry(expression, operator, expected_value, value, is_decisive):
    """Build a single validator entry with per-limit outcome.

    `is_decisive` semantics matches the v1 server-side parser
    (`apps/web/server/trpc/core/runs/validators.ts::parseOpenHTFValidator`):
      * Marginal validators (`is_decisive=False`) always set the field
        explicitly so the dashboard can render them as warning bands.
      * Decisive validators (hard limits) only set the field when they
        fail — server defaults to `true` otherwise. Always-true on PASS
        would clutter the wire and contradicts the v1 parser shape.

    `expression` is only forwarded when it adds information beyond the
    structured `operator` + `expected_value` pair. For per-limit rows
    derived from InRange/Equals/Regex the parent `str(v)` repeats the
    full multi-bound expression (e.g. `4.8 <= Marginal:4.95 <= x <= ...`)
    on every row, which the dashboard renders four times instead of one
    structured row per limit; pass `expression=None` from those callers.
    Pass an explicit string for WithinPercent (`'x' is within N% of V`)
    or fully custom validators (no `operator`).
    """
    entry = {
        "operator": operator,
        "expected_value": expected_value,
    }
    if expression is not None:
        entry["expression"] = expression
    outcome = _compare(operator, value, expected_value)
    if outcome is not None:
        entry["outcome"] = outcome
    if not is_decisive:
        entry["is_decisive"] = False
    elif outcome == Outcome.FAIL:
        entry["is_decisive"] = True
    return entry


_CUSTOM_EXPR_CACHE = {}


def _custom_expression(v):
    """Best-effort readable expression for unrecognized validators.

    Resolution order:
      1. `__str__` if the class overrides `object.__str__` (most well-
         written `ValidatorBase` subclasses do).
      2. `inspect.getsource(...)` for lambdas / plain functions, sliced
         to the lambda body. Strips trailing punctuation/whitespace.
      3. `__name__` (function name) for named callables.
      4. `"custom validator"` last-resort sentinel — never a memory
         address that would change per-run.

    Cached by `id(v)` so repeated extraction (e.g. multidim sweep with
    one validator covering thousands of points) doesn't re-run
    `inspect.getsource` per measurement.
    """
    cache_key = id(v)
    cached = _CUSTOM_EXPR_CACHE.get(cache_key)
    if cached is not None:
        return cached
    result = _custom_expression_uncached(v)
    _CUSTOM_EXPR_CACHE[cache_key] = result
    return result


def _custom_expression_uncached(v):
    import inspect
    # 1. Class-defined __str__
    cls = type(v)
    if cls.__str__ is not object.__str__:
        try:
            s = str(v)
            if s and "<function" not in s and "0x" not in s:
                return s
        except Exception:
            pass
    # 2. Lambda / function: slice source
    try:
        src = inspect.getsource(v).strip()
        # Common case: `with_validator(lambda c: c.lower() in (...))`.
        # Extract the `lambda ...` substring up to the matching close
        # paren of the surrounding call. Cheap heuristic — find first
        # `lambda` token and take to end-of-line stripped of trailing
        # `)` and `,`.
        idx = src.find("lambda ")
        if idx >= 0:
            tail = src[idx:].split("\n", 1)[0].rstrip()
            tail = tail.rstrip(",")
            depth = 0
            cut = len(tail)
            for i, ch in enumerate(tail):
                if ch == "(":
                    depth += 1
                elif ch == ")":
                    if depth == 0:
                        cut = i
                        break
                    depth -= 1
            return tail[:cut].strip().rstrip(",")
        # Plain function: first non-decorator line
        for line in src.splitlines():
            line = line.strip()
            if line.startswith("def "):
                return line.rstrip(":")
    except (OSError, TypeError):
        pass
    # 3. Function name
    name = getattr(v, "__name__", None) or getattr(cls, "__name__", None)
    if name and name != "<lambda>":
        return name
    # 4. Sentinel
    return "custom validator"


def _extract_validators(meas):
    if not meas or not meas.validators:
        return None

    from openhtf.util import validators as V

    mv = meas.measured_value
    has_value = mv is not None and mv.is_value_set
    result = []

    val = mv.value if has_value else None

    def _emit_subvalidator(inner, expr):
        """Common path for DimensionPivot / ConsistentEndDimensionPivot."""
        if isinstance(inner, (V.InRange, V.AllInRangeValidator)):
            if inner._minimum is not None:
                result.append(_val_entry(None, ">=", inner._minimum, val, True))
            if inner._maximum is not None:
                result.append(_val_entry(None, "<=", inner._maximum, val, True))
        elif isinstance(inner, V.Equals):
            result.append(_val_entry(None, "==", inner._expected, val, True))
        elif isinstance(inner, V.AllEqualsValidator):
            result.append(_val_entry(None, "==", inner.spec, val, True))
        elif isinstance(inner, V.RegexMatcher):
            result.append(_val_entry(None, "matches", inner.regex, val, True))
        else:
            result.append({"expression": expr, "outcome": _eval_outcome(v, val)})

    for v in meas.validators:
        expr = str(v) if type(v).__str__ is not object.__str__ else _custom_expression(v)

        if isinstance(v, (V.InRange, V.AllInRangeValidator)):
            if v._minimum is not None and v._maximum is not None and v._minimum == v._maximum:
                result.append(_val_entry(None, "==", v._minimum, val, True))
            else:
                if v._minimum is not None:
                    result.append(_val_entry(None, ">=", v._minimum, val, True))
                    if getattr(v, "_marginal_minimum", None) is not None:
                        result.append(_val_entry(None, ">=", v._marginal_minimum, val, False))
                if v._maximum is not None:
                    result.append(_val_entry(None, "<=", v._maximum, val, True))
                    if getattr(v, "_marginal_maximum", None) is not None:
                        result.append(_val_entry(None, "<=", v._marginal_maximum, val, False))
        elif isinstance(v, V.Equals):
            result.append(_val_entry(None, "==", v._expected, val, True))
        elif isinstance(v, V.AllEqualsValidator):
            result.append(_val_entry(None, "==", v.spec, val, True))
        elif isinstance(v, V.RegexMatcher):
            result.append(_val_entry(None, "matches", v.regex, val, True))
        elif isinstance(v, V.WithinPercent):
            # Clean expression: WithinPercent.__str__ injects the
            # literal "Marginal: None%" when marginal_percent is unset,
            # which the v1 server-side regex parser rejects. Rewrite to
            # match the regex `^'x' is within N% of V$` exactly.
            base_expr = "'x' is within {}% of {}".format(v.percent, v.expected)
            result.append(_val_entry(base_expr, ">=", v.minimum, val, True))
            result.append(_val_entry(base_expr, "<=", v.maximum, val, True))
            if v.marginal_percent is not None:
                result.append(_val_entry(base_expr, ">=", v.marginal_minimum, val, False))
                result.append(_val_entry(base_expr, "<=", v.marginal_maximum, val, False))
        elif isinstance(v, (V.DimensionPivot, V.ConsistentEndDimensionPivot)) and hasattr(v, "_sub_validator"):
            _emit_subvalidator(v._sub_validator, expr)
        else:
            # Unknown validator (custom ValidatorBase subclass or
            # user-supplied lambda). Capture outcome with a readable
            # expression — never the volatile `<function <lambda> at
            # 0x...>` repr that changes per-run and breaks dashboards.
            result.append({"expression": _custom_expression(v), "outcome": _eval_outcome(v, val)})

    return result or None


# ---------------------------------------------------------------------------
# Log extraction
# ---------------------------------------------------------------------------

def _extract_logs(test_record):
    import logging
    pd = _get_procedure_dir()
    return [
        {
            "level": logging.getLevelName(lr.level) if isinstance(lr.level, int) else str(lr.level),
            "message": lr.message,
            "timestamp_millis": lr.timestamp_millis,
            "source": _sanitize_source_file(lr.source, pd),
            "lineno": lr.lineno,
        }
        for lr in test_record.log_records
    ]


# ---------------------------------------------------------------------------
# User input plug (prompts via TUI, blocks on stdin for response)
# ---------------------------------------------------------------------------

class CliUserInput(htf.plugs.BasePlug):
    def prompt(self, message, text_input=False, timeout_s=None, image_url=None, **kwargs):
        prompt_id = str(uuid.uuid4())
        phase_name = None
        try:
            test_state = _get_executing_test()
            running = getattr(test_state, "running_phase_state", None) if test_state else None
            phase_name = getattr(running, "name", None) if running else None
        except Exception:
            # Surface the failure so OpenHTF version skew / plugin interference
            # doesn't hide as a mysterious orphan prompt with `phase_name=None`.
            # Do NOT re-raise — the prompt must still fire.
            import traceback
            traceback.print_exc()
        _emit({
            "type": Event.PROMPT,
            "prompt_id": prompt_id,
            "phase_name": phase_name,
            "message": message,
            "text_input": text_input,
            "timeout_s": timeout_s,
            "image_url": image_url,
        })
        try:
            line = _readline_interruptible()
            if line:
                resp = json.loads(line)
                return resp.get("response", "")
        except Exception:
            pass
        return ""

    def tearDown(self):
        pass


# ---------------------------------------------------------------------------
# Identify-unit handshake
# ---------------------------------------------------------------------------
#
# The framework owns identify-unit (see
# `crates/execution-engine/src/identify_unit/`). This connector is a
# thin pass-through:
#
#   1. `patched_init` captures `htf.Test(...)` unit kwargs and forwards
#      them on the `test_start` event under `unit_kwargs`.
#   2. Rust runs `execution_engine::identify(...)` with the same
#      `CliIdentifyHost` the YAML path uses, then writes a
#      `set_unit_resolved` line to the connector's stdin.
#   3. We read that line here, write `dut_id` + metadata into the
#      `test_record` via a `test_start` callback so downstream phases
#      can read them, and let OpenHTF run.
#
# Native `prompts.prompt(...)` calls in user code still fire — that's
# the deprecation signal pushing users to delete their custom SN
# prompts and let the framework own identification.
#
#   identify=False  → skip the handshake entirely; user-managed.
#   otherwise       → forward kwargs, await `set_unit_resolved`.

# Field keys forwarded to Rust on `test_start` and read back from
# `set_unit_resolved`. Any other kwargs the user passed to htf.Test
# pass through to OpenHTF untouched.
_UNIT_FIELD_KEYS = ("serial_number", "part_number", "revision_number", "batch_number")


def _await_unit_resolution():
    """Read the next `set_unit_resolved` line from stdin.

    Returns the parsed dict, or `{}` on EOF / parse failure (the run
    proceeds with whatever the user pre-supplied via kwargs; build_request
    falls back to `dut_id` either way). Blocks indefinitely — the operator
    may take time to scan a barcode; the parent CLI's signal handlers cap
    the wait via SIGTERM if the run is aborted.
    """
    try:
        line = _readline_interruptible()
        if not line:
            return {}
        msg = json.loads(line)
        if isinstance(msg, dict) and msg.get("type") == "set_unit_resolved":
            return msg
        return {}
    except Exception:
        import traceback
        traceback.print_exc()
        return {}


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    if len(sys.argv) < 2:
        print("Usage: python openhtf.py <main.py>", file=sys.stderr)
        sys.exit(1)

    _install_signal_handlers()
    test_file = sys.argv[1]
    _emit({"type": Event.BRIDGE_READY})

    # Replace UserInput plug. Idempotent via `_patch_once`.
    try:
        from openhtf.plugs import user_input
        _patch_once(user_input, "UserInput", CliUserInput)
    except ImportError:
        pass

    # Emit a pre-phase `phase_begin` event. OpenHTF has no public per-phase
    # start hook, so we wrap PhaseExecutor._execute_phase_once — the inner
    # call that runs each individual phase attempt. Wrapping the outer
    # `execute_phase` would only fire once per descriptor even when REPEAT
    # runs the phase multiple times; `_execute_phase_once` fires per attempt.
    try:
        from openhtf.core import phase_executor as _pe

        _orig_once = _pe.PhaseExecutor._execute_phase_once

        def _patched_once(self, phase_desc, *args, **kwargs):
            name = getattr(phase_desc, "name", None)
            if name and name not in ("trigger_phase", "_identify_trigger_phase"):
                _emit({"type": Event.PHASE_BEGIN, "name": name})
            return _orig_once(self, phase_desc, *args, **kwargs)

        _patch_once(_pe.PhaseExecutor, "_execute_phase_once", _patched_once)
    except ImportError:
        pass

    # Emit a live `phase_end` per attempt by wrapping PhaseState.finalize.
    # OpenHTF batches phase records into _output_callback at test end,
    # which means web/TUI consumers see every phase as "running" until
    # the whole test completes — a multi-minute test looks frozen. The
    # YAML engine emits phase transitions synchronously; this wrapper
    # brings OpenHTF to parity. Retries bump a per-phase counter so each
    # attempt carries its own retry_count, matching what the post-hoc
    # output callback produces. Dedup via _phase_end_emitted prevents
    # the fallback in _output_callback from double-firing.
    _phase_retry_live = {}
    try:
        from openhtf.core import test_state as _ts

        _orig_finalize = _ts.PhaseState.finalize

        def _patched_finalize(self, *args, **kwargs):
            result = _orig_finalize(self, *args, **kwargs)
            try:
                phase = self.phase_record
                phase_name = phase.name
                if hasattr(phase, "descriptor") and hasattr(phase.descriptor, "func"):
                    phase_name = phase.descriptor.func.__name__
                retry = _phase_retry_live.get(phase_name, 0)
                _phase_retry_live[phase_name] = retry + 1
                # The docstring map the output callback uses isn't in
                # scope here; pass the phase's codeinfo docstring via
                # the helper's own fallback.
                _emit_phase_end(phase, None, retry_count=retry)
            except Exception:
                # Never let a live-emit failure break phase execution —
                # the safety-net output callback will pick this one up.
                import traceback
                traceback.print_exc()
            return result

        _patch_once(_ts.PhaseState, "finalize", _patched_finalize)
    except ImportError:
        pass

    # Live `measurement` events: wrap MeasuredValue.set to emit each
    # write as it happens. Without this, agents only see measurements
    # batched in `phase_end` after the phase completes — a multi-second
    # phase with 50 readings looks frozen until it finishes.
    try:
        from openhtf.core.measurements import MeasuredValue as _MV

        _orig_mv_set = _MV.set

        def _patched_mv_set(self, value):
            result = _orig_mv_set(self, value)
            try:
                phase_name = None
                test_state = _get_executing_test()
                running = getattr(test_state, "running_phase_state", None) if test_state else None
                if running:
                    phase_name = getattr(running, "name", None)
                _emit({
                    "type": Event.MEASUREMENT,
                    "name": getattr(self, "name", None),
                    "value": self._cached_value if hasattr(self, "_cached_value") else value,
                    "phase_name": phase_name,
                })
            except Exception:
                # Never let a live-emit failure break a measurement.
                # The post-hoc `phase_end` is the safety net.
                import traceback
                traceback.print_exc()
            return result

        _patch_once(_MV, "set", _patched_mv_set)
    except ImportError:
        pass

    # Live `attachment` events: wrap PhaseState.attach to emit each
    # attachment as it's added. Without this, agents only see
    # attachments batched in the post-hoc output_callback — slow on
    # heavy attachment phases.
    #
    # We write a temp file so the live event carries a path agents can
    # preview before the run finishes. Files are tracked in
    # `_live_attachment_paths` and removed on normal interpreter exit
    # via atexit. Note: SIGTERM/SIGKILL paths use `os._exit` which
    # skips atexit, so cancelled runs leak temp files into /tmp until
    # the OS reclaims (acceptable; tempfile uses random names so no
    # collisions). The post-hoc upload uses the queued copy under
    # ~/.tofupilot/attachments/<queue_id>/, so deleting these doesn't
    # affect the upload pipeline.
    _live_attachment_paths = []
    try:
        import atexit as _atexit
        def _cleanup_live_attachments():
            for p in _live_attachment_paths:
                try:
                    if os.path.exists(p):
                        os.remove(p)
                except Exception:
                    pass
        _atexit.register(_cleanup_live_attachments)
    except Exception:
        pass

    try:
        import tempfile
        from openhtf.core import test_state as _ts2

        _orig_attach = _ts2.PhaseState.attach

        def _patched_attach(self, name, binary_data, mimetype=None):
            result = _orig_attach(self, name, binary_data, mimetype=mimetype) if mimetype is not None else _orig_attach(self, name, binary_data)
            try:
                rec = self.phase_record.attachments.get(name)
                final_mime = (rec.mimetype if rec else None) or "application/octet-stream"
                phase_name = getattr(self.phase_record, "name", None) or getattr(self, "name", None)
                fd, path = tempfile.mkstemp(prefix="openhtf-att-")
                with os.fdopen(fd, "wb") as f:
                    f.write(binary_data if isinstance(binary_data, (bytes, bytearray)) else str(binary_data).encode("utf-8"))
                _live_attachment_paths.append(path)
                _emit({
                    "type": Event.ATTACHMENT,
                    "name": name,
                    "path": path,
                    "mimetype": final_mime,
                    "size": len(binary_data) if hasattr(binary_data, "__len__") else 0,
                    "phase_name": phase_name,
                    "live": True,
                })
            except Exception:
                import traceback
                traceback.print_exc()
            return result

        _patch_once(_ts2.PhaseState, "attach", _patched_attach)
    except ImportError:
        pass

    # Live `phase_log` events: install a logging handler on the
    # `openhtf` logger that emits per-record. The post-hoc callback
    # still drops the full log batch on `phase_end` for completeness.
    #
    # Filtering: openhtf-internal log lines ("Tearing down all plugs",
    # "Thread finished:", phase_executor lifecycle, etc.) flood the
    # agent stream with framework noise. We accept a record only when
    # `record.pathname` is under the user's procedure_dir tree and
    # outside `.venv/site-packages` and the connector script itself.
    # Python logging captures the caller's frame at log-call site, so
    # `test.logger.info(...)` in user code stamps `main.py`, not
    # openhtf internals — which is the signal we filter on.
    try:
        import logging as _logging
        import datetime as _dt

        procedure_dir = os.path.dirname(os.path.abspath(test_file))
        _set_procedure_dir(procedure_dir)

        class _LiveLogHandler(_logging.Handler):
            def emit(self, record):
                try:
                    phase_name = None
                    test_state = _get_executing_test()
                    running = getattr(test_state, "running_phase_state", None) if test_state else None
                    if running:
                        phase_name = getattr(running, "name", None)

                    # Filter: only accept records originating from the
                    # user's procedure_dir tree, excluding installed
                    # dependencies (.venv / site-packages / connector
                    # itself). `record.pathname` is the caller's source
                    # file (Python's logging captures the frame at the
                    # log-call site), so `test.logger.info(...)` in user
                    # code stamps `main.py`, not openhtf internals. This
                    # drops phase_executor / plug-teardown / framework
                    # lifecycle noise that would otherwise flood the
                    # agent stream (~80% of log volume).
                    pathname = getattr(record, "pathname", "") or ""
                    if not (procedure_dir and pathname.startswith(procedure_dir)):
                        return
                    if (
                        "/.venv/" in pathname
                        or "/site-packages/" in pathname
                        or "/.tofupilot_openhtf.py" in pathname
                    ):
                        return

                    _emit({
                        "type": Event.PHASE_LOG,
                        "level": record.levelname,
                        "message": record.getMessage(),
                        "timestamp": _dt.datetime.fromtimestamp(record.created, _dt.timezone.utc)
                            .strftime("%Y-%m-%dT%H:%M:%S.%fZ"),
                        "phase_name": phase_name,
                        "file": _sanitize_source_file(pathname, procedure_dir),
                        "line": record.lineno,
                    })
                except Exception:
                    pass

        _live_handler = _LiveLogHandler()
        _live_handler.setLevel(_logging.DEBUG)
        # Attach to the openhtf logger only. `configure_logging` sets
        # `openhtf.propagate = False` so records never reach root —
        # attaching to root would receive nothing from `test.logger.*`.
        # User code wanting live streaming should use `test.logger.*`
        # (the openhtf idiom) rather than a top-level
        # `logging.getLogger(__name__)`. The pathname filter then drops
        # framework lifecycle noise emitted from openhtf internals.
        if not getattr(_logging.getLogger("openhtf"), "_tofupilot_live_handler", False):
            _logging.getLogger("openhtf").addHandler(_live_handler)
            setattr(_logging.getLogger("openhtf"), "_tofupilot_live_handler", True)
    except Exception:
        pass

    # Patch Test.__init__ to capture phase names (including PhaseGroup
    # setup/main/teardown phases) and inject the output callback.
    phase_docstrings = {}
    original_init = htf.Test.__init__

    def _unwrap(seq):
        """Resolve a PhaseSequence / list / tuple / None into an iterable."""
        if seq is None:
            return ()
        nodes = getattr(seq, "nodes", None)
        if nodes is not None:
            return nodes
        return seq

    def _flatten(nodes, out):
        for p in nodes:
            # PhaseGroup: recurse into setup / main / teardown in the order
            # OpenHTF executes them.
            if isinstance(p, htf.PhaseGroup):
                _flatten(_unwrap(p.setup), out)
                _flatten(_unwrap(p.main), out)
                _flatten(_unwrap(p.teardown), out)
                continue
            # PhaseDescriptor (what @htf.measures / @htf.plug produce).
            if hasattr(p, "func"):
                name = p.func.__name__
                if p.func.__doc__:
                    phase_docstrings[name] = p.func.__doc__.strip()
            elif callable(p):
                name = getattr(p, "__name__", str(p))
                if getattr(p, "__doc__", None):
                    phase_docstrings[name] = p.__doc__.strip()
            else:
                name = getattr(p, "name", str(p))
            out.append(name)

    def patched_init(self, *phases, **kwargs):
        # Pull connector-only kwargs before the rest reach OpenHTF, which
        # would reject `auto_identify` / `identify` / `revision_number` /
        # `batch_number` as unknown. `part_number` stays in kwargs for
        # backwards compatibility — older Test()s already used it as a
        # connector-level field and the original handler captured it.
        identify_off = kwargs.pop("identify", None) is False
        auto_identify = bool(kwargs.pop("auto_identify", False))
        kwargs_unit = {
            "serial_number": kwargs.pop("serial_number", "") or "",
            "part_number": kwargs.get("part_number", "") or "",
            "revision_number": kwargs.pop("revision_number", "") or "",
            "batch_number": kwargs.pop("batch_number", "") or "",
        }

        names = []
        _flatten(phases, names)

        # Forward unit kwargs + auto_identify to Rust so it can build a
        # `UnitConfig` and run `execution_engine::identify(...)`. Rust
        # replies with a `set_unit_resolved` line on stdin, which we
        # read below before OpenHTF starts running phases.
        _emit({
            "type": Event.TEST_START,
            "test_name": kwargs.get("test_name", ""),
            "procedure_id": kwargs.get("procedure_id", ""),
            "part_number": kwargs.get("part_number", ""),
            "phases": names,
            "identify": not identify_off,
            "auto_identify": auto_identify,
            "unit_kwargs": kwargs_unit,
        })

        original_init(self, *phases, **kwargs)
        self.add_output_callbacks(lambda record: _output_callback(record, phase_docstrings))
        # Stash on the Test instance so the patched `execute` knows
        # whether to wait for `set_unit_resolved`.
        self._tofupilot_identify = not identify_off

    _patch_once(htf.Test, "__init__", patched_init)

    # Wrap `Test.execute` so the framework identify-unit handshake runs
    # before any phase. Reading `set_unit_resolved` here (not in
    # `__init__`) lets us write into `test_record` once the test object
    # exists *and* respects whatever `test_start` the user supplied —
    # we wrap their callback rather than replacing it.
    original_execute = htf.Test.execute

    def patched_execute(self, test_start=None, **execute_kwargs):
        if not getattr(self, "_tofupilot_identify", True):
            return original_execute(self, test_start=test_start, **execute_kwargs)

        # Lazily import the phase_descriptor module so the connector
        # still loads on older openhtf versions that move the import
        # path (we only need it to detect the trigger-phase shape).
        try:
            from openhtf.core import phase_descriptor as _pd
            _PhaseDescriptor = _pd.PhaseDescriptor
        except Exception:
            _PhaseDescriptor = None

        def _apply_resolved_metadata(resolved):
            # Inject all CLI-resolved unit fields into test_record.metadata
            # using v2 API key naming so user phases can read them
            # naturally: `test.test_record.metadata["serial_number"]`,
            # etc. Mutations are captured back on test_end
            # (`_output_callback`).
            #
            # Don't overwrite keys the user already set via
            # `htf.Test(metadata={...})` — their value wins both at
            # inject time AND at capture time.
            try:
                test_state = _get_executing_test()
                if test_state is not None:
                    record = test_state.test_record
                    for k in ("serial_number", "part_number",
                              "revision_number", "batch_number"):
                        v = resolved.get(k)
                        if v and not record.metadata.get(k):
                            record.metadata[k] = v
                    sub_units = resolved.get("sub_units")
                    if sub_units and not record.metadata.get("sub_units"):
                        record.metadata["sub_units"] = sub_units
            except Exception:
                import traceback
                traceback.print_exc()

        # Branch on test_start shape:
        #   * PhaseDescriptor (e.g. user_input.prompt_for_test_start())
        #     — openhtf wants to invoke it as a real phase with
        #     `running_test_state`. Calling it as a plain callable
        #     crashes with "missing 1 required positional argument".
        #     Wrap into a synthetic phase that does the framework
        #     handshake first, then delegates to the user's phase.
        #   * Plain callable / None — wrap as a LambdaType-compatible
        #     function that runs the handshake and returns the SN.
        if _PhaseDescriptor is not None and isinstance(test_start, _PhaseDescriptor):
            user_phase = test_start

            @htf.PhaseOptions(requires_state=True)
            def _identify_trigger_phase(running_test_state):
                resolved = _await_unit_resolution()
                _apply_resolved_metadata(resolved)
                sn = resolved.get("serial_number")
                # If the framework resolved the SN, skip the user's
                # trigger phase entirely — running it would prompt the
                # operator a second time for an SN we already have.
                # User code passing `prompt_for_test_start()` is a
                # legacy pattern; the framework now owns identify-unit
                # and the user phase becomes a no-op. The user's phase
                # is only invoked when the framework resolved nothing
                # (e.g. `identify=False` was passed but the user still
                # supplied a trigger phase).
                if sn:
                    running_test_state.test_record.dut_id = sn
                    # Surface the skip so users notice the migration.
                    # Their existing trigger-phase side effects (logging,
                    # plug warm-up, custom metadata) are silently dropped
                    # otherwise. Use the framework state_logger so this
                    # appears in test_record.log_records and the live
                    # phase_log stream.
                    user_phase_name = (
                        getattr(getattr(user_phase, "func", None), "__name__", None)
                        or getattr(user_phase, "name", "trigger_phase")
                    )
                    try:
                        running_test_state.state_logger.info(
                            "[tofupilot] Skipped user trigger phase '%s' "
                            "because the framework resolved serial_number=%s. "
                            "Drop `test_start=user_input.prompt_for_test_start()` "
                            "from htf.Test(...).execute() to silence this notice.",
                            user_phase_name, sn,
                        )
                    except Exception:
                        pass
                    return
                # No framework SN — fall through to the user's phase.
                # Plug provisioning: openhtf normally provisions plugs
                # via PhaseExecutor before invoking a phase. We're
                # calling user_phase() outside the executor here, so
                # the plug_manager's `_plugs_by_type` won't have the
                # user phase's plug types yet — provide_plugs would
                # KeyError. Pre-init them manually. Idempotent:
                # initialize_plugs short-circuits types it already
                # holds.
                try:
                    plug_types = {plug.cls for plug in user_phase.plugs}
                    if plug_types:
                        running_test_state.plug_manager.initialize_plugs(plug_types)
                except Exception:
                    import traceback
                    traceback.print_exc()
                try:
                    user_phase(running_test_state)
                except Exception:
                    import traceback
                    traceback.print_exc()

            return original_execute(self, test_start=_identify_trigger_phase, **execute_kwargs)

        def _identify_test_start():
            # Framework owns identify-unit: block on the
            # `set_unit_resolved` reply first, then call the user's
            # test_start (if any) so a barcode-scanner test_start
            # can still mutate state. Framework's resolved SN takes
            # precedence over the user's return value — the unified
            # identify-unit contract is the single source of truth
            # for the unit being tested. A user who wants to override
            # passes `identify=False` to `htf.Test(...)`.
            resolved = _await_unit_resolution()
            user_dut = None
            if callable(test_start):
                try:
                    user_dut = test_start()
                except Exception:
                    import traceback
                    traceback.print_exc()
            sn = resolved.get("serial_number") or user_dut or ""
            _apply_resolved_metadata(resolved)
            return sn

        return original_execute(self, test_start=_identify_test_start, **execute_kwargs)

    _patch_once(htf.Test, "execute", patched_execute)

    # Load and execute the user's test
    spec = importlib.util.spec_from_file_location("__main__", test_file)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)


if __name__ == "__main__":
    main()
