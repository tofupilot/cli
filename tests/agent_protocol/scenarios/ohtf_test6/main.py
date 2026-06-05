"""Scenario 6: multi-phase test with exception in one phase."""
import openhtf as htf


def explode(test):
    raise RuntimeError("sensor not responding")


@htf.measures(htf.Measurement("voltage").in_range(4.5, 5.5))
def check_voltage(test):
    test.measurements.voltage = 5.0


if __name__ == "__main__":
    test = htf.Test(explode, check_voltage, procedure_id="SCENARIO-06")
    test.execute(lambda: "SN-EXCEPT-0001")
