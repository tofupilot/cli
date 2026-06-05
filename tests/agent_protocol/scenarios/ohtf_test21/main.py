"""Scenario 21: Segfault via ctypes (hard crash — no atexit, no stdout flush)."""
import ctypes
ctypes.string_at(0)
