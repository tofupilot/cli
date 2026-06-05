"""Scenario 14: PhaseGroup with setup/main/teardown (teardown runs even on failure)."""
import openhtf as htf


def setup_phase(test):
    test.logger.info("setup: acquiring resources")


def main_phase_fail(test):
    raise RuntimeError("boom")


def teardown_phase(test):
    test.logger.info("teardown: releasing resources")


if __name__ == "__main__":
    group = htf.PhaseGroup(
        setup=[setup_phase],
        main=[main_phase_fail],
        teardown=[teardown_phase],
    )
    test = htf.Test(group, procedure_id="SCENARIO-14")
    test.execute(lambda: "SN-GROUP-0001")
