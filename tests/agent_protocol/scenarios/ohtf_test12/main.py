"""Scenario 12: phase with attachments."""
import openhtf as htf
import os


def produce_artifact(test):
    """Save a small text attachment next to the run."""
    p = os.path.join(os.path.dirname(__file__), "report.txt")
    with open(p, "w") as f:
        f.write("scenario 12: report\n")
    test.attach_from_file(p)


@htf.measures(htf.Measurement("ok").equals(True))
def check(test):
    test.measurements.ok = True


if __name__ == "__main__":
    test = htf.Test(produce_artifact, check, procedure_id="SCENARIO-12")
    test.execute(lambda: "SN-ATTACH-0001")
