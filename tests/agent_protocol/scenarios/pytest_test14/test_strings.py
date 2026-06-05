"""Scenario 14: AST-extracted string measurements.

One equality, one membership. Both should turn into string measurements
with the captured runtime value and the right validators.
"""


def test_mode():
    mode = "production"
    assert mode == "production", "Operating mode"


def test_color():
    color = "green"
    assert color in ("red", "green", "blue"), "LED color"
