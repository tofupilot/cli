#!/usr/bin/env python3
"""
NDJSON TCP-based Python worker with complete standalone implementation.
No external dependencies beyond the Python standard library.
"""


import sys
import json
import os
import time
import traceback
import importlib.util
import warnings
import base64
import inspect
import queue
import threading
import socket
from pathlib import Path
from typing import Dict, Any, Optional, List, Union, Literal

# Save original stderr before any redirection
_original_stderr = sys.__stderr__


# ============================================================================
# Logging (inlined from tp_logs.py)
# ============================================================================

class Logs:
    def __init__(self, job_id=None, event_queue=None):
        self.entries = []
        self.job_id = job_id
        # Same rationale as LogCapturingStream: push each line onto the live
        # event queue so the orchestrator can surface them in real time
        # (essential for debugging a phase that's force-killed before it
        # reaches its natural completion).
        self.event_queue = event_queue

    def _add_log(self, level: str, message: str):
        from datetime import datetime, timezone
        import inspect as _inspect
        import os as _os

        frame = _inspect.currentframe()
        caller_frame = (
            frame.f_back.f_back
            if frame and frame.f_back and frame.f_back.f_back
            else None
        )

        file_path = None
        line_number = None
        if caller_frame:
            file_path = caller_frame.f_code.co_filename
            line_number = caller_frame.f_lineno
            try:
                if file_path.startswith("/"):
                    base_dir = _os.path.dirname(
                        _os.path.dirname(_os.path.abspath(__file__))
                    )
                    if file_path.startswith(base_dir):
                        file_path = _os.path.relpath(file_path, base_dir)
            except:
                pass

        timestamp = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")

        entry = {
            "timestamp": timestamp,
            "level": level,
            "message": message,
        }

        if file_path:
            entry["file"] = file_path
        if line_number:
            entry["line"] = line_number

        self.entries.append(entry)

        if self.event_queue is not None:
            try:
                live_event = {
                    "type": "PhaseLogLine",
                    "job_id": self.job_id,
                    "level": level,
                    "message": message,
                    "timestamp": timestamp,
                }
                if file_path:
                    live_event["file"] = file_path
                if line_number:
                    live_event["line"] = line_number
                self.event_queue.put(live_event)
            except Exception:
                pass

    def info(self, message: str):
        self._add_log("INFO", message)

    def warning(self, message: str):
        self._add_log("WARNING", message)

    def error(self, message: str):
        self._add_log("ERROR", message)

    def debug(self, message: str):
        self._add_log("DEBUG", message)


class LogCapturingStream:
    def __init__(self, logs_obj, level="INFO", job_id=None, emit_to_stderr=False,
                 event_queue=None):
        self.logs = logs_obj
        self.level = level
        self.job_id = job_id
        self.buffer = ""
        self.emit_to_stderr = emit_to_stderr
        # When provided, every captured log line is also pushed onto the
        # event queue so the Rust orchestrator sees it live — not only in
        # the final JobComplete bundle. Without this, a phase that's force-
        # killed mid-run has all its logs discarded.
        self.event_queue = event_queue

    def write(self, text):
        self.buffer += text
        while "\n" in self.buffer:
            line, self.buffer = self.buffer.split("\n", 1)
            if line.strip():
                from datetime import datetime, timezone

                timestamp = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")

                entry = {
                    "timestamp": timestamp,
                    "level": self.level,
                    "message": line.strip(),
                }
                self.logs.entries.append(entry)

                if self.event_queue is not None:
                    try:
                        self.event_queue.put({
                            "type": "PhaseLogLine",
                            "job_id": self.job_id,
                            "level": self.level,
                            "message": line.strip(),
                            "timestamp": timestamp,
                        })
                    except Exception:
                        pass

                if self.emit_to_stderr and _original_stderr:
                    try:
                        _original_stderr.write(json.dumps(entry) + "\n")
                        _original_stderr.flush()
                    except Exception:
                        pass

    def flush(self):
        pass


# ============================================================================
# Domain classes
# ============================================================================

class EventQueue:
    def __init__(self):
        self._queue = queue.Queue()
        self._closed = False
        self._lock = threading.Lock()

    def put(self, item: Any) -> None:
        with self._lock:
            if self._closed:
                return
        self._queue.put(item)

    def get(self, timeout: float = 0.1) -> Optional[Any]:
        try:
            return self._queue.get(timeout=timeout)
        except queue.Empty:
            return None

    def close(self) -> None:
        with self._lock:
            self._closed = True

    def is_closed(self) -> bool:
        with self._lock:
            return self._closed

    def empty(self) -> bool:
        return self._queue.empty()


class UIContext:
    """Simplified UI context that only allows updating display components"""

    def __init__(self, job_id: str, event_queue: EventQueue):
        object.__setattr__(self, "job_id", job_id)
        object.__setattr__(self, "event_queue", event_queue)

    def __setattr__(self, name: str, value: Any):
        if name in ("job_id", "event_queue"):
            object.__setattr__(self, name, value)
            return

        self.event_queue.put(
            {
                "type": "ui_update",
                "job_id": self.job_id,
                "action": "set_value",
                "data": {"id": name, "value": value},
            }
        )


class _PhaseResultException(Exception):
    """Internal exception for phase result control flow."""

    def __init__(self, result_value: str, message: str = ""):
        self.result_value = result_value
        self.message = message
        super().__init__(message)


class Phase:
    """Phase execution control."""

    @staticmethod
    def fail(message: str = "") -> None:
        raise _PhaseResultException(PhaseResult.FAIL, message)

    @staticmethod
    def skip(message: str = "") -> None:
        raise _PhaseResultException(PhaseResult.SKIP, message)

    @staticmethod
    def retry(message: str = "") -> None:
        raise _PhaseResultException(PhaseResult.RETRY, message)

    @staticmethod
    def stop(message: str = "") -> None:
        raise _PhaseResultException(PhaseResult.STOP, message)

    def __repr__(self):
        return f"<Phase(fail, skip, retry, stop)>"


class ValidatorLevel:
    CRITICAL = "critical"
    ALERT = "alert"
    NOTICE = "notice"


class ValidatorOutcome:
    PASS = "PASS"
    FAIL = "FAIL"
    UNSET = "UNSET"


ValidatorOutcomeType = Literal["PASS", "FAIL", "UNSET"]
ValidatorLevelType = Literal["critical", "alert", "notice"]


class PhaseResult:
    """Phase result values matching Rust PhaseResult enum"""
    CONTINUE = "Continue"
    RETRY = "Retry"
    SKIP = "Skip"
    STOP = "Stop"
    FAIL = "Fail"


PhaseResultType = Literal["Continue", "Retry", "Skip", "Stop", "Fail"]


class Axis:
    """Axis specification for multi-dimensional data matching Rust AxisSpec"""

    def __init__(
        self,
        data: List[Union[float, int, str]],
        aggregations: Optional[List["Aggregation"]] = None,
        validators: Optional[List["Validator"]] = None,
    ):
        if not isinstance(data, list):
            raise ValueError(f"Axis data must be a list, got {type(data)}")

        self.data = data
        self.aggregations = aggregations or []
        self.validators = validators or []

    def to_dict(self) -> Dict[str, Any]:
        result = {"data": self.data}
        if hasattr(self, "unit") and self.unit is not None:
            result["unit"] = self.unit
        if hasattr(self, "legend") and self.legend is not None:
            result["legend"] = self.legend
        if self.aggregations:
            result["aggregations"] = [
                a.to_dict() if hasattr(a, "to_dict") else a for a in self.aggregations
            ]
        if self.validators:
            result["validators"] = [
                v.to_dict() if hasattr(v, "to_dict") else v for v in self.validators
            ]
        return result


class MultiDim:
    """Multi-dimensional measurement specification matching Rust MultiDimensionalSpec"""

    def __init__(
        self,
        x_axis: Axis,
        y_axis: List[Axis],
    ):
        if not isinstance(x_axis, Axis):
            raise ValueError(f"x_axis must be an Axis instance, got {type(x_axis)}")

        if not isinstance(y_axis, list):
            raise ValueError(f"y_axis must be a list, got {type(y_axis)}")

        for i, axis in enumerate(y_axis):
            if not isinstance(axis, Axis):
                raise ValueError(
                    f"y_axis[{i}] must be an Axis instance, got {type(axis)}"
                )

        x_length = len(x_axis.data)
        for i, axis in enumerate(y_axis):
            if len(axis.data) != x_length:
                raise ValueError(
                    f"y_axis[{i}] data length ({len(axis.data)}) must match "
                    f"x_axis data length ({x_length})"
                )

        self.x_axis = x_axis
        self.y_axis = y_axis

    def to_dict(self) -> Dict[str, Any]:
        result = {
            "x_axis": self.x_axis.to_dict(),
            "y_axis": [axis.to_dict() for axis in self.y_axis],
        }
        return result


ScalarValueType = Union[float, int, str, bool]
MeasurementValueType = Union[float, int, str, bool, List, Dict, MultiDim]


class Validator:
    """Validator specification for Python usage"""

    def __init__(
        self,
        level: ValidatorLevelType = None,
        outcome: ValidatorOutcomeType = None,
        operator: str = None,
    ):
        if level is not None and level not in [
            ValidatorLevel.CRITICAL,
            ValidatorLevel.ALERT,
            ValidatorLevel.NOTICE,
        ]:
            raise ValueError(f"Invalid validator level: {level}")
        self.level = level

        if outcome is not None and outcome not in [
            ValidatorOutcome.PASS,
            ValidatorOutcome.FAIL,
            ValidatorOutcome.UNSET,
        ]:
            raise ValueError(f"Invalid validator outcome: {outcome}")
        self.outcome = outcome

        self.operator = operator

    def to_dict(self) -> Dict[str, Any]:
        result = {}
        if self.level is not None:
            result["level"] = self.level
        if self.outcome is not None:
            result["outcome"] = self.outcome
        if self.operator is not None:
            result["operator"] = self.operator
        return result


class Aggregation:
    """Aggregation specification for Python usage"""

    def __init__(
        self,
        aggregation_type: str,
        value: Optional[MeasurementValueType] = None,
        outcome: Optional[ValidatorOutcomeType] = None,
        validators: Optional[List[Validator]] = None,
    ):
        self.aggregation_type = aggregation_type
        self.value = value

        if outcome is not None and outcome not in [
            ValidatorOutcome.PASS,
            ValidatorOutcome.FAIL,
            ValidatorOutcome.UNSET,
        ]:
            raise ValueError(f"Invalid aggregation outcome: {outcome}")
        self.outcome = outcome

        if validators:
            for v in validators:
                if not isinstance(v, Validator):
                    raise ValueError(
                        f"Validators must be Validator instances, got {type(v)}"
                    )
        self.validators = validators or []

    def to_dict(self) -> Dict[str, Any]:
        result = {"type": self.aggregation_type}
        if self.outcome is not None:
            result["outcome"] = self.outcome
        if self.value is not None:
            result["value"] = self.value
        if self.validators:
            result["validators"] = [v.to_dict() for v in self.validators]
        return result


class Measurement:
    """Measurement for Python usage"""

    def __init__(
        self,
        name: str,
        value: Optional[MeasurementValueType] = None,
        validators: Optional[List[Validator]] = None,
        aggregations: Optional[List[Aggregation]] = None,
    ):
        if not isinstance(name, str) or not name:
            raise ValueError("Measurement name must be a non-empty string")
        self.name = name

        if value is not None:
            if isinstance(value, MultiDim):
                self.value = value.to_dict()
                self.is_multi_dimensional = True
            elif isinstance(value, (float, int, str, bool, list, dict)):
                self.value = value
                self.is_multi_dimensional = (
                    isinstance(value, dict) and "x_axis" in value
                )
            else:
                raise ValueError(f"Invalid measurement value type: {type(value)}")
        else:
            self.value = None
            self.is_multi_dimensional = False

        if validators:
            for v in validators:
                if not isinstance(v, Validator):
                    raise ValueError(
                        f"Validators must be Validator instances, got {type(v)}"
                    )
        self.validators = validators or []

        if aggregations:
            for a in aggregations:
                if not isinstance(a, Aggregation):
                    raise ValueError(
                        f"Aggregations must be Aggregation instances, got {type(a)}"
                    )
        self.aggregations = aggregations or []

        if hasattr(self, "unit") or hasattr(self, "docstring"):
            warnings.warn(
                "Python should only set measurement values and outcomes. Define metadata in YAML.",
                UserWarning,
            )

    def to_dict(self) -> Dict[str, Any]:
        result = {
            "name": self.name,
            "value": self.value,
            "timestamp": time.strftime("%H:%M:%S.%f")[:-3],
        }
        if self.validators:
            result["validators"] = [v.to_dict() for v in self.validators]
        if self.aggregations:
            result["aggregations"] = [a.to_dict() for a in self.aggregations]
        return result


class SubUnitsDict(dict):
    """Dict of sub-unit serial numbers with attribute access (read/write)."""

    def __init__(self, sub_units_dict: Dict[str, str]):
        super().__init__(sub_units_dict)
        object.__setattr__(self, '_attr_to_label', {})
        for label in sub_units_dict.keys():
            attr_name = self._sanitize_label(label)
            if attr_name:
                self._attr_to_label[attr_name] = label

    @staticmethod
    def _sanitize_label(label: str) -> Optional[str]:
        import re
        name = label.lower().replace(" ", "_").replace("-", "_")
        name = re.sub(r'[^a-z0-9_]', '', name)
        if name and name[0].isdigit():
            name = f"_{name}"
        return name if name else None

    def __getitem__(self, key: str) -> str:
        if key in self.keys():
            return super().__getitem__(key)
        key_lower = key.lower()
        for label in self.keys():
            if label.lower() == key_lower:
                return super().__getitem__(label)
        raise KeyError(f"No sub-unit named '{key}'")

    def __setitem__(self, key: str, value: str):
        if key in self.keys():
            super().__setitem__(key, value)
            return
        key_lower = key.lower()
        for label in self.keys():
            if label.lower() == key_lower:
                super().__setitem__(label, value)
                return
        raise KeyError(f"No sub-unit named '{key}'. Cannot add new sub-units at runtime.")

    def __getattr__(self, name: str) -> str:
        if name.startswith('__') or name == '_attr_to_label':
            return object.__getattribute__(self, name)
        attr_to_label = object.__getattribute__(self, '_attr_to_label')
        if name in attr_to_label:
            return self[attr_to_label[name]]
        raise AttributeError(f"No sub-unit named '{name}'")

    def __setattr__(self, name: str, value: str):
        if name.startswith('__') or name == '_attr_to_label':
            object.__setattr__(self, name, value)
            return
        attr_to_label = object.__getattribute__(self, '_attr_to_label')
        if name in attr_to_label:
            self[attr_to_label[name]] = value
        else:
            raise AttributeError(f"No sub-unit named '{name}'. Cannot add new sub-units at runtime.")


class Unit:
    def __init__(self):
        self.serial_number: Optional[str] = None
        self.batch_number: Optional[str] = None
        self.part_number: Optional[str] = None
        self.revision_number: Optional[str] = None
        self.sub_units: Optional[SubUnitsDict] = None


class _AggregationsReadProxy:
    """Read-only proxy for accessing aggregation values from cross-phase results."""

    def __init__(self, aggregations_list: list):
        lookup = {}
        for agg in aggregations_list:
            agg_type = agg.get("type", "")
            if "value" in agg and agg["value"] is not None:
                val = agg["value"]
                if isinstance(val, dict):
                    for _, v in val.items():
                        val = v
                        break
                lookup[agg_type] = val
        object.__setattr__(self, "_lookup", lookup)

    def __getattr__(self, name: str):
        if name.startswith("_"):
            raise AttributeError(f"'_AggregationsReadProxy' object has no attribute '{name}'")
        lookup = object.__getattribute__(self, "_lookup")
        if name in lookup:
            return lookup[name]
        raise AttributeError(f"No aggregation '{name}' found")

    def __repr__(self):
        return f"<_AggregationsReadProxy({list(self._lookup.keys())})>"


class _MeasurementReadProxy:
    """Read-only proxy wrapping a measurement value with aggregation access."""

    def __init__(self, value, aggregations: list):
        object.__setattr__(self, "_value", value)
        object.__setattr__(self, "_aggregations", aggregations)

    @property
    def aggregations(self):
        return _AggregationsReadProxy(object.__getattribute__(self, "_aggregations"))

    def _unwrap(self, other):
        if isinstance(other, (_MeasurementReadProxy, _MeasurementValueProxy)):
            return object.__getattribute__(other, "_value")
        return other

    def __repr__(self):
        return repr(self._value)

    def __str__(self):
        return str(self._value)

    def __bool__(self):
        return bool(self._value)

    def __hash__(self):
        return hash(self._value)

    def __float__(self):
        return float(self._value)

    def __int__(self):
        return int(self._value)

    def __index__(self):
        return self._value.__index__()

    def __eq__(self, other):
        return self._value == self._unwrap(other)

    def __ne__(self, other):
        return self._value != self._unwrap(other)

    def __lt__(self, other):
        return self._value < self._unwrap(other)

    def __le__(self, other):
        return self._value <= self._unwrap(other)

    def __gt__(self, other):
        return self._value > self._unwrap(other)

    def __ge__(self, other):
        return self._value >= self._unwrap(other)

    def __add__(self, other):
        return self._value + self._unwrap(other)

    def __radd__(self, other):
        return other + self._value

    def __sub__(self, other):
        return self._value - self._unwrap(other)

    def __rsub__(self, other):
        return other - self._value

    def __mul__(self, other):
        return self._value * self._unwrap(other)

    def __rmul__(self, other):
        return other * self._value

    def __truediv__(self, other):
        return self._value / self._unwrap(other)

    def __rtruediv__(self, other):
        return other / self._value

    def __neg__(self):
        return -self._value

    def __abs__(self):
        return abs(self._value)

    def __round__(self, ndigits=None):
        return round(self._value, ndigits)

    def __len__(self):
        return len(self._value)

    def __iter__(self):
        return iter(self._value)

    def __getitem__(self, key):
        return self._value[key]

    def __contains__(self, item):
        return item in self._value


class _PhaseResultsMeasurements:
    """Read-only proxy for accessing measurements from a completed phase with MDM support."""

    def __init__(self, data: dict):
        object.__setattr__(self, "_data", data)

    def __getattr__(self, name: str):
        if name.startswith("_"):
            raise AttributeError(f"'_PhaseResultsMeasurements' object has no attribute '{name}'")
        if name in self._data:
            value = self._data[name]
            if isinstance(value, dict) and "__value__" in value:
                raw_value = value["__value__"]
                aggregations = value.get("__aggregations__", [])
                return _MeasurementReadProxy(raw_value, aggregations)
            if isinstance(value, dict) and ("x_axis" in value or "y_axis" in value):
                return _MDMReadProxy(value)
            return value
        raise AttributeError(f"No measurement '{name}' found")

    def __repr__(self):
        return f"<_PhaseResultsMeasurements({list(self._data.keys())})>"


class PhaseResults:
    """Read-only access to a completed phase's measurements."""

    def __init__(self, function_name: str, data: dict):
        object.__setattr__(self, "_function_name", function_name)
        object.__setattr__(self, "_data", data)

    def __setattr__(self, name, value):
        raise AttributeError("PhaseResults is read-only")

    def __getattr__(self, name: str):
        if name.startswith("_"):
            raise AttributeError(f"'PhaseResults' object has no attribute '{name}'")
        if name == "measurements":
            return _PhaseResultsMeasurements(self._data)
        if name in self._data:
            return self._data[name]
        raise AttributeError(
            f"Phase '{self._function_name}' has no measurement '{name}'"
        )

    def __repr__(self):
        return f"<PhaseResults({self._function_name}: {list(self._data.keys())})>"


class Run:
    """Read-only run information"""

    def __init__(self, slot_id: str, job_id: str, retry_count: int, retry_limit: int):
        self.slot_id = slot_id
        self.job_id = job_id
        self.retry_count = retry_count
        self.retry_limit = retry_limit


class Attachments:
    def __init__(self, job_id: str, event_queue: EventQueue):
        self.job_id = job_id
        self.event_queue = event_queue

    def file(self, file_path: str, attachment_name: str = None):
        if not os.path.isabs(file_path):
            procedure_dir = Path(os.getcwd())
            file_path = str(procedure_dir / file_path)

        if attachment_name is None:
            attachment_name = os.path.basename(file_path)

        self.event_queue.put(
            {
                "type": "attach_file",
                "job_id": self.job_id,
                "source_path": file_path,
                "attachment_name": attachment_name,
            }
        )

    def data(self, data: bytes, attachment_name: str):
        self.event_queue.put(
            {
                "type": "attach_data",
                "job_id": self.job_id,
                "data": base64.b64encode(data).decode("utf-8"),
                "attachment_name": attachment_name,
            }
        )


class _YAxisAggregationsWriteProxy:
    def __init__(self, builder: "_MDMBuilder", axis_key: str):
        object.__setattr__(self, "_builder", builder)
        object.__setattr__(self, "_axis_key", axis_key)

    def __setattr__(self, name: str, value):
        if name.startswith("_"):
            object.__setattr__(self, name, value)
            return
        if isinstance(value, (_MeasurementValueProxy, _MeasurementReadProxy)):
            value = object.__getattribute__(value, "_value")
        axis_key = object.__getattribute__(self, "_axis_key")
        builder = object.__getattribute__(self, "_builder")
        aggs = builder._y_axis_aggregations.setdefault(axis_key, [])
        for agg in aggs:
            if agg["type"] == name:
                agg["value"] = value
                builder._flush()
                return
        aggs.append({"type": name, "value": value})
        builder._flush()

    def __getattr__(self, name: str):
        if name.startswith("_"):
            raise AttributeError(f"'_YAxisAggregationsWriteProxy' object has no attribute '{name}'")
        axis_key = object.__getattribute__(self, "_axis_key")
        builder = object.__getattribute__(self, "_builder")
        for agg in builder._y_axis_aggregations.get(axis_key, []):
            if agg["type"] == name:
                return agg.get("value")
        raise AttributeError(f"No aggregation '{name}' found on y-axis '{axis_key}'")


class _YAxisDataProxy:
    def __init__(self, data: list, builder: "_MDMBuilder", axis_key: str):
        object.__setattr__(self, "_data", data)
        object.__setattr__(self, "_builder", builder)
        object.__setattr__(self, "_axis_key", axis_key)

    @property
    def aggregations(self):
        return _YAxisAggregationsWriteProxy(
            object.__getattribute__(self, "_builder"),
            object.__getattribute__(self, "_axis_key"),
        )

    def __iter__(self):
        return iter(self._data)

    def __len__(self):
        return len(self._data)

    def __getitem__(self, idx):
        return self._data[idx]

    def __repr__(self):
        return repr(self._data)


class _YAxisProxy:
    def __init__(self, builder: "_MDMBuilder"):
        object.__setattr__(self, "_builder", builder)

    def __setattr__(self, key: str, data):
        if key.startswith("_"):
            object.__setattr__(self, key, data)
            return
        if not isinstance(data, list):
            raise ValueError(f"Y-axis data must be a list, got {type(data)}")
        self._builder._y_axes[key] = data
        self._builder._flush()

    def __getattr__(self, key: str):
        if key.startswith("_"):
            raise AttributeError(f"'_YAxisProxy' object has no attribute '{key}'")
        if key in self._builder._y_axes:
            return _YAxisDataProxy(self._builder._y_axes[key], self._builder, key)
        raise AttributeError(f"No y-axis '{key}' set")


class _MDMBuilder:
    def __init__(self, name: str, parent: "Measurements"):
        object.__setattr__(self, "_name", name)
        object.__setattr__(self, "_parent", parent)
        object.__setattr__(self, "_x_axis_data", None)
        object.__setattr__(self, "_y_axes", {})
        object.__setattr__(self, "_y_axis_aggregations", {})
        object.__setattr__(self, "_y_axis_proxy", _YAxisProxy(self))

    @property
    def x_axis(self):
        return self._x_axis_data

    @x_axis.setter
    def x_axis(self, data):
        if not isinstance(data, list):
            raise ValueError(f"X-axis data must be a list, got {type(data)}")
        object.__setattr__(self, "_x_axis_data", data)
        self._flush()

    @property
    def y_axis(self):
        return self._y_axis_proxy

    def _flush(self):
        if self._x_axis_data is None or not self._y_axes:
            return
        y_axis_list = []
        for key, data in self._y_axes.items():
            axis_dict = {"key": key, "data": data}
            if key in self._y_axis_aggregations:
                axis_dict["aggregations"] = self._y_axis_aggregations[key]
            y_axis_list.append(axis_dict)
        multidim_value = {
            "x_axis": {"data": self._x_axis_data},
            "y_axis": y_axis_list,
        }

        from datetime import datetime, timezone

        self._parent._measurements = [
            m for m in self._parent._measurements if m["name"] != self._name
        ]
        self._parent._measurements.append({
            "name": self._name,
            "value": {"MultiDimensional": multidim_value},
            "unit": None,
            "timestamp": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        })


class _YAxisReadProxy:
    def __init__(self, y_axes: list):
        object.__setattr__(self, "_y_axes", y_axes)
        lookup = {}
        for axis in y_axes:
            if isinstance(axis, dict):
                key = axis.get("key")
                if key:
                    lookup[key] = axis.get("data")
        object.__setattr__(self, "_lookup", lookup)

    def __getattr__(self, key: str):
        if key.startswith("_"):
            raise AttributeError(f"'_YAxisReadProxy' object has no attribute '{key}'")
        if key in self._lookup:
            return self._lookup[key]
        raise AttributeError(f"No y-axis '{key}' found")


class _MDMReadProxy:
    def __init__(self, mdm_data: dict):
        object.__setattr__(self, "_data", mdm_data)

    @property
    def x_axis(self):
        x = self._data.get("x_axis", {})
        return x.get("data") if isinstance(x, dict) else x

    @property
    def y_axis(self):
        return _YAxisReadProxy(self._data.get("y_axis", []))


class _AggregationsWriteProxy:
    def __init__(self, measurement_dict: dict):
        object.__setattr__(self, "_measurement", measurement_dict)

    def __setattr__(self, name: str, value):
        if name.startswith("_"):
            object.__setattr__(self, name, value)
            return
        if isinstance(value, (_MeasurementValueProxy, _MeasurementReadProxy)):
            value = object.__getattribute__(value, "_value")
        measurement = object.__getattribute__(self, "_measurement")
        if "aggregations" not in measurement:
            measurement["aggregations"] = []
        for agg in measurement["aggregations"]:
            if agg["type"] == name:
                agg["value"] = value
                return
        measurement["aggregations"].append({"type": name, "value": value})

    def __getattr__(self, name: str):
        if name.startswith("_"):
            raise AttributeError(f"'_AggregationsWriteProxy' object has no attribute '{name}'")
        measurement = object.__getattribute__(self, "_measurement")
        if "aggregations" in measurement:
            for agg in measurement["aggregations"]:
                if agg["type"] == name:
                    return agg.get("value")
        raise AttributeError(f"No aggregation '{name}' found")

    def __repr__(self):
        measurement = object.__getattribute__(self, "_measurement")
        types = [a["type"] for a in measurement.get("aggregations", [])]
        return f"<_AggregationsWriteProxy({types})>"


class _MeasurementValueProxy:
    """Wraps a measurement value + dict reference. Behaves as the raw value but exposes .aggregations."""

    def __init__(self, value, measurement_dict: dict):
        object.__setattr__(self, "_value", value)
        object.__setattr__(self, "_measurement", measurement_dict)

    @property
    def aggregations(self):
        return _AggregationsWriteProxy(object.__getattribute__(self, "_measurement"))

    def _unwrap(self, other):
        if isinstance(other, (_MeasurementValueProxy, _MeasurementReadProxy)):
            return object.__getattribute__(other, "_value")
        return other

    def __repr__(self):
        return repr(self._value)

    def __str__(self):
        return str(self._value)

    def __bool__(self):
        return bool(self._value)

    def __hash__(self):
        return hash(self._value)

    def __float__(self):
        return float(self._value)

    def __int__(self):
        return int(self._value)

    def __index__(self):
        return self._value.__index__()

    def __eq__(self, other):
        return self._value == self._unwrap(other)

    def __ne__(self, other):
        return self._value != self._unwrap(other)

    def __lt__(self, other):
        return self._value < self._unwrap(other)

    def __le__(self, other):
        return self._value <= self._unwrap(other)

    def __gt__(self, other):
        return self._value > self._unwrap(other)

    def __ge__(self, other):
        return self._value >= self._unwrap(other)

    def __add__(self, other):
        return self._value + self._unwrap(other)

    def __radd__(self, other):
        return other + self._value

    def __sub__(self, other):
        return self._value - self._unwrap(other)

    def __rsub__(self, other):
        return other - self._value

    def __mul__(self, other):
        return self._value * self._unwrap(other)

    def __rmul__(self, other):
        return other * self._value

    def __truediv__(self, other):
        return self._value / self._unwrap(other)

    def __rtruediv__(self, other):
        return other / self._value

    def __neg__(self):
        return -self._value

    def __abs__(self):
        return abs(self._value)

    def __round__(self, ndigits=None):
        return round(self._value, ndigits)

    def __len__(self):
        return len(self._value)

    def __iter__(self):
        return iter(self._value)

    def __getitem__(self, key):
        return self._value[key]

    def __contains__(self, item):
        return item in self._value


class Measurements:
    def __init__(self, event_queue=None, job_id=None):
        object.__setattr__(self, "_measurements", [])
        object.__setattr__(self, "_mdm_builders", {})
        # Same motivation as Logs/LogCapturingStream: emit each recorded
        # measurement onto the orchestrator's live event stream so agents
        # see partial progress without waiting for phase end.
        object.__setattr__(self, "_event_queue", event_queue)
        object.__setattr__(self, "_job_id", job_id)

    def __setattr__(self, name: str, value):
        if name.startswith("_"):
            object.__setattr__(self, name, value)
            return

        if isinstance(value, (_MeasurementValueProxy, _MeasurementReadProxy)):
            value = object.__getattribute__(value, "_value")

        if name in self._mdm_builders:
            del self._mdm_builders[name]

        from datetime import datetime, timezone

        if isinstance(value, MultiDim):
            measurement_value = {"MultiDimensional": value.to_dict()}
        elif isinstance(value, bool):
            measurement_value = {"Boolean": value}
        elif isinstance(value, (int, float)):
            measurement_value = {"Numeric": float(value)}
        elif isinstance(value, str):
            measurement_value = {"String": value}
        elif isinstance(value, list):
            measurement_value = {"Array": value}
        elif isinstance(value, dict):
            measurement_value = {"Object": value}
        elif value is None:
            measurement_value = "Null"
        else:
            measurement_value = {"Numeric": float(value)}

        measurement = {
            "name": name,
            "value": measurement_value,
            "unit": None,
            "timestamp": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        }

        self._measurements.append(measurement)

        if self._event_queue is not None:
            try:
                self._event_queue.put({
                    "type": "MeasurementRecorded",
                    "job_id": self._job_id,
                    "name": name,
                    "value": measurement_value,
                    "unit": None,
                    "timestamp": measurement["timestamp"],
                })
            except Exception:
                pass

    def __getattr__(self, name: str):
        if name.startswith("_"):
            raise AttributeError(f"'Measurements' object has no attribute '{name}'")

        if name in self._mdm_builders:
            return self._mdm_builders[name]

        for measurement in reversed(self._measurements):
            if measurement["name"] == name:
                value_wrapper = measurement["value"]
                if isinstance(value_wrapper, dict):
                    if "Numeric" in value_wrapper:
                        return _MeasurementValueProxy(value_wrapper["Numeric"], measurement)
                    elif "String" in value_wrapper:
                        return _MeasurementValueProxy(value_wrapper["String"], measurement)
                    elif "Boolean" in value_wrapper:
                        return _MeasurementValueProxy(value_wrapper["Boolean"], measurement)
                    elif "Array" in value_wrapper:
                        return _MeasurementValueProxy(value_wrapper["Array"], measurement)
                    elif "Object" in value_wrapper:
                        return _MeasurementValueProxy(value_wrapper["Object"], measurement)
                    elif "MultiDimensional" in value_wrapper:
                        return _MDMReadProxy(value_wrapper["MultiDimensional"])
                elif value_wrapper == "Null":
                    return _MeasurementValueProxy(None, measurement)
                return _MeasurementValueProxy(value_wrapper, measurement)

        builder = _MDMBuilder(name, self)
        self._mdm_builders[name] = builder
        return builder

    def to_list(self):
        return self._measurements


class Plug:
    """NDJSON TCP client for communicating with hardware plug services"""

    def __init__(self, name: str, address: str):
        self.name = name
        self.address = address

    def __getattr__(self, method_name: str):
        def method_wrapper(*args, **kwargs):
            if kwargs:
                raise TypeError(
                    f"{method_name}() does not support keyword arguments. Use positional arguments only."
                )
            return self.call(method_name, list(args))

        return method_wrapper

    def call(self, method: str, args: list = None) -> Any:
        """Call a method on the plug service via NDJSON TCP"""
        if args is None:
            args = []

        try:
            request = {
                "type": "CallMethod",
                "method": method,
                "args_json": json.dumps(args) if args else "",
            }

            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.settimeout(60)
            host, port_str = self.address.split(":")
            sock.connect((host, int(port_str)))

            try:
                sock.sendall((json.dumps(request) + "\n").encode("utf-8"))

                response_data = b""
                while True:
                    chunk = sock.recv(65536)
                    if not chunk:
                        break
                    response_data += chunk
                    if b"\n" in response_data:
                        break

                line = response_data.split(b"\n", 1)[0]
                response = json.loads(line.decode("utf-8"))
            finally:
                sock.close()

            if response.get("success"):
                result_json = response.get("result_json")
                return json.loads(result_json) if result_json else None
            else:
                raise Exception(f"Plug call failed: {response.get('error')}")

        except Exception as e:
            raise Exception(f"Failed to call {method} on {self.name}: {e}")


# ============================================================================
# Main worker functions
# ============================================================================

def load_module(module_path: str):
    """Load a Python module from file"""
    spec = importlib.util.spec_from_file_location("phase_module", module_path)
    if spec is None or spec.loader is None:
        raise ImportError(f"Cannot load module from {module_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def execute_job_streaming(command: Dict[str, Any], procedure_dir: Path):
    """Execute a single job with streaming events via generator."""
    job_id = command["job_id"]
    slot_id = command["slot_id"]
    phase_name = command["phase_name"]
    module_name = command.get("module", command.get("file", ""))
    function_name = command["function"]
    plugs = command.get("plugs", {})
    timeout_ms = command.get("timeout_ms", 300000)
    retry_count = command.get("retry_count", 0)
    retry_limit = command.get("retry_limit", 0)
    phase_results = command.get("phase_results", {})

    run = Run(slot_id, job_id, retry_count, retry_limit)
    # Measurements, Logs, UI, Attachments — each wants the live event_queue
    # so partial results reach the orchestrator before phase end (crucial
    # for long-running phases and force-kill diagnostics). Deferred until
    # after event_queue is constructed below.
    unit = Unit()

    unit_info = command.get("unit_info")
    if unit_info:
        unit.serial_number = unit_info.get("serial_number")
        unit.part_number = unit_info.get("part_number")
        unit.revision_number = unit_info.get("revision_number")
        unit.batch_number = unit_info.get("batch_number")
        sub_units = unit_info.get("sub_units", {})
        if sub_units:
            unit.sub_units = SubUnitsDict(sub_units)

    event_queue = EventQueue()
    measurements = Measurements(event_queue=event_queue, job_id=job_id)
    logs = Logs(job_id=job_id, event_queue=event_queue)
    ui = UIContext(job_id, event_queue)
    attachments = Attachments(job_id, event_queue)

    plugs_dict = {}
    for plug_name, plug_address in plugs.items():
        plugs_dict[plug_name] = Plug(plug_name, plug_address)

    original_stdout_stream = sys.stdout
    original_stderr_stream = sys.stderr

    sys.stdout = LogCapturingStream(logs, level="INFO", job_id=job_id, event_queue=event_queue)
    sys.stderr = LogCapturingStream(logs, level="WARNING", job_id=job_id, event_queue=event_queue)

    phase_result_container = {"error": None, "result": None}

    def run_phase():
        try:
            is_file_path = "/" in module_name or "\\" in module_name

            if is_file_path:
                if module_name.startswith("~"):
                    module_file = Path(module_name).expanduser()
                else:
                    module_file = procedure_dir / module_name
                if not module_file.suffix == ".py":
                    module_file = module_file.with_suffix(".py")
            else:
                leading_dots = len(module_name) - len(module_name.lstrip("."))

                if leading_dots > 0:
                    parent_path = "../" * (leading_dots - 1)
                    remaining_path = module_name[leading_dots:].replace(".", "/")
                    module_path = "phases/" + parent_path + remaining_path + ".py"
                else:
                    module_path = module_name.replace(".", "/") + ".py"

                module_file = procedure_dir / module_path

            try:
                module_file = module_file.resolve()
            except:
                pass

            # Two-stage resolution: file-path first (in-tree phases /
            # plugs of the procedure), then standard `importlib` if the
            # file isn't on disk. The importlib path lets
            # `python: shared.foo:Bar` resolve to a workspace-installed
            # wheel (uv-workspace monorepo layout) without requiring
            # relative dot-prefixed module names. Skip the fallback for
            # explicit file-path modules ("/", "\\" in name) and for
            # leading-dot or "~" prefixes — those mean "look in this
            # tree", not "look in site-packages".
            #
            # Threat model note: this DOES technically allow phase YAML
            # to call arbitrary importable modules (e.g.
            # `python: subprocess:Popen`). The procedure is already
            # trusted to ship and run Python code, so the same author
            # could just write `import subprocess; subprocess.Popen(...)`
            # inside a phase file. We're not adding capability — only
            # changing the spelling. Hosts that need stricter isolation
            # should sandbox at the venv / OS layer, not at the YAML.
            if module_file.exists():
                module = load_module(str(module_file))
            elif (
                not is_file_path
                and not module_name.startswith(".")
                and not module_name.startswith("~")
            ):
                import importlib as _importlib
                try:
                    module = _importlib.import_module(module_name)
                except ModuleNotFoundError as exc:
                    raise FileNotFoundError(
                        f"Module {module_name} not found at {module_file} "
                        f"and not importable from sys.path: {exc}"
                    ) from None
            else:
                raise FileNotFoundError(f"Module {module_name} not found at {module_file}")

            if not hasattr(module, function_name):
                raise AttributeError(f"Function {function_name} not found in {module_name}")

            func = getattr(module, function_name)

            sig = inspect.signature(func)
            kwargs = {}
            phase_instance = Phase()

            for param_name in sig.parameters:
                if param_name == "phase":
                    kwargs[param_name] = phase_instance
                elif param_name == "run":
                    kwargs[param_name] = run
                elif param_name == "measurements":
                    kwargs[param_name] = measurements
                elif param_name == "log":
                    kwargs[param_name] = logs
                elif param_name == "ui":
                    kwargs[param_name] = ui
                elif param_name == "unit":
                    kwargs[param_name] = unit
                elif param_name == "attach":
                    kwargs[param_name] = attachments
                elif param_name in phase_results:
                    kwargs[param_name] = PhaseResults(
                        param_name, phase_results[param_name]
                    )
                else:
                    matched_plug = None
                    for plug_key, plug_instance in plugs_dict.items():
                        if param_name.lower() == plug_key.lower():
                            matched_plug = plug_instance
                            break
                    if matched_plug is not None:
                        kwargs[param_name] = matched_plug
                    elif sig.parameters[param_name].default is inspect.Parameter.empty:
                        available = list(phase_results.keys()) if phase_results else []
                        raise TypeError(
                            f"Phase '{function_name}' has required parameter '{param_name}' that could not be resolved.\n"
                            f"Built-in injectables: phase, run, measurements, log, ui, unit, attach.\n"
                            f"If '{param_name}' is a plug, declare it in the phase's plugs config.\n"
                            f"If '{param_name}' refers to another phase's results, make sure that phase completes before this one (add it to depends_on).\n"
                            f"Available phase results: {available}"
                        )

            phase_result: Optional[PhaseResultType] = None
            error_message: Optional[str] = None

            try:
                return_value = func(**kwargs)

                if return_value is None:
                    phase_result = PhaseResult.CONTINUE
                else:
                    raise ValueError(
                        f"Phase '{function_name}' returned unexpected value: {return_value!r}.\n"
                        f"Phase functions should not return values. To pass, just return normally.\n"
                        f"To control flow, call phase.fail(msg), phase.skip(msg), phase.retry(msg), or phase.stop(msg).\n"
                        f"Example:\n"
                        f"  def {function_name}(phase, measurements):\n"
                        f"      voltage = 3.3\n"
                        f"      measurements.voltage = voltage\n"
                        f"      if voltage < 2.5:\n"
                        f"          phase.fail(f'Voltage too low: {{voltage}} V')"
                    )
            except _PhaseResultException as e:
                phase_result = e.result_value
                error_message = e.message if e.message else None

                if error_message:
                    logs.info(error_message)

            unit_info = None
            if (
                unit.serial_number
                or unit.batch_number
                or unit.part_number
                or unit.revision_number
                or unit.sub_units
            ):
                unit_info = {
                    "serial_number": unit.serial_number,
                    "batch_number": unit.batch_number,
                    "part_number": unit.part_number,
                    "revision_number": unit.revision_number,
                    "sub_units": dict(unit.sub_units) if unit.sub_units else None,
                    "status": "tested",
                }

            result_dict = {
                "success": True,
                "phase_result": phase_result,
                "measurements": measurements.to_list(),
                "logs": logs.entries,
                "unit": unit_info,
            }

            phase_result_container["result"] = result_dict

        except SystemExit as e:
            exit_code = e.code if isinstance(e.code, int) else 1
            error_msg = f"Phase called sys.exit({exit_code})"
            print(error_msg, file=sys.stderr)

            if "logs" in locals():
                logs.error(error_msg)

            phase_result_container["result"] = {
                "success": False,
                "error": error_msg,
                "exit_code": exit_code,
                "phase_result": PhaseResult.FAIL,
                "measurements": (
                    measurements.to_list() if "measurements" in locals() else []
                ),
                "logs": logs.entries if "logs" in locals() else [],
            }

        except Exception as e:
            error_msg = traceback.format_exc()

            if "logs" in locals():
                logs.error(error_msg)

            phase_result_container["error"] = error_msg
            phase_result_container["result"] = {
                "success": False,
                "error": error_msg,
                "phase_result": PhaseResult.FAIL,
                "measurements": (
                    measurements.to_list() if "measurements" in locals() else []
                ),
                "logs": logs.entries if "logs" in locals() else [],
            }

    try:
        thread = threading.Thread(target=run_phase, daemon=False)
        thread.start()

        timeout_seconds = timeout_ms / 1000.0 if timeout_ms else None

        start_time = time.time()
        while thread.is_alive():
            if timeout_seconds and (time.time() - start_time) > timeout_seconds:
                event_queue.close()
                yield {
                    "type": "result",
                    "data": {
                        "success": False,
                        "error": f"Phase execution timed out after {timeout_ms}ms",
                        "phase_result": PhaseResult.FAIL,
                        "measurements": measurements.to_list(),
                        "logs": logs.entries,
                    }
                }
                return

            event = event_queue.get(timeout=0.1)
            if event is not None:
                yield {"type": "event", "data": event}

        thread.join(timeout=1.0)

        while not event_queue.empty():
            event = event_queue.get(timeout=0.1)
            if event is not None:
                yield {"type": "event", "data": event}

        if phase_result_container["error"]:
            raise Exception(phase_result_container["error"])

        if phase_result_container["result"]:
            yield {"type": "result", "data": phase_result_container["result"]}
        else:
            yield {
                "type": "result",
                "data": {
                    "success": False,
                    "error": "Phase execution failed to produce result",
                    "phase_result": PhaseResult.FAIL,
                    "measurements": measurements.to_list(),
                    "logs": logs.entries,
                }
            }

    finally:
        event_queue.close()
        sys.stdout = original_stdout_stream
        sys.stderr = original_stderr_stream


# ============================================================================
# NDJSON TCP Server Implementation
# ============================================================================

def _measurement_to_ndjson(m: dict) -> dict:
    """Convert internal measurement dict to NDJSON-compatible dict"""
    result = {
        "name": m["name"],
        "value": m.get("value"),
        "timestamp": m.get("timestamp", ""),
    }
    if "unit" in m and m["unit"]:
        result["unit"] = m["unit"]
    if "result" in m and m["result"]:
        result["result"] = m["result"]
    if "aggregations" in m and m["aggregations"]:
        serialized_aggs = []
        for agg in m["aggregations"]:
            serialized_agg = {"type": agg["type"]}
            if "value" in agg and agg["value"] is not None:
                serialized_agg["value"] = agg["value"]
            serialized_aggs.append(serialized_agg)
        result["aggregations"] = serialized_aggs
    return result


def _log_to_ndjson(log: dict) -> dict:
    """Convert internal log dict to NDJSON-compatible dict"""
    result = {
        "timestamp": log.get("timestamp", ""),
        "level": log.get("level", "INFO"),
        "message": log.get("message", ""),
    }
    if "file" in log and log["file"]:
        result["file"] = log["file"]
    if "line" in log and log["line"]:
        result["line"] = log["line"]
    return result


def handle_connection(conn: socket.socket, procedure_dir: Path):
    """Handle a single TCP connection: read command, stream events, write result."""
    try:
        # Read command (one JSON line)
        buf = b""
        while b"\n" not in buf:
            chunk = conn.recv(65536)
            if not chunk:
                return
            buf += chunk

        line, _ = buf.split(b"\n", 1)
        command = json.loads(line.decode("utf-8"))

        # Parse phase_results: values are JSON strings that need deserialization
        phase_results_raw = command.get("phase_results", {})
        phase_results = {}
        for k, v in phase_results_raw.items():
            try:
                phase_results[k] = json.loads(v) if isinstance(v, str) else v
            except (json.JSONDecodeError, TypeError):
                phase_results[k] = v
        command["phase_results"] = phase_results

        try:
            final_result = None

            for item in execute_job_streaming(command, procedure_dir):
                if item["type"] == "event":
                    event = item["data"]
                    if event["type"] == "attach_file":
                        msg = {
                            "type": "AttachFile",
                            "job_id": event["job_id"],
                            "source_path": event["source_path"],
                            "attachment_name": event["attachment_name"],
                        }
                        conn.sendall((json.dumps(msg) + "\n").encode("utf-8"))
                    elif event["type"] == "attach_data":
                        msg = {
                            "type": "AttachData",
                            "job_id": event["job_id"],
                            "data": event["data"],
                            "attachment_name": event["attachment_name"],
                        }
                        conn.sendall((json.dumps(msg) + "\n").encode("utf-8"))
                    elif event["type"] == "ui_update":
                        msg = {
                            "type": "UiUpdate",
                            "job_id": event["job_id"],
                            "action": event["action"],
                            "data_json": json.dumps(event["data"]),
                        }
                        conn.sendall((json.dumps(msg) + "\n").encode("utf-8"))
                    elif event["type"] == "PhaseLogLine":
                        msg = {
                            "type": "PhaseLogLine",
                            "job_id": event["job_id"],
                            "level": event["level"],
                            "message": event["message"],
                            "timestamp": event["timestamp"],
                        }
                        if event.get("file"):
                            msg["file"] = event["file"]
                        if event.get("line"):
                            msg["line"] = event["line"]
                        conn.sendall((json.dumps(msg) + "\n").encode("utf-8"))
                    elif event["type"] == "MeasurementRecorded":
                        msg = {
                            "type": "MeasurementRecorded",
                            "job_id": event["job_id"],
                            "name": event["name"],
                            "value_json": json.dumps(event["value"]),
                            "timestamp": event["timestamp"],
                        }
                        if event.get("unit"):
                            msg["unit"] = event["unit"]
                        conn.sendall((json.dumps(msg) + "\n").encode("utf-8"))
                elif item["type"] == "result":
                    final_result = item["data"]

            if final_result:
                job_result = {
                    "type": "JobComplete",
                    "success": final_result["success"],
                    "measurements": [
                        _measurement_to_ndjson(m)
                        for m in final_result.get("measurements", [])
                    ],
                    "logs": [
                        _log_to_ndjson(log)
                        for log in final_result.get("logs", [])
                    ],
                    "error": final_result.get("error_message") or final_result.get("error"),
                    "exit_code": final_result.get("exit_code"),
                }

                if "phase_result" in final_result and final_result["phase_result"] is not None:
                    job_result["phase_result_json"] = json.dumps(final_result["phase_result"])

                if "unit" in final_result and final_result["unit"] is not None:
                    job_result["unit_json"] = json.dumps(final_result["unit"])

                conn.sendall((json.dumps(job_result) + "\n").encode("utf-8"))

        except Exception as e:
            error_msg = f"Worker error: {str(e)}"
            print(f"ERROR: {error_msg}", file=_original_stderr)
            msg = {"type": "Error", "message": error_msg}
            conn.sendall((json.dumps(msg) + "\n").encode("utf-8"))

    except Exception as e:
        print(f"Connection handler error: {e}", file=_original_stderr)
    finally:
        conn.close()


def _start_parent_watchdog():
    """Exit when the spawning CLI process dies.

    The worker is a TCP server with no stdin pipe to the parent, so a
    SIGKILLed parent (crash, force-quit, watchdog) otherwise leaves the
    worker running forever. Unix: poll getppid() — the kernel reparents
    orphans to pid 1 (or a subreaper), so a change means the parent is
    gone. Windows: hold a SYNCHRONIZE handle to the parent and wait on
    it; the wait returns when the process exits.
    """
    parent_pid = os.getppid()

    if os.name == "nt":
        def wait_windows():
            import ctypes
            SYNCHRONIZE = 0x00100000
            handle = ctypes.windll.kernel32.OpenProcess(SYNCHRONIZE, False, parent_pid)
            if not handle:
                return
            ctypes.windll.kernel32.WaitForSingleObject(handle, 0xFFFFFFFF)
            os._exit(0)

        threading.Thread(target=wait_windows, daemon=True).start()
        return

    def poll_unix():
        while True:
            if os.getppid() != parent_pid:
                os._exit(0)
            time.sleep(2)

    threading.Thread(target=poll_unix, daemon=True).start()


def serve(procedure_dir: Path):
    """Start NDJSON TCP server and print port to stdout"""
    original_stdout = sys.stdout

    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 0))
    port = server.getsockname()[1]
    server.listen(5)

    # Print port for Rust to discover
    print(f"NDJSON_PORT:{port}", flush=True, file=original_stdout)
    print(f"NDJSON worker listening on port {port}", file=_original_stderr)

    try:
        while True:
            conn, addr = server.accept()
            # Handle each connection in a thread
            t = threading.Thread(
                target=handle_connection,
                args=(conn, procedure_dir),
                daemon=True,
            )
            t.start()
    except KeyboardInterrupt:
        print("Worker interrupted", file=_original_stderr)
    finally:
        server.close()


def main():
    if len(sys.argv) != 2:
        print("Usage: tp_worker.py <procedure_dir>", file=sys.stderr)
        sys.exit(1)

    procedure_dir = Path(sys.argv[1]).resolve()
    if not procedure_dir.exists():
        print(f"Error: Procedure directory not found: {procedure_dir}", file=sys.stderr)
        sys.exit(1)

    # Put the procedure root on sys.path so phase modules can import
    # sibling packages (utils/, drivers/, helpers/) by name. Without
    # this, `spec_from_file_location` loads `phases/foo.py` in isolation
    # and any `from utils.bar import …` raises ModuleNotFoundError.
    procedure_dir_str = str(procedure_dir)
    if procedure_dir_str not in sys.path:
        sys.path.insert(0, procedure_dir_str)

    _start_parent_watchdog()
    serve(procedure_dir)


if __name__ == "__main__":
    main()
