"""Shared stages. `power_off` is the teardown the signal test asserts on."""

from pathlib import Path

MARKER = Path(__file__).resolve().parent.parent / "teardown.marker"


def power_on(phase, run):
    MARKER.unlink(missing_ok=True)


def power_off(phase, run):
    MARKER.write_text("powered off\n")
