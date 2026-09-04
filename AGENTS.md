# jjpr

## Project overview

Rust CLI tool (`jjpr`) for managing stacked pull requests in Jujutsu (jj) repositories. Shells out to `jj` for version control; talks directly to forge APIs via `ureq` (sync HTTP client).

## Architecture

- `src/jj/` — Jj trait + JjRunner (shells out to jj binary), template strings, type definitions
- `src/forge/` — Forge trait + backends (GitHub, GitLab, Forgejo) using `ForgeClient` (ureq HTTP wrapper), token resolution, remote URL parsing, PR comment generation
- `src/graph/` — Change graph construction from bookmarks, traversal toward trunk
- `src/submit/` — Analyze target stack, resolve multi-bookmark segments, plan submission, execute (push/PR/comments)
- `src/auth.rs` — Auth test/help commands
- `notes/forges/` — internal research on forges: feature deep-dives (e.g. GitHub native stacks) and candidate-forge evaluations. Not user docs; those are `docs/src/forges.md`.

## Key conventions

- Traits (`Jj`, `Forge`) for all external I/O — enables testing with stubs
- Test stubs use `Mutex<Vec<String>>` for recording calls (traits require Send + Sync)
- Co-located `#[cfg(test)] mod tests` in every module
- jj templates produce line-delimited JSON; `escape_json()` includes surrounding quotes
- Edition 2024 with let-chains for collapsible if-let patterns
- Requires jj 0.36+ (bookmark auto-tracking on push)

## Testing

```
cargo test               # Unit + jj integration (fast, ~2s)
cargo clippy --locked --tests -- -D warnings  # Must be clean (CI's exact flags)
JJPR_E2E=1 cargo test  # E2E against real GitHub (slow, requires gh auth)
```

E2E tests use `michaeldhopkins/forge-e2e-sandbox` (private repo, shared across forges/projects — see the `forge-e2e-testing` skill). Each run creates uniquely-prefixed bookmarks and cleans up PRs/branches on Drop.

### Fuzzing (project specifics)

General method — the two budgets, the target-kind catalog, the seeding ladder, coverage, CI shape, the gotchas — is in the **`rust-fuzzing` skill**. How to *run* it is in `fuzz/README.md`. This section is only what is true of jjpr.

**What makes jjpr fuzzable is that it parses other programs' output.** jjpr never reads a repository directly; it reads the stdout of `jj` and the JSON of a forge API. Both are trust boundaries jjpr does not control: a jj upgrade can change a template's output, and GitHub's stacked-PR API is an unversioned public preview. That is the shape fuzzing is for, and it is why the targets cluster on parse boundaries rather than on jjpr's own logic.

The six targets and what each asserts are tabulated in `fuzz/README.md`. What matters when adding to them:

- **`jj_output` is the trust boundary, and it discards the parse result** — availability only. A *wrong* parse is out of scope for it by construction; the structural claims live in `graph_invariants`, which consumes the same parsers and then asserts on what was built. Those two compose deliberately: fuzz one input, exercise subprocess bytes → parse → traversal → `ChangeGraph`.
- **`forge_payload` prunes NESTED keys, not just top-level ones.** This is what found the `lenient_ref` bug: `#[serde(default)]` defends only the level it is written at, so `"base": {}` — an object present but missing its required `ref` — failed the *entire* stack payload. The three hand-written partial-payload tests had only ever dropped whole keys. When adding a preview-grade field, the question is not "is it defaulted" but "what happens when it is present and malformed".
- **The lenient/strict split is deliberate.** `Stack` / `StackPr` / `PrStackRef` degrade to `None`; `PullRequest::base`/`head` stay strict. A PR with no base is a broken response from a *stable* API and should fail loudly rather than silently retarget at `""`. Don't "fix" that asymmetry.
- **`remote_url` fuzzes the CREDENTIAL, not the URL.** The URL shape is fixed and known-good and the assertion is exact equality against the expected owner/repo/host, so a failure means the userinfo-stripping broke rather than the URL being malformed. Note the trap already hit here once: asserting "the token does not appear in the output" reads as the stronger check but is unsound — a one-character token is a substring of `codeberg.org`.
- **`comment_roundtrip` is structure-aware (`arbitrary`), not byte-mutated**, because its input is a *stack*, not a string. It also pins an ordering: `parse_comment_data` returns the FIRST `JJPR_DATA` line, and the entries rendered below it contain attacker-influenced text (a bookmark name can come from a remote). Today the data line is written above them. Moving it to a footer would turn a bookmark name into a way to substitute the whole stack's metadata — the roundtrip assertion is what would catch that.

**Vocabulary and seeds.** Dictionaries in `fuzz/dict/` are derived from the template constants in `src/jj/templates.rs` (the regeneration command is in `fuzz/README.md`) — jjpr has no registry to generate from the way safe-chains does, so **these are the one hand-maintained artifact and they rot if a template changes without them**. Two schema details that seeds get wrong if written from memory: `RawBookmark` has no `hasRemote`/`isSynced` (they are derived, not parsed), and `isWorkingCopy` / `conflict` / `empty` are emitted by the template as quoted **strings** (`"true"`), not JSON booleans. Deriving a dictionary and seeds from `templates.rs` automatically, instead of by hand, is the obvious next step.

**Do not read a green run as "no bugs" until the targets have been mutation-tested.** The first 15-minute run over all six was clean at ~21.4M executions — and three of the targets were not actually asserting what their comments claimed. `comment_roundtrip` missed the ordering regression it existed for (a fuzzer will not synthesise valid base64 of valid JSON by chance, so the forgery is now *constructed* by the target); `remote_url` `continue`d past a `None` detection, staying green when credential-stripping was deleted; and `graph_invariants`' cycle assertion was sound but unreachable, because every seed was a single bookmark while the code path needs a multi-segment graph. Adding one two-bookmark seed immediately produced **two real bugs** — `fix(jj)` and `fix(graph)` in this stack — and then a divergent-at-different-depths seed showed `fix(graph)`'s first version was itself incomplete: it rejected a self-edge (the shape in the reproducer) when the invariant was acyclicity, so `A -> B -> A` still closed a loop. Both were then re-found by the per-push replay seeds. The rule that came out of it: mutation-test the target *and* the seeds, and treat any `continue` in a target as a place it stops asserting.

**What gets committed to `fuzz/corpus/`, and what does not (decided 2026-08-03).** Only **grammar seeds** — hand-written inputs that teach the fuzzer the shape of a valid `jj` template line. Those are what a COLD corpus starts from, and the difference they make is reachability, not speed: without them libFuzzer is discovering that the input is line-delimited JSON at all, and for `comment_roundtrip` it will never synthesise valid base64 of valid JSON by chance. The measured evidence is in this file already — the baseline corpora below all grew *from* a handful of seeds, and adding one two-bookmark seed immediately produced two real bugs.

**A crash reproducer is NOT a grammar seed and does not get committed.** The nightly's first real find (below) was tempting to keep as a seed, but the regression guard for a fixed bug is a unit test: deterministic, instant, legible, and it runs in every `cargo test`. A committed crash input only re-finds a bug that is already fixed and already covered, and the corpus accumulates nightly anyway, so the 40 minutes it once took to find is not worth carrying in git. Write the unit test; delete the artifact.

**The 10 GB Actions cache is not a constraint here — measured 2026-08-03.** Total repo cache was **1.01 GB**, of which **all six fuzz corpora together were ~2.6 MB** (largest: `graph_invariants` at 547 KB). The ~1 GB is `Swatinem/rust-cache` compilation artifacts. Two GitHub rules matter and neither bites: eviction is **LRU** (oldest entries drop until under the limit, it is not a wipe), and any cache **untouched for 7 days is deleted**. Corpus keys embed the run id with prefix `restore-keys`, so each night writes a fresh entry, the newest is always restored, and stale ones age out — self-pruning. External storage (R2 and friends) would be solving a problem that does not exist.

The real exposure is **continuity, not size**: if the nightly stops running for more than 7 days, every corpus cache is deleted and the next run starts cold. That is exactly when grammar seeds earn their ~30 KB.

**The nightly budget is 15 minutes per shard, cut from 4h on 2026-09-04, and its job is now mostly to keep that cache alive.** Two consecutive nights each lost one shard to the runner dying underneath it — "runner has received a shutdown signal" 2h19m into `jj_output` shard 1, then "lost communication with the server" on `config_load` shard 1 — on different targets and shards, with zero crash artifacts and nothing on GitHub's status page. That is background attrition of hosted runners, and a 4h job is mostly a long exposure to it; nothing inside a job survives its runner dying, so there was no fix to write. Neither death lost anything, because the merge job runs on a failed shard and kept the surviving shard's finds. But 48 runner-hours a night from a free pool was also more than this repo should take, and the corpora were at saturation by 2026-08-01, so the marginal hours were buying little. Fifteen minutes keeps the merge and coverage machinery, keeps the cache touched nightly independent of push cadence, and sits far below the point where the runner deaths happened. The replay gate shares the cache read-only (the key has no commit hash; PR branches read entries written on main; a restore counts as an access), which is why the two workflows could stay decoupled. The retire-the-nightly-and-commit-the-corpus alternative was built and then abandoned in favour of this: it reversed the seeds-only policy above for a problem a smaller budget solves outright. When reading a red Fuzz badge, check the job annotation before triaging it as a crash.

**Baseline (2026-08-01, 6 targets × 15 min in parallel, local).** ~21.4M executions. Corpus after, from a handful of committed seeds: `graph_invariants` 2430, `jj_output` 2301, `forge_payload` 1897, `config_load` 1310, `comment_roundtrip` 573, `remote_url` 222. Treat those as the saturation baseline — a nightly that stops growing a target means change tactics (seed, dictionary, new target) rather than add hours.

`remote_url` is the outlier at 222 and plateaued early, which is expected rather than wrong: `is_shape_preserving` rejects most mutations by design, so the mutator has little room. It is a replay-grade target already; if it ever needs more, the lever is seeding real remote shapes, not runtime.

The one artifact produced was a `slow-unit` in `comment_roundtrip` — a **false positive**. It measured ~0.03 ms per execution when re-timed over 2000 runs on an idle machine, four orders of magnitude under libFuzzer's threshold; it was recorded only because 18 sanitizer-instrumented processes were competing for CPU. Hence the CI check fails on `crash-*`/`timeout-*`/`oom-*` and merely uploads `slow-unit-*`. When the perf signal is what you actually want, run targets one at a time.

**First real crash from the nightly (2026-08-03).** `graph_invariants` failed on both shards after ~40 minutes, producing 29 distinct inputs that all reached one panic: `traversal.rs` truncated a merge parent's commit id for display with `cid[..cid.len().min(12)]`. `.min(12)` guards a SHORT id but not a MULTI-BYTE one — if byte 12 lands inside a character, Rust panics, so a display truncation became a crash. Fixed by truncating with `chars().take(12)`.

Three things worth carrying forward. The identical bug existed a second time in `merge/execute.rs` on a change id, unreachable by this target and found only by grepping for the pattern — **when a fuzzer finds a byte-slice bug, grep the tree for the same shape before closing it**. `forge/http.rs` had already solved it correctly in `truncate_body`, so the codebase knew the hazard in one place and missed it in two. And the crash is not reachable from a healthy jj, which emits hex ids — that is the point of fuzzing a parse boundary rather than the logic behind it: jjpr does not control what it is parsing, and the right answer to strange input is an error, not a panic.

**Not yet fuzzed, and why.** Revset *construction* (`segment_range` and friends) is where a recent real bug lived, but jj rejects bookmark names containing revset metacharacters (`a|b`, `a&b`, `x(y)` are all refused — verified), so the injection surface is much smaller than it looks; it would need a target that shells out to jj, which no target does today. `src/merge/` and `src/submit/` planning logic is invariant-shaped and would suit a target, but needs stub seams first.

### Mutation testing (project specifics)

General method is in the **`rust-mutation-testing` skill**. This section is only what is true of jjpr, and most of it exists because a first attempt was measured and found wrong.

**What runs:** `.github/workflows/mutants.yml` on PRs and pushes to main, restricted to mutants overlapping the diff (`--in-diff`). Nothing runs a full tree in CI yet.

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

**Three traps the `--in-diff` job hit, all measured 2026-08-02:**

- **`diff.mnemonicPrefix` silently disables it.** Set in this author's global git config, it makes `git diff` emit `i/`/`w/` instead of `a/`/`b/`; cargo-mutants matches nothing, logs `No mutants to filter` and exits 0 — a gate reporting green while testing nothing. The workflow now forces the prefixes and fails if they are absent. Note it works fine on a CI runner's clean config, so this breaks locally and not in CI, which is the harder direction to notice.
- **A one-line edit selects the enclosing function's mutants**, not just that line's. Touching one comparison in `parse_owner_repo` selected the `||`→`&&` mutant *and* both `replace parse_owner_repo -> ...` mutants anchored at the signature. So touching an untested function surfaces its whole pre-existing gap as a "new" finding — honest scope, but decide deliberately whether that blocks a PR.
- **Don't reproduce the job locally with `git diff main..`** — this repo is jj-colocated, and jj parks git `HEAD` at `@-`. So `main..HEAD` silently omits the working-copy commit, i.e. exactly the change you are trying to test. It produces a real, correctly-prefixed diff of the *wrong* range, and cargo-mutants then reports `No mutants to filter` — indistinguishable from the `mnemonicPrefix` failure above, and it cost a second wrong conclusion after that one was fixed. Use `jj diff --from main --to @ --git` locally. CI is unaffected: it checks out a real git commit, so `HEAD` is where it looks.

**First real run on a source change** (the `fix(forge)` empty-path-components commit, 2026-08-02): 12 mutants selected, **10 caught, 2 unviable, 0 missed, 57s** — the whole job, not per mutant. Two things worth carrying forward. The unviable pair were both `replace ... -> Some(Default::default())` on functions returning `Option<RepoInfo>`, and `RepoInfo` has no `Default` — unviable is the tool failing to compile its own mutant, not a gap in the tests. And 57s for 12 mutants is nowhere near the ~88s/mutant full-tree figure above: a small diff amortises one baseline build across all of them, so **do not extrapolate `--in-diff` timings to a full run, or the reverse.**

**A sweep breaks `--in-diff` outright, and the next push proved it (2026-08-03).** The 0.38.0 reformat touched 45 files, and because rustfmt moves lines *inside* functions, selection went from 12 mutants to **236** — 20x, from a commit that changed no behaviour at all. The job ran 138 of them and was cancelled by the 45-minute timeout, so the gate silently contributed nothing to that release. `--in-diff` assumes a focused diff; a reformat or mass refactor violates the assumption rather than merely straining it. The job now counts the selection first and skips above 100 with a warning, because the alternatives are worse: a longer timeout makes a slow gate slower without bounding it, and `--shard` sampling reports a fraction of the work as a pass that nobody can distinguish from a full run.

Two related facts measured on that same cancelled run. **A cancelled job does NOT lose its `if: always()` steps** — Summarise and Archive both ran and succeeded, so a timed-out run still reports partial results; the cost of a timeout is the wasted 45 minutes, not a lost report. And **~20s/mutant** is the figure CI actually delivers with a warm cache, which is where the limit of 100 comes from (~33 minutes) rather than from the 88s/mutant local measurement.

**First whole-file run, `src/watch.rs`, 2026-08-03** (102 of 105 completed): **32 caught, 52 missed, 6 timeout, 12 unviable** — a **38% catch rate** against the 95% measured on `forge/remote.rs`. That spread is the point: a single tree-wide score would have averaged these into a number that describes neither file. Mutation testing is worth running per-file on the code you are about to change, not once for a headline figure.

**Re-measured the same day after five tests: 57 caught, 37 missed, 1 timeout, 12 unviable — 61%.** The useful number is not the delta in the score, it is that **caught rose by 25 while only 12 mutants were targeted directly**. Tests that drive a long function end-to-end cover guards and arithmetic nobody aimed at, so on badly-tested code the first tests that reach the function at all pay roughly double. Timeouts also fell 6 -> 1: those were mutants that made the loop spin forever, and fixing the give-up logic terminated them without a test being written for it.

Read the distribution, not the total. Misses inside `run_watch_loop` fell 24 -> 9, and none of the nine are in the retry/exit control flow — so the thing that made decomposition unsafe is gone even though the file's score is still far from 95%. The remaining misses cluster in reporting functions (`report_orphaned_prs`, `report_reconcile_failure`, `print_initial_watch_status`), where a `==` -> `!=` changes which message prints rather than what jjpr does. Chasing those buys a better number without buying much safety; judging them as equivalent is often the honest call.

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

**Not done yet:** a completed full-tree run. At ~88s/mutant that needs sharding across machines, and its missed list — not an estimate — is what should decide whether a nightly gate is worth adding.

## After every code change

Three things, every time — not at the end of a branch, not before pushing:

1. **`cargo fmt`** — not `--check`, the real thing. CI runs `cargo fmt --check` and fails on any difference, so an unformatted tree is a red build, and a *batch* of unformatted commits is a red build plus a reformatting diff tangled into unrelated work.
2. **`cargo clippy --locked --tests -- -D warnings`** — the exact CI invocation. `-D warnings` is the part that matters: plenty of lints are `warn` locally and therefore invisible, and `too_many_lines` in particular fires on things that look harmless (see below).
3. **`cargo install --path .`** — reinstall the local binary so the `jjpr` on `PATH` matches the source you just changed.

The install is non-optional because jjpr is a tool you actually run: it is a local install with no outward side effect, and skipping it leaves you testing a stale binary. Install on every change; push only when asked.

**Run fmt and clippy per change, not per branch.** Both gates are cheap and both are strict in CI, so the only thing deferring them buys is discovering a wall of failures at push time. Two concrete ways this has already gone wrong here:

- Formatting went unchecked until 0.38.0 and drifted a whole style edition behind. The correction touched **45 files**, and an incidental `cargo fmt` during unrelated work got snapshotted into whatever commit happened to be `@`, briefly turning a one-file bug fix into a 7000-line diff. Under jj the working copy is snapshotted on every command, so an unformatted tree is not inert — it is waiting to attach itself to your next commit.
- That same reformat pushed `run_watch_loop` from under clippy's line limit to 284/275 **without adding a statement or a branch**, failing a `-D warnings` build for a pure layout change. Formatting and linting are coupled here; checking one without the other is how you find out at push time.

`cargo fmt --check` runs in both `ci.yml` and `release.yml`. It is deliberately the first step in each — it needs no build and no jj, so it fails in seconds rather than after the suite.

## Commit style

Every commit message must use a conventional-commit prefix so `git cliff` produces real release notes (`cliff.toml` has `filter_unconventional = true` — unprefixed commits silently disappear from the changelog).

- `feat:` → Features (minor bump candidate).
- `fix:` → Bug Fixes (patch).
- `docs:` → Documentation.
- `refactor:` → Refactor.
- `test:` → Testing.
- `perf:` → Performance.
- `chore:` / `ci:` / `build:` → Miscellaneous.
- `!` suffix marks a breaking change: `feat!:`, `fix!:`. Forces a minor bump in 0.x.

Subject ≤ 70 chars. Body explains *why* and lists any breaking migration steps.

## Before pushing

Every push must pass these steps. CI runs `cargo fmt --check`, `cargo check --locked`, `cargo test`, `cargo clippy --locked --tests -- -D warnings`, and `cargo deny` — a stale lockfile, a formatting difference, or a single clippy warning fails the build. `release.yml` duplicates all of them as the publish gate, so a gate added to one must be added to the other.

None of this should be news by the time you get here: fmt and clippy belong to *every code change* (see above), and this list is the final check, not the first time you run them.

0. **`cargo fmt`** — if this produces a diff, you skipped a step earlier. Commit it with the change it belongs to rather than as a trailing "fix formatting" commit.
1. **Bump the version** in `Cargo.toml` when adding features or making behavioral changes (semver: patch for fixes, minor for new features/behavioral changes).
   - "Behavioral change" includes becoming *more* permissive. 0.37.0 went out as a minor, not a patch, because accepting previously-rejected remote URLs turned some working single-remote repos into ambiguous ones. "Everything it changes was already broken" is a claim worth testing before it justifies a patch.
2. **Update Cargo.lock** — run `cargo check` after any `Cargo.toml` change so the lockfile stays in sync. CI uses `--locked` and will reject a stale lockfile.
   - **`fuzz/Cargo.lock` carries the version too, and nothing in the normal flow touches it.** It only moves when someone builds a fuzz target, so a version bump leaves it behind silently — it sat at 0.36.0 through the entire 0.36.1 release. Nothing fails, because the fuzz jobs do not pass `--locked`, which is exactly why it goes unnoticed. Refresh it with `cargo +nightly fuzz build <target>` (any target) when bumping.
3. **`cargo test`** — all tests must pass.
4. **`cargo clippy --locked --tests -- -D warnings`** — exact CI flags. `-D warnings` promotes warnings to errors, which catches things plain `cargo clippy --tests` doesn't (e.g., `too_many_lines` is `warn` locally but fails CI). Must be clean.
5. **Review and regenerate the docs.** Any change to commands, flags, output, behavior, configuration fields, or forge support must be reviewed against `docs/src/` and the page(s) updated in the same commit. **Every time you edit anything under `docs/src/` (or anything that should change the rendered site), run `./generate-docs.sh` immediately afterwards.** That rebuilds `docs/book/` and mirrors it into `~/projects/michaeldhopkins.com/public/docs/jjpr/`. Don't batch edits and skip the rebuild — running the script is part of the same task as the edit. Commit the source changes in jjpr and the rendered changes in `michaeldhopkins.com` separately.
   - The README is intentionally minimal — only the title, install snippet, and a pointer to the docs site. Don't grow it back into the main reference; behavior and option docs go in `docs/src/`.
   - The doc pages are hand-edited prose. The only auto-generated artifact is `docs/src/version-footer.js`, synced from `Cargo.toml` by `generate-docs.sh`. Don't edit it by hand; bump `Cargo.toml` and re-run the script.
   - When in doubt about which page a change belongs in, consult `docs/src/SUMMARY.md` for the navigation map.
