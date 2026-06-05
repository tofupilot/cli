#!/usr/bin/env bash
# Create yaml_test3..12 — framework edge cases & crashes.
set -euo pipefail

PYPROJECT='[project]
name = "procedure"
version = "0.1.0"
requires-python = ">=3.9"
dependencies = []'

mk() {
  local dir=$1
  mkdir -p "$dir/phases"
  echo "$PYPROJECT" > "$dir/pyproject.toml"
  touch "$dir/phases/__init__.py"
  ln -sfn /tmp/ohtf_test/.venv "$dir/.venv"
}

# Y3: failing measurement → FAIL exit 1
mk /tmp/yaml_test3
cat > /tmp/yaml_test3/procedure.yaml <<'YAML'
name: Y3 Failing Measurement
version: 1.0.0
main:
  - key: voltage
    name: Voltage
    python: phases.voltage
    measurements:
      - name: v
        unit: V
        validators:
          - operator: "<="
            expected_value: 3.0
YAML
cat > /tmp/yaml_test3/phases/voltage.py <<'PY'
def voltage(measurements, log):
    measurements.v = 5.0  # out of range
PY

# Y4: phase raises Python exception
mk /tmp/yaml_test4
cat > /tmp/yaml_test4/procedure.yaml <<'YAML'
name: Y4 Phase Exception
version: 1.0.0
main:
  - key: boom
    name: Boom
    python: phases.boom
YAML
cat > /tmp/yaml_test4/phases/boom.py <<'PY'
def boom(log):
    raise RuntimeError("phase exploded")
PY

# Y5: dependency order (phase B depends_on phase A)
mk /tmp/yaml_test5
cat > /tmp/yaml_test5/procedure.yaml <<'YAML'
name: Y5 Dependencies
version: 1.0.0
main:
  - key: a
    name: A
    python: phases.a
  - key: b
    name: B
    python: phases.b
    depends_on: [a]
YAML
cat > /tmp/yaml_test5/phases/a.py <<'PY'
def a(log):
    log.info("A ran")
PY
cat > /tmp/yaml_test5/phases/b.py <<'PY'
def b(log):
    log.info("B ran (after A)")
PY

# Y6: parallel phases (both depend only on initialize, independent of each other)
mk /tmp/yaml_test6
cat > /tmp/yaml_test6/procedure.yaml <<'YAML'
name: Y6 Parallel
version: 1.0.0
execution:
  workers: 4
main:
  - key: init
    name: Init
    python: phases.init
  - key: v
    name: V
    python: phases.v
    depends_on: [init]
  - key: i
    name: I
    python: phases.i
    depends_on: [init]
  - key: t
    name: T
    python: phases.t
    depends_on: [init]
YAML
for p in init v i t; do
  cat > /tmp/yaml_test6/phases/$p.py <<PY
def $p(log):
    log.info("$p ran")
PY
done

# Y7: missing phase module → crash at phase runtime or schema validation
mk /tmp/yaml_test7
cat > /tmp/yaml_test7/procedure.yaml <<'YAML'
name: Y7 Missing Module
version: 1.0.0
main:
  - key: ghost
    name: Ghost
    python: phases.does_not_exist
YAML

# Y8: YAML syntax error
mk /tmp/yaml_test8
cat > /tmp/yaml_test8/procedure.yaml <<'YAML'
name: Y8 Bad YAML
version: 1.0.0
main:
  - key: a
    name: A
    python: phases.a
  - key: b  # missing colon after key
    name B
    python: phases.b
YAML

# Y9: missing required fields (no `main`)
mk /tmp/yaml_test9
cat > /tmp/yaml_test9/procedure.yaml <<'YAML'
name: Y9 No Main
version: 1.0.0
YAML

# Y10: phase's Python file has a syntax error
mk /tmp/yaml_test10
cat > /tmp/yaml_test10/procedure.yaml <<'YAML'
name: Y10 Python SyntaxError
version: 1.0.0
main:
  - key: broken
    name: Broken
    python: phases.broken
YAML
cat > /tmp/yaml_test10/phases/broken.py <<'PY'
def broken(log):
    return 1 +
PY

# Y11: phase's Python has ImportError
mk /tmp/yaml_test11
cat > /tmp/yaml_test11/procedure.yaml <<'YAML'
name: Y11 Python ImportError
version: 1.0.0
main:
  - key: missing
    name: Missing
    python: phases.missing
YAML
cat > /tmp/yaml_test11/phases/missing.py <<'PY'
import this_module_does_not_exist
def missing(log):
    pass
PY

# Y12: unknown depends_on reference
mk /tmp/yaml_test12
cat > /tmp/yaml_test12/procedure.yaml <<'YAML'
name: Y12 Unknown Dependency
version: 1.0.0
main:
  - key: a
    name: A
    python: phases.a
    depends_on: [ghost_phase_does_not_exist]
YAML
cat > /tmp/yaml_test12/phases/a.py <<'PY'
def a(log):
    pass
PY

echo "Done."
