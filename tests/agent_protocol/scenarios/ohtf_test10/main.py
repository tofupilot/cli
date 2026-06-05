"""Scenario 10: phase with repeat_limit=3 (retries on fail)."""
import openhtf as htf
from openhtf import PhaseOptions


_counter = {"n": 0}


@htf.PhaseOptions(repeat_limit=3)
@htf.measures(htf.Measurement("attempt").equals(3))
def flaky_phase(test):
    _counter["n"] += 1
    test.measurements.attempt = _counter["n"]


if __name__ == "__main__":
    test = htf.Test(flaky_phase, procedure_id="SCENARIO-10")
    test.execute(lambda: "SN-RETRY-0001")
