"""Scenario 12: a slow test — verify phase events arrive live (not all at end)."""
import time


def test_quick_one():
    assert True


def test_slow():
    # 2s is enough to confirm phase_started lands before phase_finished
    # without making the validator suite painful to wait on.
    time.sleep(2.0)
    assert True


def test_quick_two():
    assert True
