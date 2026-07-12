# How it works

jjpr is a coordination layer over `jj` and your forge's API. It does
no version control of its own. Every VCS step shells out to the `jj`
binary, and PR state comes from forge APIs over HTTP.

## Stack discovery

jjpr discovers stacks by walking each bookmark toward trunk. Starting
from a bookmarked commit, it follows parent commits (taking the first
parent through merges) until it reaches the trunk branch or a foreign
remote bookmark (see [Foreign bases](#foreign-bases) below). Each walk
produces a path. Bookmarks on that path become segments of a stack.

The result is an adjacency graph keyed by jj change IDs.

- **Segment**: a contiguous run of commits between two bookmarked
  commits, or between trunk and the lowest bookmark.
- **Stack**: a chain of segments rooted at trunk and ending at a
  bookmark with no bookmarked descendants.

Multiple bookmarks at the same commit collapse into one segment.
Bookmarks that don't connect to your other stacks (a fork from trunk
that isn't an ancestor of any current bookmark, for example) form
their own stacks.

## Submission planning

When you run `submit`, `merge`, or `watch`, jjpr:

1. Builds the change graph from `jj log` output.
2. Identifies the target stack (explicit bookmark argument, or
   inferred from `trunk()..@`).
3. Compares each bookmark against forge state. Does a PR exist? Does
   it point at the right base? Has the title or body drifted?
4. Plans the minimum set of pushes, PR creations, base retargets, and
   body updates needed to reach a consistent state.
5. Executes the plan.

Step 4 is what makes the command idempotent. If the plan is empty,
jjpr prints "Stack is up to date" and exits.

## Merge orchestration

`merge` and `watch` walk segments from the bottom up. A segment is
mergeable when its PR is non-draft, has the required approvals, has no
requested changes, has no merge conflicts, and (when CI is required)
has passing CI.

After a merge succeeds, jjpr:

1. Fetches the updated default branch.
2. Reconciles the rest of the stack onto the new base. Rebase by
   default, or merge commits if `reconcile_strategy = "merge"`.
3. Pushes the updated bookmarks.
4. Retargets the next PR's base if needed.
5. Re-evaluates the next segment.

This continues until the stack is empty or the next segment is
blocked.

## Merge commits

`jj new A B` produces a commit with two parents. jjpr follows the
first parent through the merge, so the merge commit and its
first-parent ancestry stay in the current stack. The other parent (or
parents) form independent stacks. PRs for merge bookmarks include a
note saying which branches were merged and that the diff may include
their changes until those PRs land.

Diamond-shaped stacks (two branches that fork from a common ancestor
and re-merge) are tracked as two separate stacks for submission. The
merge commit's PR carries the explanatory note about the combined
diff.

## Foreign bases

If a commit in your stack's ancestry has a remote bookmark that isn't
one of your local bookmarks, jjpr treats it as a foreign base. Your
first PR targets that branch instead of the default branch, and the
status output annotates the stack with `(based on <branch>)`.

This handles the common case of stacking on a coworker's PR branch.
Override with `--base <branch>` when needed.

`submit`, `watch`, and `merge` only ever act on your own bookmarks
(commits you authored). `status` is the exception: it shows the whole
stack down to trunk regardless of author, so a coworker's branch you've
stacked on appears as its own segment — attributed to them, and marked
so it's clear the mutating commands leave it alone. See
[status](commands/status.md).

## What jjpr never does

- Modify the working copy without an explicit user-driven command.
- Reimplement jj operations. Every VCS step is `jj <subcommand>`.
- Cache forge state across invocations. Each run re-fetches.

## Code map

For contributors, the source is laid out as follows.

- `src/jj/`: `Jj` trait and the `JjRunner` implementation that shells
  out to the `jj` binary.
- `src/forge/`: `Forge` trait and per-forge backends (GitHub, GitLab,
  Forgejo) over a shared `ureq` HTTP client.
- `src/graph/`: change graph construction and traversal.
- `src/submit/`: analysis, planning, and execution for `submit`.
- `src/merge/`: merge planning, execution, and the watch loop.
- `src/watch.rs`: the `watch` command's outer loop and helpers.
