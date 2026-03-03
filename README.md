# jjpr

Multi-forge stacked pull requests for [Jujutsu](https://jj-vcs.github.io/jj/). Push, create, merge, and sync stacked PRs/MRs on GitHub, GitLab, and Forgejo from one tool.

## Why jjpr?

- **Multi-forge** — GitHub, GitLab, and Forgejo/Codeberg in one binary, auto-detected from your remote URL
- **Stack merging** — `jjpr merge` merges from the bottom up with live re-evaluation: merge a PR, rebase the rest, retarget bases, check the next one, repeat
- **Merge commits** — `jj new A B` handled naturally; jjpr follows the first parent and lets other parents form independent stacks
- **Pure HTTP** — talks directly to forge APIs via `ureq`; no `gh` or `glab` CLI required (though existing credentials are picked up automatically)
- **Idempotent** — run `jjpr submit` repeatedly as you work; it converges to the correct state, pushing only what changed
- **Stack-awareness comments** — every PR gets a navigation comment showing its position in the stack
- **Foreign base detection** — automatically targets PRs at a coworker's branch when your stack builds on one

## Install

```
git clone https://github.com/michaeldhopkins/jjpr
cargo install --path jjpr
```

Requires Rust 1.88+.

## Usage

```
jjpr                              # Show stack overview
jjpr submit                       # Submit stack (inferred from working copy)
jjpr submit <bookmark>            # Submit stack up to bookmark
jjpr submit --dry-run             # Preview without executing
jjpr submit --reviewer alice,bob  # Request reviewers on all PRs
jjpr submit --remote upstream     # Use a specific git remote
jjpr submit --draft               # Create new PRs as drafts
jjpr submit --ready               # Mark existing draft PRs as ready
jjpr merge                        # Merge stack from the bottom up
jjpr merge <bookmark>             # Merge stack up to bookmark
jjpr merge --merge-method rebase  # Use rebase merge method
jjpr merge --no-ci-check          # Merge even if CI hasn't passed
jjpr merge --dry-run              # Preview without executing
jjpr submit --base coworker-feat  # Override auto-detected base branch
jjpr merge --base coworker-feat   # Override auto-detected base branch
jjpr config init                  # Create default config file
jjpr config init --repo           # Create repo-local config at .jj/jjpr.toml
jjpr --no-fetch                   # Show stacks without fetching
jjpr submit --no-fetch            # Submit without fetching first
jjpr auth test                    # Test forge authentication
jjpr auth setup                   # Show auth setup instructions
```

### Stack overview

Run `jjpr` with no arguments to see your current stacks and their PR/MR status. This is read-only — it fetches the latest state but doesn't push or modify anything.

```
  auth (1 change, #42 open, needs push)
  profile (2 changes, #41 draft, synced)
```

When you have multiple independent stacks, they're labeled:

```
Stack 1:
  auth (1 change, #42 open, synced)
  profile (2 changes, #43 open, synced)

Stack 2:
  payments (1 change, #44 draft, needs push)
  checkout (3 changes, #45 open, synced)
```

### Submitting a stack

`jjpr submit` (or `jjpr submit profile`) will:

1. Push all bookmarks in the stack to the remote
2. Create PRs for bookmarks that don't have one yet
3. Update PR base branches to maintain the stack structure
4. Update PR bodies when commit descriptions have changed
5. Add/update a stack-awareness comment on each PR

Submit is idempotent — run it repeatedly as you work. After rebasing, editing commit messages, or restacking with `jj rebase`, just run `jjpr submit` again and it will push the updated commits, fix PR base branches, and sync descriptions. If everything is already up to date, it reports "Stack is up to date."

PRs are created with the commit description as the title and body.

When no bookmark is specified, jjpr infers the target from your working copy's position — it finds which stack overlaps with `trunk()..@` and submits up to the topmost bookmark.

### Stacking on other branches

jjpr auto-detects when your stack is based on someone else's branch. If a commit in your stack's ancestry has a remote bookmark that isn't one of your own, jjpr treats it as a foreign base and targets your first PR at that branch instead of the default branch (e.g., `main`).

```
  auth (1 change, #42 open, synced)
  profile (1 change, needs push)
  (based on coworker-feat)
```

Use `--base <branch>` on `submit` or `merge` to override auto-detection — for example, when the coworker hasn't pushed yet, or when you want to target a specific branch.

### Draft PRs

Use `--draft` to create new PRs as drafts. Existing PRs are not affected.

Use `--ready` to convert all draft PRs in the stack to ready-for-review. These flags are mutually exclusive.

### PR descriptions

PR title and body are derived from the first commit's description in each bookmark's segment.

The PR body is wrapped in HTML comment markers. When you re-submit after changing a commit message, only the managed section is updated — any text you add above or below (screenshots, notes, test plans) is preserved.

If you manually remove the markers from the PR body, jjpr will stop updating the description for that PR.

The PR title is not automatically updated after creation. If you change your commit's first line, jjpr will warn you about the drift.

### Merging a stack

`jjpr merge` merges your stack from the bottom up. For each PR, it checks:

- PR is not a draft
- CI checks pass (configurable)
- Required number of approvals (configurable)
- No changes requested
- No merge conflicts

If the bottommost PR is mergeable, jjpr merges it, fetches the updated default branch, rebases the remaining stack onto it with `jj rebase`, pushes all remaining bookmarks, and retargets the next PR's base if needed. Then it checks the next PR and continues until blocked or done.

If a PR is blocked (e.g., CI pending), jjpr reports why and stops. Run `jjpr merge` again once the blocker is resolved.

```
  Skipping 'auth' — PR #42 already merged
  Merging 'profile' (PR #43, squash)...
    https://github.com/o/r/pull/43
  Fetching remotes...
  Rebasing remaining stack onto main...
  Pushing 'settings'...
  Updating PR #44 base to 'main'...
  Blocked at 'settings' (PR #44):
    - CI checks are pending
  Run `jjpr merge` again once the issue is resolved.
```

CLI flags override the config file: `--merge-method`, `--required-approvals`, `--no-ci-check`.

### Configuration

jjpr uses an optional global config at `~/.config/jjpr/config.toml` (or `$XDG_CONFIG_HOME/jjpr/config.toml`). Run `jjpr config init` to create one with defaults:

```toml
# Merge method: "squash", "merge", or "rebase"
merge_method = "squash"

# Number of approving reviews required before merging
required_approvals = 1

# Whether CI checks must pass before merging
require_ci_pass = true
```

#### Repo-local config

You can also create a repo-local config at `.jj/jjpr.toml` (inside the `.jj/` directory, which is gitignored). Run `jjpr config init --repo` to create one. Repo-local settings override global settings.

This is useful for setting the forge type and token for self-hosted instances:

```toml
# Forge type: "github", "gitlab", or "forgejo"
forge = "forgejo"

# Environment variable name containing the API token
forge_token_env = "FORGEJO_TOKEN"
```

When `forge` is set in config, auto-detection is skipped and the configured forge type is used directly. The token is read from the env var named by `forge_token_env` (or the forge's default: `GITHUB_TOKEN`, `GITLAB_TOKEN`, or `FORGEJO_TOKEN`).

If no config file exists, defaults are used. CLI flags always override the config file.

### Fetching

By default, `jjpr` fetches all remotes before operating to ensure it has the latest state. Use `--no-fetch` to skip this (useful for offline work or when you've just fetched).

### Reviewers

Use `--reviewer alice,bob` to request reviewers. Reviewers are applied to all PRs in the stack — both newly created and existing ones.

## Requirements

- Rust 1.88+ (for building from source)
- [jj](https://jj-vcs.github.io/jj/) 0.36+ (Jujutsu VCS)
- A colocated jj/git repository with a supported remote

Authentication is token-based. jjpr talks directly to forge APIs — no CLI tools required.

| Forge | Token env var | CLI fallback |
|-------|--------------|--------------|
| GitHub | `GITHUB_TOKEN` or `GH_TOKEN` | `gh auth login` (reads stored credentials) |
| GitLab | `GITLAB_TOKEN` | `glab auth login` (reads stored credentials) |
| Forgejo/Codeberg | `FORGEJO_TOKEN` | — |

If you already use `gh` or `glab`, jjpr picks up your existing credentials automatically — no extra setup needed.

For Forgejo/Codeberg, generate an API token with `repo` scope from your instance's settings (e.g., `https://codeberg.org/user/settings/applications`) and export it:

```
export FORGEJO_TOKEN=your_token_here
```

For self-hosted Forgejo, also set the forge type in `.jj/jjpr.toml`:

```toml
forge = "forgejo"
```

## How it works

jjpr auto-detects the forge from your remote URL and talks directly to forge APIs via HTTP. It shells out to `jj` for version control operations, discovers stacks by walking bookmarks toward trunk, builds an adjacency graph, and plans submissions by comparing local state with the forge.

Auto-detection recognizes `github.com`, `gitlab.com`, and `codeberg.org` (plus Enterprise subdomains for GitHub/GitLab). For self-hosted instances, set `forge` in `.jj/jjpr.toml` — see [Repo-local config](#repo-local-config).

Merge commits (`jj new A B`) are supported: jjpr follows the first parent through the merge and lets the other parent(s) form independent stacks. PRs for merge bookmarks include a note explaining which branches were merged and that the diff may include their changes until those PRs land.

## Development

```
cargo test               # Unit tests + jj integration tests
cargo clippy --tests      # Lint everything
JJPR_E2E=1 cargo test  # Include E2E tests (requires gh auth + network)
```

### Test tiers

- **Unit tests**: Fast, no I/O, use stub implementations of `Jj` and `Forge` traits
- **jj integration tests**: Real `jj` binary against temp repos, no network
- **E2E tests**: Real `jj` + real forge against [jjpr-testing-environment](https://github.com/michaeldhopkins/jjpr-testing-environment), guarded by `JJPR_E2E` env var

## License

MIT
