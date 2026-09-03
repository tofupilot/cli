"""Shared stages: run once per execution, belong to every slot's run."""


def power_on(phase, run):
    run.metadata["rack"] = "R-1"


def power_off(phase, run):
    pass
