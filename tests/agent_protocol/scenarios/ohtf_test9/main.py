"""Scenario 9: skip phase via PhaseResult.SKIP."""
import openhtf as htf


def skip_me(test):
    return htf.PhaseResult.SKIP


@htf.measures(htf.Measurement("voltage").in_range(4.5, 5.5))
def check_voltage(test):
    test.measurements.voltage = 5.0


if __name__ == "__main__":
    test = htf.Test(skip_me, check_voltage, procedure_id="SCENARIO-09")
    test.execute(lambda: "SN-SKIP-0001")
