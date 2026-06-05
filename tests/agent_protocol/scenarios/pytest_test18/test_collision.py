"""Scenario 18: multi-assert test where both asserts target the same
identifier (`voltage`). The extractor merges them onto a single
measurement that carries two validators (`>= 4.0` and `<= 6.0`). The
phase passes because both validators pass against the captured value.
"""


def test_voltage_two_bounds():
    voltage = 5.0
    assert voltage >= 4.0
    assert voltage <= 6.0
