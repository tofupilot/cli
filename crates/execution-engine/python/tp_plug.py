#!/usr/bin/env python3
"""
Per-Plug Service - NDJSON TCP implementation.
No external dependencies beyond the Python standard library.
"""

import importlib
import importlib.util
import sys
import traceback
import time
import gc
import json
import threading
import select
import socket
from typing import Dict, Any
from pathlib import Path

# Save original stderr before any redirection
_original_stderr = sys.__stderr__


# ============================================================================
# Logging (inlined from tp_logs.py)
# ============================================================================

class Logs:
    def __init__(self, job_id=None):
        self.entries = []
        self.job_id = job_id

    def _add_log(self, level: str, message: str):
        from datetime import datetime, timezone
        import inspect
        import os

        frame = inspect.currentframe()
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
                    base_dir = os.path.dirname(
                        os.path.dirname(os.path.abspath(__file__))
                    )
                    if file_path.startswith(base_dir):
                        file_path = os.path.relpath(file_path, base_dir)
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

    def info(self, message: str):
        self._add_log("INFO", message)

    def warning(self, message: str):
        self._add_log("WARNING", message)

    def error(self, message: str):
        self._add_log("ERROR", message)

    def debug(self, message: str):
        self._add_log("DEBUG", message)


class LogCapturingStream:
    def __init__(self, logs_obj, level="INFO", job_id=None, emit_to_stderr=False):
        self.logs = logs_obj
        self.level = level
        self.job_id = job_id
        self.buffer = ""
        self.emit_to_stderr = emit_to_stderr
        # Handler threads and a running method may print concurrently.
        self._lock = threading.Lock()

    def write(self, text):
        with self._lock:
            self._write_locked(text)

    def _write_locked(self, text):
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

                if self.emit_to_stderr and _original_stderr:
                    try:
                        _original_stderr.write(json.dumps(entry) + "\n")
                        _original_stderr.flush()
                    except Exception:
                        pass

    def flush(self):
        pass


# ============================================================================
# Plug Service
# ============================================================================

class PlugHandler:
    """Handles plug lifecycle and method calls"""

    def __init__(
        self,
        procedure_dir: Path,
        plug_key: str,
        display_name: str,
        plug_config: Dict[str, Any],
    ):
        self.procedure_dir = procedure_dir
        self.plug_key = plug_key
        self.display_name = display_name
        self.plug_config = plug_config
        self.plug_instance = None
        self._initialized = False
        self._init_error = None
        self._init_start_time = time.time()
        # One method call at a time on the instance, like a SCPI socket.
        # Control requests (GetStatus, Cleanup, Shutdown) do NOT take
        # this lock, so a long or abandoned method never blocks the
        # engine's health probe or its shutdown sequence.
        self._call_lock = threading.Lock()
        # Set once the service must stop accepting connections: after a
        # Shutdown, or after a Cleanup on an initialized plug.
        self.shutdown_event = threading.Event()
        # serve() waits for an in-flight Cleanup before exiting so
        # interpreter teardown cannot cut __del__ short.
        self.cleanup_started = threading.Event()
        self.cleanup_done = threading.Event()

        # Initialize logging
        self.logs = Logs()

        # Redirect stdout/stderr (emit_to_stderr=True sends JSON to Rust)
        sys.stdout = LogCapturingStream(self.logs, level="INFO", emit_to_stderr=True)
        sys.stderr = LogCapturingStream(self.logs, level="WARNING", emit_to_stderr=True)

        # Add procedure directory to path
        if str(procedure_dir) not in sys.path:
            sys.path.insert(0, str(procedure_dir))

        # Start background initialization
        self._init_thread = threading.Thread(target=self._background_init, daemon=True)
        self._init_thread.start()

    def _background_init(self):
        try:
            self._create_plug_instance()
            self._initialized = True
        except Exception as e:
            self._init_error = str(e)
            error_msg = f"Failed to initialize plug {self.display_name}:\n{traceback.format_exc()}"
            self.logs.error(error_msg)

    def _create_plug_instance(self):
        try:
            file_path = self.plug_config.get("file")
            class_name = self.plug_config.get("class")

            if not file_path:
                raise ValueError("Plug config must contain 'file' field")
            if not class_name:
                raise ValueError("Plug config must contain 'class' field")

            file_path = Path(file_path)
            if not file_path.exists():
                raise FileNotFoundError(f"Plug file not found: {file_path}")

            module_name = f"_plug_{file_path.stem}_{id(self)}"

            spec = importlib.util.spec_from_file_location(module_name, file_path)
            if spec is None or spec.loader is None:
                raise ImportError(f"Cannot load module from {file_path}")

            module = importlib.util.module_from_spec(spec)
            sys.modules[module_name] = module
            spec.loader.exec_module(module)

            plug_class = getattr(module, class_name)
            config = self.plug_config.get("config") or {}
            self.plug_instance = plug_class(**config)

        except Exception as e:
            error_msg = (
                f"Failed to create plug {self.display_name}:\n{traceback.format_exc()}"
            )
            self.logs.error(error_msg)
            raise e

    def _get_plug_state(self) -> Dict[str, str]:
        if not self.plug_instance:
            return {}

        state = {}
        for attr_name in dir(self.plug_instance):
            if attr_name.startswith("_"):
                continue
            try:
                value = getattr(self.plug_instance, attr_name)
                if not callable(value) and isinstance(
                    value, (str, int, float, bool, type(None))
                ):
                    state[attr_name] = str(value)
            except Exception:
                pass

        return state

    def handle_request(
        self, request: Dict[str, Any], conn: "socket.socket | None" = None
    ) -> Dict[str, Any]:
        """Handle a single NDJSON request and return a response dict."""
        req_type = request.get("type", "")

        if req_type == "CallMethod":
            return self._handle_call_method(request, conn)
        elif req_type == "GetStatus":
            return self._handle_get_status()
        elif req_type == "Cleanup":
            return self._handle_cleanup()
        elif req_type == "Shutdown":
            return self._handle_shutdown()
        else:
            return {"success": False, "error": f"Unknown request type: {req_type}"}

    @staticmethod
    def _caller_gone(conn) -> bool:
        """True when the worker already closed its end (phase deadline hit,
        worker killed). A queued request whose caller is gone must not run:
        the phase already failed, and firing an instrument write minutes
        later is worse than dropping it."""
        if conn is None:
            return False
        try:
            readable, _, _ = select.select([conn], [], [], 0)
            if not readable:
                return False
            return conn.recv(1, socket.MSG_PEEK) == b""
        except OSError:
            return True

    def _handle_call_method(
        self, request: Dict[str, Any], conn=None
    ) -> Dict[str, Any]:
        try:
            # Wait for background initialization
            if self._init_thread and self._init_thread.is_alive():
                self._init_thread.join(timeout=30)
                if self._init_thread.is_alive():
                    return {"success": False, "error": "Plug initialization timeout after 30s"}

            if self._init_error:
                return {"success": False, "error": f"Plug initialization failed: {self._init_error}"}

            if not self.plug_instance:
                return {"success": False, "error": "Plug not initialized"}

            method_name = request.get("method", "")
            if not hasattr(self.plug_instance, method_name):
                return {
                    "success": False,
                    "error": f"Method {method_name} not found on {self.display_name}",
                }

            args_json = request.get("args_json", "")
            args = json.loads(args_json) if args_json else []
            kwargs_json = request.get("kwargs_json", "")
            kwargs = json.loads(kwargs_json) if kwargs_json else {}

            abandoned = {
                "success": False,
                "error": f"{method_name} skipped: caller closed the connection before it ran",
            }
            # Checked before waiting for the lock and again after getting
            # it: the wait can be long when many slots share the plug.
            if self._caller_gone(conn):
                return abandoned
            with self._call_lock:
                if self._caller_gone(conn):
                    return abandoned
                # Bound the method only now: a Cleanup that won the lock
                # first has dropped the instance, and a reference taken
                # earlier would keep it alive past its __del__.
                instance = self.plug_instance
                if instance is None:
                    return {
                        "success": False,
                        "error": f"{method_name} skipped: plug {self.display_name} was cleaned up",
                    }
                result = getattr(instance, method_name)(*args, **kwargs)
                state = self._get_plug_state()

            result_json = json.dumps(result) if result is not None else None

            return {
                "success": True,
                "result_json": result_json,
                "state": state,
            }

        except Exception as e:
            error_msg = traceback.format_exc()
            self.logs.error(
                f"Failed to call {self.display_name}.{request.get('method', '?')}:\n{error_msg}"
            )
            return {"success": False, "error": error_msg}

    def _handle_get_status(self) -> Dict[str, Any]:
        if self._init_thread and self._init_thread.is_alive():
            init_duration = time.time() - self._init_start_time
            if init_duration > 60:
                return {
                    "success": False,
                    "error": f"Plug initialization timeout after {init_duration:.1f}s",
                }
            return {
                "success": False,
                "error": f"Plug initializing... ({init_duration:.1f}s elapsed)",
            }

        if self._init_error:
            return {"success": False, "error": f"Plug initialization failed: {self._init_error}"}

        if not self.plug_instance:
            return {"success": False, "error": "Plug not initialized"}

        # Health probe must answer even while a method runs; the state
        # snapshot is best effort and skipped when the instance is busy.
        state: Dict[str, str] = {}
        if self._call_lock.acquire(blocking=False):
            try:
                state = self._get_plug_state()
            finally:
                self._call_lock.release()
        return {"success": True, "state": state}

    def _handle_cleanup(self) -> Dict[str, Any]:
        self.cleanup_started.set()
        try:
            return self._cleanup()
        finally:
            self.cleanup_done.set()

    def _cleanup(self) -> Dict[str, Any]:
        cleanup_start = time.time()

        self.logs.info(f"Plug '{self.display_name}' starting cleanup")

        if self.plug_instance:
            # Rust allows 5 s for Cleanup. Wait up to 3 s for an in-flight
            # method so dropping the reference really runs __del__; past
            # that, the running method keeps its bound reference and
            # __del__ is deferred until it returns or the process exits.
            acquired = self._call_lock.acquire(timeout=3.0)
            try:
                self.logs.info(f"  -> Deleting plug instance (will call __del__)")
                del self.plug_instance
                self.plug_instance = None
                gc.collect()
            finally:
                if acquired:
                    self._call_lock.release()
            if acquired:
                self.logs.info(f"  -> Garbage collection completed (__del__ has finished)")
            else:
                # sys.stderr is the WARNING-level capture stream Rust reads.
                print(
                    f"Plug '{self.display_name}': a method is still running; "
                    "__del__ deferred until it returns",
                    file=sys.stderr,
                    flush=True,
                )

        cleanup_duration = time.time() - cleanup_start
        self.logs.info(
            f"Plug '{self.display_name}' cleanup complete ({cleanup_duration:.2f}s)"
        )

        if self._initialized:
            self.shutdown_event.set()

        return {
            "success": True,
            "message": f"Plug {self.display_name} cleaned up",
            "cleanup_duration_seconds": cleanup_duration,
        }

    def _handle_shutdown(self) -> Dict[str, Any]:
        self.logs.info(f"Plug '{self.display_name}' shutting down")
        self.shutdown_event.set()
        return {"success": True, "message": "Service shutting down"}


def handle_connection(conn: socket.socket, handler: PlugHandler):
    """Handle a single TCP connection: read request, write response."""
    try:
        # A client that connects and never sends must not pin a thread.
        conn.settimeout(30.0)
        buf = b""
        while b"\n" not in buf:
            chunk = conn.recv(65536)
            if not chunk:
                return
            buf += chunk
        conn.settimeout(None)

        line, _ = buf.split(b"\n", 1)
        request = json.loads(line.decode("utf-8"))

        response = handler.handle_request(request, conn)
        conn.sendall((json.dumps(response) + "\n").encode("utf-8"))

    except Exception as e:
        _original_stderr.write(f"Connection handler error: {e}\n")
        _original_stderr.flush()
        try:
            error_resp = {"success": False, "error": str(e)}
            conn.sendall((json.dumps(error_resp) + "\n").encode("utf-8"))
        except Exception:
            pass
    finally:
        conn.close()


def serve(
    procedure_dir: Path, plug_key: str, display_name: str, plug_config: Dict[str, Any]
):
    """Start NDJSON TCP plug service"""
    original_stdout = sys.stdout

    handler = PlugHandler(procedure_dir, plug_key, display_name, plug_config)

    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 0))
    port = server.getsockname()[1]
    # Every worker opens one connection per call. With N slots in flight
    # the connects arrive in a burst; with the old `listen(5)` the overflow
    # was reset (macOS, Windows: ECONNRESET / broken pipe) or stalled on
    # SYN retransmits (Linux). The backlog is a safety margin only:
    # connections are accepted immediately into a thread and queue on the
    # handler's call lock, not in the kernel.
    server.listen(min(1024, socket.SOMAXCONN))

    # Print port for Rust to discover
    print(f"NDJSON_PORT:{port}", flush=True, file=original_stdout)

    print(
        f"NDJSON plug service '{display_name}' listening on port {port}",
        file=_original_stderr,
        flush=True,
    )

    shutdown_event = handler.shutdown_event

    try:
        while not shutdown_event.is_set():
            server.settimeout(1.0)
            try:
                conn, addr = server.accept()
            except socket.timeout:
                continue

            # One thread per connection: accept never waits on a running
            # method. Method calls serialize on handler._call_lock.
            threading.Thread(
                target=handle_connection, args=(conn, handler), daemon=True
            ).start()
    except KeyboardInterrupt:
        print(f"Plug '{display_name}' interrupted", file=_original_stderr)
    finally:
        server.close()
        # Interpreter exit freezes daemon threads. Give a Cleanup that is
        # still running __del__ a bounded chance to finish first; Rust
        # kills the process a few seconds after Shutdown anyway.
        if handler.cleanup_started.is_set():
            handler.cleanup_done.wait(3.0)


if __name__ == "__main__":
    import argparse

    try:
        parser = argparse.ArgumentParser(description="Per-Plug NDJSON TCP Service")
        parser.add_argument(
            "--procedure-dir", required=True, help="Procedure directory"
        )
        parser.add_argument("--plug-name", required=True, help="Plug key")
        parser.add_argument("--display-name", required=True, help="Plug display name")
        parser.add_argument(
            "--plug-config", required=True, help="Plug configuration (JSON)"
        )

        args = parser.parse_args()

        procedure_dir = Path(args.procedure_dir)
        if not procedure_dir.exists():
            print(
                f"Error: Procedure directory not found: {procedure_dir}",
                file=sys.stderr,
                flush=True,
            )
            sys.exit(1)

        try:
            plug_config = json.loads(args.plug_config)
        except json.JSONDecodeError as e:
            print(f"Error: Invalid plug config JSON: {e}", file=sys.stderr, flush=True)
            sys.exit(1)

        serve(procedure_dir, args.plug_name, args.display_name, plug_config)
    except Exception as e:
        print(f"FATAL ERROR in tp_plug.py: {e}", file=sys.stderr, flush=True)
        import traceback

        traceback.print_exc(file=sys.stderr)
        sys.exit(1)
