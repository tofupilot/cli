"""Scenario 9: AST-extracted single bound + pytest.approx(abs=...).

Two tests: one uses `assert latency < 200`, another uses
`assert v == pytest.approx(5.0, abs=0.2)`. Both should be promoted to
measurements with the right limits and runtime values.
"""

import pytest


def test_latency():
    latency = 100
    assert latency < 200, "Latency [ms]"


def test_voltage_approx():
    v = 5.05
    assert v == pytest.approx(5.0, abs=0.2), "Voltage [V]"
