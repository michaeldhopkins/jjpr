# watch

`jjpr watch` runs in a loop and manages the full lifecycle of your
stack. It creates draft PRs, promotes them when CI passes, merges
them when approved, and syncs the rest of the stack after each
merge.

```
jjpr watch                            # auto-detect the stack from working copy
jjpr watch <bookmark>                 # watch the stack ending at <bookmark>
jjpr watch --timeout 60               # stop after 60 minutes
jjpr watch --no-ci-check              # merge without waiting for CI
```

## What it does

Each cycle:

1. Creates draft PRs for bookmarks in the stack that don't have one
   yet.
2. Marks drafts as ready when their CI checks pass. Reviewers are not
   added automatically.
3. Merges PRs from the bottom up once they're approved and mergeable.
4. Syncs the stack after each merge: rebases downstream, pushes
   updated bookmarks, retargets PR bases.
5. Reports blockers. When a PR needs approval but has no reviewers,
   the loop says so and continues.

Press `Ctrl+C` to exit. The next run resumes from wherever the stack
is now.

## Flags

| Flag | Effect |
|---|---|
| `--timeout <MINUTES>` | Exit after this many minutes regardless of state |
| `--no-ci-check` | Treat PRs with non-passing CI as mergeable |
| `--merge-method <method>` | `squash`, `merge`, or `rebase` (overrides config) |
| `--required-approvals <N>` | Override the config's approval threshold |
| `--reconcile-strategy <strategy>` | `rebase` or `merge` for post-merge stack syncing |
| `--reviewer <users>` | Comma-separated reviewers; requested per scope each iteration |
| `--reviewer-scope <scope>` | `bottom` (default), `leaf`, or `all` |
| `--ready` | Create new PRs as ready instead of as drafts (skips the promote phase) |
| `--base <branch>` | Override the auto-detected stack base |
| `--remote <name>` | Override the git remote name |
| `--no-fetch` | Skip `git fetch` before starting |

## Sample session

Two bookmarks set up as a stack, then a single `jjpr watch`
invocation handles the rest:

```
$ jj bookmark set auth
$ jj bookmark set profile
$ jjpr watch
Watching stack for 'profile'...

  Creating PR (draft) for 'auth'...
    https://github.com/o/r/pull/42
  Creating PR (draft) for 'profile'...
    https://github.com/o/r/pull/43

  Marked 'auth' as ready (CI passing)

  'profile' (#43): needs review approval but has no reviewers
    hint: run `jjpr submit --reviewer <username>` to request reviewers

  Merging 'auth' (PR #42, squash)...
    https://github.com/o/r/pull/42

  Waiting for 'profile':
    - Insufficient approvals (0/1)
  profile: Approval received (1/1)

  Merging 'profile' (PR #43, squash)...

Done. 2 PRs merged.
```

## When no bookmark exists

If you run `jjpr watch` before setting any bookmark in the working
copy's ancestry, it waits for one to appear:

```
Waiting for a bookmark in the working copy's ancestry...
    hint: jj bookmark set <name>
```

Run `jj bookmark set <name>` in another shell and the loop picks it up
within a few seconds.

## Reviewers and scope

`--reviewer alice,bob` requests reviewers each iteration. Default scope
is `bottom` — the request lands on the lowest live PR. As that PR merges,
the next iteration's bottom (which is now what was middle) gets the
request. This means a reviewer is always being asked to review the PR
that will land next, not the entire stack at once.

`--reviewer-scope leaf` requests on the topmost live PR; `all` requests
on every PR (the pre-0.21 default).

## Ready vs. draft

By default, watch creates new PRs as drafts and **promotes** them to
ready when their CI checks pass. This gives you a "hold while CI runs"
window. Pass `--ready` to skip both: new PRs are created as ready and
the promote phase is a no-op.
