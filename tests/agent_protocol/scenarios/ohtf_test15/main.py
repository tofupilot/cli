"""Scenario 15: phase timeout_s triggers failure."""
import time
import openhtf as htf


@htf.PhaseOptions(timeout_s=0.5)
def slow_phase(test):
    time.sleep(5)  # exceeds 0.5s timeout


if __name__ == "__main__":
    test = htf.Test(slow_phase, procedure_id="SCENARIO-15")
    test.execute(lambda: "SN-TIMEOUT-0001")
