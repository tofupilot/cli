"""Scenario 16: phase logs at different levels."""
import openhtf as htf


def noisy(test):
    test.logger.debug("dbg line")
    test.logger.info("info line")
    test.logger.warning("warn line")
    test.logger.error("error line — but phase still passes")


if __name__ == "__main__":
    test = htf.Test(noisy, procedure_id="SCENARIO-16")
    test.execute(lambda: "SN-LOGS-0001")
