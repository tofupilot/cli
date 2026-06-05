"""Scenario 19: Exception raised before htf.Test is constructed."""
import openhtf as htf

raise RuntimeError("boot-time explosion")
