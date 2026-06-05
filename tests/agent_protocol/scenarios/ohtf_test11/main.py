"""Scenario 11: PhaseResult.REPEAT explicit retry (fails twice, passes on 3rd)."""
import openhtf as htf

_n = {"c": 0}


@htf.PhaseOptions(repeat_limit=4)
def flaky(test):
    _n["c"] += 1
    test.logger.info(f"attempt {_n['c']}")
    if _n["c"] < 3:
        return htf.PhaseResult.REPEAT
    return htf.PhaseResult.CONTINUE


if __name__ == "__main__":
    test = htf.Test(flaky, procedure_id="SCENARIO-11")
    test.execute(lambda: "SN-REPEAT-0001")
