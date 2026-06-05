"""Scenario 16: variable-bound assertion rejected by extraction rule.

The bounds (`v_min`, `v_max`) are not literal `Constant` nodes, so the
numeric closed-range pattern does not match. The phase still passes;
no measurement is emitted.
"""


def test_variable_bounds():
    v_min = 4.8
    v_max = 5.2
    v = 5.0
    assert v_min <= v <= v_max
