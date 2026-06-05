"""Scenario 20: Subprocess killed mid-phase (simulated via sys.exit)."""
import sys
import openhtf as htf


def finish_normally(test):
    test.logger.info("ok")


def mid_phase_exit(test):
    # Bypass OpenHTF's error handling by killing the interpreter outright.
    sys.exit(42)


if __name__ == "__main__":
    test = htf.Test(finish_normally, mid_phase_exit, procedure_id="SCENARIO-20")
    test.execute(lambda: "SN-EXIT-0001")
