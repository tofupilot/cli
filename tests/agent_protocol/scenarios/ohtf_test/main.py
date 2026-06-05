"""Scenario 1: simple OpenHTF test with measurements, no UI prompts."""
import openhtf as htf
from openhtf.util import units


@htf.measures(htf.Measurement("firmware_version").equals("1.4.3"))
def pcba_firmware_version(test):
    test.measurements.firmware_version = "1.4.3"


@htf.measures(htf.Measurement("input_voltage").in_range(4.5, 5).with_units(units.VOLT))
def check_voltage_input(test):
    test.measurements.input_voltage = 4.7


@htf.measures(htf.Measurement("temperature_c").in_range(-10, 50))
def check_temperature(test):
    test.measurements.temperature_c = 22.5


if __name__ == "__main__":
    test = htf.Test(
        pcba_firmware_version,
        check_voltage_input,
        check_temperature,
        procedure_id="SCENARIO-01",
        part_number="00220",
        revision="A",
    )
    test.execute(lambda: "SN-0001")
