"""Scenario 8: dimensioned (multi-dim) measurement."""
import openhtf as htf
from openhtf.util import units


@htf.measures(
    htf.Measurement("voltage_vs_temp")
    .with_dimensions("celsius")
    .with_units(units.VOLT)
)
def sweep_voltage(test):
    for temp in [0, 25, 50]:
        test.measurements.voltage_vs_temp[temp] = 5.0 - 0.01 * temp


if __name__ == "__main__":
    test = htf.Test(sweep_voltage, procedure_id="SCENARIO-08")
    test.execute(lambda: "SN-SWEEP-0001")
