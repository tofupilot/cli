"""Scenario 2: a single failing test — surfaces FAIL outcome + traceback."""


def test_will_fail():
    expected = 5
    actual = 3
    assert actual == expected, f"got {actual}, expected {expected}"
