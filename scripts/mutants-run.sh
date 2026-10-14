#!/bin/bash
#
# Run `cargo mutants` for mutants.yml under a wall-clock budget, and decide the job's verdict from
# mutants.out/missed.txt rather than from cargo-mutants' exit status.
#
# Usage: scripts/mutants-run.sh [cargo-mutants args...]
#
#   MUTANTS_BUDGET         seconds cargo-mutants may run before it is stopped (default 900); the
#                          rest of the 20-minute job (checkout, toolchain, cache restore and save,
#                          summary, artifact) needs about five minutes
#   MUTANTS_BUDGET_PASSES  1 when running out of budget is an acceptable result (a rotating slice,
#                          whose remainder comes round again); default 0, where it exits 124 so a
#                          partial changed-code run is never read as a pass
#   MUTANTS_GRACE          seconds between TERM and KILL when stopping at the budget (default 30)
#   MUTANTS_STATUS         file that receives "complete" or "budget" (default mutants-run-status.txt)
#
# Exit 2: missed.txt is non-empty, always. Exit 0: nothing missed (a TIMEOUT counts as caught).
# Exit 124 (or 0 with MUTANTS_BUDGET_PASSES=1): stopped at the budget. Anything else is
# cargo-mutants' own code for a harness fault (4 baseline failed, 5 and 6 bad diff), or 1 when it
# exits 2 yet reports no missed mutant.
#
# Why: a mutant that hangs (a `+=` made `-=` never advances its loop) runs until the test timeout,
# and a slice holding several of them outran the 20-minute job limit, which CANCELS the job and
# loses the run, MISSED mutants included. Exit 3 used to be mapped to 0 outright, which also
# passed a run that had missed mutants.

set -uo pipefail

budget="${MUTANTS_BUDGET:-900}"
if [[ ! $budget =~ ^[0-9]+$ ]]; then
  echo "::error::MUTANTS_BUDGET must be a whole number of seconds, got '$budget'"
  exit 70
fi
status_file="${MUTANTS_STATUS:-mutants-run-status.txt}"
hit_budget="$(mktemp -u)"

cargo mutants "$@" &
miner=$!
(
  deadline=$((SECONDS + budget))
  while [ "$SECONDS" -lt "$deadline" ]; do sleep 1; done
  : > "$hit_budget"
  kill -TERM "$miner" 2>/dev/null
  for ((i = 0; i < ${MUTANTS_GRACE:-30}; i++)); do
    kill -0 "$miner" 2>/dev/null || exit 0
    sleep 1
  done
  kill -KILL "$miner" 2>/dev/null
) &
watcher=$!

wait "$miner"
rc=$?
kill "$watcher" 2>/dev/null
wait "$watcher" 2>/dev/null

stopped=0
if [ -e "$hit_budget" ]; then
  rm -f "$hit_budget"
  case "$rc" in
    0 | 1 | 137 | 143) stopped=1 ;;
  esac
fi

if [ "$stopped" -eq 1 ]; then
  echo "budget" > "$status_file"
  echo "::warning::cargo-mutants stopped after ${budget}s; the rest of this run went untested. Outcomes so far are in mutants.out."
else
  echo "complete" > "$status_file"
fi

if [ -s mutants.out/missed.txt ]; then
  echo "::error::mutants MISSED; see mutants.out/missed.txt"
  exit 2
fi

if [ "$stopped" -eq 1 ]; then
  [ "${MUTANTS_BUDGET_PASSES:-0}" = 1 ] && exit 0
  exit 124
fi

case "$rc" in
  0) exit 0 ;;
  3)
    echo "::notice::timeouts counted as caught; see mutants.out/timeout.txt"
    exit 0
    ;;
  2)
    echo "::error::cargo-mutants exited 2 but missed.txt is empty or absent"
    exit 1
    ;;
  *) exit "$rc" ;;
esac
