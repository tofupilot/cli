*** Settings ***
Documentation    Scenario 8: keyword raises a runtime exception (NOT a
...              Measure assertion). The wire outcome must be ERROR, not
...              FAIL — Measure failures are limit violations; bare
...              exceptions are crashes.
Library          Collections

*** Test Cases ***
Crashes On Bad Index
    @{items}=    Create List    a    b
    # Out-of-range access — Robot reports as FAIL, but with no
    # Measure assertion → connector wires ERROR.
    Log    ${items}[99]
