# undo and redo

`jjpr undo` takes back the last `submit`, `merge` or `watch` command:
the local repo and what it changed on the forge. `jjpr redo` puts it back.

```
jjpr undo                  # take back the last jjpr command
jjpr undo --dry-run        # say what it would do, change nothing
jjpr undo --force          # also close PRs it opened
jjpr undo --list           # the recorded commands, newest first
jjpr redo                  # put back what undo took back
```

## What it undoes

| The command did | Undo does |
|---|---|
| Rebased, abandoned or moved bookmarks locally | Restores the repo to the operation before the command |
| Pushed a branch | Force-pushes the commit the forge had before |
| Pushed a new branch | Deletes it. If it opened a PR on the branch, only with `--force`, after closing the PR |
| Opened a PR | Closes it, with `--force` |
| Retargeted a PR | Retargets it back |
| Wrote a stack comment or a PR description | Puts the earlier text back |
| Marked a draft ready | Makes it a draft again |
| Requested reviewers | Withdraws the requests |

A PR that a push closed is reopened; one someone closed stays closed. One `jjpr undo` takes back the whole
command, however many jj operations it made. A restack is a rebase, an
abandon and a push, and one undo takes back all three.

## What it does not undo

- **A merge.** Undo refuses a command that merged a PR. To back out a
  merged change, revert it on the forge.
- **Dismissed approvals.** A force-push can dismiss approvals, and putting the
  old commit back does not restore them.

## Closing PRs needs `--force`

A PR is public, so undo leaves the PRs a command opened, and their
branches, until you ask:

```
$ jjpr undo
Undoing `jjpr submit` from 14:02:
  Restore the local repo to operation 4f2a9c1e0b7d, from before the submit
  Delete the stack comment on #44
  Delete the stack comment on #43
Not undone:
  #44 and its branch 'profile' stay open: closing a PR needs --force
  #43 and its branch 'auth' stay open: closing a PR needs --force
To close them too: jjpr undo --force
Undid `jjpr submit` from 14:02. To put it back: jjpr redo
```

## When undo refuses

Undo changes nothing when it would destroy work. It refuses when:

- **The repo changed since the command.** A commit was amended, or the
  working copy has edits. Undoing would discard them. The message lists the jj
  operations since. To keep that work, change the stack with jj until it is
  what you want, then run `jjpr submit`.
- **Someone pushed to a branch since.** Undo never overwrites commits jjpr
  did not push, even with `--force`.
- **Something jjpr wrote was changed on the forge.** A comment was edited, or
  a PR was retargeted. `--force` restores it anyway and names each item in a
  warning.
- **`jjpr watch` is running in the repo**, in any of its workspaces. It
  would redo the work on its next poll. Stop it first.
- **Another jjpr command is changing the repo.** Wait for it to finish.

## How far back

jjpr doesn't cap undo history. You can undo back until the newest change
that jjpr can't handle.

`jjpr redo` puts back what undo took back, oldest first. Running any jjpr
command that changes something ends redo.

The record lives in the repo's `.jj/repo/jjpr/undo/`, shared by all
workspaces of the repo.

## Flags

| Flag | Effect |
|---|---|
| `--force` | Close PRs the command opened and delete their branches; restore forge items someone changed since, with a warning for each |
| `--list` | (`undo` only) List the recorded commands instead |
| `--dry-run` | Print what would be done, and change nothing on the forge or in the record. Like any jj command, it takes in working-copy edits first |
