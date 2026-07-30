# Merge queue support (feature request)

Status: **not implemented**. Raised 2026-07-30 while scoping GitHub native
stacks; carved out as its own feature because it is not native-stack-specific
and because a half-model of it is worse than none.

jjpr currently has no concept of a merge queue. It merges by calling the
forge's merge endpoint and treating success as "landed". On a repository whose
base branch requires a merge queue, that model is wrong in a way that is not
obvious from the code.

## Why it needs real support rather than a flag

A queued merge breaks three assumptions jjpr is built on.

**"Merged" stops being synchronous.** jjpr's merge loop merges a PR, then
reconciles local and forge state on the assumption the merge has happened. A
queued PR has not merged. It is *scheduled*. Everything downstream of the merge
call, base retargets included, is premature.

**Terminal is not final.** GitHub's async merge API returns `enqueued` as a
**terminal** status: there is nothing further to poll on that request. The merge
itself may complete minutes later, or be ejected from the queue and never
complete. A tool that treats a terminal status as "done" reports success for
something that may not happen.

**Atomicity weakens.** For stacked PRs, a direct merge is all-or-nothing. Per
the `gh-stack` CLI reference, queued stack members "are added to the queue
together but merge as the queue processes them" and "may land in separate
groups rather than all at once". So the guarantee jjpr would otherwise advertise
for a stack merge does not hold on a queue repo. An explicit merge method is
also ignored with a warning, because the queue chooses.

## What is already known

Detection (from `gh-stack`'s client, and the shape jjpr would need):

- GraphQL `repository.mergeQueue(branch:)` is non-null when the branch has one.
- A `MERGE_QUEUE` entry in `repository.ref(qualifiedName:).rules` also indicates
  one, covering the ruleset-configured case.

Driving it:

- `PUT /repos/{o}/{r}/pulls/{n}/merge-async` with `merge_action: "merge_queue"`
  forces the queue path; `"default"` routes to the queue automatically when the
  base branch requires one.
- Forcing `merge_queue` on a branch without a queue is **accepted at submit**
  (`202`) and fails only at poll time (`failed`, "Cannot perform a merge queue
  merge for a branch with no merge queue"). Verified live.
- `enqueuePullRequest` (GraphQL) is rejected for PRs in a native stack.

Not yet established:

- What jjpr should poll to learn a queued PR's eventual outcome. The merge
  request's own uuid is terminal at `enqueued`, so the answer is elsewhere:
  probably the PR's state, or the queue entry.
- Ejection semantics: how a tool learns a PR was ejected, and what it should do
  about the rest of the stack.
- Whether jjpr should refuse, hand off, or genuinely wait. All three are
  defensible and they imply very different UX.

## Interim position

Until this is designed, jjpr should **not pretend to support queues**. If a
merge would route to a queue, say so and stop, rather than reporting a merge
that has not happened. That is the honest failure mode and it is cheap.

## Related

- `notes/forges/github-native-stacks.md` — the async merge API, its status
  values, and the live verification behind the claims above.
- GitHub's queue support for native stacks was still rolling out at the time of
  writing, with users reporting `404`s on queue-enabled repos.
