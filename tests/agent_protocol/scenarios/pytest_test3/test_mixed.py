"""Scenario 3: pass + fail + skip + xfail in one file."""
import pytest


def test_passes():
    assert True


def test_fails():
    assert 0 == 1


@pytest.mark.skip(reason="not yet implemented")
def test_skipped():
    assert False  # never runs


@pytest.mark.xfail(reason="known broken — should report as xfail")
def test_xfail():
    assert 0 == 1
