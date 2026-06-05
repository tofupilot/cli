*** Settings ***
Documentation    Scenario 1: three trivial passing tests.
Library          Collections

*** Test Cases ***
Adds Two Numbers
    Should Be Equal As Integers    2    2

Uppercases A String
    Should Be Equal    ABC    ABC

Sorts A List
    @{actual}=    Create List    1    2    3
    @{expected}=    Create List    1    2    3
    Lists Should Be Equal    ${actual}    ${expected}
