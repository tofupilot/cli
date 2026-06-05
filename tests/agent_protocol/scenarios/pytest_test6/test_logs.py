"""Scenario 6: print + logging + pytest.fail surface in phase_log."""
import logging

import pytest


def test_uses_print():
    print("hello from print")
    print("second line", flush=True)
    assert True


def test_uses_logging():
    log = logging.getLogger("scenario6")
    log.info("info-level message")
    log.warning("warning-level message")
    assert True


def test_calls_pytest_fail():
    pytest.fail("explicit pytest.fail() with reason")
