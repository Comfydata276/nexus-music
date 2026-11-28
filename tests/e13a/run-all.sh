#!/usr/bin/env bash
# Every mode, twice, for one build (and, for "new", the component's own workers held and its
# disk blocked as well).   tests/e13a/run-all.sh old|new
set -u
cd "$(dirname "$0")/../.."
build="$1"
modes="absent refused blackhole found stalled saturated ended half"
for repeat in 1 2; do
  for mode in $modes; do
    E13A_REPEAT=$repeat python3 tests/e13a/suite.py run "$build" "$mode" > /dev/null 2>&1 || echo "run $build $mode $repeat: exit $?"
    echo "$build $mode ${hook:-} $repeat: $(grep -E "^[0-9]+ failed" "$HOME/.cache/nexus-music-android/shots/nexus-w1/runs/$build-$mode-$repeat/run.log")"
  done
  if [ "$build" = new ]; then
    for hook in held disk-hang disk-full; do
      for mode in absent stalled; do
        E13A_REPEAT=$repeat python3 tests/e13a/suite.py run "$build" "$mode" "$hook" > /dev/null 2>&1 || echo "run $build $mode $hook $repeat: exit $?"
        echo "$build $mode ${hook:-} $repeat: $(grep -E "^[0-9]+ failed" "$HOME/.cache/nexus-music-android/shots/nexus-w1/runs/$build-$mode-$hook-$repeat/run.log")"
      done
    done
  fi
done
