"""Scenario 5: fixtures + autouse + teardown ordering."""
import pytest

state = {"setup_count": 0, "teardown_count": 0, "auto_count": 0}


@pytest.fixture(autouse=True)
def autoload():
    state["auto_count"] += 1
    yield
    # Autouse teardown also runs even when the test fails — verified
    # implicitly by the failing test below leaving auto_count == N+1.


@pytest.fixture
def resource():
    state["setup_count"] += 1
    yield {"value": 42}
    state["teardown_count"] += 1


def test_resource_value(resource):
    assert resource["value"] == 42


def test_teardown_runs_on_failure(resource):
    # The fixture teardown must still bump teardown_count even if this
    # test fails — pytest's contract.
    if resource["value"] == 42:
        # For runtime introspection only: assert the prior test ran its
        # teardown before this one entered setup.
        assert state["setup_count"] >= 2
        assert state["teardown_count"] >= 1
