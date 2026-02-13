# jjpr

## Project overview

Rust CLI tool (`jjpr`) for managing stacked pull requests in Jujutsu (jj) repositories. Shells out to `jj` and `gh` for all external operations — no async runtime, no HTTP client libraries.

## Architecture

- `src/jj/` — Jj trait + JjRunner (shells out to jj binary), template strings, type definitions
- `src/github/` — GitHub trait + GhCli (shells out to gh CLI), PR comment generation, remote URL parsing
- `src/graph/` — Change graph construction from bookmarks, traversal toward trunk
- `src/submit/` — Analyze target stack, resolve multi-bookmark segments, plan submission, execute (push/PR/comments)
- `src/auth.rs` — Auth test/help commands

## Key conventions

- Traits (`Jj`, `GitHub`) for all external I/O — enables testing with stubs
- Test stubs use `Mutex<Vec<String>>` for recording calls (traits require Send + Sync)
- Co-located `#[cfg(test)] mod tests` in every module
- jj templates produce line-delimited JSON; `escape_json()` includes surrounding quotes
- Edition 2024 with let-chains for collapsible if-let patterns

## Testing

```
cargo test               # Unit + jj integration (fast, ~2s)
cargo clippy --tests      # Must be clean
STACKER_E2E=1 cargo test  # E2E against real GitHub (slow, requires gh auth)
```

E2E tests use `michaeldhopkins/stacker-testing-environment` (private repo). Each run creates uniquely-prefixed bookmarks and cleans up PRs/branches on Drop.

## README

Keep README.md up to date when adding features, commands, or changing usage patterns.
