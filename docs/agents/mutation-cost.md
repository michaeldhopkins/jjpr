# Mutation testing: what each tool costs

**Never put a mutation run between yourself and finishing a change.** There are three tools here and they cost four orders of magnitude apart, so reaching for the wrong one is the whole difference between mutation testing being useful and being a tax:

| tool | cost | what it is for |
|---|---|---|
| `cargo mutants --file <f>` | **~23 min** | A MAP. Where is this file weak? Once per file you are about to work on seriously. |
| Hand-applying one mutant | **~15 s** | VERIFICATION. Does this specific test actually fail without the fix? |
| `--in-diff` in CI | ~2 min, async | The per-change gate. Already wired; nobody waits on it. |

Effectively all the verification value comes from the middle row. Edit the line, run the one test, revert — `sed -i '' '<line>s/+= 1/-= 1/'` then `cargo test --lib <test>`. Doing that a dozen times is what proved every test written for `watch.rs`, and each answer arrived in seconds.

The full runner is a **planning** activity, not a gate. It found nothing by itself: it pointed at 24 misses, and the bugs were found by writing the tests it pointed toward. Re-running all 105 mutants to check a four-line refactor — which happened here, and blocked a push for 23 minutes — is the anti-pattern. The targeted check answered the identical question in two minutes.

**Verify that your mutation actually applied before believing a SURVIVED.** Three separate false readings happened in one session, each of which would have been recorded as a coverage gap: a `sed` aimed one line off silently changed nothing and exited 0; `grep 'x *= 1'` treated `*` as a quantifier and reported an applied mutation as missing; and `grep -F '-= 1'` parsed `-=` as an option (needs `--`). A mutation that was never applied looks exactly like one nothing detects. Assert the edit landed — `grep -Fq -- '<mutated text>'` — before running the test.

The distribution mattered more than the total. 24 of the 52 misses sit inside `run_watch_loop` and another 10 in `run_merge_phase`, and they are the retry counters, their give-up thresholds, and the negated guards — `+=` survives `-=`, `>=` survives `<`, `delete !` survives. So watch's error-handling state machine is unverified, and any refactor of it is unguarded. Full detail and the sequencing that follows from it are in TODO.md; the general lesson is that **a MISSED cluster inside one function is a stronger signal than the file's score**, because it says which change you cannot safely make.

Also worth knowing before running one: a whole-file run is long enough that it will outlive a session. This one was stopped before finishing, but `mutants.out/{caught,missed,unviable,timeout}.txt` are written incrementally, so the partial results were complete enough to act on. Read those files rather than relying on the command's final summary line.
