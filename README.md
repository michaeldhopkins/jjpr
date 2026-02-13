# jjpr

Manage stacked pull requests in [Jujutsu](https://jj-vcs.github.io/jj/) repositories.

`jjpr` discovers your bookmark stacks, pushes branches, creates GitHub PRs with correct base branches, and keeps stack-awareness comments in sync across all PRs in a stack.

## Install

```
cargo install --path .
```

## Usage

```
jjpr                              # Show stack overview
jjpr submit                       # Submit stack (inferred from working copy)
jjpr submit <bookmark>            # Submit stack up to bookmark
jjpr submit --dry-run             # Preview without executing
jjpr submit --reviewer alice,bob  # Request reviewers on new PRs
jjpr submit --remote upstream     # Use a specific git remote
jjpr submit --draft               # Create new PRs as drafts
jjpr submit --ready               # Mark existing draft PRs as ready
jjpr auth test                    # Test GitHub authentication
jjpr auth setup                   # Show auth setup instructions
```

### Stack overview

Run `jjpr` with no arguments to see your current stacks:

```
  auth (1 change, needs push)
  profile (2 changes, synced)
```

### Submitting a stack

`jjpr submit` (or `jjpr submit profile`) will:

1. Push all bookmarks in the stack to the remote
2. Create PRs for bookmarks that don't have one yet
3. Update PR base branches to maintain the stack structure
4. Update PR bodies when commit descriptions have changed
5. Add/update a stack-awareness comment on each PR

PRs are created with the commit description as the title and body.

When no bookmark is specified, jjpr infers the target from your working copy's position — it finds which stack overlaps with `trunk()..@` and submits up to the topmost bookmark.

### Draft PRs

Use `--draft` to create new PRs as drafts. Existing PRs are not affected.

Use `--ready` to convert all draft PRs in the stack to ready-for-review. These flags are mutually exclusive.

## Requirements

- [jj](https://jj-vcs.github.io/jj/) (Jujutsu VCS)
- [gh](https://cli.github.com/) (GitHub CLI, authenticated)
- A colocated jj/git repository with a GitHub remote

## How it works

jjpr shells out to `jj` and `gh` for all operations. It discovers stacks by walking bookmarks toward trunk, builds an adjacency graph, and plans submissions by comparing local state with GitHub.

Merge commits in a bookmark's ancestry cause that bookmark to be excluded (jjpr only handles linear stacks).

## Development

```
cargo test               # Unit tests + jj integration tests
cargo clippy --tests      # Lint everything
JJPR_E2E=1 cargo test  # Include E2E tests (requires gh auth + network)
```

### Test tiers

- **Unit tests** (88): Fast, no I/O, use stub implementations of `Jj` and `GitHub` traits
- **jj integration tests** (6): Real `jj` binary against temp repos, no network
- **E2E tests** (1): Real `jj` + real GitHub against [jjpr-testing-environment](https://github.com/michaeldhopkins/jjpr-testing-environment), guarded by `JJPR_E2E` env var

## License

MIT
