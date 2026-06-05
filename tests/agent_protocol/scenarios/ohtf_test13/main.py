"""Scenario 13: phases with docstrings."""
import openhtf as htf


def phase_a(test):
    """First phase: warms up the widget."""


def phase_b(test):
    """Second phase: measures the output."""


if __name__ == "__main__":
    test = htf.Test(phase_a, phase_b, procedure_id="SCENARIO-13")
    test.execute(lambda: "SN-DOCS-0001")
