"""Scenario 1: three trivial passing tests."""


def test_first():
    assert 1 + 1 == 2


def test_second():
    assert "abc".upper() == "ABC"


def test_third():
    assert sorted([3, 1, 2]) == [1, 2, 3]
