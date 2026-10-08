# Undo and redo: open design questions

Two questions the owner raised on 2026-10-08, after undo and redo became all
or nothing. He approved both recommendations the same day, and both are
built (`src/undo/step_back.rs`, the redo checks in `planner.rs`). Each section ends with a recommendation.
Facts marked *measured* were run against real forges (`tests/undo_e2e.rs`,
2026-10-06) or a real jj (0.45.1, 2026-10-08); the rest is each forge's
documented behaviour.

## 1. Redo after a forced undo

`jjpr undo --force` closes the PRs a command opened and deletes their
branches, and restores forge items someone else changed. Between that undo
and a redo, other people can act. What can redo put back, what can it not,
and when does it refuse?

### What can happen in between

| Someone, after the forced undo | What redo meets |
|---|---|
| Commented on a closed PR | Nothing to restore: the comments stay, and the reopened PR shows them |
| Edited the description or a stack comment | The text differs from what undo left |
| Retargeted a PR | The base differs |
| Reopened a PR undo closed | Already open: redo skips the reopen |
| Pushed to a branch undo force-pushed back (a resubmit's PR stays open) | The branch is at their commit, not jjpr's old one |
| Recreated a branch undo deleted, at some other commit | Same: the branch is not where undo left it |
| Approved (or requested changes) before the undo | See approvals below |
| Deleted the base branch of a closed PR | Reopening may be refused |
| Merged a PR (one the undo left open) | The PR cannot be put back as it was |

### Per forge

| | GitHub | GitLab | Forgejo / Gitea |
|---|---|---|---|
| Reopen, branch untouched since close | yes (*measured*) | yes (*measured*) | yes (*measured*) |
| Reopen after branch deleted and pushed again **at the same commit** | yes (*measured*) | yes (*measured*) | yes (*measured*) |
| Reopen after the branch was **force-pushed or recreated at another commit** while closed | **no**, 422 (*measured*) | yes (*measured*) | yes (*measured*) |
| Reopen with the head branch missing (not measured) | no | no | no |
| Reopen with the base branch deleted (not measured) | no (GitHub retargets open PRs on delete, never closed ones) | no | no |
| Approvals survive close and reopen | yes | yes | yes |
| A push dismisses approvals | when "dismiss stale reviews" is on | when "reset approvals on push" is on | when "dismiss stale approvals" is on |
| Can jjpr re-create someone's approval | no | no | no |

Redo pushes each branch back to the exact commit the PR had when undo
closed it, and only then reopens, so the "same commit" row applies and all
three forges reopen. The GitHub rule bites only if someone moved the branch
in between, and then redo is already refusing (below).

### Coworker commits

The hard case. Undo of a resubmit force-pushes the branch back to the old
commit while the PR stays open; a coworker pushes a fix on top of it; now
`jjpr redo` would force-push jjpr's newer commit over theirs.

Redo cannot both restore jjpr's commit and keep theirs: the two are
different histories, and combining them is a rebase or merge with its own
conflicts. That is a decision about code, which jj makes well and jjpr
should not make silently. Overwriting would destroy their work, which undo
already refuses to do even with `--force`.

What redo can do is make the way forward short. Their commits are on the
remote; `jj git fetch` brings them in as `<branch>@origin`, and the user
rebases one side onto the other and runs `jjpr submit`.

### What redo cannot restore at all

Nothing jjpr does brings these back, so they must never block a redo. They
belong in its output, once, after the steps:

- **Approvals a push dismissed.** jjpr already knows when a push will
  dismiss approvals (`approvals_dismissed_by_push`, used by submit's
  warning). Redo's force-push can dismiss approvals given on the undone
  state, too.
- **Notifications and timeline events.** Closing and reopening shows in the
  PR's history; re-requesting review notifies the reviewer again.
- **CI runs** against the commits undo put back.

### Recommendation

1. **A branch someone moved blocks redo, even with `--force`**, as it does
   undo. The message names the branch and gives the way forward in one
   line: fetch, rebase onto theirs, `jjpr submit`. Redo never preserves or
   merges coworker commits itself.
2. **Text someone edited (a description, a stack comment, a base) needs
   `--force`**, which writes jjpr's version back with a warning naming
   each, as undo does today.
3. **Before reopening, redo checks what each forge needs**: the head branch
   back at the closing commit (redo's own push does this), and the base
   branch present. A missing base branch blocks; it is the one reopen
   failure redo cannot fix. Do not open a replacement PR in its place: a new
   number loses the review history and is not a redo.
4. **Redo's output ends with what it could not restore**, without blocking:
   approvals its pushes dismissed (counted with the existing helper),
   comments others left while the PRs were closed (so nobody misses them),
   and review requests sent again.
5. **No new state is needed.** All of this fits the blocker model: (1) and
   the base check are blockers `--force` cannot clear, (2) are the ones it
   can, and (4) is an informational list like the review-request note.

## 2. The `jj describe` refusal

After `jjpr submit`, the user runs `jj describe` (or amends, or edits
files). `jjpr undo` refuses: undoing the submit restores the repo to before
it, and that would discard the describe. The owner asked whether, instead,
undo should point at `jj undo` then `jjpr undo`, or behave like an alias so
that `jjpr undo` twice works.

### Option A: tell the user to run `jj undo`, then `jjpr undo`

No code beyond the message. Pitfalls, measured on jj 0.45.1:

- **`jj undo` snapshots the working copy first, and then undoes that
  snapshot.** With an unsaved edit on disk, the first `jj undo` undoes the
  snapshot, not the describe, and the file **disappears from disk**
  ("removed 1 files"). The describe is still there. The edit survives only
  in the operation log.
- **The count is not one.** A describe plus edits is two operations, a
  rebase can be several, and the user cannot see the count without
  `jj op log`. Repeated `jj undo` walks further back on jj 0.45.1; jjpr
  supports jj from 0.36, where the semantics need checking (jj's own
  changelog dates multi-step undo earlier, but our jj-versions matrix has
  not measured it).
- **Too far is worse than too little.** One `jj undo` more than needed
  undoes jjpr's own last operation, and `jjpr undo` then refuses for the
  opposite reason, with the repo now matching neither end of the entry.
- **The operation log is shared by every workspace.** `jj undo` takes back
  the newest operation in the repo, which may be another workspace's.
- It reintroduces `jj undo` as the recovery, which the owner rejected for
  the recovering page.

### Option B: `jjpr undo` steps back over the operations since

The first `jjpr undo` sees that the only obstacle is local jj work after
the command, and takes back just that: it restores the repo (local part
only, `--what repo`) to the command's own end operation and records this as
an undo entry of its own. It says what it took back and that `jjpr redo`
puts it back. The second `jjpr undo` finds the repo as the command left it
and undoes the command.

```
$ jjpr undo
Undid 1 jj operation since `jjpr submit` from 14:02:
  describe commit 5d6e7f80
To put it back: jjpr redo. To undo the submit: jjpr undo
$ jjpr undo
Undoing `jjpr submit` from 14:02:
  ...
```

Why it is safer than A: jjpr computes the exact operation to return to, so
there is no count to get wrong and no overshoot; it snapshots first and
counts the snapshot among the operations it takes back, so it can say that
file edits leave the disk instead of letting them vanish; and redo brings
everything back, edits included, since it restores a later operation that
still holds them.

Pitfalls, and what each needs:

- **Other workspaces.** An operation since that came from another workspace
  (or changed another workspace's working-copy commit) would be undone
  there too. Block, and name the workspace.
- **Fetches.** A `jj git fetch` since moves remote-tracking refs, which
  `--what repo` leaves alone, but it can also move local bookmarks that
  followed their remote. Taking that back is surprising and is not the
  user's own work. Block, with the existing advice (change the stack with
  jj, then `jjpr submit`).
- **Pushes by hand.** A `jj git push` since changed the forge. Restoring
  the local repo alone would leave the forge ahead of it. Block.
- **Edits on disk.** The snapshot jjpr takes counts as an operation since.
  Taking it back removes the edits from disk (redo restores them). This is
  exactly the case that loses work silently under option A, so it should
  need `--force` and say which files leave the disk.
- **Concurrent operations.** jj merges divergent operation heads into one;
  the operations since are then a graph, not a list. jjpr already reads
  them with `ops_since`; the rule is that every one of them must pass the
  checks above.
- **Colocated git.** A raw `git commit` or `git checkout` shows up as an
  "import git refs" operation. It is local work, but it is the user's git
  state; restoring it is fine for jj, and jj exports the restored refs back
  to git. Treat it like any other local operation.
- **A running `jjpr watch`** would redo what was undone. Already blocked.
- **Redo after the second undo.** Redo takes back the command's undo first,
  then the jj step, oldest last, as the journal already orders them.

### Recommendation

**Option B, with the limits above.** `jjpr undo` twice is what the owner
sketched, it never asks the user to count operations, and it keeps
`jj undo` out of the advice. Concretely:

1. When the only blockers are that the repo changed since, and every
   operation since is local, in this workspace, and neither fetched nor
   pushed, `jjpr undo` takes back those operations as an entry of its own
   and says what it took back, how to put it back (`jjpr redo`), and that
   the next `jjpr undo` undoes the command.
2. If those operations include uncommitted edits on disk, it needs
   `--force`, and the message lists the files that would leave the disk
   and says `jjpr redo` brings them back.
3. Any other operation since (another workspace, a fetch, a push) keeps
   today's refusal: "Use jj to get the stack into the state you want, then
   run `jjpr submit`."
4. The refusal never suggests `jj undo`.

Building it is a new entry kind in the journal (local only, no forge
records), a classifier over the operations since (workspace, kind), and the
two messages; the planner, executor and rollback are reused unchanged. It
needs captured `jj op log` output for each supported jj version, since the
classifier parses operation descriptions.
