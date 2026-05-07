# Troubleshooting

## "No bookmark in working copy ancestry"

`jjpr` (or `jjpr status`) defaults to scoping output to the stack
containing your working copy. If no bookmark is in `trunk()..@`,
nothing matches.

Two fixes:

```
jj bookmark set <name>                # mark the current change
```

or to see every stack regardless of working-copy position:

```
jjpr status --all
```

`watch` is the exception. It waits for a bookmark to appear, polling
every few seconds, instead of exiting.

## "skipping '\<name\>' (points to a missing or conflicted commit)"

A local bookmark points at a commit that no longer exists, usually
because the corresponding PR was squash-merged on the forge and the
commit was rewritten. The warning includes the cleanup command:

```
jj bookmark forget <name>
jj git push --deleted
```

After cleanup, re-run jjpr.

## "cannot push — some commits have unresolved conflicts"

A commit in your stack has unresolved merge conflicts (often from a
rebase that couldn't auto-resolve). jjpr won't push conflicted
commits.

```
jj edit <change-id>                   # the change ID is in the error message
# resolve the conflicts
jjpr submit
```

## "Local sync failed"

jjpr couldn't push or rebase locally after a merge, so it stopped
before merging the next PR. Common causes: a `jj rebase` while jjpr
was running, divergent change IDs from concurrent editing, or a
conflicted push.

The just-merged PR is fine on the forge. The remaining open PRs are
still open with their bases retargeted; only their local branches are
out of date.

To accept the forge state:

```
jj git fetch
jj bookmark set <bookmark> -r <bookmark>@origin
```

To fix local state and push it:

```
jj git fetch
jj rebase -s <change-id> -d main
# resolve any conflicts
jjpr submit
```

Then re-run `jjpr merge` to continue. `jjpr watch` retries
automatically on the next poll once you fix things.

## "Forge reconcile failed"

The forge merge succeeded but a follow-up API call (refresh PR list,
retarget the next base, update stack-info comments) returned an error.
Local state is fine; only the post-merge bookkeeping didn't complete.

Retry with `jjpr merge`. If it keeps failing, check `jjpr auth test`
for token issues, then forge status / network connectivity.

## Authentication errors

```
jjpr auth test
```

Reports the detected forge, where the token came from, and what the
forge said. Common cases:

- **No token found**: set `GITHUB_TOKEN`, `GITLAB_TOKEN`, or
  `FORGEJO_TOKEN`, or run `gh auth login` / `glab auth login`.
- **403 / insufficient scope**: regenerate the token with `repo`
  scope (GitHub/Forgejo) or `api` scope (GitLab).
- **Self-hosted instance**: set `forge = "..."` in `.jj/jjpr.toml`.
  See [Forge support](forges.md).

## "PR title not updated after creation"

By design. jjpr creates the PR title from the commit's first line but
doesn't rewrite it on subsequent submits. If the first line changes,
jjpr warns about the drift and leaves the title alone so your manual
edits are preserved.

To re-sync, edit the PR title on the forge directly.

## "merge already in progress" warnings

A previous `merge` call returned a transient 502 or 503 right after
GitHub started processing the merge. jjpr polls the PR state for up to
30 seconds to confirm. If the merge actually completed, jjpr
continues. If not, it reports the failure and exits. Re-run to retry.

You only see the polling output when the network round-trip takes a
while. Otherwise it's silent.
