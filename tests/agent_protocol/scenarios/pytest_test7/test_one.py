"""First module that uses conftest's `shared_value`."""


def test_shared_in_one(shared_value):
    assert shared_value == "from-conftest"
