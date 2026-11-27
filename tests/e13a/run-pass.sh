#!/usr/bin/env bash
# One pass of the suite. Every mode of the stand-in, the released app ("old") and the app with
# the component ("new") one after the other, so that whatever else this machine is doing
# falls on both alike; then the component's look held, the component switched off, the two
# scenarios, and each deliberate fault, which must be caught.
#
#   tests/e13a/run-pass.sh <pass number>        exits 1 if anything is not as it must be
set -u
cd "$(dirname "$0")/../.."
pass="$1"
bad=0
runs="$HOME/.cache/nexus-music-android/shots/nexus-w1/runs"
say() { echo "$*"; }
must_pass() {   # label, command...
  local label="$1"; shift
  if "$@" > /dev/null 2>&1; then say "$label pass $pass: ok"; else say "$label pass $pass: FAILED"; bad=1; fi
}
must_fail() {   # a deliberate fault: the check must catch it
  local label="$1"; shift
  if "$@" > /dev/null 2>&1; then say "$label pass $pass: NOT CAUGHT"; bad=1; else say "$label pass $pass: caught, as it must be"; fi
}
export E13A_REPEAT=$pass
for mode in absent refused blackhole found stalled saturated ended half; do
  must_pass "old-$mode" python3 tests/e13a/suite.py run old "$mode"
  must_pass "new-$mode" python3 tests/e13a/suite.py run new "$mode"
done
must_pass "new-absent-held" python3 tests/e13a/suite.py run new absent held
must_pass "new-stalled-held" python3 tests/e13a/suite.py run new stalled held
must_pass "new-absent-switch-off" python3 tests/e13a/suite.py run new absent switch-off
must_pass "offline scenario" python3 tests/e13a/suite.py offline new
must_pass "late-answer scenario" python3 tests/e13a/suite.py late-answer new
must_fail "CONTROL window's thread held 0.3 s a look" python3 tests/e13a/suite.py run new absent sabotage
must_fail "CONTROL offline not honoured" env E13A_SABOTAGE=1 python3 tests/e13a/suite.py offline new
must_fail "CONTROL late answer not dropped" env E13A_SABOTAGE=1 python3 tests/e13a/suite.py late-answer new
must_pass "phases (with their own control)" python3 tests/e13a/phases.py new
exit $bad
