# Troubleshooting

Setup and day-to-day questions. If a stack is in a bad state after a
merge, a conflict or a failed push, see
[Recovering from bad state](recovering.md).

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
