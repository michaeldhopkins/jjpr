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
