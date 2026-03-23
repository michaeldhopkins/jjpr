# jjpr

Multi-forge stacked pull requests for [Jujutsu](https://jj-vcs.github.io/jj/). Push, create, merge, and sync stacked PRs/MRs on GitHub, GitLab, and Forgejo from one tool.

## Why jjpr?

- **Watch mode** — `jjpr watch` is an always-on assistant that creates draft PRs, promotes them when CI passes, and merges them when approved — hands-free
- **Multi-forge** — GitHub, GitLab, and Forgejo/Codeberg in one binary, auto-detected from your remote URL
- **Stack merging** — merges from the bottom up with live re-evaluation: merge a PR, sync the rest, retarget bases, check the next one, repeat
- **No force pushes** — downstream branches are synced via merge commits (append-only), avoiding force push events that clutter GitHub PR timelines
- **Merge commits** — `jj new A B` handled naturally; jjpr follows the first parent and lets other parents form independent stacks
- **Pure HTTP** — talks directly to forge APIs via `ureq`; no `gh` or `glab` CLI required (though existing credentials are picked up automatically)
- **Idempotent** — run commands repeatedly as you work; they converge to the correct state
- **Stack-awareness comments** — PRs in multi-PR stacks get a navigation comment showing their position (single PRs look like normal PRs)
- **Foreign base detection** — automatically targets PRs at a coworker's branch when your stack builds on one

## Install

### Homebrew

```
brew tap michaeldhopkins/tap
brew install jjpr
```

### Arch Linux (AUR)

```
yay -S jjpr-bin    # Pre-built binary
yay -S jjpr        # Build from source
```

### cargo-binstall (pre-built binary)

```
cargo binstall jjpr
```

### From crates.io

```
cargo install jjpr
```

### From source

```
git clone https://github.com/michaeldhopkins/jjpr
cargo install --path jjpr
```

Requires Rust 1.88+.

## Quick start

Create bookmarks for your stack, then let `jjpr watch` handle the rest:

```
jj bookmark set auth                  # create bookmarks for your changes
jj bookmark set profile
jjpr watch                            # watch creates draft PRs, promotes when
                                      # CI passes, merges when approved
```

That's it. `jjpr watch` runs in a loop (Ctrl+C to exit) and manages the full lifecycle of your stack.

## Usage

```
jjpr watch                            # Watch and auto-manage the stack
jjpr watch --timeout 60               # Stop watching after 60 minutes
jjpr                                  # Show stacks with CI/review/mergeability status
jjpr status                           # Same as above (alias for discoverability)
jjpr submit                           # Push bookmarks and create/update PRs
jjpr submit --reviewer alice,bob      # Request reviewers on all PRs
jjpr submit --draft                   # Create new PRs as drafts
jjpr submit --ready                   # Mark existing draft PRs as ready
jjpr merge                            # Merge stack from the bottom up (one-shot)
jjpr merge --merge-method rebase      # Use rebase merge method
jjpr merge --no-ci-check              # Merge even if CI hasn't passed
jjpr submit --base coworker-feat      # Override auto-detected base branch
jjpr config init                      # Create default config file
jjpr config init --repo               # Create repo-local config at .jj/jjpr.toml
jjpr auth test                        # Test forge authentication
jjpr auth setup                       # Show auth setup instructions
```

### Watching your stack

`jjpr watch` is the primary way to use jjpr. It runs in a loop and handles everything:

1. **Creates draft PRs** for bookmarks that don't have PRs yet
2. **Marks drafts as ready** when CI checks pass (does not add reviewers)
3. **Merges PRs** from the bottom up when they're approved and mergeable
4. **Syncs the stack** after each merge (retargets bases, pushes updates)
5. **Reports blockers** — if a PR needs review approval but has no reviewers, watch tells you

Stack comments (showing each PR's position in the stack) are added automatically when the stack has 2 or more PRs. A single-PR stack looks like a normal PR to reviewers.

```
Watching stack for 'settings'...

  Creating PR (draft) for 'auth'...
    https://github.com/o/r/pull/42
  Creating PR (draft) for 'settings'...
    https://github.com/o/r/pull/43

  Marked 'auth' as ready (CI passing)

  'settings' (#43): needs review approval but has no reviewers
    hint: run `jjpr submit --reviewer <username>` to request reviewers

  Merging 'auth' (PR #42, squash)...
    https://github.com/o/r/pull/42

  Waiting for 'settings':
    - Insufficient approvals (0/1)
  settings: Approval received (1/1)

  Merging 'settings' (PR #43, squash)...

Done — 2 PRs merged.
```

Use `--timeout <MINUTES>` to set a maximum wait time. Press Ctrl+C to exit at any time.

### Stack overview

Run `jjpr` (or `jjpr status`) with no arguments to see your current stacks and their PR/MR status. This is read-only — it fetches the latest state but doesn't push or modify anything.

Each PR shows its mergeability, CI status, and review state:

```
  auth (1 change, #42 open, synced)
    ✓ mergeable  ✓ CI passing  ✓ 1 approval
  profile (2 changes, #43 open, needs push)
    ✗ CI failing  ✗ 0/1 approvals  ⚠ changes requested
```

Draft PRs show a simplified status:

```
  payments (1 change, #44 draft, synced)
    — draft
```

When you have multiple independent stacks, they're labeled:

```
Stack 1:
  auth (1 change, #42 open, synced)
    ✓ mergeable  ✓ CI passing  ✓ 1 approval
  profile (2 changes, #43 open, synced)
    ✓ mergeable  ✓ CI passing  ✓ 1 approval

Stack 2:
  payments (1 change, #44 draft, needs push)
    — draft
  checkout (3 changes, #45 open, synced)
    ✗ CI pending  ✗ 0/1 approvals
```

### Submitting a stack

`jjpr submit` gives you manual control when you need it. It will:

1. Push all bookmarks in the stack to the remote
2. Create PRs for bookmarks that don't have one yet
3. Update PR base branches to maintain the stack structure
4. Update PR bodies when commit descriptions have changed
5. Add/update stack-awareness comments on multi-PR stacks

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

Use `--base <branch>` on `submit`, `merge`, or `watch` to override auto-detection — for example, when the coworker hasn't pushed yet, or when you want to target a specific branch.

### Conflicts

Before pushing, jjpr checks for unresolved conflicts in your stack. If any commits have conflicts (e.g., from a rebase that couldn't auto-resolve), jjpr reports which commits are affected and stops:

```
Error: cannot push — some commits have unresolved conflicts:

  pnnmmvmu (feat/deferment-roles): add Billings::DueDatePolicy specs

To resolve: jj edit pnnmmvmu, fix the conflicts, then re-run jjpr submit.
```

### Draft PRs

Use `--draft` to create new PRs as drafts. Existing PRs are not affected.

Use `--ready` to convert all draft PRs in the stack to ready-for-review. These flags are mutually exclusive.

### PR descriptions

PR title and body are derived from the first commit's description in each bookmark's segment.

The PR body is wrapped in HTML comment markers. When you re-submit after changing a commit message, only the managed section is updated — any text you add above or below (screenshots, notes, test plans) is preserved.

If you manually remove the markers from the PR body, jjpr will stop updating the description for that PR.

The PR title is not automatically updated after creation. If you change your commit's first line, jjpr will warn you about the drift.

### Merging a stack (one-shot)

`jjpr merge` is the one-shot alternative to `jjpr watch` — it merges what it can right now and exits. For each PR, it checks:

- PR is not a draft
- CI checks pass (configurable)
- Required number of approvals (configurable)
- No changes requested
- No merge conflicts

If the bottommost PR is mergeable, jjpr merges it, fetches the updated default branch, syncs the remaining stack, pushes all remaining bookmarks, and retargets the next PR's base if needed. Then it checks the next PR and continues until blocked or done.

By default, the remaining stack is synced via **merge commits** — each downstream bookmark gets a merge commit incorporating the new base. This is append-only, so pushes are fast-forward and avoid force push events on GitHub. You can switch to the old rebase behavior with `reconcile_strategy = "rebase"` in config (see [Configuration](#configuration)).

#### Retry on transient errors

Merge API calls are retried automatically on transient HTTP errors (502, 503). If GitHub returns a 405 "merge already in progress", jjpr polls the PR state for up to 30 seconds to confirm the merge completed. No action needed — this is transparent.

#### Local divergence

If your local commits have diverged from the remote (e.g., after a local `jj rebase`), jjpr continues merging PRs on the forge and reports local issues at the end:

```
  Merging 'auth' (#42, squash)...
  Fetching remotes...
  Rebasing remaining stack onto main...
  Pushing 'profile'...
  Warning: failed to push 'profile': conflicted commits
  Skipping local sync (local state already diverged)
  Merging 'profile' (#43, squash)...

Done — 2 PRs merged.

Note: local state is out of sync with the forge:
  Failed to push 'profile': conflicted commits

To accept the forge state (discard local divergence):
  jj git fetch
  jj bookmark set profile -r profile@origin

Or to fix local state and push it to the forge:
  jj git fetch && jj rebase -s kpqxywzy -d main
  # resolve any conflicts, then:
  jjpr submit
```

Divergent change IDs (multiple commits sharing the same ID, typically from editing sessions) are also handled as local warnings rather than fatal errors. jjpr merges on the forge and reports the divergence for you to resolve locally.

CLI flags override the config file: `--merge-method`, `--required-approvals`, `--no-ci-check`, `--reconcile-strategy`.

#### Merge method

The `merge_method` setting (or `--merge-method` flag) controls how the forge combines the PR when it lands:

- **`squash`** (default) — All commits in the PR are squashed into a single commit on the target branch. Keeps the main branch history linear and clean.
- **`merge`** — A merge commit is created, preserving the individual commits from the PR branch. Useful when you want to retain granular commit history.
- **`rebase`** — Commits are rebased onto the target branch individually (no merge commit). Linear history like squash, but preserves each commit separately.

#### Reconcile strategy

The `reconcile_strategy` setting controls how the remaining stack is synced after a PR is merged:

- **`merge`** (default) — Creates merge commits on downstream branches that incorporate the updated base. Pushes are fast-forward, so no force push events appear on GitHub PR timelines.
- **`rebase`** — Rebases downstream commits onto the new base. Rewrites commit history, which causes force pushes — these show up as immutable events on GitHub.

### Configuration

jjpr uses an optional global config at `~/.config/jjpr/config.toml` (or `$XDG_CONFIG_HOME/jjpr/config.toml`). Run `jjpr config init` to create one with defaults:

```toml
# How the forge combines the PR when it lands: "squash", "merge", or "rebase"
merge_method = "squash"

# Number of approving reviews required before merging
required_approvals = 1

# Whether CI checks must pass before merging
require_ci_pass = true

# How to sync the remaining stack after merging a PR: "merge" or "rebase"
reconcile_strategy = "merge"
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

Reviewer requests are idempotent: if a reviewer is already requested on a PR, they won't be re-requested, so re-running `jjpr submit --reviewer alice` as you grow a stack only affects PRs where Alice isn't already a reviewer.

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

MIT or Apache-2.0
