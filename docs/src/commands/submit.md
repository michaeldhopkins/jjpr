# submit

`jjpr submit` is the manual control point. It pushes bookmarks and
creates or updates PRs on the forge. Use it when you don't want the
`watch` loop, or when you're debugging.

```
jjpr submit                           # push and update everything in the stack
jjpr submit --reviewer alice,bob      # request reviewers on the bottom PR (default scope)
jjpr submit --reviewer alice --reviewer-scope all   # request on every PR
jjpr submit --reviewer alice --reviewer-scope leaf  # request on the topmost PR
jjpr submit --draft                   # create new PRs as drafts
jjpr submit --ready                   # mark existing draft PRs as ready
jjpr submit --base coworker-feat      # override auto-detected base branch
```

## What it does

1. Pushes all bookmarks in the stack to the remote.
2. Creates PRs for bookmarks that don't have one yet.
3. Updates PR base branches to maintain the stack structure.
4. Updates PR bodies when commit descriptions have changed.
5. Adds or updates the stack-awareness comment on multi-PR stacks.
   Single-PR stacks don't get a comment, so they look like a normal PR
   to reviewers.

Submit is idempotent. Run it as often as you want. After rebasing,
editing commit messages, or restacking with `jj rebase`, re-run `jjpr
submit`. It pushes the new commits, fixes PR base branches, and syncs
descriptions. If everything is already up to date, it reports "Stack
is up to date."

When no bookmark is specified, jjpr infers the target from the working
copy's position. It finds which stack overlaps `trunk()..@` and
submits up to the topmost bookmark.

If pushing new commits to an already-approved PR whose base resets
approvals on push (GitHub's "dismiss stale reviews", GitLab's "reset
approvals on push", Forgejo's "dismiss stale approvals"), jjpr reports
the approvals the push dropped so the loss isn't silent:

```
  Pushing 'auth'...
    https://github.com/o/r/pull/42
    ⚠ dismissed 1 approval on #42 — base 'main' resets approvals on push
```

## Flags

| Flag | Effect |
|---|---|
| `--reviewer <users>` | Comma-separated list of reviewers to request |
| `--reviewer-scope <scope>` | Which PRs receive requests: `bottom` (default), `leaf`, or `all` |
| `--draft` | Create new PRs as drafts |
| `--ready` | Mark existing draft PRs as ready |
| `--base <branch>` | Override auto-detected base branch |
| `--remote <name>` | Override the git remote name |
| `--no-fetch` | Skip `git fetch` before starting |
| `--dry-run` | Print what would happen without doing it |

## PR titles and bodies

Title and body come from the first commit's description in each
bookmark's segment. A trailing block of git trailers (`Co-authored-by:`,
`Signed-off-by:`, and similar) is stripped from the body so commit
attribution doesn't show up as the PR description.

The body is wrapped in HTML comment markers. Text you add above or
below the markers (screenshots, notes, test plans) is always preserved.

When you re-submit, jjpr reconciles the managed section between the
markers against the commit message. It records a fingerprint of whatever
it last wrote there, which lets it tell two situations apart:

- You changed the commit message, so the PR is stale. jjpr updates the
  managed section.
- You edited the text between the markers directly on the forge. jjpr
  leaves it alone.

If both the commit message and the on-forge text changed since jjpr last
wrote them, jjpr can't tell which you meant to keep, so it leaves the PR
untouched rather than overwrite your edit and prints a note telling you so.
To make the change take, edit the commit message (the source of truth for
the managed section).

PRs created before fingerprinting get a fingerprint recorded on the next
submit. A pre-fingerprint PR whose managed text was hand-edited away from
its commit message is left untouched, never overwritten.

If you remove the markers from the PR body entirely, jjpr stops updating
that PR's description.

The PR title is not automatically updated after creation. If you
change the commit's first line, jjpr warns you about the drift.

## Drafts

`--draft` creates new PRs as drafts. Existing PRs are unaffected.

`--ready` converts every draft PR in the stack to ready-for-review.

The two flags are mutually exclusive.

## Reviewers

`--reviewer alice,bob` requests reviewers. By default the request lands
only on the **bottom** PR (the one closest to your default branch),
because that's where review attention is needed first in a stack. As
the bottom merges, the next iteration of `submit` (or `watch`) targets
whichever PR is now lowest. `--reviewer-scope leaf` requests on the
topmost PR; `--reviewer-scope all` requests on every PR.

In `0.20.x` and earlier, requests went to every PR by default. If you
relied on that, pass `--reviewer-scope all` explicitly.

Idempotent: a reviewer who's already on a PR isn't re-requested.

## Stacking on someone else's branch

If a commit in your stack's ancestry has a remote bookmark that isn't
one of your own, jjpr treats it as a foreign base and targets your
first PR at that branch instead of the default branch (e.g., `main`).
The status output reflects this:

```
  auth (1 change, PR open, push up to date)
    https://github.com/o/r/pull/42
  profile (1 change, not pushed yet)
    no PR yet — run `jjpr submit`
  (based on coworker-feat)
```

Use `--base <branch>` to override auto-detection. Useful when the
coworker hasn't pushed yet.

## Conflict check

Before pushing, jjpr checks for unresolved conflicts in the stack.
Conflicts halt the operation:

```
Error: cannot push — some commits have unresolved conflicts:

  pnnmmvmu (feat/deferment-roles): add Billings::DueDatePolicy specs

To resolve: jj edit pnnmmvmu, fix the conflicts, then re-run jjpr submit.
```

## Stack-awareness comment

Multi-PR stacks get a comment on every PR linking the others:

```
This PR is part of a stack:

1. [`feat/auth`](https://github.com/o/r/pull/41)
1. **`feat/profile` <-- this PR**
1. [`feat/settings`](https://github.com/o/r/pull/43)
```

The list always reflects the current local stack in base-to-top order.
As you rebase, split, or reorder commits, the order is recomputed every
submit so each PR's comment stays in sync with the others.

When a PR in the stack is merged or closed and its bookmark is no
longer in the local graph, it moves into a collapsible history block
at the bottom of the comment, rendered with strikethrough:

```
1. **`feat/profile` <-- this PR**
1. [`feat/settings`](https://github.com/o/r/pull/43)

<details><summary>2 earlier closed/merged PRs</summary>

1. ~~[`feat/foundation`](https://github.com/o/r/pull/39)~~
1. ~~[`feat/migration`](https://github.com/o/r/pull/40)~~

</details>
```

Up to seven historical entries are shown, sorted most-recent first by
the forge's merge timestamp. Older entries past the cap stay in the
embedded data (so future submits can reconstruct full history) but
aren't displayed:

```
<summary>7 earlier closed/merged PRs (+3 older entries hidden)</summary>
```

The comment also embeds a base64-encoded JSON payload of the stack
state inside an HTML comment marker. jjpr reads this on the next
submit to inherit fossil metadata (PR numbers, merge timestamps) for
PRs whose local bookmarks have been cleaned up. Don't edit it; jjpr
rewrites the whole comment on every submit.

Single-PR stacks don't get a comment.
