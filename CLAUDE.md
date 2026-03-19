# jjpr

## Project overview

Rust CLI tool (`jjpr`) for managing stacked pull requests in Jujutsu (jj) repositories. Shells out to `jj` for version control; talks directly to forge APIs via `ureq` (sync HTTP client).

## Architecture

- `src/jj/` — Jj trait + JjRunner (shells out to jj binary), template strings, type definitions
- `src/forge/` — Forge trait + backends (GitHub, GitLab, Forgejo) using `ForgeClient` (ureq HTTP wrapper), token resolution, remote URL parsing, PR comment generation
- `src/graph/` — Change graph construction from bookmarks, traversal toward trunk
- `src/submit/` — Analyze target stack, resolve multi-bookmark segments, plan submission, execute (push/PR/comments)
- `src/auth.rs` — Auth test/help commands

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
cargo clippy --tests      # Must be clean
JJPR_E2E=1 cargo test  # E2E against real GitHub (slow, requires gh auth)
```

E2E tests use `michaeldhopkins/jjpr-testing-environment` (private repo). Each run creates uniquely-prefixed bookmarks and cleans up PRs/branches on Drop.

## Before pushing

Every push must pass these steps. CI runs `cargo check --locked`, `cargo test`, `cargo clippy`, and `cargo deny` — a stale lockfile or clippy warning will fail the build.

1. **Bump the version** in `Cargo.toml` when adding features or making behavioral changes (semver: patch for fixes, minor for new features/behavioral changes).
2. **Update Cargo.lock** — run `cargo check` after any `Cargo.toml` change so the lockfile stays in sync. CI uses `--locked` and will reject a stale lockfile.
3. **`cargo test`** — all tests must pass.
4. **`cargo clippy --tests`** — must be clean (warnings are errors in CI).
5. **`cargo install --path .`** — install the updated binary locally.
6. **Update README.md** when adding features, commands, or changing usage patterns.
