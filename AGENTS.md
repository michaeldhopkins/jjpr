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
3. **`cargo test`** — all tests must pass.
4. **`cargo clippy --locked --tests -- -D warnings`** — exact CI flags. `-D warnings` promotes warnings to errors, which catches things plain `cargo clippy --tests` doesn't (e.g., `too_many_lines` is `warn` locally but fails CI). Must be clean.
5. **Review and regenerate the docs.** Any change to commands, flags, output, behavior, configuration fields, or forge support must be reviewed against `docs/src/` and the page(s) updated in the same commit. **Every time you edit anything under `docs/src/` (or anything that should change the rendered site), run `./generate-docs.sh` immediately afterwards.** That rebuilds `docs/book/` and mirrors it into `~/projects/michaeldhopkins.com/public/docs/jjpr/`. Don't batch edits and skip the rebuild — running the script is part of the same task as the edit. Commit the source changes in jjpr and the rendered changes in `michaeldhopkins.com` separately.
   - The README is intentionally minimal — only the title, install snippet, and a pointer to the docs site. Don't grow it back into the main reference; behavior and option docs go in `docs/src/`.
   - The doc pages are hand-edited prose. The only auto-generated artifact is `docs/src/version-footer.js`, synced from `Cargo.toml` by `generate-docs.sh`. Don't edit it by hand; bump `Cargo.toml` and re-run the script.
   - When in doubt about which page a change belongs in, consult `docs/src/SUMMARY.md` for the navigation map.
