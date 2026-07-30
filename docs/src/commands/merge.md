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

## GitHub native stacks

GitHub's own stacked pull requests are a separate feature from jjpr's stacks.
If a PR has been registered as a member of a GitHub native stack, with
`gh stack submit` or from the web UI, GitHub refuses to merge it through the
API endpoint jjpr uses and returns a 403.

jjpr checks for this before merging anything, so a stack it cannot finish is
reported up front rather than partway up:

```
  Blocked at 'profile' (#221):
    - In native stack #223 (2 of 4); GitHub refuses API merges of stacked PRs. Run `gh stack merge 221`, which lands #221 and everything below it

This stack is registered as a GitHub native stack, which jjpr cannot merge.
Merge it with `gh stack merge`, or dissolve the native stack with
`gh stack unstack 223` to hand merging back to jjpr.
```

Two things to know about the remedies:

- `gh stack merge <pr>` merges that PR **and every PR below it** in one
  atomic operation. It needs the extension: `gh extension install github/gh-stack`.
- `gh stack unstack <stack>` removes every PR it can from the stack, not only
  the one you name. PRs that have already merged stay in it. Once the open PRs
  are unstacked, `jjpr merge` works on them normally.

`jjpr watch` stops on this rather than polling. Stack membership does not
clear on its own, so waiting would never resolve it.

`jjpr submit` is mostly unaffected: pushing to a stacked PR, creating PRs,
and updating descriptions and comments all work. The one exception is
retargeting a PR's base, which GitHub rejects for any stacked PR. See
[Reshaping a native stack](submit.md#reshaping-a-native-stack).

This applies to GitHub only. GitLab and Forgejo have no equivalent feature.

## Reconcile failures

After each merge, jjpr reconciles two things: local state (fetch, rebase,
push the remaining stack) and forge state (refresh PR list, retarget the
next base, update stack-info comments). If either fails, jjpr stops at
the next PR rather than merging it. Merging without a local rebase would
mix in the previous PR's changes; merging against stale forge state can
target the wrong base.

### Preserving approvals

A **merge-commit** or **rebase-merge** landing leaves the merged commit in
trunk's history, so the remaining stack already sits on trunk. jjpr skips
the rebase and force-push entirely and only retargets the next PR's base:

```
  Merging 'auth' (#42, merge)...
  Fetching remotes...
  Remaining stack already based on main; skipping rebase
  Updating #43 base to 'main'...
```

This matters when the base branch resets approvals on push (GitHub's
"dismiss stale reviews", GitLab's "reset approvals on push", Forgejo's
"dismiss stale approvals"): the skipped force-push would otherwise drop a
standing approval on the descendant PR for nothing. A **squash** landing
orphans the descendant's parent, so the rebase and force-push are
genuinely required — and jjpr reports how many approvals that push drops:

```
  Merging 'auth' (#42, squash)...
  Fetching remotes...
  Rebasing remaining stack onto main...
  Pushing 'profile'...
    ⚠ dismissed 2 approvals on #43 — base 'main' resets approvals on push
  Updating #43 base to 'main'...
```

### Conflicted rebase or merge

A rebase or merge that jj completes can still leave conflicts: jj records them
in the commit rather than failing. jjpr checks every bookmark after syncing and
pushes only the ones that came out clean, stopping at the first conflicted one.

```
  Merging 'auth' (#42, squash)...
  Rebasing remaining stack onto main...
  Pushing 'profile'...
  Blocked at 'settings' (#44):
    - Local sync failed

Note: local state is out of sync with the forge:
  Rebase of 'settings' onto 'main' has conflicts; skipping push
```

Bookmarks below the conflict are pushed normally. Resolve with `jj resolve`,
then re-run `jjpr merge`.

### Local sync failed

Failed fetch, failed rebase, conflicted push, or a divergent change ID.

```
  Merging 'auth' (#42, squash)...
  Fetching remotes...
  Rebasing remaining stack onto main...
  Pushing 'profile'...
  Warning: failed to push 'profile': conflicted commits
  Updating #43 base to 'main'...
  Blocked at 'profile' (#43):
    - Local sync failed

Run `jjpr merge` again once the issue is resolved.

Note: local state is out of sync with the forge:
  Failed to push 'profile': conflicted commits

To accept the forge state (discard local divergence):
  jj git fetch
  jj bookmark set profile -r profile@origin

Or to fix local state and push it to the forge:
  jj git fetch && jj rebase -s <root-change-id> -d main
  # resolve any conflicts, then:
  jjpr submit
```

`<root-change-id>` is the oldest commit in the next segment (jjpr fills
this in for you). Using the bookmark tip's change ID rebases only the
tip and strands earlier commits under the old base.

The forge-side parts of reconcile (next PR's base retarget, stack-info
comment update) still run before jjpr stops. Only the merge itself waits.

### Forge reconcile failed

The forge merge succeeded but a follow-up API call (list_open_prs,
update_pr_base, comment update) returned an error. Recovery is usually
to retry; persistent failures point at network or permission issues.

```
  Blocked at 'profile' (#43):
    - Forge reconcile failed

Note: forge reconcile failed:
  Failed to retarget #43 base to 'main': 502 bad gateway

Retry with `jjpr merge` (or wait for `jjpr watch` to retry).
```

`jjpr watch` keeps polling through both kinds of failure and resumes
automatically once the next reconcile succeeds, so persistent watch
sessions self-heal.
