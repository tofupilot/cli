"""Scenario 2: measurement fails validation (out-of-range)."""
import openhtf as htf
from openhtf.util import units


@htf.measures(htf.Measurement("input_voltage").in_range(4.5, 5).with_units(units.VOLT))
def check_voltage_too_low(test):
    test.measurements.input_voltage = 3.1  # out of range


@htf.measures(htf.Measurement("temperature_c").in_range(-10, 50))
def check_temperature(test):
    test.measurements.temperature_c = 22.5


if __name__ == "__main__":
    test = htf.Test(
        check_voltage_too_low,
        check_temperature,
        procedure_id="SCENARIO-02",
    )
    test.execute(lambda: "SN-FAIL-0001")
