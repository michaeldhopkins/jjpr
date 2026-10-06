# Recovering from bad state

Put the local stack right, then run `jjpr submit` and the forge follows
it. Every jj command, jjpr's included, can be undone:

```
jj undo                        # undo the last operation
jj op log                      # list operations, newest first
jj op restore <operation-id>   # put the repo back to that point
```

## A merged PR's commits are still in the PR above it

The PR below was squash- or rebase-merged on the forge, and the PR
above it still carries its commits. Rebase onto trunk, dropping what
already landed:

```
jj log -r 'roots(trunk()..top)'      # the oldest commit above trunk
jj rebase -s <that change> -d main --skip-emptied
jjpr submit
```

## jjpr could not check whether a PR below was merged

If nothing below was merged, carry on. If something was, run the
`jj rebase` the warning prints, then `jjpr submit`. If the error repeats,
run `jjpr auth test`.

## A merged PR sits under a merge commit

Move the merge commit onto trunk, keeping each parent that was not
merged:

```
jj log -r 'trunk()..top'                         # find the merge commit's parents
jj rebase -s <merge commit> -d main -d <unmerged parent>
jjpr submit
```

## Merged commits are still in `jj log` after a restack

They are no longer part of any PR. Remove them with the `jj abandon`
the warning prints, or leave them.

## A bookmark is skipped as stale

The bookmark points at a commit jj cannot place, usually because its
PR was merged on the forge. `jjpr submit` removes it once the forge
confirms the merge. To remove it yourself:

```
jj bookmark forget <name>
```

## A rebase left conflicts

jjpr pushes nothing that has conflicts. Resolve them on top of the
conflicted change and fold the fix in:

```
jj new <change-id>       # the change named in the message
# fix the conflicted files
jj squash
jjpr submit
```

Or undo the rebase with `jj op log` and `jj op restore`.

## Submit or merge stops on a divergent change

Two commits share one change ID. Keep the one you want and abandon the
other:

```
jj log -r 'change_id(<change-id>)'
jj abandon <commit-id of the copy to drop>
jjpr submit
```

To keep both as separate changes, run `jj duplicate <commit-id>`, then
`jj abandon <commit-id>`.

## A push failed

Someone may have pushed to the branch, or the forge may protect it.
Look before pushing again:

```
jj git fetch
jj log -r '<bookmark> | <bookmark>@origin'
```

To keep the forge's version, run
`jj bookmark set <bookmark> -r <bookmark>@origin`. To keep yours, run
`jjpr submit`.

## Local sync failed

After a merge, jjpr could not update the PRs above it on your machine.
The merged PR is fine, and the rest stay open. Either rebase them:

```
jj git fetch
jj rebase -s <change-id> -d main
jjpr submit
```

Or take the forge's version of each bookmark:

```
jj git fetch
jj bookmark set <bookmark> -r <bookmark>@origin
```

Then run `jjpr merge` again. `jjpr watch` picks it up by itself.

## Forge reconcile failed

The merge happened, but a follow-up change on the forge did not (a
base branch or the stack comment). Run `jjpr merge` again. If it keeps
failing, see the table below.

## The forge returned an error

| Status | Usually means | Do this |
|---|---|---|
| 401 | The token is missing, expired or revoked | `jjpr auth test`, then log in again |
| 403 | The token lacks a scope, or the organization requires SSO for it | Give it `repo` (GitHub, Forgejo) or `api` (GitLab), or authorize it for the organization |
| 404 | The token cannot see the repository, or the PR is gone | `jjpr auth test`, and check the remote URL |
| 405, 409 | The forge refused the merge: checks, reviews or a merge method the repository does not allow | Open the PR on the forge to see what it waits for |
| 422 | The forge rejected the change, often because a branch it names is gone | `jj git fetch`, then run the command again |
| 429 | Too many requests | Wait a few minutes, then run again |
| 500–504 | The forge is having trouble | Run again in a minute |

## Watch gave up

`jjpr watch` stops after 10 errors in a row. The lines above that
message show the error. Fix it, then start `jjpr watch` again.
