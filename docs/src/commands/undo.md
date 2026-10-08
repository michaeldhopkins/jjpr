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

## All or nothing

Undo takes back the whole command or changes nothing. It checks the repo
and the forge before it starts, and when anything would stop part of the
undo it lists everything that would, and changes nothing. `--dry-run` shows
the steps and what blocks them.

If a step fails once it has started (the forge stops answering, say), undo
puts back the steps it already took. If it cannot put them back either, it
says the command is partly undone: run `jjpr undo` again to finish, or
`jjpr redo` to put back what it did.

Redo works the same way.

## Closing PRs needs `--force`

A PR is public, so undo closes the PRs a command opened, and deletes their
branches, only when you ask:

```
$ jjpr undo
Error: jjpr can't undo all of `jjpr submit` from 14:02 without --force, so it changed nothing:
  - #44, which the submit opened, would be closed
  - #43, which the submit opened, would be closed
Nothing else stands in the way. Rerun with --force to undo all of it: jjpr undo --force
```

## When undo refuses

Undo changes nothing when it would destroy work, or when it can't take back
all of the command. It refuses when:

- **The repo changed since the command, and not only here.** A fetch, a
  push, or another workspace's work since would be lost. The message lists the
  jj operations since. To keep that work, change the stack with jj until it is
  what you want, then run `jjpr submit`.
- **Someone pushed to a branch since.** Undo never overwrites commits jjpr
  did not push, even with `--force`.
- **GitHub would have to reopen a PR the command's push closed.** GitHub
  won't reopen a PR whose branch moved while it was closed.
- **Something jjpr wrote was changed on the forge.** A comment was edited, or
  a PR was retargeted. `--force` restores it anyway and names each item in a
  warning.
- **`jjpr watch` is running in the repo**, in any of its workspaces. It
  would redo the work on its next poll. Stop it first.
- **Another jjpr command is changing the repo.** Wait for it to finish.

## jj work since the command

If you amended or reworded a commit after the command, the first `jjpr undo`
takes back only that jj work, and the second takes back the command.
`jjpr redo` puts back each in turn.

```
$ jj describe profile -m "Add profile page and avatar"
$ jjpr undo
Undid 1 jj operation since `jjpr submit` from 14:02:
  0711f01f23f4 describe commit 613b9b054cb0
The submit is not undone yet. Run jjpr undo again to take it back.
To put the jj work back instead: jjpr redo
```

Edits on disk that you haven't committed leave the disk too, so they need
`--force`; `jjpr redo` brings them back.

## Redo

`jjpr redo` puts back what undo took back, and checks first in the same
way. Someone else's commits on a branch block it, even with `--force`, and
the message says how to keep them. It reopens the PRs undo closed once their
branches are back, but not onto a base branch the forge has since deleted.
It ends by naming what it can't restore: approvals its pushes dismiss,
comments left while a PR was closed, and review requests it sends again.

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
