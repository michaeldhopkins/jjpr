# `jjpr undo` and `jjpr redo`

Internal notes: why undo works the way it does, what was measured, and what
each forge allows. User docs are `docs/src/commands/undo.md`.

## Why not `jj undo`

`jj undo` steps back one jj operation. A jjpr command is many: `submit` makes
one per bookmark pushed, and a restack is at least a rebase, an abandon and a
push. And no jj command touches the forge: branches jjpr force-pushed, bases
it retargeted, comments it rewrote and PRs it opened stay as they are. So
`jjpr undo` takes back the newest jjpr command whole, locally and on the
forge, and refuses when someone else has acted since.

## Decisions (owner, 2026-10-06)

1. An entry that landed a merge is never undone, and ends the history
   before it.
2. Undo relies on `jj op restore --what repo`, marked experimental in jj, and
   the `jj-versions` CI matrix guards it.
3. Closing a PR the command opened (and deleting its branch) needs
   `--force`. Without it, undo changes nothing and says to rerun with it.
4. History is kept back to the newest thing that cannot be undone, and pruned
   behind it when one is detected: a merge, a jj operation that no longer
   exists, or a change to the repo between two recorded commands (an amend,
   working-copy edits, or a fetch that brought commits: undoing the second
   cannot bring back what the first left).
5. A forge item someone changed since jjpr wrote it (a comment, a body, a
   base) refuses the undo; `--force` restores it with a warning naming it.
   A created PR with comments or reviews from others is named the same way
   when `--force` closes it. A branch someone pushed to is never overwritten,
   with or without `--force`.
6. No confirmation prompt; `--dry-run` shows the plan.
7. `jjpr redo` replays what undo took back, from the same journal.
8. **No partial undo or redo** (2026-10-08). A real run checks the repo and
   the forge first and starts only when it can take back the whole command;
   otherwise it lists every blocker and changes nothing. A dry run lists the
   steps that would go through and every blocker. Blockers are either
   cleared by `--force` (someone changed what jjpr wrote, closing a PR it
   opened, a base branch the forge no longer has) or not (a merge, a branch
   someone pushed to, a PR GitHub will not reopen, a write jjpr could not
   read first, the repo changed since). The design analysis for redo after a
   forced undo and for the `jj describe` recovery is `notes/undo-redo-design.md`.

## jj behaviour it rests on

Measured on jj 0.36.0, 0.40.0 and 0.45.1 against a bare remote, the same on
all three; the `tests/undo.rs` suite also passes on 0.37.0 and 0.38.0.

| Measured | Result |
|---|---|
| `op restore --what repo <start>`, then push | Local bookmarks go back, `@origin` stays at what jjpr pushed, and the push puts the old commits back on the remote |
| Someone else pushed in between | The push is refused: "unexpectedly moved on the remote (stale info)". jj's lease is the race guard |
| Plain `op restore` (repo and remote-tracking), then push | "Nothing changed": the remote keeps jjpr's commits. This is why `jj undo` can never reach the forge |
| Uncommitted edits, then restore | jj snapshots first, then checks out the old `@`: the edit is gone from disk |
| Pushing several bookmarks, one stale | Not atomic: the others are pushed. So undo pushes one bookmark at a time |
| `jj bookmark set` to a hidden commit | Makes it visible again, as a divergent change. So each push moves the bookmark, pushes, and restores (`--what repo`) the operation before the move |
| `jj git push --bookmark x` with `x` deleted locally | Deletes the remote branch |

## How it works

- `submit`, `merge` and `watch` wrap their `Jj` and `Forge` in
  `RecordingJj` / `RecordingForge` (`main.rs`, `resolve_stack`). The start
  operation is taken after submit's snapshot and fetch, so undo never rolls
  those back. Each push and forge write is written to the journal before it
  is attempted and confirmed after; a refused write is removed. The value a
  write replaced comes from reads jjpr already makes (the open PR list, each
  PR's comments); only a write to a PR jjpr has not read costs one extra
  read. A write whose prior value still cannot be learned is noted as
  `missed`, and undo says it leaves it.
- Every jj command of jjpr's that can change the repo runs through
  `Recorder::around`, which notes the operations it made. The entry ends at
  jjpr's own last operation, not at the repo's head, so a `jj` command run
  in watch's sleep is never folded into a watch entry. Operations someone
  else made between two of jjpr's commands, and working-copy snapshots taken
  during one, are noted as absorbed, and undo refuses the entry: undoing it
  would discard them.
- `watch` starts a new entry at the top of each poll (`Jj::checkpoint`). A
  poll that changes nothing, or only fetches, leaves no entry.
- At the end, the entry records a fingerprint of the repo at its end
  operation: every local bookmark's target, and every head a remote branch
  does not account for.
- Undo snapshots the working copy and requires the fingerprint to match what
  the entry (or the last undo of it) left. It reads every branch, PR and
  comment the entry touched from the forge, plans the reverse (`plan.rs`,
  pure), and runs it: restore the local repo, push each branch back, reopen
  a PR the push closed (submit reads each PR's state right after pushing,
  and the recorder notes a PR it had read open that is now closed), reverse
  the forge writes newest first, then close created PRs (`--force`) before
  deleting their branches. Each step carries the value it replaces, and
  each step taken yields its inverse (`rollback.rs`). When a step fails,
  the executor runs the inverses newest first and then restores the local
  repo to the operation it started from, so the run ends where it began; it
  then reads the forge again and names anything still not as it was (the
  failed write may have landed after all). A push that reached the remote
  but whose local clean-up failed counts as taken. Every step updates the
  journal, so a run that cannot even put itself back leaves the entry
  `PartlyUndone` with exactly the steps that stand marked; `jjpr undo` then
  finishes it and `jjpr redo` takes it back, each planning from the forge
  as it is. A comment posted again under a new
  id is renamed in every entry that names it, and within one run the plan
  follows each comment through the entry's records (one record can post a
  comment back that the next one edits or deletes).
- An entry left `Running` by a process that died (Ctrl-C) is completed at
  undo time from what it saved, including its last own operation.
- Redo is the same plan run forwards, from the end operation.
- Undo and redo take a lock in the journal directory (a dead holder's lock is
  moved aside under a unique name and checked again, never deleted blind). A
  `jjpr watch` whose process is alive blocks them, in any workspace: each
  watcher leaves a `watch-<pid>` marker in the shared store. So does any
  other jjpr command
  whose entry is still running in a live process. A dry run takes no lock
  and no working-copy snapshot, and says it did not check edits on disk.
- The journal is one JSON file per entry in `.jj/repo/jjpr/undo/`, shared by
  every workspace of the repo, as the operation log is.

## Per forge

Settled by `tests/undo_e2e.rs` on 2026-10-06, on all three sandboxes.

| | GitHub | GitLab | Forgejo (Codeberg) |
|---|---|---|---|
| Undo a resubmit, redo it | passes | passes | passes |
| Close created PRs (`--force`), delete branches, redo reopens them | passes | passes | passes |
| Undo a restack after an out-of-band squash | passes | passes | passes |
| Reopen with the branch untouched | yes | yes | yes |
| Reopen after the branch was deleted and pushed again at the same commit | yes | yes | yes |
| Reopen after the branch was force-pushed while closed | **no**: 422 "branch was force-pushed or recreated" | yes | yes |
| Draft | GraphQL `convertPullRequestToDraft` | `Draft:` title prefix | `WIP:` title prefix |
| Ready | GraphQL `markPullRequestReadyForReview` | drop the title prefix | drop the title prefix |
| Withdraw review requests | `DELETE …/requested_reviewers` | `PUT reviewer_ids` with the rest | `DELETE …/requested_reviewers` |
| Branch head | `git/ref/heads/<b>` | `repository/branches/<b>` | `branches/<b>` |
| Comment bodies | as written | the final newline dropped | as written |

Two findings outside undo, both fixed here because redo needs them:

- **`mark_pr_ready` did nothing useful on GitLab or Forgejo.** It sent
  `{"draft": false}`. GitLab answers 400 ("at least one parameter must be
  provided"), so `submit --ready` and watch's promotion failed there; Forgejo
  ignores the field. Both now drop the title's draft marker.
- GitLab drops a comment's final newline, so undo compares text ignoring
  trailing whitespace and line endings.

Because of the GitHub reopen rule, undoing a push that GitHub auto-closed a
PR over cannot reopen it, so on GitHub undo refuses such an entry whole.

## Not covered

- Approvals a force-push dismissed are not restored, and notifications are
  not unsent.
- jjpr's `create_pr` on GitLab sends `draft: true`, which GitLab does not
  read either; a `--draft` MR is probably opened ready. Not changed here.
- A user's `jj` command that lands *inside* one of jjpr's own jj commands
  (between its first and last operation) is not told apart from jjpr's,
  unless it is a snapshot or a reconcile. Between commands it is caught.
- A fetch that brings new commits between two jjpr commands ends the history
  before them, as an amend does. Telling a fetched-only change apart would
  need the fingerprint to ignore bookmarks that only followed their remote.
- If jjpr dies after the forge took a write but before the journal recorded
  it (posting a comment again, say), a resumed undo can repeat that write.
  The same gap applies to a write that timed out but landed: put-back does
  not reverse it, and the check afterwards names it.
- A Ctrl-C or crash mid-run gets no put-back: the entry is left
  `PartlyUndone` as the journal last saw it, and the next undo or redo plans
  from there.
- Only undo and redo take the journal lock, so the guard against a
  concurrent command is best-effort: undo sees a running submit or watch
  only once it has written its entry (at its first push or forge write). A
  submit still planning, or started during an undo, is not stopped; the
  fingerprint and push checks of the next undo catch what it changed.
