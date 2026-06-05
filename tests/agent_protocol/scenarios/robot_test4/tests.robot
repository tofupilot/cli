*** Settings ***
Documentation    Scenario 4: numeric, string, boolean measurements via the keyword library.
Library          tofupilot_robot

*** Test Cases ***
Supply Voltage Is Stable
    Measure Numeric    voltage    5.01    min=4.8    max=5.2    unit=V    description=Supply voltage

Firmware Version Matches
    Measure String    firmware_version    1.4.2    expected=1.4.2

Self Test Reports OK
    Measure Boolean    self_test    ${True}    expected=${True}
