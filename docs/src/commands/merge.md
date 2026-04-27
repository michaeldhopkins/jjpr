# merge

`jjpr merge` is the one-shot alternative to `jjpr watch`. It merges
whatever it can right now and exits.

```
jjpr merge                            # merge the inferred stack
jjpr merge <bookmark>                 # merge the stack ending at <bookmark>
jjpr merge --merge-method rebase      # use rebase merge method
jjpr merge --no-ci-check              # merge even if CI hasn't passed
jjpr merge --dry-run                  # preview without merging
```

## What it does

For each PR in the stack, starting from the bottom, jjpr checks:

- PR is not a draft.
- CI checks pass (configurable).
- Required number of approvals reached (configurable).
- No changes requested.
- No merge conflicts.

If the bottommost PR is mergeable, jjpr merges it, fetches the updated
default branch, syncs the remaining stack, pushes all remaining
bookmarks, and retargets the next PR's base. Then it checks the next
PR and continues until blocked or done.

By default, the remaining stack is synced by rebasing downstream
commits onto the new base. Switch to merge-based reconciliation with
`reconcile_strategy = "merge"` (see
[Configuration](../configuration.md)). That creates merge commits
instead, avoiding force pushes.

## Flags

| Flag | Effect |
|---|---|
| `--merge-method <method>` | `squash`, `merge`, or `rebase` (overrides config) |
| `--required-approvals <N>` | Override the config's approval threshold |
| `--no-ci-check` | Treat PRs with non-passing CI as mergeable |
| `--reconcile-strategy <strategy>` | `rebase` or `merge` for post-merge stack syncing |
| `--base <branch>` | Override the auto-detected stack base |
| `--remote <name>` | Override the git remote name |
| `--no-fetch` | Skip `git fetch` before starting |
| `--dry-run` | Print what would happen without merging |

CLI flags override the config file.

## Retry on transient errors

Merge API calls retry automatically on transient HTTP errors (502,
503). If the forge returns a 405 "merge already in progress", jjpr
polls the PR state for up to 30 seconds to confirm completion. No
user action needed; this is transparent in normal operation.

## Local divergence

If your local commits have diverged from the remote (after a local
`jj rebase`, for example), jjpr continues merging PRs on the forge and
reports local issues at the end:

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

Divergent change IDs (multiple commits sharing the same ID, often
from editing sessions) are handled the same way: as local warnings
rather than fatal errors. jjpr merges on the forge and reports the
divergence for you to resolve locally.
