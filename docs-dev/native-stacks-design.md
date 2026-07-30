# GitHub native stacks: design

Status: **in progress**. Research is complete (see
`notes/forges/github-native-stacks.md` for the evidence behind every claim
here). This document is the correctness argument that has to exist before any
of it is implemented.

This first section maps the reconcile machinery as it stands, because a third
mode cannot be added to it responsibly without that map. Nothing here proposes
changes yet.

## The reconcile path today

`reconcile_after_merge` runs after each successful merge and calls two halves:
`reconcile_local_state` (jj side) and `reconcile_forge_state` (forge side).
Only the first is complicated.

### Control flow of `reconcile_local_state`

Seven exits, in order. Four of them are early returns, and the order between
them is the whole design.

| # | Step | On failure / hit |
|---|---|---|
| 1 | **Divergence gate (pre-fetch)** | `Present` → return concurrent-gate warning. Never rebase onto a divergent state |
| 2 | **`jj git fetch`** | error → re-check divergence first, then return a fetch warning |
| 3 | **Capture `post_fetch_op`** | the rollback anchor; see below |
| 4 | **Divergence gate (post-fetch)** | `Present` → return concurrent-gate warning |
| 5 | **`is_rooted_in` skip** | true → print "already based on X", **return success with no rebase and no push** |
| 6 | **Strategy branch** (`Rebase` \| `Merge`) | each has its own failure returns |
| 7 | **Divergence gate (post-rebase)** | `Present` → restore to `post_fetch_op`, return concurrent-gate warning |
| 8 | **Push loop** | per-bookmark; reads approvals-at-risk *before* each push |

Load-bearing details:

- **Divergence is checked three times**, not once, and it fails *safe*: a read
  error is reported as `Present`, because the likely cause is lock contention
  from the very concurrent writer being guarded against. "Can't tell" must
  never read as "clean".
- **`post_fetch_op` is the rollback floor.** Step 7 restores only the rebase,
  never past the fetch, because rolling past it would discard a concurrent
  process's work. jj preserves both sides' commits, so recovery is
  work-preserving by construction rather than by care.
- **Step 5 returns for the whole remainder**, having inspected only
  `segments[seg_idx + 1]`. It is an all-or-nothing decision about the rest of
  the stack made from one segment.
- **The `Merge` strategy stops at the first failure** (`break`), pushing only
  the bookmarks that succeeded before it. `Rebase` is all-or-nothing: one
  `rebase_onto` covers every descendant, so either all remaining bookmarks push
  or none do.

### What `is_rooted_in` actually asks

```
parents(<root>) ~ ::<base>      empty  ⇒  true
```

"Every parent of the subtree root is already an ancestor of the base", i.e. the
subtree sits cleanly on the base and needs no rebase. It was added in v0.35.0
for one purpose: a merge-commit or rebase-merge landing leaves the merged commit
in trunk, so the descendants are already based correctly and rebasing them would
rewrite SHAs — dismissing standing approvals for nothing.

## The finding that changes the shape of the work

**That skip already discriminates the two cascade outcomes correctly**, by
accident. Verified against live stacks, running the exact revset on the
post-fetch state:

| Scenario | `parents(survivor) ~ ::main` | `is_rooted_in` | jjpr does | Correct? |
|---|---|---|---|---|
| Cascade **succeeded** | empty | true | skip rebase and push | **yes** — adopts the server's commit |
| Cascade **declined** (conflict) | non-empty | false | rebase + force-push | **yes** — clears the bloated diff |

The reasoning is structural, not coincidental. A successful cascade leaves the
survivor parented on the new trunk, so it *is* rooted there. A declined cascade
leaves the survivor on its pre-merge parent, which a squash landing has dropped
from trunk, so it is not. The same predicate answers both.

So the central worry — that jjpr would blindly overwrite the server's rebase —
is **already handled** by code written for an unrelated reason. That removes
the largest piece of what looked like the work.

## What this does not solve

Three things survive, and they are what the rest of this design has to address.

**1. Timing, not discrimination.** `is_rooted_in` is only correct if jjpr reads
state *after* the cascade has settled. The cascade is asynchronous (~2–3s in
two samples, on a tiny repo) and jjpr's reconcile fetches immediately after
merging. Fetch too early and the survivor looks un-rebased, which is
indistinguishable from a declined cascade — jjpr concludes "declined" and
force-pushes, racing GitHub. There is no settle signal to wait on: the base
retarget precedes the rebase but happens in both cases, and `mergeable_state`
sits at `unknown` indefinitely. So the wait must be bounded and the push in the
declined branch must carry the `sha` guard, so a late cascade is rejected
rather than silently clobbered.

**2. Stranded descendant work.** When `is_rooted_in` is true, jjpr returns at
step 5 — correct for the bookmark, but it does nothing about unpushed commits
that descended from the rewritten commit. jj reparents those onto the
*abandoned commit's parent*, losing the merged-below content, and conflicting
if the WIP built on it. The repair is a single
`jj rebase -s <wip-change-id> -d <bookmark>` and it also clears the conflict;
the WIP's change ID survives the cascade, so the target is identifiable. But
nothing calls it today.

**3. Step 5 inspects one segment and decides for all of them — and that is
unsound for native stacks. Confirmed, not hypothetical.**

A **mixed cascade outcome is reachable**. Built a 4-PR stack `a→b→c→d` where
only `d` conflicts with trunk, then merged up to `b`:

```
c: head 914640ce → 6a99383c, base → main     rebased
d: head af4e22c1 unchanged,  base mix0731-c  declined, and not even retargeted
```

GitHub rebases survivors until one fails, then stops. `d` is left based on the
now-orphaned old `c`, and it is genuinely broken: its diff lists **four** files
(`a`, `b`, `c`, `sh`) instead of its own one, `mergeable: false`,
`mergeable_state: dirty`, `compare` → `diverged ahead 4 behind 3`.

What jjpr does with that, running its exact predicate on the post-fetch state:

```
parents(mix0731-c) ~ ::main@origin   empty      → is_rooted_in TRUE  → SKIP, return
parents(mix0731-d) ~ ::main@origin   non-empty  → needs a rebase that never happens
```

Step 5 looks at `c`, sees it correctly rooted, and returns for the entire
remainder. `d` is never examined and never repaired.

**The check is not buggy; its precondition is violated.** For jjpr's own
merges the remaining stack always moves as a unit — `Rebase` strategy issues
one `jj rebase -s <root> -d <trunk>` that reparents every descendant, so if the
first survivor is rooted the rest necessarily are. A native stack breaks that
invariant, because an external actor rebases *part* of the chain. Any design
that keeps the single-segment shortcut inherits a silent failure whenever a
cascade partially succeeds.

So the earlier good news needs qualifying: `is_rooted_in` discriminates the two
outcomes correctly **per segment**, and step 5 applies it to only one segment.
The predicate is right; the loop around it is not.

## Open, before this document can propose an implementation

- How the bounded wait interacts with `jjpr watch`'s own poll loop, which is
  already a timing-sensitive state machine.
- Whether the strand repair belongs in reconcile or in a separate explicit
  command, given jjpr is otherwise working-copy-agnostic and this touches the
  user's WIP.
- Whether step 5's single-segment shortcut is replaced by a per-segment check
  or kept with a native-stack-specific bypass (it is unsound for a partial
  cascade either way — see above).

## Should jjpr repair what GitHub declined to rebase?

Working answer: **yes, when the repair can actually complete** — but "can
complete" is a stronger condition than "the rebase is clean", and jjpr must
leave the repo untouched when it cannot.

### Why GitHub declines: at least two causes, and they differ for jjpr

**Cause 1 — content conflict.** The survivor's changes conflict with trunk.

- jjpr's `jj rebase` **succeeds** (exit 0) and produces a **conflicted commit**.
  jj records conflicts in commits rather than failing, so
  `if let Err(e) = jj.rebase_onto(...)` never fires.
- The push is then refused **by jj itself**:
  `Won't push commit <sha> since it has conflicts`.
- Net: the remote is safe, but the user's local stack has been rewritten into a
  conflicted state, and jjpr reports a raw push failure rather than the cause.

**Cause 2 — the survivor's branch forbids force-pushes.** No content conflict
at all; the rewrite is simply not permitted. Verified by protecting only the
survivor branch and merging below it: the head stayed put while the base was
still retargeted.

- jjpr's rebase is **clean** — no conflict.
- The push is refused **by GitHub**:
  `refs/heads/<branch> (reason: protected branch hook declined)`.
- Net: local rewritten, remote unchanged, and the two now diverge.

These are not variations on one thing. In cause 1 the local rebase is the
problem; in cause 2 the local rebase is fine and the *push* is the problem. A
design that tests only for conflicts handles half of it. And the list is not
proven exhaustive — these are the two found, not the two that exist.

### Can jjpr know in advance?

**Whether the rebase is clean: yes, reliably and locally.** Rebase, then ask
`is_conflicted`. That is not a prediction, it is a cheap trial — and jjpr
already does exactly this in the `Merge` strategy (`merge_into`, then
`is_conflicted`, then push only what is clean). **Shipped since this was
written:** `Rebase` now has the same screen, both strategies check the
segment's whole `<root>::<bookmark>` range rather than the tip, and
`is_conflicted` itself was fixed — it silently reported *clean* for any
multi-commit revset.

**Whether the push will be accepted: no, not without trying it.** Nothing
readable in advance distinguishes cause 2. GitHub's `mergeable` field is no
help — it sits at `unknown` indefinitely after a declined cascade.

### What that implies

The precondition for auto-repair is *rebase is clean **and** push succeeds*,
and only the first is knowable before acting. Since both failure modes leave
the user's local repo rewritten while the remote is unchanged, the repair has
to be undoable — and jjpr already has the mechanism. `reconcile_local_state`
captures `post_fetch_op` and uses `restore_operation` to roll back exactly one
bad rebase without crossing the fetch. The same anchor covers this:

1. rebase; if `is_conflicted`, restore to `post_fetch_op` and report the
   conflict as the reason GitHub declined — do not leave the user holding it;
2. otherwise push; if the push is rejected, restore to `post_fetch_op` and
   report that the branch forbids the rewrite;
3. otherwise the repair succeeded.

This keeps the automatic behaviour the leaning asks for, while making the
failure path leave no trace rather than handing back a conflicted or diverged
stack. It needs no new machinery: the conflict screen is already in place, so what
remains is using the existing `post_fetch_op` rollback anchor in two more
places so a failed repair leaves no trace.

Resolved since this was written: the conflict screen applies to jjpr's *own*
merges too, not just native-stack ones. Cause 1 is reachable without native
stacks at all — any trunk movement can make the post-merge rebase conflict —
so it shipped as an independent fix.
