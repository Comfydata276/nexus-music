#!/usr/bin/env bash
# One pass of the suite. Every mode of the stand-in, the released app ("old") and the app with
# the component ("new") one after the other, so that whatever else this machine is doing
# falls on both alike; then the component's look held (it is no longer behind a switch), the
# scenarios, the phases, and each deliberate fault, which must be caught FOR ITS REASON.
#
#   tests/e13a/run-pass.sh <pass number>        exits 1 if anything is not as it must be
set -u
cd "$(dirname "$0")/../.."
pass="$1"
bad=0
out="$HOME/.cache/nexus-music-android/shots/nexus-w1/run-pass-$pass"
mkdir -p "$out"
must_pass() {   # label, command...
  local label="$1"; shift
  if "$@" > "$out/$label.txt" 2>&1; then echo "$label pass $pass: ok"; else echo "$label pass $pass: FAILED"; bad=1; fi
}
must_fail() {   # label, the reason it must fail for (a pattern), command...
  local label="$1" reason="$2"; shift 2
  if "$@" > "$out/$label.txt" 2>&1; then
    echo "$label pass $pass: NOT CAUGHT"; bad=1
  elif grep -Eq "$reason" "$out/$label.txt"; then
    echo "$label pass $pass: caught, for its reason"
  else
    echo "$label pass $pass: failed, but NOT for its reason ($reason)"; bad=1
  fi
}
export E13A_REPEAT=$pass
for mode in absent refused blackhole found stalled saturated ended half; do
  must_pass "old-$mode" python3 tests/e13a/suite.py run old "$mode"
  must_pass "new-$mode" python3 tests/e13a/suite.py run new "$mode"
done
must_pass "new-absent-held" python3 tests/e13a/suite.py run new absent held
must_pass "new-stalled-held" python3 tests/e13a/suite.py run new stalled held
must_pass "offline-scenario" python3 tests/e13a/suite.py offline new
must_pass "sign-out-scenario" python3 tests/e13a/suite.py sign-out new
must_pass "late-answer-scenario" python3 tests/e13a/suite.py late-answer new
must_pass "phases" python3 tests/e13a/phases.py new
# Slice W7: what sign-in left, a refused music login, the session ended and the music
# login's number risen while offline.
must_pass "refused-scenario" python3 tests/e13a/suite.py refused found
must_pass "refused-backend-down-scenario" python3 tests/e13a/suite.py refused absent
must_pass "ended-offline-scenario" python3 tests/e13a/suite.py ended-offline
must_pass "count-offline-scenario" python3 tests/e13a/suite.py count-offline
must_fail "CONTROL-window-thread-held" "FAILED compare: .*over " python3 tests/e13a/suite.py run new absent sabotage
must_fail "CONTROL-offline-ignored" "looks dispatched after Offline mode was on" env E13A_SABOTAGE=1 python3 tests/e13a/suite.py offline new
must_fail "CONTROL-sign-out-ignored" "looks dispatched for the account that signed out" env E13A_SABOTAGE=1 python3 tests/e13a/suite.py sign-out new
must_fail "CONTROL-late-answer-shown" "shown under B" env E13A_SABOTAGE=1 python3 tests/e13a/suite.py late-answer new
exit $bad
