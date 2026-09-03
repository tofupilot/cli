"""Slot stages: one instance per slot, each with its own unit."""


def prep(phase, run, unit):
    assert unit.serial_number == f"SMOKE-{run.slot_id}", unit.serial_number


def measure(phase, run, measurements):
    measurements.voltage = 3.3


def release(phase, run):
    pass
