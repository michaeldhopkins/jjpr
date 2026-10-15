# status

`jjpr status` (and bare `jjpr`) shows the stack containing your
working copy and its PR/MR state, down to your default branch —
including a coworker's branch you've stacked on. It's read-only. It
fetches the latest state but doesn't push or modify anything.

```
jjpr                                  # current stack (inferred from working copy)
jjpr status                           # same
jjpr st                               # same, using the short alias
jjpr status profile                   # scope to the stack containing 'profile'
jjpr status --all                     # show every local stack
```

`st` is a short alias for `status`, matching jj's own `jj st`.

The default scope matches `submit`, `merge`, and `watch`: the stack
inferred from the working copy. Pass a bookmark to scope to a specific
stack, or `--all` to see every local stack at once.

## Flags

| Flag | Effect |
|---|---|
| `--all` | Show every local stack instead of only the current one. Mutually exclusive with a positional bookmark. |
| `--no-fetch` | Skip `git fetch` before reporting |
| `--verbose` | Print each jj call and forge request to stderr, with how long it took |

## Output

Each segment shows its bookmark, a direct link to the PR, and the PR's
mergeability, CI status, and review state:

```
  auth (1 change, PR open, push up to date)
    https://github.com/o/r/pull/42
    ✓ mergeable  ✓ CI passing  ✓ 1/1 approvals
    next: ready to merge (jjpr merge, or jjpr watch will merge it)
  profile (2 changes, PR open, push needs updating)
    https://github.com/o/r/pull/43
    ✗ conflicts  ✗ CI failing  ⚠ changes requested  ✗ 0/1 approvals
    next: address the requested changes, push, and ask for re-review
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
    ✓ mergeable  ✓ CI passing  ✓ 1/1 approvals
    next: ready to merge (jjpr merge, or jjpr watch will merge it)
  profile (2 changes, PR open, push up to date)
    https://github.com/o/r/pull/43
    ✓ mergeable  ✓ CI passing  ✓ 1/1 approvals
    next: ready to merge (jjpr merge, or jjpr watch will merge it)

Stack 2:
  payments (1 change, PR draft, push up to date)
    https://github.com/o/r/pull/44
  checkout (3 changes, PR open, push needs updating)
    https://github.com/o/r/pull/45
    ✗ CI pending  ✗ 0/1 approvals
    next: one more review required
```

## Approvals at risk from a squash landing

When the base branch resets approvals on push (GitHub's "dismiss stale
reviews", GitLab's "reset approvals on push", Forgejo's "dismiss stale
approvals") and one of your approved PRs is stacked on top of another
open PR, a **squash** landing of the lower PR force-pushes a fresh commit
onto yours and drops its approvals. `status` warns ahead of time:

```
  api (2 changes, PR open, push up to date)
    https://github.com/o/r/pull/124
    ✓ mergeable  ✓ CI passing  ✓ 2/2 approvals
    next: ready to merge (jjpr merge, or jjpr watch will merge it)
    ⚠ a squash-landing of #123 would dismiss 2 approvals
```

The note is conditional on how the lower PR lands: a **merge-commit**
landing keeps your approval, because jjpr skips the needless rebase in
that case (see [merge](merge.md)).

## PRs in a GitHub native stack

A PR registered as a member of a GitHub native stack cannot be merged by jjpr,
because GitHub refuses the merge endpoint for stacked PRs. Status flags those,
so the mergeability line above it is not read as an invitation to run
`jjpr merge`:

```
  auth (1 change, PR open, push up to date)
    https://github.com/owner/repo/pull/345
    ✓ mergeable  ✗ 0/1 approvals
    next: one more review required
    ⚠ in native stack #348 (1 of 3), so jjpr cannot merge it; `gh stack merge 345` lands it
```

The position is 1-based from the bottom of the stack, and the note says what
the command would land, because merging a stacked PR lands every PR below it
too. At position 1 that is one PR; at position 3 the note reads
`lands it and the 2 below`.

This reads the stack membership GitHub embeds in the PR data jjpr already
fetches, so it costs no extra request. See
[GitHub native stacks](merge.md#github-native-stacks) for the ways to land such
a stack, and [Reshaping a native stack](submit.md#reshaping-a-native-stack) for
what submit can and cannot do with one.

## Divergent changes

When two commits carry the same change ID — a *divergent* change, which jj marks
`??` — the segment carrying it is flagged:

```
  feat-a (1 change, #41 open)
  feat-b (1 change, #42 open)  ?? divergent
```

Status always shows the stack, even when it is in this state; it is where you
look to find out which segment to fix. `jjpr submit`, `jjpr watch` and
`jjpr merge` all refuse while a divergent change is in the stack — see
[submit](submit.md#divergent-changes).

Resolve with `jj abandon <commit>` to drop one copy, or `jj duplicate` to give it
its own change ID.

## Branches that aren't yours

`status` shows the whole stack down to your default branch, including a
coworker's branch you've stacked on. Those segments are attributed to
their author, show the base's mergeability, and are marked so you know
`submit`, `watch`, and `merge` leave them alone (only `status` shows
them; the mutating commands act only on your own bookmarks):

```
  auth-ui (1 change, PR draft, push up to date)
    https://github.com/o/r/pull/205
  auth-api (2 changes, PR open, push needs updating)
    https://github.com/o/r/pull/204
    ✓ mergeable  ✓ CI passing  ✓ 1/1 approvals
    next: ready to merge (jjpr merge, or jjpr watch will merge it)
  platform-refactor (3 changes, PR open by @dana, jjpr won't submit or merge it)
    https://github.com/o/r/pull/198
    ✓ mergeable
```

The rich segment above appears when you have a local bookmark at the
coworker's commit. If instead you rebased straight onto their remote
branch (`jj rebase -d their-branch@origin`, no local bookmark), it shows
up as an attributed base footer:

```
  my-feature (2 changes, PR open, push up to date)
    https://github.com/o/r/pull/210
  (based on their-branch — PR open by @dana)
    https://github.com/o/r/pull/198
```

When a branch has already merged, its segment says so and — if the
remote branch is gone but the local bookmark lingers — points you at the
cleanup:

```
  cycle-events (1 change, PR merged by @dana)
    https://github.com/o/r/pull/512
    ✓ merged on 2026-04-20; remote branch deleted, local bookmark is stale
       clean up: jj bookmark forget cycle-events
```

If the stack you're on is *entirely* someone else's — you're sitting on
their branch with nothing of your own yet — `status` recognizes that
instead of pretending there's a stack to act on:

```
On cycle-events — someone else's merged branch:

  cycle-events (1 change, PR merged by @dana)
    https://github.com/o/r/pull/512
    ✓ merged on 2026-04-20; remote branch deleted, local bookmark is stale
       clean up: jj bookmark forget cycle-events

Nothing of yours to submit here.
```

## Glossary

| Field | Meaning |
|---|---|
| `push up to date` / `push needs updating` / `not pushed yet` | Whether your local commits are reflected on the pushed PR branch: matching, pushed but since changed, or never pushed |
| PR link (`https://.../pull/42`) | Direct link to the PR or MR on the forge |
| `PR open` / `PR draft` | The PR's state on the forge |
| `no PR yet` | This segment has not been submitted; run `jjpr submit` |
| `PR open by @user` / `PR merged by @user` | Someone else's PR — the author is named. Only `status` shows these; the mutating commands ignore them |
| `jjpr won't submit or merge it` | This segment is someone else's; `submit`/`watch`/`merge` leave it alone |
| `✓ merged … local bookmark is stale` | The PR merged and its remote branch is gone, but a local bookmark remains; the line below shows how to remove it |
| `✓ mergeable` / `✗ conflicts` | Whether the forge reports the PR can merge without conflicts |
| `✓ CI passing` / `✗ CI pending` / `✗ CI failing` | Aggregate check status for the head commit |
| `✓ 1/1 approvals` / `✗ 0/1 approvals` | Approving reviews against the number required (`required_approvals` in config) |
| `next: …` | The one thing left to do: how many more reviews, to address requested changes, or that the PR is ready to merge. Omitted when approvals are met but something `jjpr merge` would refuse on remains: conflicts, unknown mergeability, CI that the config requires and is not passing, a native stack or a divergent change |
| `⚠ changes requested` | At least one reviewer has requested changes |
| `⚠ a squash-landing of #N would dismiss …` | A lower PR's squash landing would force-push this approved PR and drop its approvals |
| `⚠ in native stack #N …` | This PR belongs to a GitHub native stack, which jjpr cannot merge; the note names the command that can and what it would land |
