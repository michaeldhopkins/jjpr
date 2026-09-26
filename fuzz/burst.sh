#!/usr/bin/env bash
# One fuzz burst: mutate a target for BUDGET seconds, then fold what it found into its corpus.
# Run by .github/workflows/fuzz.yml, and the same way locally from the repo root:
#
#   fuzz/burst.sh <libfuzzer-binary> <target> <budget-seconds>
#
#   cargo +nightly fuzz build jj_output
#   fuzz/burst.sh fuzz/target/aarch64-apple-darwin/release/jj_output jj_output 60
#
# Uses fuzz/dict/<target>.dict when it exists. Works in fuzz/corpus/<target> (replaced by the
# minimized union), fuzz/new/<target> (this burst's finds, emptied once merged) and
# fuzz/artifacts/<target>/ (crash-/timeout-/oom-/slow-unit- inputs). Mutates nothing else.
#
# Why the budget is timed here and not with -max_total_time alone: libFuzzer counts that budget
# from process start, and loading the corpus counts. In safe-chains, -max_total_time=2 over a 4k
# corpus spent ~10s loading and stopped at INITED having mutated nothing, and on a runner a 12k
# corpus took ~4.5 min to load. A flat budget against a growing corpus quietly becomes a replay.
# So the clock starts at INITED, and SIGINT ends the run. libFuzzer exits 72 on an interrupt (the
# cargo-fuzz build rejects -interrupted_exit_code), which is mapped to 0 here, and only when this
# script sent the interrupt.
#
# Finds go to a per-target fuzz/new/<target>, not a shared fuzz/new: run locally one target after
# another, a shared directory would merge one target's finds into the next target's corpus.
#
# Exit status: the fuzzer's, if it stopped on its own (a crash, timeout, oom or leak is non-zero);
# 0 after a clean interrupt; 2 if only the merge failed. The merge runs regardless, so the corpus
# keeps what the burst found before a crash. Under Actions, a successful merge sets the step output
# `merged=true`, which is what lets the workflow save the corpus.
set -uo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <libfuzzer-binary> <target> <budget-seconds>" >&2
  exit 64
fi
BIN="$1"; T="$2"; BUDGET="$3"
case "$BUDGET" in
  '' | *[!0-9]*) echo "budget must be whole seconds, got '$BUDGET'" >&2; exit 64 ;;
esac
LOAD_CAP=3600

CORPUS="fuzz/corpus/$T"
NEW="fuzz/new/$T"
ARTIFACTS="fuzz/artifacts/$T/"
mkdir -p "$CORPUS" "$NEW" "$ARTIFACTS"
DICT=()
if [ -s "fuzz/dict/$T.dict" ]; then DICT=(-dict="fuzz/dict/$T.dict"); fi
echo "Restored corpus: $(find "$CORPUS" -type f | wc -l | tr -d ' ') inputs; budget ${BUDGET}s after load"

LOG="$(mktemp)"
MIN="$(mktemp -d)"
trap 'rm -rf "$LOG" "$MIN"' EXIT

# ${DICT[@]+"${DICT[@]}"}: an empty array under `set -u` is an error in bash 3.2 (macOS).
"$BIN" ${DICT[@]+"${DICT[@]}"} \
  -max_total_time=$((BUDGET + LOAD_CAP)) \
  -timeout=25 \
  -rss_limit_mb=4096 \
  -print_final_stats=1 \
  -artifact_prefix="$ARTIFACTS" \
  "$NEW" "$CORPUS" 2>"$LOG" &
PID=$!

START=$SECONDS
while kill -0 "$PID" 2>/dev/null && ! grep -q 'INITED' "$LOG"; do sleep 1; done
echo "Loaded in $((SECONDS - START))s"

END=$((SECONDS + BUDGET))
while kill -0 "$PID" 2>/dev/null && [ "$SECONDS" -lt "$END" ]; do sleep 1; done
INTERRUPTED=0
if kill -INT "$PID" 2>/dev/null; then INTERRUPTED=1; fi
wait "$PID"
RC=$?
if [ "$INTERRUPTED" -eq 1 ] && [ "$RC" -eq 72 ]; then RC=0; fi
grep -E 'INITED|DONE|interrupted|Dictionary|stat::number_of_executed_units|ERROR|SUMMARY|Test unit written' "$LOG" || true
if [ "$RC" -ne 0 ]; then tail -40 "$LOG"; fi
echo "fuzzer exit status: $RC"

echo "Prior corpus + seeds: $(find "$CORPUS" -type f | wc -l | tr -d ' ') inputs"
echo "New this burst: $(find "$NEW" -type f | wc -l | tr -d ' ')"
if ! "$BIN" -merge=1 -timeout=25 -rss_limit_mb=4096 \
  -artifact_prefix="$ARTIFACTS" \
  "$MIN" "$CORPUS" "$NEW" 2>"$LOG"; then
  tail -40 "$LOG"
  echo "::error::merge failed; corpus left as restored and not marked for saving"
  [ "$RC" -ne 0 ] && exit "$RC"
  exit 2
fi
# The committed seed-* inputs stay under their own names: the merge renames what it keeps to a
# content hash and drops what adds no coverage, and a local run would otherwise delete tracked
# files from the working copy.
find "$CORPUS" -maxdepth 1 -type f -name 'seed-*' -exec mv {} "$MIN"/ \;
rm -rf "$CORPUS" "$NEW"
mv "$MIN" "$CORPUS"
chmod 755 "$CORPUS"
echo "Minimized corpus: $(find "$CORPUS" -type f | wc -l | tr -d ' ') inputs"
if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "merged=true" >> "$GITHUB_OUTPUT"; fi

exit "$RC"
