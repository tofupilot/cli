"""Second module that uses the same conftest fixture."""


def test_shared_in_two(shared_value):
    assert shared_value.startswith("from-")
