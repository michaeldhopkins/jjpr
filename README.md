# stacker

Manage stacked pull requests in [Jujutsu](https://jj-vcs.github.io/jj/) repositories.

`stk` discovers your bookmark stacks, pushes branches, creates GitHub PRs with correct base branches, and keeps stack-awareness comments in sync across all PRs in a stack.

## Install

```
cargo install --path .
```

## Usage

```
stk                              # Show stack overview
stk submit <bookmark>            # Submit stack up to bookmark
stk submit <bookmark> --dry-run  # Preview without executing
stk submit <bookmark> --reviewer alice,bob
stk submit <bookmark> --remote upstream
stk auth test                    # Test GitHub authentication
stk auth setup                   # Show auth setup instructions
```

### Stack overview

Run `stk` with no arguments to see your current stacks:

```
  auth (1 change, needs push)
  profile (2 changes, synced)
```

### Submitting a stack

`stk submit profile` will:

1. Push all bookmarks in the stack to the remote
2. Create PRs for bookmarks that don't have one yet
3. Update PR base branches to maintain the stack structure
4. Add/update a stack-awareness comment on each PR

PRs are created with the commit description as the title and body.

## Requirements

- [jj](https://jj-vcs.github.io/jj/) (Jujutsu VCS)
- [gh](https://cli.github.com/) (GitHub CLI, authenticated)
- A colocated jj/git repository with a GitHub remote

## How it works

Stacker shells out to `jj` and `gh` for all operations. It discovers stacks by walking bookmarks toward trunk, builds an adjacency graph, and plans submissions by comparing local state with GitHub.

Merge commits in a bookmark's ancestry cause that bookmark to be excluded (stacker only handles linear stacks).

## Development

```
cargo test               # Unit tests + jj integration tests
cargo clippy --tests      # Lint everything
STACKER_E2E=1 cargo test  # Include E2E tests (requires gh auth + network)
```

### Test tiers

- **Unit tests** (67): Fast, no I/O, use stub implementations of `Jj` and `GitHub` traits
- **jj integration tests** (4): Real `jj` binary against temp repos, no network
- **E2E tests** (1): Real `jj` + real GitHub against [stacker-testing-environment](https://github.com/michaeldhopkins/stacker-testing-environment), guarded by `STACKER_E2E` env var

## License

MIT
