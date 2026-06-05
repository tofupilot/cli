# Agent Protocol Tests

Test suite for the `tofupilot run --json` agent protocol.

`ci_smoke.py` is the CI gate (`.github/workflows/test-cli.yml` → `e2e-smoke`):
it drives the in-repo `demo-operator-ui` procedure end-to-end, answers every
`ui_request`, and asserts the run reaches `run_finished`. It exits non-zero on
failure.

The remaining scripts below are a deeper local regression harness; they hard-
code `/tmp/` paths and require manual venv setup, so they are **not** in CI.

## Layout

- `audit_protocol.py`  — strict event-ordering invariants across OpenHTF scenarios
- `yaml_audit.py`      — same for the YAML framework (plus UI components, plugs)
- `stress_test.py`     — edge cases: wrong types, unicode, large payloads, rapid fire, timeouts
- `test_protocol.py`   — error-path probes (malformed stdin, unknown request_id, duplicate response)
- `drive_cli.py`       — reference driver that answers UI prompts with valid values
- `drive_all.sh`       — run every OpenHTF scenario end-to-end via `drive_cli.py`
- `yaml_scaffold.sh`   — recreate the `/tmp/yaml_test*` scenario dirs from scratch
- `scenarios/`         — procedure fixtures for each scenario (YAML + Python + `pyproject.toml`)

## Running

The scripts hard-code paths under `/tmp/` (that's where the scenarios were
originally created). To run from this checkout, copy `scenarios/*` to
`/tmp/` and set up a venv with `openhtf` installed, then point each
scenario's `.venv` symlink at it:

Run these from the crate root (the directory holding `Cargo.toml`):

```bash
# 1. Copy fixtures into /tmp
cp -r tests/agent_protocol/scenarios/* /tmp/

# 2. Create one shared venv with OpenHTF (used by every scenario)
python3 -m venv /tmp/ohtf_test/.venv
/tmp/ohtf_test/.venv/bin/pip install openhtf

# 3. Symlink every scenario to that venv
for d in /tmp/ohtf_test* /tmp/yaml_test*; do
  [ -d "$d" ] && ln -sfn /tmp/ohtf_test/.venv "$d/.venv"
done

# 4. Build the CLI
cargo build

# 5. Run the suites (CLI binary path, then a procedure dir where needed)
python3 tests/agent_protocol/audit_protocol.py  ./target/debug/tofupilot
python3 tests/agent_protocol/yaml_audit.py      ./target/debug/tofupilot
python3 tests/agent_protocol/stress_test.py     ./target/debug/tofupilot <procedure-dir>
python3 tests/agent_protocol/test_protocol.py   ./target/debug/tofupilot <procedure-dir>
```

`<procedure-dir>` is any OpenHTF or YAML procedure directory (for example one of
the copied scenarios under `/tmp`).

## Coverage (last green run)

- **YAML framework** — 29 scenarios (`yaml_audit.py`):
  happy single phase, measurements, failing measurements, phase exception,
  dependency order, parallel workers, missing phase module, YAML syntax error,
  empty `main`, python syntax/import error inside phase, unknown `depends_on`,
  all 11 UI component types in one phase, display-only auto-continue,
  parallel UI prompts, image_choice/image_checklist, attachments, UI bound to
  measurement, text_input with pattern/length, slider + number boundaries,
  full journey, required UI answered, required UI timeout, unit metadata,
  pre-bake UI, simple plug, plug `__init__` raises, plug state persists,
  plug missing module.

- **OpenHTF framework** — 22 scenarios (`audit_protocol.py`):
  happy measurements, failing measurements, text prompt, confirm prompt,
  image prompt, multi-prompt, exception, repeat_limit, PhaseResult.REPEAT,
  attachments, docstrings, PhaseGroups (setup/main/teardown), phase timeout,
  log levels, SyntaxError, ImportError, boot exception, mid-phase `sys.exit`,
  segfault, YAML operator-ui procedure (parallel workers).

- **Protocol stress** — 19 cases (`stress_test.py`):
  happy-path per component type, wrong-type per component, missing required,
  unknown field, malformed JSON, unknown request_id, duplicate response,
  large textarea, unicode, empty string, multiselect multi-value,
  multiselect empty array, numeric boundaries, numeric-as-string coercion,
  switch-as-string, full pre-bake, partial pre-bake + agent, UI timeout fires,
  rapid junk stdin followed by real responses.
