"""Scenario 15: multi-assert test with two distinct measurements.

Each assert binds a different identifier (`v` and `t`) to its own
single assignment, so the AST extractor promotes BOTH to measurements.
The phase carries two measurement entries -- `v` with a numeric range
and `t` with a single bound -- and the rolled-up phase is PASS because
every validator passes against the captured value.
"""


def test_two_measurements():
    v = 5.0
    t = 25.0
    assert 4.8 <= v <= 5.2, "Supply voltage [V]"
    assert t >= 0.0, "Temperature [degC]"
