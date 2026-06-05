"""conftest.py shared across two test modules — verifies pytest discovers
fixtures correctly even when split across files."""
import pytest


@pytest.fixture
def shared_value():
    return "from-conftest"
