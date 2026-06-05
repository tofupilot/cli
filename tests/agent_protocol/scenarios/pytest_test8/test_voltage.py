"""Scenario 8: AST-extracted numeric range measurement.

The connector should pick up `assert 4.8 <= v <= 5.2` and produce a
measurement named `v` with min=4.8, max=5.2, unit "V", description
"Supply voltage", and a runtime value of 5.0.
"""


def test_supply_voltage():
    v = 5.0
    assert 4.8 <= v <= 5.2, "Supply voltage [V]"
