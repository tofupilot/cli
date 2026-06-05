"""TofuPilot keyword library for Robot Framework.

Robot has no native measurement primitive. This library exposes
`Measure Numeric` / `Measure String` / `Measure Boolean` keywords so a
user can declare a measurement with limits inside a `.robot` test:

    *** Settings ***
    Library    tofupilot_robot

    *** Test Cases ***
    Supply Voltage Is Stable
        Measure Numeric    voltage    5.01    min=4.8    max=5.2    unit=V
        ...    description=Supply voltage

The keyword validates the runtime value against the limits (failing the
test with a descriptive message on violation) and stashes a structured
measurement payload on a module-level dict keyed by the current Robot
test id. The TofuPilot listener reads that dict in its `end_test` hook
and bundles the payload into the `phase_end` event with the same wire
shape pytest emits (`measurements: [...]` with `name`, `units`,
`measured_value`, `validators[]`, rolled-up `outcome`).

The listener is the only code that imports from this module's globals,
so the keyword library remains a clean Robot-side surface. The listener
holds the contract for the wire shape; the keyword holds the contract
for what the user types in `.robot`.

NOTE: this file is also embedded in the Rust CLI via `include_str!`
and written next to `.tofupilot_robot.py` at run time so users don't
need to pre-install anything. A future PyPI release can split it out.
"""

from __future__ import annotations

import threading
from typing import Any

from robot.libraries.BuiltIn import BuiltIn
from robot.api.deco import keyword


# Module-level dict: { robot_test_id: [measurement_payload, ...] }.
# The listener reads this in `end_test` and clears the entry. We never
# keep state across runs; each `end_test` consumes its own entry.
#
# A lock guards reads/writes — Robot can run tests in parallel via
# Pabot, so concurrent appends from different test workers must not
# corrupt the list. The listener, however, runs in the main interpreter
# (Pabot spawns separate processes, each with its own listener), so
# cross-worker visibility is not a concern; the lock is purely for
# in-process safety.
_LOCK = threading.Lock()
_MEASUREMENTS: dict[str, list[dict[str, Any]]] = {}
# Tests where a `Measure *` keyword raised an AssertionError. The
# listener reads this in `end_test` to distinguish a measurement-driven
# FAIL (limit violation, expected outcome) from a non-measurement crash
# (Python exception in user code or framework error → ERROR on the wire).
_MEASUREMENT_FAILURES: set[str] = set()


def _current_test_id() -> str:
    """Resolve the running test's id from Robot's BuiltIn library.

    `${TEST NAME}` is the test name; `${SUITE SOURCE}::${TEST NAME}` is
    a stable id that survives same-named tests in different suites.
    Returns an empty string when called outside a test (e.g. setup),
    in which case measurements are dropped.
    """
    try:
        bi = BuiltIn()
        suite_source = bi.get_variable_value("${SUITE SOURCE}", default="")
        test_name = bi.get_variable_value("${TEST NAME}", default="")
        if not test_name:
            return ""
        if suite_source:
            return f"{suite_source}::{test_name}"
        return test_name
    except Exception:
        return ""


def _append_measurement(payload: dict[str, Any]) -> None:
    test_id = _current_test_id()
    if not test_id:
        return
    with _LOCK:
        _MEASUREMENTS.setdefault(test_id, []).append(payload)


def consume_measurements(test_id: str) -> list[dict[str, Any]]:
    """Listener-only API: pop the measurement list for `test_id`.
    Returns an empty list when no `Measure *` keywords ran.
    """
    with _LOCK:
        return _MEASUREMENTS.pop(test_id, [])


def consume_measurement_failure(test_id: str) -> bool:
    """Listener-only API: pop and return True if a `Measure *` keyword
    raised AssertionError for `test_id`. Lets the listener distinguish
    a limit violation (expected FAIL) from a runtime crash (ERROR).
    """
    with _LOCK:
        if test_id in _MEASUREMENT_FAILURES:
            _MEASUREMENT_FAILURES.discard(test_id)
            return True
        return False


def _mark_measurement_failure() -> None:
    test_id = _current_test_id()
    if not test_id:
        return
    with _LOCK:
        _MEASUREMENT_FAILURES.add(test_id)


# ---------------------------------------------------------------------------
# Outcome strings (mirror of the listener / pytest connector wire enum)
# ---------------------------------------------------------------------------

_PASS = "PASS"
_FAIL = "FAIL"
_UNSET = "UNSET"


def _coerce_float(raw: Any) -> float | None:
    if raw is None:
        return None
    if isinstance(raw, bool):
        return None
    try:
        return float(raw)
    except (TypeError, ValueError):
        return None


def _validators_for_numeric(value: float | None,
                            min_value: float | None,
                            max_value: float | None) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    if min_value is not None:
        outcome = _UNSET if value is None else (_PASS if value >= min_value else _FAIL)
        out.append({"operator": ">=", "expected_value": min_value, "outcome": outcome})
    if max_value is not None:
        outcome = _UNSET if value is None else (_PASS if value <= max_value else _FAIL)
        out.append({"operator": "<=", "expected_value": max_value, "outcome": outcome})
    return out


def _roll_up(validators: list[dict[str, Any]]) -> str:
    if not validators:
        return _PASS
    outcomes = {v.get("outcome") for v in validators}
    if _FAIL in outcomes:
        return _FAIL
    if _UNSET in outcomes:
        return _UNSET
    return _PASS


# ---------------------------------------------------------------------------
# Keywords
# ---------------------------------------------------------------------------


@keyword("Measure Numeric")
def measure_numeric(name: str,
                    value: Any,
                    min: Any = None,  # noqa: A002 — keyword arg name is part of the public surface
                    max: Any = None,  # noqa: A002
                    unit: str | None = None,
                    description: str | None = None) -> float:
    """Record a numeric measurement and validate it against optional limits.

    Robot syntax:

        Measure Numeric    voltage    5.01    min=4.8    max=5.2    unit=V

    The runtime value is coerced to float. `min` / `max` are inclusive
    bounds; absent bounds skip that validator. A bound violation fails
    the test with a clear message — the listener still bundles the
    measurement payload (with FAIL validators) on `end_test`.
    """
    measured = _coerce_float(value)
    min_v = _coerce_float(min)
    max_v = _coerce_float(max)
    validators = _validators_for_numeric(measured, min_v, max_v)
    payload: dict[str, Any] = {"name": name}
    if unit:
        payload["units"] = unit
    if description:
        payload["description"] = description
    if measured is not None:
        payload["measured_value"] = measured
    if validators:
        payload["validators"] = validators
    payload["outcome"] = _roll_up(validators)
    _append_measurement(payload)

    if measured is None:
        # Couldn't coerce — that itself is a failure (the user passed
        # a non-numeric value to Measure Numeric).
        _mark_measurement_failure()
        raise AssertionError(
            f"Measure Numeric {name!r}: value {value!r} is not numeric"
        )
    if min_v is not None and measured < min_v:
        _mark_measurement_failure()
        raise AssertionError(
            f"Measure Numeric {name!r}: {measured} < min {min_v}"
        )
    if max_v is not None and measured > max_v:
        _mark_measurement_failure()
        raise AssertionError(
            f"Measure Numeric {name!r}: {measured} > max {max_v}"
        )
    return measured


@keyword("Measure String")
def measure_string(name: str,
                   value: Any,
                   expected: str | None = None,
                   allowed: Any = None,
                   description: str | None = None) -> str:
    """Record a string measurement with optional equality / membership check.

    Robot syntax:

        Measure String    firmware_version    ${ver}    expected=1.4.2
        Measure String    operating_mode      ${mode}   allowed=normal,debug,prod

    `allowed` accepts a comma-separated string (Robot literals can't
    pass Python lists conveniently) or a Python list / tuple. Empty
    items are dropped.
    """
    measured = "" if value is None else str(value)
    validators: list[dict[str, Any]] = []
    if expected is not None:
        outcome = _PASS if measured == expected else _FAIL
        validators.append({"operator": "==", "expected_value": expected, "outcome": outcome})
    allowed_list: list[str] | None = None
    if allowed is not None:
        if isinstance(allowed, (list, tuple)):
            allowed_list = [str(x) for x in allowed if str(x)]
        else:
            allowed_list = [s.strip() for s in str(allowed).split(",") if s.strip()]
    if allowed_list:
        outcome = _PASS if measured in allowed_list else _FAIL
        validators.append({"operator": "in", "expected_value": list(allowed_list), "outcome": outcome})

    payload: dict[str, Any] = {"name": name, "measured_value": measured}
    if description:
        payload["description"] = description
    if validators:
        payload["validators"] = validators
    payload["outcome"] = _roll_up(validators)
    _append_measurement(payload)

    if expected is not None and measured != expected:
        _mark_measurement_failure()
        raise AssertionError(
            f"Measure String {name!r}: {measured!r} != expected {expected!r}"
        )
    if allowed_list and measured not in allowed_list:
        _mark_measurement_failure()
        raise AssertionError(
            f"Measure String {name!r}: {measured!r} not in {allowed_list!r}"
        )
    return measured


@keyword("Measure Boolean")
def measure_boolean(name: str,
                    value: Any,
                    expected: Any = None,
                    description: str | None = None) -> bool:
    """Record a boolean measurement with optional equality check.

    Robot truthiness handles the usual values (`True`, `False`,
    `${True}`, `${False}`, `1`, `0`, `yes`, `no`, …) via Robot's
    builtin conversion. We accept what Robot hands us and coerce.
    """

    def _to_bool(raw: Any) -> bool:
        if isinstance(raw, bool):
            return raw
        if isinstance(raw, (int, float)):
            return bool(raw)
        s = str(raw).strip().lower()
        return s in ("true", "yes", "1", "on", "y", "t")

    measured = _to_bool(value)
    validators: list[dict[str, Any]] = []
    if expected is not None:
        target = _to_bool(expected)
        outcome = _PASS if measured == target else _FAIL
        validators.append({"operator": "==", "expected_value": target, "outcome": outcome})

    payload: dict[str, Any] = {
        "name": name,
        "measured_value": measured,
    }
    if description:
        payload["description"] = description
    if validators:
        payload["validators"] = validators
    payload["outcome"] = _roll_up(validators)
    _append_measurement(payload)

    if expected is not None:
        target = _to_bool(expected)
        if measured != target:
            _mark_measurement_failure()
            raise AssertionError(
                f"Measure Boolean {name!r}: {measured} != expected {target}"
            )
    return measured
