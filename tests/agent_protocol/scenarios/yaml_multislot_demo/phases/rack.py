"""Shared stages, once per fixture."""
import time


def power_on(phase, run):
    run.metadata["rack"] = "R-1"
    time.sleep(2)


def power_off(phase, run):
    time.sleep(2)
