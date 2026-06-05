"""Scenario 3: operator prompt (UserInput) — tests agent-protocol UI round-trip."""
import openhtf as htf
from openhtf.plugs import user_input


@htf.plug(prompts=user_input.UserInput)
def ask_operator_confirm(test, prompts):
    response = prompts.prompt(
        message="Is the UUT connected with USB and debug cable?",
        text_input=True,
    )
    test.logger.info(f"operator said: {response!r}")


@htf.measures(htf.Measurement("firmware_version").equals("1.4.3"))
def pcba_firmware_version(test):
    test.measurements.firmware_version = "1.4.3"


if __name__ == "__main__":
    test = htf.Test(
        ask_operator_confirm,
        pcba_firmware_version,
        procedure_id="SCENARIO-03",
    )
    test.execute(lambda: "SN-PROMPT-0001")
