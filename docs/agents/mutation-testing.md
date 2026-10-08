# Mutation testing (project specifics)

General method is in the **`rust-mutation-testing` skill**. This section is only what is true of jjpr, and most of it exists because a first attempt was measured and found wrong.

**What runs:** `.github/workflows/mutants.yml`, never gating and never on a schedule.

- **PRs and pushes to main:** mutants overlapping the diff (`--in-diff`). This is where most findings come from: 0.40.2's diff had five survivors in new code, found in 13 minutes and all killed by `test(jj): kill the mutants 0.40.2's diff left alive`.
- **Pushes to main only:** one rotating slice of the whole tree, `--shard k/48` with `k = run_number % 48`, `timeout-minutes: 20`, `mutants.out` uploaded as `mutants-slice`. Every 48 runs the tree has been looked at once. **N = 48:** 1331 mutants on 2026-09-27 at the ~20s/mutant CI delivers gives N ≈ 44 for 10 minutes of mutants; 48 leaves room for the job's own baseline build and test run. The first slice (run 36320880236) took **8.3 minutes** for 28 mutants, so 48 holds. Revisit it when the denominator has drifted well away from 1331.
- **A MISSED mutant from a slice is old code, not the push's.** It still gets a test, or an exclusion with its reason, like any other miss; record slice findings below.

**Slice findings:** recorded in `docs/agents/mutation-slices.md`.

There is no whole-tree sweep and none is planned; the slice replaces it. A local sweep would take ~30 hours at the rate measured below. Partial runs so far:

| Date | Scope | Result |
|---|---|---|
| 2026-08-02 | whole tree, stopped | 163 of 1246 mutants in 3h22m (~88s/mutant), all in `main.rs`: 77 caught |
| 2026-08-02 | `src/forge/remote.rs` | 39 caught / 2 missed / 15 unviable, 95%; both misses killed |
| 2026-08-03 | `src/watch.rs` | 102 of 105: 38%, then 61% after five tests; remaining misses in TODO.md |

So the backlog in untouched code is unknown until the slices have gone round once, and a miss in a touched-but-old function is not necessarily new.

**Measured 2026-08-02**, so nobody re-derives it:

- **1246 mutants** across the tree (`cargo mutants --list | wc -l`), concentrated in `main.rs` 190, `watch.rs` 105, `merge/execute.rs` 99, `submit/plan.rs` 88, `forge/github.rs` 84.
- **~88s/mutant**, i.e. a full run is hours. Baseline is `12s build + 17s test`, and the cost is **build/link-bound, not test-bound** — cutting the per-mutant suite from 17s to 5s moved a 56-mutant file from 241s to 213s, a 12% gain. Restricting the test command is not the lever here.
- `forge/remote.rs`: 39 caught / 2 missed / 15 unviable → **95%**. The two misses were both `||`→`&&` in an emptiness guard; one test killed both.

**Two things a first pass got wrong, recorded so the next one doesn't:**

- **`src/main.rs` is NOT untestable and must not be excluded.** A partial run showed "76 of 76 missed mutants in main.rs" and the obvious inference was that its command handlers are e2e-only. Wrong: **all 163 tested mutants were in main.rs**, 77 of them *caught* — cargo-mutants walks files in order and the run simply never reached anything else.
- **Do not restrict to `--lib`/`--bins`.** `tests/cli.rs` drives the real binary via `assert_cmd` and is exactly what catches the `cmd_submit -> Ok(())` class. Dropping it converts caught into missed and reads as a test-quality problem.

Consequently there is **no `.cargo/mutants.toml`** — nothing measured justifies a setting, and a config built on the misreading above would have been worse than none.

**Reading a MISSED mutant.** Three legitimate responses, in order: write the test that kills it; judge it *equivalent* (cannot change observable behaviour) and say why; or exclude the code with a comment. Never delete the code to make it go away.

**Before any of those, check whether the code is reachable only from e2e-gated tests — a structural MISSED that no test-writing will fix.** Counted 2026-08-02: 41 `#[test]` functions gate on `jj_available()`, and **10 of them** (in `tests/e2e.rs`, `tests/tty_watch.rs`, `tests/parity.rs`) *also* sit behind `JJPR_E2E`, which no CI job sets and which a normal `cargo test` does not set either. Their coverage is therefore invisible to every mutation run anyone will realistically do, so a mutant covered only by them reports MISSED no matter how good those tests are. The other 31 are gated on jj alone and DO count — which is why installing jj matters and why `jj_available()` now panics in CI rather than skipping. When triaging, separate "untested" from "tested only where mutation cannot see it"; conflating them is how a green suite gets rewritten to chase a phantom gap.

Do not turn that into the reflex the `main.rs` entry above warns about, though. "It is only covered by e2e" is the same shape of excuse as "its handlers are e2e-only", and that one was wrong — the run had simply not reached the other files. Earn it: name the specific e2e test that covers the line, confirm it is behind `JJPR_E2E`, and confirm the run actually reached the file. Only then is a MISSED structural rather than real.

Beware a second finding riding along: writing the test for the `||` guard surfaced `parse_gitlab_path` keeping a leading slash on an empty namespace component. That is a real issue but not the one the mutant proved — it was recorded rather than fixed mid-triage, and fixing it later on its own terms is what showed the filed symptom was the rare one (see TODO.md, "empty path components"). Record, then measure, then fix.

The `--in-diff` traps and measured runs are in `docs/agents/mutation-runs.md`; what each tool costs and how to verify a mutant, in `docs/agents/mutation-cost.md`.
