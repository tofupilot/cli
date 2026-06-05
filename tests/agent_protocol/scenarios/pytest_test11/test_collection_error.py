"""Scenario 11: deliberate syntax error to surface as RunCrashed.

The deliberate `def 1nope` is not valid Python — pytest's collection
phase fails to import this module and emits a `pytest_collectreport`
with `failed=True`. The connector turns this into a phase_log ERROR
and the run reports ERROR overall.
"""

def 1nope():  # noqa: E999 — intentional syntax error
    pass
