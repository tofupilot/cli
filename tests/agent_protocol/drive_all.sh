#!/usr/bin/env bash
# Drive every OpenHTF scenario end-to-end through the JSON agent protocol.
# Usage: drive_all.sh <cli-binary> [scenario-root]
#   <cli-binary>     path to the compiled `tofupilot` binary
#   [scenario-root]  dir holding the ohtf_test* scenarios (default: /tmp)
# See README.md for how to materialize the scenarios.
set -eu
CLI=${1:?usage: drive_all.sh <cli-binary> [scenario-root]}
SCENARIO_ROOT=${2:-/tmp}
DRIVER="$(dirname "$0")/drive_cli.py"

for DIR in "$SCENARIO_ROOT"/ohtf_test "$SCENARIO_ROOT"/ohtf_test2 "$SCENARIO_ROOT"/ohtf_test3 "$SCENARIO_ROOT"/ohtf_test4 "$SCENARIO_ROOT"/ohtf_test5 "$SCENARIO_ROOT"/ohtf_test6 "$SCENARIO_ROOT"/ohtf_test7 "$SCENARIO_ROOT"/ohtf_test8 "$SCENARIO_ROOT"/ohtf_test9 "$SCENARIO_ROOT"/ohtf_test10 "$SCENARIO_ROOT"/ohtf_test11 "$SCENARIO_ROOT"/ohtf_test12 "$SCENARIO_ROOT"/ohtf_test13 "$SCENARIO_ROOT"/ohtf_test14 "$SCENARIO_ROOT"/ohtf_test15 "$SCENARIO_ROOT"/ohtf_test16; do
  echo
  echo "==================================="
  echo "Scenario: $DIR"
  echo "==================================="
  python3 "$DRIVER" "$CLI" "$DIR" 2>&1
done
