# submit

`jjpr submit` is the manual control point. It pushes bookmarks and
creates or updates PRs on the forge. Use it when you don't want the
`watch` loop, or when you're debugging.

```
jjpr submit                           # push and update everything in the stack
jjpr submit --reviewer alice,bob      # request reviewers on every PR
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

## Flags

| Flag | Effect |
|---|---|
| `--reviewer <users>` | Comma-separated list of reviewers to request |
| `--draft` | Create new PRs as drafts |
| `--ready` | Mark existing draft PRs as ready |
| `--base <branch>` | Override auto-detected base branch |
| `--remote <name>` | Override the git remote name |
| `--no-fetch` | Skip `git fetch` before starting |
| `--dry-run` | Print what would happen without doing it |

## PR titles and bodies

Title and body come from the first commit's description in each
bookmark's segment.

The body is wrapped in HTML comment markers. When you re-submit after
changing a commit message, only the managed section is updated. Text
you add above or below the markers (screenshots, notes, test plans)
is preserved.

If you remove the markers from the PR body, jjpr stops updating that
PR's description.

The PR title is not automatically updated after creation. If you
change the commit's first line, jjpr warns you about the drift.

## Drafts

`--draft` creates new PRs as drafts. Existing PRs are unaffected.

`--ready` converts every draft PR in the stack to ready-for-review.

The two flags are mutually exclusive.

## Reviewers

`--reviewer alice,bob` requests reviewers on every PR in the stack,
new and existing. Idempotent: a reviewer who's already on a PR isn't
re-requested.

## Stacking on someone else's branch

If a commit in your stack's ancestry has a remote bookmark that isn't
one of your own, jjpr treats it as a foreign base and targets your
first PR at that branch instead of the default branch (e.g., `main`).
The status output reflects this:

```
  auth (1 change, #42 open, synced)
  profile (1 change, needs push)
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
