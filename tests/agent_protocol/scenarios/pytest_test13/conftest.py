"""Scenario 13: conftest.py exists, no test_*.py files.

`has_pytest` returns true (conftest.py is present), so the connector
spawns pytest. With no test files to collect, pytest exits with status
5 — and the connector should report SKIP, not PASS, so the dashboard
makes the empty-collection state visible.
"""
