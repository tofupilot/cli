"""Scenario 4: confirm-only prompt (text_input=False)."""
import openhtf as htf
from openhtf.plugs import user_input


@htf.plug(prompts=user_input.UserInput)
def confirm_action(test, prompts):
    prompts.prompt(message="Please press any key when fixture is closed.", text_input=False)
    test.logger.info("operator acknowledged")


@htf.measures(htf.Measurement("voltage").in_range(4.5, 5.5))
def check_voltage(test):
    test.measurements.voltage = 5.0


if __name__ == "__main__":
    test = htf.Test(confirm_action, check_voltage, procedure_id="SCENARIO-04")
    test.execute(lambda: "SN-CONFIRM-0001")
