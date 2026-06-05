"""Scenario 17: failing-range assert still emits a measurement.

The captured value (5.5) lands outside the [4.8, 5.2] bounds, so the
assert raises AssertionError — but the connector must still emit the
measurement with outcome=FAIL. Pytest's assert rewriter raises after
the comparison, so the trace probe sees the value before the unwind.
"""


def test_failing_range():
    voltage = 5.5
    assert 4.8 <= voltage <= 5.2, "Supply voltage [V]"
