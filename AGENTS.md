# jjpr

## Project overview

Rust CLI tool (`jjpr`) for managing stacked pull requests in Jujutsu (jj) repositories. Shells out to `jj` for version control; talks directly to forge APIs via `ureq` (sync HTTP client).

## Architecture

- `src/jj/` — Jj trait + JjRunner (shells out to jj binary), template strings, type definitions
- `src/forge/` — Forge trait + backends (GitHub, GitLab, Forgejo) using `ForgeClient` (ureq HTTP wrapper), token resolution, remote URL parsing, PR comment generation
- `src/graph/` — Change graph construction from bookmarks, traversal toward trunk
- `src/submit/` — Analyze target stack, resolve multi-bookmark segments, plan submission, execute (push/PR/comments)
- `src/undo/` — `jjpr undo` / `jjpr redo`: `submit`, `merge` and `watch` record their pushes and forge writes (with the values they replaced) through `RecordingJj` / `RecordingForge` into a journal in `.jj/repo/jjpr/undo/`; undo plans the reverse (`plan.rs`, pure) and runs it. Design, measurements and per-forge findings: `notes/undo.md`
- `src/connect.rs` — finding the repo root and building the forge client, shared by `main.rs` and undo
- `src/auth.rs` — Auth test/help commands
- `notes/forges/` — internal research on forges: feature deep-dives (e.g. GitHub native stacks) and candidate-forge evaluations. Not user docs; those are `docs/src/forges.md`.

## Key conventions

- Traits (`Jj`, `Forge`) for all external I/O — enables testing with stubs
- Test stubs use `Mutex<Vec<String>>` for recording calls (traits require Send + Sync)
- Forge backends (`github.rs`, `gitlab.rs`, `forgejo.rs`) and `ForgeClient` are tested against `src/forge/test_server.rs`, a `cfg(test)` stub HTTP server on loopback (standard library only). Route the verb and path, assert on `request_lines()` and the recorded body. Any new backend method gets a test there; the e2e suite does not count for mutation testing (below)
- Co-located `#[cfg(test)] mod tests` in every module
- A file under `src/` holds at most 400 production lines (inline test items do not count), enforced by `tests/file_length.rs`. The nine files over it when the gate went in (2026-09-26) are pinned at their size: they may shrink, never grow, and a shrink lowers the pin in the same change. New code goes in a new module, never into a pinned file
- `tests/lint_suppressions.rs` pins the `#[allow]`/`#[expect]` count (8) and the `clippy.toml` thresholds loosened past clippy's defaults, which may only fall, and fails when a baseline lint (`unwrap_used`, `dbg_macro`, `undocumented_unsafe_blocks` and the rest) leaves `Cargo.toml` or weakens. Remove a suppression on any item you touch
- jj templates produce line-delimited JSON; `escape_json()` includes surrounding quotes
- Edition 2024 with let-chains for collapsible if-let patterns
- Requires jj 0.36+. Where a spelling differs across that range, `src/jj/version.rs` picks it from `jj --version` (today only pushing a new bookmark, which 0.36/0.37 refuse without `git.push-new-bookmarks`). `tests/jj_compat.rs` parses real output captured from jj 0.33.0, 0.36.0, 0.37.0, 0.38.0, 0.40.0, 0.45.1 and 0.46.0 (`tests/fixtures/jj/`, recapture command in the file's header), and CI's `jj-versions` job runs the suite against 0.36 through 0.46.0. Spellings that do NOT span the range, and so must not be used: the `divergent()` revset (0.38+), `<change>/N` (0.37+), `all:` (gone in 0.38; use `change_id(x)`), `--allow-new` (gone in 0.36)
- `jj bookmark list` prints one line per *ref* (local, each tracked remote pointing elsewhere, a differing `@git`), so `BOOKMARK_TEMPLATE` emits `remote` (null for local) and only local lines become bookmarks. Captured on every version above; see `parse_bookmark_output`
- Dependencies move through the owner's `jjpr-deps` upkeep job, never Dependabot; it also adopts each new release of vcs-runner.

## Testing

```
cargo test               # Unit + jj integration (fast, ~2s)
cargo clippy --locked --tests -- -D warnings  # Must be clean (CI's exact flags)
JJPR_E2E=1 cargo test  # E2E against real GitHub (slow, requires gh auth)
```

E2E tests use `michaeldhopkins/forge-e2e-sandbox` (private repo, shared across forges/projects — see the `forge-e2e-testing` skill). Each run creates uniquely-prefixed bookmarks and cleans up PRs/branches on Drop.

### Fuzzing and mutation testing (project specifics)

The detail lives in topic files; read the one that matches what you are about to do:

- `docs/agents/fuzzing.md`: why jjpr is fuzzable, what each target asserts, dictionaries, seeds and the corpus policy. Read before adding or changing a fuzz target, a seed or a template the dictionaries derive from.
- `docs/agents/fuzzing-ci.md`: the replay gate and burst on main, the retired nightly, the baseline and past finds. Read before changing a fuzz workflow or `fuzz/burst.sh`, or when a Fuzz run is red.
- `docs/agents/mutation-testing.md`: what `mutants.yml` runs, the slice size, measured costs and how to read a MISSED mutant. Read before triaging any MISSED mutant.
- `docs/agents/mutation-slices.md`: findings from each rotating slice. Add an entry when you kill a slice's misses.
- `docs/agents/mutation-runs.md`: the `--in-diff` traps and the measured runs behind the job's limits. Read before running `--in-diff` locally or changing `mutants.yml`.
- `docs/agents/mutation-cost.md`: what each mutation tool costs and how to verify a single mutant by hand. Read before starting any local `cargo mutants` run.

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

Every push must pass these steps. CI runs `cargo fmt --check`, `cargo check --locked`, `cargo test`, `cargo clippy --locked --tests -- -D warnings`, and `cargo deny check` (advisories, bans, licenses and sources; `tests/ci_rules.rs` fails if a workflow narrows it) — a stale lockfile, a formatting difference, or a single clippy warning fails the build. `release.yml` duplicates all of them as the publish gate, so a gate added to one must be added to the other.

None of this should be news by the time you get here: fmt and clippy belong to *every code change* (see above), and this list is the final check, not the first time you run them.

0. **`cargo fmt`** — if this produces a diff, you skipped a step earlier. Commit it with the change it belongs to rather than as a trailing "fix formatting" commit.
1. **Bump the version** in `Cargo.toml` when adding features or making behavioral changes (semver: patch for fixes, minor for new features/behavioral changes).
   - "Behavioral change" includes becoming *more* permissive. 0.37.0 went out as a minor, not a patch, because accepting previously-rejected remote URLs turned some working single-remote repos into ambiguous ones. "Everything it changes was already broken" is a claim worth testing before it justifies a patch.
2. **Update Cargo.lock** — run `cargo check` after any `Cargo.toml` change so the lockfile stays in sync. CI uses `--locked` and will reject a stale lockfile.
   - **`fuzz/Cargo.lock` carries the version too, and nothing in the normal flow touches it.** It only moves when someone builds a fuzz target, so a version bump leaves it behind silently — it sat at 0.36.0 through the entire 0.36.1 release. Nothing fails, because the fuzz jobs do not pass `--locked`, which is exactly why it goes unnoticed. Refresh it with `cargo +nightly fuzz build <target>` (any target) when bumping.
3. **`cargo test`** — all tests must pass.
4. **`cargo clippy --locked --tests -- -D warnings`** — exact CI flags. `-D warnings` promotes warnings to errors, which catches things plain `cargo clippy --tests` doesn't (e.g., `too_many_lines` is `warn` locally but fails CI). Must be clean.
5. **Review and regenerate the docs.** Any change to commands, flags, output, behavior, configuration fields, or forge support must be reviewed against `docs/src/` and the page(s) updated in the same commit. **Every time you edit anything under `docs/src/` (or anything that should change the rendered site), run `./generate-docs.sh` immediately afterwards.** That rebuilds `docs/book/`, so a broken build or bad link surfaces right away. Don't batch edits and skip the rebuild — running the script is part of the same task as the edit. The release workflow publishes the book to michaeldhopkins.com on each release; never commit rendered docs into the `michaeldhopkins.com` repo by hand.
   - The README is intentionally minimal — only the title, install snippet, and a pointer to the docs site. Don't grow it back into the main reference; behavior and option docs go in `docs/src/`.
   - The doc pages are hand-edited prose. The only auto-generated artifact is `docs/src/version-footer.js`, synced from `Cargo.toml` by `generate-docs.sh`. Don't edit it by hand; bump `Cargo.toml` and re-run the script.
   - When in doubt about which page a change belongs in, consult `docs/src/SUMMARY.md` for the navigation map.
