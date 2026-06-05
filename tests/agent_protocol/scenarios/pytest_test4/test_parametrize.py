"""Scenario 4: parametrize expands to one phase per case (distinct nodeids)."""
import pytest


@pytest.mark.parametrize("x,y", [(1, 1), (2, 2), (3, 3)])
def test_equal(x, y):
    assert x == y


@pytest.mark.parametrize(
    "value",
    [10, pytest.param(99, marks=pytest.mark.xfail(reason="known mismatch"))],
)
def test_under_100(value):
    assert value < 100 or value == 99
