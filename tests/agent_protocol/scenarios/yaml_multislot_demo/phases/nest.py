"""Slot stages, one instance per nest. Slot 3 fails its measurement so
the grid shows a mixed outcome."""
import time


def prep(phase, run, unit):
    time.sleep(1 + 0.5 * int(run.slot_id[-1]))


def seat(phase, run):
    pass


def measure(phase, run, measurements):
    time.sleep(2)
    measurements.voltage = 3.3 if run.slot_id != "s3" else 2.1
    if run.slot_id == "s3":
        phase.fail("voltage 2.1 V below 3.0 V limit")


def burn_in(phase, run):
    time.sleep(4 + int(run.slot_id[-1]))


def release(phase, run):
    time.sleep(1)
