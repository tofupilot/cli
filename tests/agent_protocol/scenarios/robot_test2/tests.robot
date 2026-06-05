*** Settings ***
Documentation    Scenario 2: single failing test.

*** Test Cases ***
This Should Fail
    Should Be Equal    one    two
