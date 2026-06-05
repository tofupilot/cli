"""pytest connector for TofuPilot CLI.

Runs the user's pytest suite under a thin plugin that streams NDJSON events
to stdout (collected nodeids → phase_plan, per-test phase_begin / phase_end,
captured logs). Identify-unit is the only stdin handshake, mirroring
`connector/openhtf.py`.

Users do NOT need to import or configure anything — plain pytest scripts
work end-to-end. The connector additionally extracts measurements from
plain `assert` statements via AST parsing at collection time: a test
whose body contains a single recognized assertion pattern (numeric
range, single bound, equality / approx; string equality / membership;
boolean equality) is promoted to a measurement with limits and a
runtime-captured value. Description / unit ride on the assert message
formatted as `"description [unit]"`.

Usage: python pytest.py <test_path>
       <test_path> can be a directory or a specific test file. Empty /
       absent → pytest scans cwd.
"""
import ast
import inspect
import json
import logging
import os
import re
import select
import signal
import sys
import textwrap
import time
import traceback


# Dup the OS-level stdin fd at module load. pytest's default
# `--capture=fd` does `os.dup2(<tempfile>, 0)` once `pytest.main(...)`
# starts global capturing, which silently rewires fd 0 to point at an
# empty tempfile. A literal `0` after that returns EOF on every read,
# so the identify-unit handshake in `pytest_collection_finish` would
# return `{}` immediately, the unit would be empty, and the parent's
# `set_unit_resolved` reply would race against the child exiting and
# show up as a broken-pipe write. Owning a private dup of fd 0 keeps
# the parent pipe reachable regardless of what pytest's capture plugin
# does to fd 0 later.
_STDIN_FD = os.dup(0)


def _readline_interruptible():
    """Read one line from the OS-level stdin fd, releasing the GIL
    often enough for the SIGTERM/SIGINT handlers to fire promptly.
    Mirror of the openhtf connector helper, but bound to the raw fd
    we captured at startup so pytest's capture plugin can't desync us
    by swapping `sys.stdin`.
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
    # Hard-exit on SIGTERM/SIGINT — same reasoning as the openhtf
    # connector. `os._exit` skips finalizers; pytest's own signal
    # handling would otherwise turn an interrupt into an `INTERNALERROR`
    # that doesn't surface phase outcomes cleanly.
    os._exit(130)


def _install_signal_handlers():
    try:
        signal.signal(signal.SIGTERM, _exit_on_term)
        signal.signal(signal.SIGINT, _exit_on_term)
        signal.siginterrupt(signal.SIGTERM, True)
        signal.siginterrupt(signal.SIGINT, True)
    except (ValueError, OSError):
        pass


# ---------------------------------------------------------------------------
# Event protocol — mirror of `connector/events.rs::PythonEvent`
# ---------------------------------------------------------------------------

class Event:
    BRIDGE_READY = "bridge_ready"
    TEST_START = "test_start"
    TEST_END = "test_end"
    PHASE_BEGIN = "phase_begin"
    PHASE_END = "phase_end"
    ATTACHMENT = "attachment"
    MEASUREMENT = "measurement"
    PHASE_LOG = "phase_log"
    WARNING = "warning"


class Outcome:
    PASS = "PASS"
    FAIL = "FAIL"
    ERROR = "ERROR"
    SKIP = "SKIP"
    ABORTED = "ABORTED"
    # `XFAIL` = expected fail, observed fail (passive xfail marker).
    # `XPASS` = expected fail, observed pass under a strict xfail
    # marker (pytest treats that as a real failure). Both collapse at
    # the Rust → SDK boundary: XFAIL → Skip, XPASS → Fail. The wire
    # strings are preserved for live consumers; the persisted phase
    # outcome doesn't distinguish expected from unexpected.
    XFAIL = "XFAIL"
    XPASS = "XPASS"
    # Validator-only outcome — used for measurements whose runtime
    # value never showed up in the trace (e.g. a multi-assert test
    # raised before the variable was assigned). Rust's
    # `build_measurement` maps anything other than PASS / FAIL to
    # `Outcome::Unset` already; the explicit constant keeps
    # the wire payload self-documenting.
    UNSET = "UNSET"


# ---------------------------------------------------------------------------
# AST-based measurement extraction
# ---------------------------------------------------------------------------
#
# At collection time we parse each test function's source (NOT the live
# module — pytest's assertion rewriter mutates the loaded AST, so we'd
# read post-rewrite shapes if we used `getsourcefile` -> `module.__dict__`).
# `inspect.getsource(func)` reads from disk, which gives us the original
# tokens.
#
# A test is promoted to a measurement only when ALL of:
#   * The body has exactly one top-level `Assert` statement.
#   * That assert matches one of the recognized patterns:
#       - numeric closed range (n1 <= x <= n2 / n1 < x < n2 etc.)
#       - numeric single bound (x op n)
#       - numeric equality (x == n)
#       - pytest.approx(n, abs=k) — `rel=` form rejected
#       - string equality (x == "literal")
#       - string membership (x in ("a", "b", ...))
#       - boolean equality (x == True / False)
#   * The matched identifier is assigned exactly once in the function
#     before the assert, via `name = <expr>` (single Name target).
#
# Anything else: phase reports outcome only, no measurement.

_MSG_RE = re.compile(r"^(.+?)\s*\[([^\]]+)\]\s*$")


def _parse_message(msg):
    """Split an assert message of shape ``"description [unit]"`` into
    ``(description, unit)``. Plain text → ``(text, None)``. Empty / None
    → ``(None, None)``.
    """
    if not msg:
        return (None, None)
    m = _MSG_RE.match(msg)
    if m:
        return (m.group(1).strip(), m.group(2).strip())
    return (msg.strip() or None, None)


def _const_number(node):
    """Return the numeric value of an AST node when it is a literal
    `Constant` (or a unary +/- of one). Otherwise None — variable
    references and expressions are intentionally rejected.
    """
    if isinstance(node, ast.UnaryOp) and isinstance(node.op, (ast.UAdd, ast.USub)):
        inner = _const_number(node.operand)
        if inner is None:
            return None
        return -inner if isinstance(node.op, ast.USub) else inner
    if isinstance(node, ast.Constant) and isinstance(node.value, (int, float)) \
            and not isinstance(node.value, bool):
        return float(node.value)
    return None


def _const_string(node):
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value
    return None


def _const_bool(node):
    if isinstance(node, ast.Constant) and isinstance(node.value, bool):
        return node.value
    return None


def _is_pytest_approx_call(node):
    """True for `pytest.approx(...)` or `approx(...)` calls — covers the
    common idioms `from pytest import approx` and bare `pytest.approx`.
    """
    if not isinstance(node, ast.Call):
        return False
    func = node.func
    if isinstance(func, ast.Attribute) and func.attr == "approx":
        return True
    if isinstance(func, ast.Name) and func.id == "approx":
        return True
    return False


def _approx_bounds(call):
    """Extract `(min, max)` from a `pytest.approx(value, abs=tol)` call.
    `rel=` form is rejected unconditionally (regardless of whether the
    value parses) — relative tolerance has no equivalent in TofuPilot
    measurement limits. Positional `abs` (second positional arg) is also
    accepted because pytest.approx's signature is
    `approx(expected, rel=None, abs=None, nan_ok=False)`.
    """
    if not call.args:
        return None
    expected = _const_number(call.args[0])
    if expected is None:
        return None
    # Reject `rel=` outright — even non-literal values count as rejection.
    for kw in call.keywords:
        if kw.arg == "rel":
            return None
    abs_tol = None
    for kw in call.keywords:
        if kw.arg == "abs":
            abs_tol = _const_number(kw.value)
    if abs_tol is None:
        return None
    return (expected - abs_tol, expected + abs_tol)


class _AssertConstraint:
    """One recognized assert reduced to the limit shape it imposes.
    A measurement may carry several of these stacked as separate
    validators (e.g. `assert x >= 0` followed by `assert x <= 5`
    yields two constraints on the same measurement).
    """

    __slots__ = (
        "kind",
        "min_value",
        "max_value",
        "expected_value",
        "allowed_values",
    )

    def __init__(
        self,
        kind,
        min_value=None,
        max_value=None,
        expected_value=None,
        allowed_values=None,
    ):
        # "numeric" | "string" | "boolean"
        self.kind = kind
        self.min_value = min_value
        self.max_value = max_value
        self.expected_value = expected_value
        self.allowed_values = allowed_values


class _MeasurementSpec:
    """AST-extracted measurement schema. Runtime value is filled in at
    test call time via sys.settrace. `constraints` holds the validator
    list — one per matching assert against this identifier.
    """

    __slots__ = (
        "name",
        "unit",
        "description",
        "constraints",
    )

    def __init__(
        self,
        name,
        unit=None,
        description=None,
        constraints=None,
    ):
        self.name = name
        self.unit = unit
        self.description = description
        self.constraints = constraints if constraints is not None else []


def _match_assignment_targets(stmts, name):
    """Return True when `name` is assigned exactly once via a top-level
    `Assign` with a single `Name(id=name)` target. AnnAssign / AugAssign
    / tuple-unpacking are intentionally not counted — the spec is strict.
    """
    count = 0
    for stmt in stmts:
        if isinstance(stmt, ast.Assign) \
                and len(stmt.targets) == 1 \
                and isinstance(stmt.targets[0], ast.Name) \
                and stmt.targets[0].id == name:
            count += 1
    return count == 1


def _match_numeric_compare(left, ops, comparators):
    """Match the comparators against numeric patterns. Returns a dict of
    measurement parameters or None.
    """
    # N1 closed range: <num> op <name> op <num>. `_const_number` accepts
    # both bare `Constant` and `UnaryOp(USub|UAdd, Constant)` so the
    # negative-prefixed form (e.g. `-10 <= x <= 50`) is covered without
    # a second branch.
    if len(ops) == 2 and isinstance(comparators[0], ast.Name):
        low = _const_number(left)
        high = _const_number(comparators[1])
        if low is not None and high is not None \
                and isinstance(ops[0], (ast.LtE, ast.Lt)) \
                and isinstance(ops[1], (ast.LtE, ast.Lt)):
            return {
                "name": comparators[0].id,
                "kind": "numeric",
                "min_value": low,
                "max_value": high,
            }
    if len(ops) != 1:
        return None
    op = ops[0]
    rhs = comparators[0]
    # N2/N3: <name> op <num>
    if isinstance(left, ast.Name):
        rhs_num = _const_number(rhs)
        if rhs_num is not None:
            if isinstance(op, (ast.GtE, ast.Gt)):
                return {
                    "name": left.id,
                    "kind": "numeric",
                    "min_value": rhs_num,
                }
            if isinstance(op, (ast.LtE, ast.Lt)):
                return {
                    "name": left.id,
                    "kind": "numeric",
                    "max_value": rhs_num,
                }
            if isinstance(op, ast.Eq):
                return {
                    "name": left.id,
                    "kind": "numeric",
                    "expected_value": rhs_num,
                    "min_value": rhs_num,
                    "max_value": rhs_num,
                }
        # N4: <name> == pytest.approx(<num>, abs=<num>)
        if isinstance(op, ast.Eq) and _is_pytest_approx_call(rhs):
            bounds = _approx_bounds(rhs)
            if bounds is not None:
                return {
                    "name": left.id,
                    "kind": "numeric",
                    "min_value": bounds[0],
                    "max_value": bounds[1],
                }
    # symmetric: <num> op <name>
    if isinstance(rhs, ast.Name):
        lhs_num = _const_number(left)
        if lhs_num is not None:
            # Flip the operator direction
            if isinstance(op, (ast.LtE, ast.Lt)):
                return {
                    "name": rhs.id,
                    "kind": "numeric",
                    "min_value": lhs_num,
                }
            if isinstance(op, (ast.GtE, ast.Gt)):
                return {
                    "name": rhs.id,
                    "kind": "numeric",
                    "max_value": lhs_num,
                }
            if isinstance(op, ast.Eq):
                return {
                    "name": rhs.id,
                    "kind": "numeric",
                    "expected_value": lhs_num,
                    "min_value": lhs_num,
                    "max_value": lhs_num,
                }
    return None


def _match_string_compare(left, ops, comparators):
    if len(ops) != 1:
        return None
    op = ops[0]
    rhs = comparators[0]
    # S1: <name> == "literal"
    if isinstance(left, ast.Name) and isinstance(op, ast.Eq):
        s = _const_string(rhs)
        if s is not None:
            return {
                "name": left.id,
                "kind": "string",
                "expected_value": s,
            }
    # S2: <name> in (...) or [...]
    if isinstance(left, ast.Name) and isinstance(op, ast.In) \
            and isinstance(rhs, (ast.Tuple, ast.List)):
        items = []
        for elt in rhs.elts:
            s = _const_string(elt)
            if s is None:
                return None
            items.append(s)
        if items:
            return {
                "name": left.id,
                "kind": "string",
                "allowed_values": items,
            }
    return None


def _match_boolean_compare(left, ops, comparators):
    if len(ops) != 1 or not isinstance(ops[0], ast.Eq):
        return None
    rhs = comparators[0]
    if isinstance(left, ast.Name):
        b = _const_bool(rhs)
        if b is not None:
            return {
                "name": left.id,
                "kind": "boolean",
                "expected_value": b,
            }
    return None


def _extract_measurement_specs(func_source):
    """Match a test body against the recognized measurement patterns
    and group them by target identifier. Returns a (possibly empty)
    list of `_MeasurementSpec` in first-seen source order, where each
    spec carries one or more `_AssertConstraint`s — multiple asserts
    on the same `<name>` (which `<name> = <expr>` is assigned exactly
    once) stack as additional validators on the same measurement.

    Rules per assert:
      * Must be a top-level `Assert` whose test is `ast.Compare`.
      * Must match one of the numeric / string / boolean shapes.
      * The matched identifier must be assigned exactly once via
        `<name> = <expr>` in the body. Multiple assignments make the
        runtime snapshot ambiguous, so the assert is skipped. The
        first matching assert seeds the measurement's name / unit /
        description; subsequent asserts on the same name only
        contribute additional validators (their per-assert messages
        are ignored — descriptions and units belong on the
        measurement, not its validators).

    Asserts that don't match a recognized shape are ignored without
    affecting the others; the test stays a measurement-bearing phase
    if at least one assert matches.
    """
    try:
        # Indented def (e.g. nested in a class) needs dedenting before
        # `ast.parse` accepts it.
        tree = ast.parse(textwrap.dedent(func_source))
    except SyntaxError:
        return []
    func_node = next(
        (n for n in tree.body if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))),
        None,
    )
    if func_node is None:
        return []
    body = func_node.body

    # Strip a leading docstring so it doesn't count toward statement counts.
    if body and isinstance(body[0], ast.Expr) and isinstance(body[0].value, ast.Constant) \
            and isinstance(body[0].value.value, str):
        body = body[1:]

    by_name = {}
    order = []
    for idx, stmt in enumerate(body):
        if not isinstance(stmt, ast.Assert):
            continue
        test = stmt.test
        if not isinstance(test, ast.Compare):
            continue
        matched = _match_numeric_compare(test.left, test.ops, test.comparators)
        if matched is None:
            matched = _match_string_compare(test.left, test.ops, test.comparators)
        if matched is None:
            matched = _match_boolean_compare(test.left, test.ops, test.comparators)
        if matched is None:
            continue

        name = matched["name"]
        # Identifier must have exactly one `<name> = <expr>` somewhere
        # in the body. Multiple assignments make the runtime snapshot
        # ambiguous (which value matters?), so reject. Checking the
        # full body (not just the prefix before this assert) means
        # subsequent asserts on the same identifier reuse the first
        # match's verdict instead of accidentally accepting because
        # the *prefix* still has only one assignment.
        if not _match_assignment_targets(body, name):
            continue

        constraint = _AssertConstraint(
            kind=matched["kind"],
            min_value=matched.get("min_value"),
            max_value=matched.get("max_value"),
            expected_value=matched.get("expected_value"),
            allowed_values=matched.get("allowed_values"),
        )

        if name in by_name:
            by_name[name].constraints.append(constraint)
            continue

        msg_text = None
        if stmt.msg is not None:
            msg_text = _const_string(stmt.msg)
        description, unit = _parse_message(msg_text)

        spec = _MeasurementSpec(
            name=name,
            unit=unit,
            description=description,
            constraints=[constraint],
        )
        by_name[name] = spec
        order.append(name)

    return [by_name[n] for n in order]


def _capture_local_values(func, names):
    """Run `sys.settrace` scoped to a test call to snapshot the local
    values of every name in `names` at each line in the test frame.
    Returns `(install, uninstall, get_values)`; `get_values()` returns
    `{name: (value, captured_bool)}` after the test has run. Names that
    never appeared in `frame.f_locals` (e.g. because the test raised
    before their assignment) come back with `(None, False)`.

    The trace function fires on every `line` event for the test frame.
    Works even when other tracers (coverage, debuggers) are active
    provided they call back into our trace function — Python's
    settrace API only allows one tracer at a time, so we restore the
    previous tracer on exit.
    """
    captured = {n: {"value": None, "have": False} for n in names}
    target_code = func.__code__

    prev_trace = sys.gettrace()

    def tracer(frame, event, arg):
        # Limit to the test function's frame to keep overhead tiny.
        if frame.f_code is target_code:
            if event == "line":
                locs = frame.f_locals
                for n, slot in captured.items():
                    if n in locs:
                        slot["value"] = locs[n]
                        slot["have"] = True
            return tracer
        # Don't trace into nested calls — return None for non-test
        # frames so the line tracer doesn't fire there.
        return None

    def install():
        sys.settrace(tracer)

    def uninstall():
        sys.settrace(prev_trace)

    def get_values():
        return {n: (slot["value"], slot["have"]) for n, slot in captured.items()}

    return install, uninstall, get_values


def _build_measurement_payload(spec, value, value_captured):
    """Translate a `_MeasurementSpec` + runtime value into the
    structured `phase_end.measurements` shape expected by
    `connector/mod.rs::extract_run_measurements` and
    `build_measurement` on the Rust side.

    Returns a dict with `name`, optional `units`, `measured_value`,
    `validators` (list — one per `_AssertConstraint` on the spec), and
    a rolled-up `outcome`. When the runtime value never showed up,
    every validator's outcome lands as `UNSET` rather than a misleading
    `PASS`.
    """
    m = {"name": spec.name}
    if spec.unit:
        m["units"] = spec.unit
    if spec.description:
        m["description"] = spec.description
    if value_captured and value is not None:
        if isinstance(value, bool):
            m["measured_value"] = value
        elif isinstance(value, (int, float)):
            m["measured_value"] = float(value)
        else:
            m["measured_value"] = value

    have_value = value_captured and value is not None
    validators = []
    for c in spec.constraints:
        validators.extend(_validators_for_constraint(c, value, have_value))

    if validators:
        m["validators"] = validators
        outcomes = {v.get("outcome") for v in validators}
        if Outcome.FAIL in outcomes:
            m["outcome"] = Outcome.FAIL
        elif Outcome.UNSET in outcomes:
            m["outcome"] = Outcome.UNSET
        else:
            m["outcome"] = Outcome.PASS
    return m


def _validators_for_constraint(c, value, have_value):
    """Project one `_AssertConstraint` to zero or more validator dicts.
    Mirrors the original assert: a closed-range constraint emits two
    validators (`>=` and `<=`); equality / membership / boolean each
    emit one. Outcome defaults to `UNSET` whenever the runtime value
    is missing — a value-less assert can't claim PASS.
    """
    out = []
    if c.kind == "numeric":
        if c.min_value is not None:
            if have_value:
                try:
                    outcome = Outcome.PASS if float(value) >= c.min_value else Outcome.FAIL
                except (TypeError, ValueError):
                    outcome = Outcome.FAIL
            else:
                outcome = Outcome.UNSET
            out.append({
                "operator": ">=",
                "expected_value": c.min_value,
                "outcome": outcome,
            })
        if c.max_value is not None:
            if have_value:
                try:
                    outcome = Outcome.PASS if float(value) <= c.max_value else Outcome.FAIL
                except (TypeError, ValueError):
                    outcome = Outcome.FAIL
            else:
                outcome = Outcome.UNSET
            out.append({
                "operator": "<=",
                "expected_value": c.max_value,
                "outcome": outcome,
            })
    elif c.kind == "string":
        if c.expected_value is not None:
            if have_value:
                outcome = Outcome.PASS if value == c.expected_value else Outcome.FAIL
            else:
                outcome = Outcome.UNSET
            out.append({
                "operator": "==",
                "expected_value": c.expected_value,
                "outcome": outcome,
            })
        if c.allowed_values is not None:
            if have_value:
                outcome = Outcome.PASS if value in c.allowed_values else Outcome.FAIL
            else:
                outcome = Outcome.UNSET
            out.append({
                "operator": "in",
                "expected_value": list(c.allowed_values),
                "outcome": outcome,
            })
    elif c.kind == "boolean":
        if c.expected_value is not None:
            if have_value:
                outcome = Outcome.PASS if bool(value) == bool(c.expected_value) else Outcome.FAIL
            else:
                outcome = Outcome.UNSET
            out.append({
                "operator": "==",
                "expected_value": c.expected_value,
                "outcome": outcome,
            })
    return out


# pytest's terminalreporter would otherwise write `.` / `F` / banner lines
# to the same fd 1 we use for NDJSON, corrupting the wire. We disable the
# terminal plugin via `-p no:terminal` when invoking pytest_module.main
# (see `main()` below). That removes the primary noise source.
#
# As a defense in depth — in case a user's pyproject re-enables the
# plugin or some other plugin writes to fd 1 directly — we also dup fd 1
# to a private fd we own (`_STDOUT_FD`) and route `_emit` through it.
# Tests' own captured `print()` still surfaces via `report.capstdout`
# → phase_log; this shielding only protects the wire channel.
_STDOUT_FD = os.dup(1)


def _emit(event):
    line = (json.dumps(event, default=str) + "\n").encode("utf-8")
    try:
        os.write(_STDOUT_FD, line)
    except OSError:
        # The duplicated fd should be valid for the run's lifetime; if
        # we hit a write error fall back to the standard print so the
        # event has at least a chance of escaping. `print` itself can
        # raise BrokenPipeError when the parent has closed its end —
        # silently drop on double failure (the parent is gone, nothing
        # left to do).
        try:
            print(json.dumps(event, default=str), flush=True)
        except OSError:
            pass


# ---------------------------------------------------------------------------
# Identify-unit handshake (mirrors openhtf path)
# ---------------------------------------------------------------------------

def _await_unit_resolution():
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


# ---------------------------------------------------------------------------
# Plugin
# ---------------------------------------------------------------------------

# Pytest outcome strings → wire outcome.
#   "passed"  → PASS
#   "failed"  → FAIL  (assertion / exception in body)
#   "skipped" → SKIP
# xfail / xpass are not raw outcomes — they appear via `report.wasxfail`
# or `report.outcome == "passed"` with the xfail marker. We split:
#   xfail (expected fail, observed fail) → XFAIL on the live wire,
#       collapses to SKIP at the SDK boundary. The persisted phase
#       outcome can't distinguish expected from generic skip.
#   xpass non-strict (expected fail, observed pass) → PASS. pytest
#       silently de-escalates; nothing extra to surface.
#   xpass strict (expected fail, observed pass) → XPASS on the live
#       wire, collapses to FAIL at the SDK boundary.
# `error` (collection / fixture setup / teardown errors) → ERROR.
def _wire_outcome(report):
    if getattr(report, "wasxfail", None) is not None:
        # report.wasxfail set means xfail/xpass marker fired.
        if report.outcome == "passed":
            # Non-strict xpass: pytest collapses this to a pass at
            # the report level. Surface as plain PASS — no special
            # signal is needed.
            return Outcome.PASS
        if report.outcome == "failed":
            # Strict xpass: marker required failure but the test
            # passed, so pytest records it as a failure. Distinct
            # XPASS wire string for live consumers; SDK boundary
            # maps to FAIL.
            return Outcome.XPASS
        # passive xfail (test failed as expected) → XFAIL
        return Outcome.XFAIL
    if report.outcome == "passed":
        return Outcome.PASS
    if report.outcome == "skipped":
        return Outcome.SKIP
    # report.outcome == "failed" — distinguish a test-body failure from
    # a fixture/collection error. pytest sets `when` on the report:
    #   "setup"    → fixture or setup error → ERROR
    #   "call"     → assertion / exception in test body → FAIL
    #   "teardown" → fixture cleanup error → ERROR
    when = getattr(report, "when", "call")
    if when in ("setup", "teardown"):
        return Outcome.ERROR
    return Outcome.FAIL


def _phase_name_from_nodeid(nodeid, strip_file_prefix=False):
    """Project a pytest nodeid (`tests/test_x.py::test_y[param]`) onto
    a wire phase name. With `strip_file_prefix=True` we drop everything
    up to and including the first `::` so the operator UI shows
    `test_y[param]` instead of `tests/test_x.py::test_y[param]`. The
    plugin only enables stripping when collection turned up exactly
    one source file — otherwise dropping the prefix would collide
    `file_a.py::test_x` with `file_b.py::test_x`.
    """
    if strip_file_prefix:
        sep = nodeid.find("::")
        if sep != -1:
            return nodeid[sep + 2 :]
    return nodeid




class _LiveLogHandler(logging.Handler):
    """Stream each log record out as a phase_log event. Pytest's own
    `caplog` plugin already captures records into reports, but those are
    only delivered after the test finishes — for live UIs we want them
    to land as the test runs.
    """

    def __init__(self, plugin):
        super().__init__()
        self._plugin = plugin

    def emit(self, record):  # noqa: A003 — logging API
        try:
            phase_name = self._plugin.current_phase
            _emit({
                "type": Event.PHASE_LOG,
                "level": record.levelname,
                "message": self.format(record),
                "timestamp": _iso_timestamp(record.created),
                "phase_name": phase_name,
                "file": getattr(record, "pathname", None),
                "line": getattr(record, "lineno", None),
            })
        except Exception:
            pass


def _iso_timestamp(epoch_seconds):
    import datetime as _dt
    return _dt.datetime.fromtimestamp(epoch_seconds, _dt.timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%S.%fZ"
    )


class TofuPilotPlugin:
    """pytest plugin: streams phase events, captures logs / output,
    extracts measurements from plain `assert` statements via AST, and
    runs the identify-unit handshake before the first test executes.
    """

    def __init__(self, identify_enabled, unit_kwargs, auto_identify):
        self.identify_enabled = identify_enabled
        self.unit_kwargs = unit_kwargs
        self.auto_identify = auto_identify
        self.current_phase = None
        # AST-extracted measurement schema, keyed by phase_name. Each
        # entry is a `_MeasurementSpec`; runtime value / outcome are
        # filled in during `pytest_pyfunc_call` via sys.settrace.
        self._measurement_specs = {}
        # Final structured measurements per phase, ready to bundle into
        # `phase_end.measurements`.
        self._ast_measurements = {}
        # Resolved unit fields from the framework (Rust side). Stamped
        # into test_end.metadata so build_request can use them.
        self.resolved_unit = {}
        # Phase plan emitted in test_start. Captured by
        # `pytest_collection_finish` so we know the test body before
        # phases run.
        self.phase_plan = []
        # When the collected suite touches exactly one source file we
        # strip the `path/to/file.py::` prefix from phase names — the
        # operator UI is more readable that way. `pytest_collection_finish`
        # decides this once and every later helper consults the flag.
        self._strip_file_prefix = False
        # Per-phase start time so phase_end can carry duration even
        # when pytest's own clock isn't surfaced through the report.
        self._phase_start_ms = {}
        # Wall-clock test_start_millis stamped at session start.
        self.session_start_ms = None
        # End-of-session aggregated outcome: any FAIL/ERROR → FAIL,
        # else PASS. Empty session (no tests collected) → PASS.
        self._observed_outcomes = []
        # Logging handler reference so we can detach in
        # `pytest_sessionfinish` and not leak across re-runs in the
        # same interpreter (pytest-xdist, repl).
        self._log_handler = None
        # Test-name from pyproject if available; surfaced on test_start
        # the same way the openhtf path forwards htf.Test(test_name=...).
        self.test_name = ""

    # --- session lifecycle ---------------------------------------------

    def pytest_sessionstart(self, session):
        self.session_start_ms = int(time.time() * 1000)
        # Attach a logging handler to the root logger so any
        # `logging.info(...)` from user code or its libs flows through.
        # Pytest disables propagation for some namespaces by default;
        # attach to root for broad coverage.
        self._log_handler = _LiveLogHandler(self)
        self._log_handler.setLevel(logging.DEBUG)
        # Format as plain message — Rust event_router doesn't render the
        # level prefix again.
        self._log_handler.setFormatter(logging.Formatter("%(message)s"))
        logging.getLogger().addHandler(self._log_handler)
        # Make sure root logger lets DEBUG through; user code that calls
        # `logging.debug(...)` would otherwise be silently dropped.
        if logging.getLogger().level == logging.NOTSET:
            logging.getLogger().setLevel(logging.DEBUG)

    def pytest_collection_modifyitems(self, session, config, items):
        """Walk each collected test item and try to match its body
        against the recognized assertion patterns. Specs are stashed by
        nodeid so `pytest_pyfunc_call` knows which tests to instrument.
        Each test can carry zero, one, or many specs (one per
        recognized assert with a matching single assignment).
        """
        for item in items:
            func = getattr(item, "function", None)
            if func is None:
                continue
            try:
                source = inspect.getsource(func)
            except (OSError, TypeError):
                continue
            specs = _extract_measurement_specs(source)
            if specs:
                self._measurement_specs[item.nodeid] = specs

    def pytest_pyfunc_call(self, pyfuncitem):
        """Install a `sys.settrace` hook that snapshots the local value
        of the matched identifier on every line in the test frame.

        We only intercept the call when there's a matched measurement
        spec; otherwise we return `None` so pytest (or pytest-asyncio,
        for `async def` tests) runs its default. When we do intercept,
        we replicate pytest's default `func(**testargs)` invocation
        sandwiched between settrace install/uninstall, and return
        `True` to tell pytest "I handled the call; don't run the
        default". Async tests are skipped entirely — calling an async
        function synchronously yields an unawaited coroutine that
        silently no-ops, and we'd emit a phantom measurement.
        """
        # Bail on async tests: synchronous func(**kwargs) on a
        # coroutine function returns the coroutine without awaiting,
        # so the body never runs and our trace probe never fires.
        if inspect.iscoroutinefunction(pyfuncitem.function):
            return None
        specs = self._measurement_specs.get(pyfuncitem.nodeid)
        if not specs:
            return None
        func = pyfuncitem.obj
        names = [s.name for s in specs]
        install, uninstall, get_values = _capture_local_values(func, names)
        # pytest_pyfunc_call's default behaviour is to call
        # pyfuncitem.obj(**fixture_kwargs); we replicate it here so we
        # can sandwich settrace around the actual call. Returning True
        # tells pytest "I handled the call; don't run the default".
        funcargs = pyfuncitem.funcargs
        testargs = {
            arg: funcargs[arg]
            for arg in pyfuncitem._fixtureinfo.argnames
        }
        install()
        phase_name = _phase_name_from_nodeid(pyfuncitem.nodeid, self._strip_file_prefix)
        try:
            func(**testargs)
        finally:
            uninstall()
            # Always emit measurements, even when the test raised
            # (assert / xfail). The trace probe captured whatever was
            # in scope at the point of failure — assigned variables
            # surface their value, unassigned ones come back as
            # `(None, False)` and get UNSET validators downstream.
            snapshot = get_values()
            for spec in specs:
                value, captured = snapshot.get(spec.name, (None, False))
                payload = _build_measurement_payload(spec, value, captured)
                self._ast_measurements.setdefault(phase_name, []).append(payload)
                live = {
                    "type": Event.MEASUREMENT,
                    "name": spec.name,
                    "value": payload.get("measured_value"),
                    "phase_name": phase_name,
                }
                if spec.unit:
                    live["unit"] = spec.unit
                _emit(live)
        return True

    def pytest_collection_finish(self, session):
        """Build the phase plan and run the identify-unit handshake."""
        # If every collected nodeid points at the same source file,
        # strip the `<file>::` prefix from phase names. Multi-file
        # suites keep the prefix to avoid `file_a.py::test_x` and
        # `file_b.py::test_x` collapsing onto one phase name.
        files = set()
        for item in session.items:
            nodeid = item.nodeid
            sep = nodeid.find("::")
            files.add(nodeid[:sep] if sep != -1 else nodeid)
            if len(files) > 1:
                break
        self._strip_file_prefix = len(files) == 1
        self.phase_plan = [
            _phase_name_from_nodeid(item.nodeid, self._strip_file_prefix)
            for item in session.items
        ]
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
            # Still drain stdin reply so the parent isn't out of sync
            # if it sent one anyway (it won't, but defending here keeps
            # the protocol robust to future tweaks).
            self.resolved_unit = {}

    # --- per-test events ------------------------------------------------

    def pytest_runtest_logstart(self, nodeid, location):
        phase_name = _phase_name_from_nodeid(nodeid, self._strip_file_prefix)
        self.current_phase = phase_name
        self._phase_start_ms[phase_name] = int(time.time() * 1000)
        _emit({"type": Event.PHASE_BEGIN, "name": phase_name})

    def pytest_runtest_logreport(self, report):
        # Pytest emits three reports per test (setup / call / teardown).
        # We collapse to one phase_end per test:
        #   * Emit on `call` for normal tests.
        #   * Emit on `setup` when setup failed (no call will happen).
        #   * Emit on `teardown` when teardown failed but call passed,
        #     so the phase outcome reflects the cleanup error.
        # Skipped tests appear as `setup` with outcome `skipped` (e.g.
        # `pytest.skip()` in the body bubbles up as a setup-phase skip);
        # emit there so we still see one phase_end per nodeid.
        phase_name = _phase_name_from_nodeid(report.nodeid, self._strip_file_prefix)

        if report.when == "setup":
            if report.failed or report.skipped:
                self._emit_phase_end(report, phase_name)
        elif report.when == "call":
            self._emit_phase_end(report, phase_name)
        elif report.when == "teardown":
            # Only emit if call already passed AND teardown failed —
            # otherwise we'd double-emit a phase_end for a test that
            # already had its `call`-phase report emitted above.
            if report.failed:
                # The phase_end for `call` was a PASS; promote to ERROR
                # by emitting a second phase_end with retry_count=1 so
                # downstream sees the cleanup failure as a separate
                # attempt. This matches the spec's "teardown runs even
                # on failure" expectation.
                self._emit_phase_end(report, phase_name, retry_count=1)

    def _emit_phase_end(self, report, phase_name, retry_count=0):
        outcome = _wire_outcome(report)
        self._observed_outcomes.append(outcome)

        # Capture stdout / stderr / log sections pytest collected for
        # this report. Concatenate into a single `phase_log` payload
        # appended after phase_end (so timeline is "begin, live logs,
        # end, captured-batch logs"). In practice, live logs from the
        # logging handler land as separate events; this captures
        # `print()` / `sys.stderr.write(...)` content that pytest
        # buffered.
        captured = self._gather_captured(report)
        if captured:
            _emit({
                "type": Event.PHASE_LOG,
                "level": "INFO",
                "message": captured,
                "timestamp": _iso_timestamp(time.time()),
                "phase_name": phase_name,
            })

        # Surface tracebacks on FAIL / ERROR as a separate log entry —
        # makes "what went wrong" visible without forcing agents to
        # parse stderr.
        if outcome in (Outcome.FAIL, Outcome.ERROR) and getattr(report, "longrepr", None):
            _emit({
                "type": Event.PHASE_LOG,
                "level": "ERROR",
                "message": str(report.longrepr),
                "timestamp": _iso_timestamp(time.time()),
                "phase_name": phase_name,
            })

        # AST-extracted measurements are bundled on the structured
        # `phase_end.measurements` field. Each entry already carries
        # the runtime value (captured via sys.settrace at test call) and
        # the static limits parsed from the assert statement. Live
        # `measurement` events have already streamed earlier from
        # `pytest_pyfunc_call`. Gate on `retry_count == 0` so a
        # teardown-failure double-emit doesn't bundle the AST payload
        # twice — the second phase_end stays metadata-only.
        if retry_count == 0:
            measurements = self._ast_measurements.pop(phase_name, [])
        else:
            measurements = []
        # No need to roll up failing measurement validators into the
        # phase outcome here: the assert that yielded the measurement
        # is the test's only assert (selection rule), so pytest already
        # surfaces it as FAIL when bounds are violated.

        start_ms = self._phase_start_ms.get(phase_name, int(time.time() * 1000))
        end_ms = int(time.time() * 1000)
        # Pytest's own duration field is more accurate when present.
        duration = getattr(report, "duration", None)
        if isinstance(duration, (int, float)) and duration > 0:
            start_ms = end_ms - int(duration * 1000)

        _emit({
            "type": Event.PHASE_END,
            "name": phase_name,
            "outcome": outcome,
            "start_time_millis": start_ms,
            "end_time_millis": end_ms,
            "measurements": measurements,
            "retry_count": retry_count,
            "docstring": None,
        })

    def _gather_captured(self, report):
        parts = []
        for section_name, content in getattr(report, "sections", []) or []:
            if not content:
                continue
            parts.append(f"[{section_name}]\n{content}".rstrip())
        # Pytest also exposes captured stdout/stderr/log via
        # `report.capstdout` / `report.capstderr` / `report.caplog` for
        # the `call` phase in modern pytest (>=3.3). Use those when
        # `sections` is empty (older pytest may differ).
        if not parts:
            for attr, label in (
                ("capstdout", "stdout"),
                ("capstderr", "stderr"),
                ("caplog", "log"),
            ):
                v = getattr(report, attr, None)
                if v:
                    parts.append(f"[{label}]\n{v}".rstrip())
        return "\n".join(parts).strip()

    # --- session end ----------------------------------------------------

    def pytest_sessionfinish(self, session, exitstatus):
        # Aggregate outcome: PASS only if every observed outcome is PASS
        # AND no collection error fired. exitstatus mirrors pytest's
        # public exit-code contract:
        #   0 — all passed
        #   1 — tests failed
        #   2 — usage / interrupted
        #   3 — internal error
        #   4 — usage error (cli)
        #   5 — no tests collected
        # For our wire outcome, collapse:
        #   exitstatus == 0 → PASS
        #   exitstatus == 1 → FAIL
        #   exitstatus == 5 → ERROR — empty collection is a
        #                     procedure-config bug (pytest declared but
        #                     no test_*.py files shipped), not a
        #                     successful no-op. The run_outcome enum
        #                     has no SKIP variant at the run level.
        #   anything else  → ERROR
        if exitstatus == 0:
            outcome = Outcome.PASS
        elif exitstatus == 1:
            outcome = Outcome.FAIL
        elif exitstatus == 5:
            outcome = Outcome.ERROR
            _emit({
                "type": Event.WARNING,
                "message": "No pytest tests collected — pyproject declared pytest but no test_*.py files were found.",
            })
        else:
            outcome = Outcome.ERROR

        end_ms = int(time.time() * 1000)
        metadata = {}
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

        # Detach the logging handler so a re-run in the same interpreter
        # (e.g. dev REPL test) doesn't double-emit phase_log events.
        if self._log_handler is not None:
            try:
                logging.getLogger().removeHandler(self._log_handler)
            except Exception:
                pass
            self._log_handler = None

    # --- collection failure ---------------------------------------------

    def pytest_collectreport(self, report):
        """A collection error (e.g. SyntaxError in a test module) never
        produces per-test reports — the run otherwise looks empty. Surface
        it as a phase_log so the agent sees the cause before
        sessionfinish reports ERROR.
        """
        if report.failed:
            _emit({
                "type": Event.PHASE_LOG,
                "level": "ERROR",
                "message": f"Collection error: {report.longrepr}",
                "timestamp": _iso_timestamp(time.time()),
                "phase_name": None,
            })


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def _build_pyproject_unit_kwargs(test_root):
    """Read `[tool.tofupilot]` from `pyproject.toml` if present so users
    can declare `serial_number = "SN-1"` etc. without modifying their
    test code. Returns `(unit_kwargs, auto_identify)`. All keys are
    optional. Empty / absent → ({}, False).
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


def main():
    _install_signal_handlers()
    _emit({"type": Event.BRIDGE_READY})

    # Args: positional test_path (file or dir). Empty → cwd.
    test_path = sys.argv[1] if len(sys.argv) >= 2 else os.getcwd()
    test_root = test_path if os.path.isdir(test_path) else os.path.dirname(
        os.path.abspath(test_path)
    ) or os.getcwd()

    unit_kwargs, auto_identify = _build_pyproject_unit_kwargs(test_root)
    if os.environ.get("TOFUPILOT_AUTO_IDENTIFY"):
        auto_identify = True

    plugin = TofuPilotPlugin(
        identify_enabled=True,
        unit_kwargs=unit_kwargs,
        auto_identify=auto_identify,
    )

    # We invoke pytest in-process so the plugin can hook session events
    # without needing a separate `conftest.py` injected into the user's
    # tree. `--rootdir` pins discovery to the test path; `-q` keeps
    # stdout terse (our NDJSON owns stdout — we want pytest's terminal
    # output silenced to avoid interleaving non-JSON lines, but we
    # can't fully silence it without losing failure detail. So we run
    # quiet mode and rely on our own phase_end / phase_log events.).
    try:
        import pytest as pytest_module  # noqa: F811 — already imported at top
    except ImportError:
        # Pytest itself missing from the venv — the connector's purpose
        # is moot; surface the failure as test_end ERROR so the Rust
        # side cleans up.
        _emit({"type": Event.WARNING, "message": "pytest not installed in venv"})
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

    # `-p no:terminal` disables pytest's terminalreporter — the plugin
    # that prints `.` / `F` per test and the summary banner. Without it,
    # pytest's only writes to fd 1 are user-test `print(...)` calls
    # (captured by the capture plugin into `report.capstdout`), so our
    # NDJSON wire is clean. Dropping `-q` / `-rN` / `--tb=short` is
    # mandatory: those flags are owned by the terminal plugin and
    # become unrecognized when it's disabled.
    #
    # `_emit` still writes through `_STDOUT_FD` (a dup of fd 1 captured
    # at module load) as defense in depth: if a user's pyproject opts
    # back into the terminal plugin or another plugin writes raw to
    # stdout, our wire stays unaffected.
    args = [test_path, "-p", "no:cacheprovider", "-p", "no:terminal"]
    exit_code = pytest_module.main(args, plugins=[plugin])
    return int(exit_code) if exit_code is not None else 0


if __name__ == "__main__":
    sys.exit(main())
