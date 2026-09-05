"""Per-slot soak, long enough for the test to interrupt it."""

import time


def soak(phase, run):
    time.sleep(30)
