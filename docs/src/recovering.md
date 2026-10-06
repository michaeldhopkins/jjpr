# Recovering from bad state

jjpr changes two places: your local repo and the forge. Every local
change is a jj operation, so it can be undone. The forge follows your
local stack the next time you submit. So recovery is nearly always:
put the local stack right, then run `jjpr submit`.

```
jj undo                       # undo the last operation, jjpr's included
jj op log                     # every operation, newest first
jj op restore <operation-id>  # put the whole repo back to that point
jjpr submit                   # make the forge match the local stack
```

## A merged PR's commits are still in the PR above it

The PR below was squash- or rebase-merged on the forge, and the PR
above it was pushed still sitting on the old commits. Submit normally
catches this and says `Rebasing 'top' onto main ('bottom' below it was
merged)`. It misses a merge older than the forge's newest 100 merged PRs
(50 on Forgejo), and one the forge rewrote before merging (GitLab's
"Rebase" button).

```
jj log -r 'roots(trunk()..top)'                # the oldest commit above trunk
jj rebase -s <that change> -d main --skip-emptied
jjpr submit
```

`--skip-emptied` drops the commits whose content is already on main.

## jjpr could not check whether a PR below was merged

```
  Warning: could not check whether a PR below 'top' was merged: <error>
```

The forge did not answer, so jjpr pushed without restacking. If nothing
below was merged, there is nothing to do. If something was, run the
command the warning prints, then `jjpr submit`. A network or token
problem shows up in `jjpr auth test`.

## A merged PR sits under a merge commit

```
  Warning: 'bottom' below 'top' was merged, but 'top' starts with a merge commit.
```

jjpr swaps the merged parent for trunk itself when it can. It stops
when the merge's other parent is also merged work, or when more than
one of your commits builds on the merged one. Move each commit that
sat on the merged one, keeping its other parents:

```
jj log -r 'trunk()..top'                       # find them and their parents
jj rebase -s <commit> -d main -d <other parent>
jjpr submit
```

Leave out any other parent that was itself merged.

## Merged commits are still in `jj log` after a restack

Harmless: the PR above no longer contains them. jjpr abandons them
unless a bookmark, your working copy or other work sits on them. To
drop them anyway, run the `jj abandon` the warning prints.

## A restack left conflicts

```
Error: cannot push; some commits have unresolved conflicts
```

Nothing was pushed. Resolve the conflict, or put the stack back as it
was before the rebase:

```
jj new <change-id>          # resolve on top of it, then: jj squash
jj op log                   # or find the operation before the rebase
jj op restore <operation-id>
jjpr submit
```

## Submit refuses a divergent change

Two commits share one change ID, usually after editing the same change
in two places. Keep one copy, abandon the other:

```
jj log -r 'change_id(<change-id>)'
jj abandon <commit-id of the copy to drop>
jjpr submit
```

## A push was rejected

Someone else pushed to the branch, or the forge protects it. Fetch and
look before pushing again:

```
jj git fetch
jj log -r '<bookmark> | <bookmark>@origin'
```

If the remote copy is the one to keep:
`jj bookmark set <bookmark> -r <bookmark>@origin`. If yours is, run
`jjpr submit` again.
