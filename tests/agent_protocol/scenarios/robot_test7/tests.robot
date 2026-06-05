*** Settings ***
Documentation    Scenario 7: Suite Setup raises. All tests are aborted.
...              The connector wires ERROR (no measurement assertion
...              fired, so the FAIL Robot reports collapses to ERROR)
...              and emits a "Suite Setup failed" phase_log surfaced
...              via result.setup.status in listener API v3.
Suite Setup      Fail    suite-setup intentionally aborts the run

*** Test Cases ***
Would Have Run
    Log    this body never executes
