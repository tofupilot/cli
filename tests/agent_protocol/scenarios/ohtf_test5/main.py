"""Scenario 5: prompt with image + text input."""
import openhtf as htf
from openhtf.plugs import user_input


@htf.plug(prompts=user_input.UserInput)
def visual_inspection(test, prompts):
    response = prompts.prompt(
        message="Does the board match the reference image?",
        text_input=True,
        image_url="https://example.com/reference.png",
    )
    test.logger.info(f"visual inspection: {response!r}")


if __name__ == "__main__":
    test = htf.Test(visual_inspection, procedure_id="SCENARIO-05")
    test.execute(lambda: "SN-VISUAL-0001")
