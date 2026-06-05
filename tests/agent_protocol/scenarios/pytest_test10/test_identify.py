"""Scenario 10: bare run with no preset SN — connector must emit
test_start with phase plan, block on identify, accept resolved SN."""


def test_unit_present():
    # Body is trivial; the scenario validates the identify_request /
    # identify_resolved handshake on the wire, not the test logic.
    assert True
