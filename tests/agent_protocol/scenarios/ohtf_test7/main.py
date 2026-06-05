"""Scenario 7: multiple prompts in a single phase."""
import openhtf as htf
from openhtf.plugs import user_input


@htf.plug(prompts=user_input.UserInput)
def multi_step_inspection(test, prompts):
    a = prompts.prompt(message="Enter operator ID:", text_input=True)
    b = prompts.prompt(message="Enter fixture slot letter (A/B/C):", text_input=True)
    test.logger.info(f"operator={a!r} slot={b!r}")


if __name__ == "__main__":
    test = htf.Test(multi_step_inspection, procedure_id="SCENARIO-07")
    test.execute(lambda: "SN-MULTI-0001")
