#!/usr/bin/env bash
set -u
CLI=/Users/julienbuteau/sources/tofupilot/.claude/worktrees/cli-json-agent-protocol/apps/cli/target/debug/tofupilot
DRIVER=/tmp/drive_cli.py

for DIR in /tmp/ohtf_test /tmp/ohtf_test2 /tmp/ohtf_test3 /tmp/ohtf_test4 /tmp/ohtf_test5 /tmp/ohtf_test6 /tmp/ohtf_test7 /tmp/ohtf_test8 /tmp/ohtf_test9 /tmp/ohtf_test10 /tmp/ohtf_test11 /tmp/ohtf_test12 /tmp/ohtf_test13 /tmp/ohtf_test14 /tmp/ohtf_test15 /tmp/ohtf_test16; do
  echo
  echo "==================================="
  echo "Scenario: $DIR"
  echo "==================================="
  python3 "$DRIVER" "$CLI" "$DIR" 2>&1
done
