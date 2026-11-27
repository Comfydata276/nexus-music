#!/usr/bin/env bash
# One pass of the suite: every mode, and for each mode every build named, one after the other
# (so that whatever else this machine is doing at the time falls on both alike); then, for
# "new", its own workers held and its disk blocked.
#
#   tests/e13a/run-pass.sh <pass number> old [new]
set -u
cd "$(dirname "$0")/../.."
pass="$1"; shift
summary() { echo "$1 pass $pass: $(grep -E '^[0-9]+ failed' "$HOME/.cache/nexus-music-android/shots/nexus-w1/runs/$1-$pass/run.log" 2>/dev/null)"; }
for mode in absent refused blackhole found stalled saturated ended half; do
  for build in "$@"; do
    E13A_REPEAT=$pass python3 tests/e13a/suite.py run "$build" "$mode" > /dev/null 2>&1
    summary "$build-$mode"
  done
done
for build in "$@"; do
  [ "$build" = new ] || continue
  for hook in held disk-hang disk-full; do
    for mode in absent stalled; do
      E13A_REPEAT=$pass python3 tests/e13a/suite.py run new "$mode" "$hook" > /dev/null 2>&1
      summary "new-$mode-$hook"
    done
  done
done
