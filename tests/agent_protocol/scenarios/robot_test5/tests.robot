*** Settings ***
Documentation    Scenario 5: Test Template (parametrize-equivalent) — same keyword reused with multiple data rows.
Library          tofupilot_robot

*** Test Cases ***
Voltage 4.9V Within Range
    Measure In Range    voltage_a    4.9

Voltage 5.0V Within Range
    Measure In Range    voltage_b    5.0

Voltage 5.1V Within Range
    Measure In Range    voltage_c    5.1

*** Keywords ***
Measure In Range
    [Arguments]    ${name}    ${value}
    Measure Numeric    ${name}    ${value}    min=4.8    max=5.2    unit=V
