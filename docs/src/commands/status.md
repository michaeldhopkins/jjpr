# status

`jjpr status` (and bare `jjpr`) shows the stack containing your
working copy and its PR/MR state. It's read-only. It fetches the
latest state but doesn't push or modify anything.

```
jjpr                                  # current stack (inferred from working copy)
jjpr status                           # same
jjpr status profile                   # scope to the stack containing 'profile'
jjpr status --all                     # show every local stack
```

The default scope matches `submit`, `merge`, and `watch`: the stack
inferred from the working copy. Pass a bookmark to scope to a specific
stack, or `--all` to see every local stack at once.

## Flags

| Flag | Effect |
|---|---|
| `--all` | Show every local stack instead of only the current one. Mutually exclusive with a positional bookmark. |
| `--no-fetch` | Skip `git fetch` before reporting |

## Output

Each segment shows its bookmark, a direct link to the PR, and the PR's
mergeability, CI status, and review state:

```
  auth (1 change, PR open, push up to date)
    https://github.com/o/r/pull/42
    ✓ mergeable  ✓ CI passing  ✓ 1 approval
  profile (2 changes, PR open, push needs updating)
    https://github.com/o/r/pull/43
    ✗ conflicts  ✗ CI failing  ⚠ changes requested  ✗ 0 approvals
```

Draft PRs show just the link. Their CI and review detail stays hidden
until the PR is marked ready:

```
  payments (1 change, PR draft, push up to date)
    https://github.com/o/r/pull/44
```

A segment you haven't submitted yet has no PR:

```
  cleanup (2 changes, not pushed yet)
    no PR yet — run `jjpr submit`
```

With `--all`, multiple independent stacks are labeled:

```
Stack 1:
  auth (1 change, PR open, push up to date)
    https://github.com/o/r/pull/42
    ✓ mergeable  ✓ CI passing  ✓ 1 approval
  profile (2 changes, PR open, push up to date)
    https://github.com/o/r/pull/43
    ✓ mergeable  ✓ CI passing  ✓ 1 approval

Stack 2:
  payments (1 change, PR draft, push up to date)
    https://github.com/o/r/pull/44
  checkout (3 changes, PR open, push needs updating)
    https://github.com/o/r/pull/45
    ✗ CI pending  ✗ 0 approvals
```

## Glossary

| Field | Meaning |
|---|---|
| `push up to date` / `push needs updating` / `not pushed yet` | Whether your local commits are reflected on the pushed PR branch: matching, pushed but since changed, or never pushed |
| PR link (`https://.../pull/42`) | Direct link to the PR or MR on the forge |
| `PR open` / `PR draft` | The PR's state on the forge |
| `no PR yet` | This segment has not been submitted; run `jjpr submit` |
| `✓ mergeable` / `✗ conflicts` | Whether the forge reports the PR can merge without conflicts |
| `✓ CI passing` / `✗ CI pending` / `✗ CI failing` | Aggregate check status for the head commit |
| `✓ N approvals` / `✗ 0 approvals` | Count of approving reviews (the required threshold comes from config) |
| `⚠ changes requested` | At least one reviewer has requested changes |
