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

Each PR shows mergeability, CI status, and review state:

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

With `--all`, multiple independent stacks are labeled:

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

## Glossary

| Field | Meaning |
|---|---|
| `synced` / `needs push` | Whether the local bookmark matches the forge |
| `#NN open` / `#NN draft` | PR number and state on the forge |
| `✓ mergeable` | The forge reports the PR can merge without conflicts |
| `✓ CI passing` / `✗ CI pending` / `✗ CI failing` | Aggregate check status for the head commit |
| `✓ N/M approvals` | Approving reviews vs. required approvals |
| `⚠ changes requested` | At least one reviewer has requested changes |
