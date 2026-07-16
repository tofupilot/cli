def set_metadata(log, run, unit):
    # Run-level metadata
    run.metadata["line"] = "L4"
    run.metadata["operator_note"] = "coolant loop rework"
    run.metadata["cycles"] = 3
    run.metadata["stable"] = True

    # Operator/identify metadata is readable in phases (auto_identify
    # default was MOD-1)
    assert unit.metadata.get("modification") == "MOD-1", unit.metadata

    # Unit-level: overrides the identify-form MOD default per key
    unit.metadata["modification"] = "MOD-42"
    unit.metadata["calibrated"] = True

    # Validation errors raise at assignment with a clear message
    try:
        run.metadata["bad key!"] = 1
        raise AssertionError("invalid metadata key was accepted")
    except ValueError:
        pass

    try:
        run.metadata["bad_value"] = {"nested": True}
        raise AssertionError("non-scalar metadata value was accepted")
    except TypeError:
        pass

    # |= and reassignment route through validation too
    run.metadata |= {"merged": "ok"}
    try:
        run.metadata |= {"bad": [1, 2]}
        raise AssertionError("|= bypassed validation")
    except TypeError:
        pass
    try:
        run.metadata = {"bad": float("nan")}
        raise AssertionError("reassignment bypassed validation")
    except ValueError:
        pass

    # setdefault(key) without a default reports absence, doesn't insert None
    assert run.metadata.setdefault("missing") is None
    assert "missing" not in run.metadata

    log.info(f"run_md={dict(run.metadata)} unit_md={dict(unit.metadata)}")
