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

**Baseline (2026-08-01, 6 targets × 15 min in parallel, local).** ~21.4M executions. Corpus after, from a handful of committed seeds: `graph_invariants` 2430, `jj_output` 2301, `forge_payload` 1897, `config_load` 1310, `comment_roundtrip` 573, `remote_url` 222. Treat those as the saturation baseline — a nightly that stops growing a target means change tactics (seed, dictionary, new target) rather than add hours.

`remote_url` is the outlier at 222 and plateaued early, which is expected rather than wrong: `is_shape_preserving` rejects most mutations by design, so the mutator has little room. It is a replay-grade target already; if it ever needs more, the lever is seeding real remote shapes, not runtime.

The one artifact produced was a `slow-unit` in `comment_roundtrip` — a **false positive**. It measured ~0.03 ms per execution when re-timed over 2000 runs on an idle machine, four orders of magnitude under libFuzzer's threshold; it was recorded only because 18 sanitizer-instrumented processes were competing for CPU. Hence the CI check fails on `crash-*`/`timeout-*`/`oom-*` and merely uploads `slow-unit-*`. When the perf signal is what you actually want, run targets one at a time.

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

**Not done yet:** a completed full-tree run. At ~88s/mutant that needs sharding across machines, and its missed list — not an estimate — is what should decide whether a nightly gate is worth adding.

## After every code change

**`cargo install --path .`** — reinstall the local binary so the `jjpr` on `PATH` matches the source you just changed.

jjpr is a tool you actually run, so this is a non-optional final step of *every* code change — not a release step and not a pushing step. Never defer it to "before pushing" or wait for approval: it is a local install with no outward side effect, and skipping it leaves you running a stale binary. Install on every change; push only when asked.

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

Every push must pass these steps. CI runs `cargo check --locked`, `cargo test`, `cargo clippy`, and `cargo deny` — a stale lockfile or clippy warning will fail the build.

1. **Bump the version** in `Cargo.toml` when adding features or making behavioral changes (semver: patch for fixes, minor for new features/behavioral changes).
2. **Update Cargo.lock** — run `cargo check` after any `Cargo.toml` change so the lockfile stays in sync. CI uses `--locked` and will reject a stale lockfile.
   - **`fuzz/Cargo.lock` carries the version too, and nothing in the normal flow touches it.** It only moves when someone builds a fuzz target, so a version bump leaves it behind silently — it sat at 0.36.0 through the entire 0.36.1 release. Nothing fails, because the fuzz jobs do not pass `--locked`, which is exactly why it goes unnoticed. Refresh it with `cargo +nightly fuzz build <target>` (any target) when bumping.
3. **`cargo test`** — all tests must pass.
4. **`cargo clippy --locked --tests -- -D warnings`** — exact CI flags. `-D warnings` promotes warnings to errors, which catches things plain `cargo clippy --tests` doesn't (e.g., `too_many_lines` is `warn` locally but fails CI). Must be clean.
5. **Review and regenerate the docs.** Any change to commands, flags, output, behavior, configuration fields, or forge support must be reviewed against `docs/src/` and the page(s) updated in the same commit. **Every time you edit anything under `docs/src/` (or anything that should change the rendered site), run `./generate-docs.sh` immediately afterwards.** That rebuilds `docs/book/` and mirrors it into `~/projects/michaeldhopkins.com/public/docs/jjpr/`. Don't batch edits and skip the rebuild — running the script is part of the same task as the edit. Commit the source changes in jjpr and the rendered changes in `michaeldhopkins.com` separately.
   - The README is intentionally minimal — only the title, install snippet, and a pointer to the docs site. Don't grow it back into the main reference; behavior and option docs go in `docs/src/`.
   - The doc pages are hand-edited prose. The only auto-generated artifact is `docs/src/version-footer.js`, synced from `Cargo.toml` by `generate-docs.sh`. Don't edit it by hand; bump `Cargo.toml` and re-run the script.
   - When in doubt about which page a change belongs in, consult `docs/src/SUMMARY.md` for the navigation map.
