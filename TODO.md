# jjpr TODO

## Fixed: `recovery_scenarios` flake (2026-07-31)

`equivalent_restacks_collapse_to_one_commit` failed ~10% of runs. Two causes,
both fixed:

- **The test's premise was only usually true.** It performs two "identical"
  restacks and asserts they collapse to one commit. A commit's identity
  includes its committer timestamp, so when the two rebases straddled a clock
  tick they produced *different* commits — a divergent change, the opposite of
  what the test demonstrates. Fixed by pinning `JJ_TIMESTAMP` on just those two
  operations (`Repo::jj_fixed_time`), which is the minimum that makes
  "identical" true. Measured: 10/100 divergent before, 0/120 after.
- **The harness turned a command failure into a wrong answer.** `Repo::out`
  returned stdout without checking the exit status, so when
  `jj log -r <divergent-id>` errored (jj refuses to resolve an ambiguous change
  ID) it yielded an empty string, which failed an unrelated assertion as
  `left: 0`. The real error was never shown, which is why three sightings
  produced no diagnosis. `out` now asserts success and prints stderr.

The second fix is the more valuable one: any jj failure in this suite is now
self-diagnosing rather than surfacing as a nonsense value somewhere else.

**Adversarial review then found the first fix had papered over a real
behaviour.** The test's rationale claimed "the common two-watch race needs no
resolution at all". Measured: restacks 2 seconds apart diverge **12/12**. Two
`jjpr watch` processes poll 30s apart, so a real race essentially never
collapses — the claim was false, and pinning the clock made the test assert it
under conditions that do not occur. Corrected by scoping that test's doc to the
narrow mechanism it actually covers, and adding
`restacks_a_realistic_race_apart_in_time_do_diverge`, which asserts divergence
for the case jjpr must survive. **jjpr's divergence gating is therefore the
load-bearing path for concurrent watches, not a rare fallback.**

Also added `assert_timestamp_pin_works`: `JJ_TIMESTAMP` maps to
`debug.commit-timestamp`, a debug knob with no stability guarantee. A bad value
errors, but a future jj that simply ignored the variable would silently return
these tests to timing-dependence. The guard turns that into a failure.

## GitHub native stacks — detection shipped (0.36.0), rest remaining

Research: `notes/forges/github-native-stacks.md`. GitHub's stacked PRs went to
public preview 2026-07-30 and are now enabled on every repo, so users can stack
jjpr's PRs at any time with `gh-stack` or the web UI.

Shipped: pre-merge detection. `PullRequest.stack` (`PrStackRef`) is parsed off
the payload jjpr already fetches, and `evaluate_segment` returns
`BlockReason::NativeStack` before spending any per-PR request or mutating
anything. `merge` and `watch` both stop and name `gh stack merge` /
`gh stack unstack`. Verified end to end against a real stack, including the A/B
that unstacking lets jjpr proceed.

Also shipped: submit's base-retarget guard. Probing established that a native
stack blocks exactly one thing jjpr does, the base retarget (`422`, even for a
no-op); force-push, PR creation, body/title edits and comments all succeed. So
ordinary submits are untouched and only a stack *reshape* conflicts.
`create_submission_plan` diverts such a retarget into
`native_stack_base_conflicts` and execute refuses before phase 1, since submit
pushes first and retargets in phase 3.

Also shipped: merge's post-merge reconcile. `reconcile_forge_state` skips the
retarget when the *next* PR is stacked (reachable even though merge refuses
stacked PRs, because a native stack can be rooted on any branch, so the PR that
merges may be unstacked while the one above it is not). It reports the same
`BlockReason::NativeStack` the pre-merge check emits rather than a bespoke
warning, via `ReconcileState::native_stack_block` — deliberately not a
`*_failed` flag, so it never renders as a retryable "forge reconcile failed".

Remaining, roughly in order:

Direction set 2026-07-30: jjpr should support native stacks on the forge's own
terms. Read them, merge them through the async merge API, and on a partial
merge **let the server's cascade rebase flow down to us** rather than
re-asserting jj's commits.

**Research status (2026-07-31): the three design-blocking questions are
answered** — see "The three design-blocking questions" in the notes. GitHub
enforces branch rules on a stack merge but never names the offending PR; a
stranded WIP is repairable with a single `jj rebase` and the change ID
survives; and there is **no** settle signal for the cascade, so the two
outcomes can only be told apart by a bounded wait, which forces the `sha`
guard on any push in the declined branch.

The reconcile map is now written up in `docs-dev/native-stacks-design.md`,
along with its headline result: **the `is_rooted_in` skip added in v0.35.0
already discriminates the two cascade outcomes correctly** (verified live —
rooted ⇒ adopt, not rooted ⇒ rebase and repair). The worry that jjpr would
blindly overwrite the server's rebase is already handled by code written for
an unrelated reason.

What survives: the timing race (no settle signal, so the wait must be bounded
and the declined-branch push must carry the `sha` guard), the stranded-WIP
repair (a one-line `jj rebase` that nothing currently calls), and — now
confirmed — **step 5's single-segment check is unsound for native stacks**. A
mixed cascade outcome is reachable: GitHub rebases survivors until one
conflicts, then stops. jjpr inspects only the first survivor, sees it rooted,
skips, and never repairs the broken one above it. The predicate is right per
segment; the loop around it assumes the remaining stack moves as a unit, which
holds for jjpr's own rebase and not for an external partial cascade.

**Nothing here gets implemented before the whole system is mapped and its
correctness argued.** The investigation has already reversed its own
conclusions twice (the `delete_branch_on_merge` scare, and "adoption is free"),
which is the evidence for that rule rather than an argument against it. Design
doc first: `docs-dev/native-stacks-design.md`.

Decisions taken:

- **Merge gating stays jjpr's.** Before triggering an all-at-once native merge,
  inspect every PR the merge would land and apply jjpr's own standards
  (`required_approvals`, `require_ci_pass`, changes-requested). If any fails,
  don't merge, and explain it in **the same language jjpr already uses for its
  own stacks** — reuse `BlockReason` and `format_block_reason` rather than
  inventing a second vocabulary for "GitHub would have allowed this but we
  don't".
- **Merge queues are out of scope here** and tracked separately in
  `docs-dev/merge-queue-support.md`. Until that is designed, a merge that would
  route to a queue should be refused with an explanation, not half-modelled.
- **Where jjpr's model and GitHub's disagree, present rather than resolve.**
  Which stack `status` shows, whether the nav comment coexists with GitHub's
  map, partially-stacked chains, one jjpr stack spanning two native stacks, and
  diamonds that native stacks cannot represent: these are all differences jjpr
  can *detect*. Surface them to the user. No automatic resolution until we know
  what the right one is.

- **Wire up `Forge::native_stacks` (S).** Implemented and tested in
  `src/forge/github.rs`, called from nowhere. Surface native stacks in
  `status`. Not tidying: verified against a live stack that `status` shows no
  hint of native-stack membership *and* prints `✓ mergeable` for PRs that
  `jjpr merge` then refuses. The two commands contradict each other today.
  Its doc comment also claims a 404 means "preview not enabled"; a 404 equally
  means the token cannot see the repo.
- **Pre-flight the whole merge range (S, folds into the merge work).** GitHub
  fails a stack merge containing a closed or draft member with
  "Pull request must be open and not in draft mode" and **does not say which
  one**. jjpr should check open/draft state across the range and name the
  offender, alongside the approval/CI gating already decided.
- **Merge stacked PRs via `PUT /pulls/{n}/merge-async` (M).** Replaces today's
  refusal. Needs: poll to a terminal state (submit returns 202 even for
  failures), the `sha` guard, `409` uuid recovery, and a `404` message that
  mentions `contents: write`. Merging PR N lands everything below it, so the
  user must be told what will land before it does. Under a merge queue the
  result is `enqueued` and atomicity is weaker; refuse for now
  (`docs-dev/merge-queue-support.md`).
  **Scope the first cut to whole-stack merges — now verified, not assumed.**
  Merging the top PR produces no cascade whatsoever: survivor heads unchanged,
  bookmarks untouched, unpushed WIP intact, and jj sees an ordinary
  squash-merge. Every cascade hazard below is specific to *partial* merges.
- **Adopt the cascade rather than fight it (L — restored; it is not M).**
  Empirically mapped in three shapes; see "The cascade rebase, from jj's side"
  in the notes. jj supplies the provenance (no state store needed on jjpr's
  side), but adoption is **not** free:
  - *No local descendants*: `jj git fetch` adopts correctly on its own.
  - *Local edits to the same bookmark*: jj raises a **conflicted bookmark**
    carrying base/ours/theirs. Surface it; don't guess.
  - *Unpushed work descending from the rewritten commit* — the common shape —
    **jj silently strands it**. It reparents the WIP onto the abandoned
    commit's *parent*, not onto GitHub's replacement, and the working copy
    loses the merged-below content. jjpr must detect descendants and rebase
    them onto the adopted commit itself. This is the real work.
  - **The cascade is asynchronous.** Right after `merge-async` says `merged`,
    the refs still show the pre-cascade heads; the rebase lands seconds later.
    jjpr's reconcile fetches immediately after merging, so it can race the
    cascade, see nothing changed, and push its own rebase over it.
  - **The cascade is best-effort, and its two outcomes need opposite
    responses.** If the rebase would conflict, GitHub completes the merge,
    retargets the survivor's base, and silently *skips* the rebase — leaving a
    PR whose diff includes the already-merged changes (verified: 3 files where
    1 was expected, `diverged ahead 3 behind 3`). So jjpr must read the outcome
    and branch: head changed → adopt and do not push; head unchanged but base
    moved → rebase locally and force-push to clear the bloat. Force-pushing a
    stacked PR is permitted, so the recovery exists — but "always adopt" and
    "always rebase" are both wrong.
  - **A merge-commit landing rebases the survivor anyway**, even though the
    merged commit stays an ancestor of trunk. jjpr's `is_rooted_in` skip
    (added to preserve approvals) therefore no longer prevents a rewrite on a
    native stack; it only prevents jjpr adding a second one.
  - Also: `reconcile_local_state` / `reconcile_forge_state` must stop
    force-pushing over an adopted commit, and the change ID *does* change on
    adoption, so anything keyed on it across a merge boundary must tolerate
    that.

## Multi-identity ownership — Tier 1 shipped (0.33.0), Tier 2 remaining

Full spec: `docs-dev/identity-ownership.md`. Shipped: `owned()` email-union
discovery (`Identity`, `JjRunner::set_identity`), config `[identity]`, seeded in
status/submit/merge, the Tier-1 login match that fixes the reported status label
(verified in beancounter) with no `/user/emails` call, and Tier-2 lazy
augmentation in `resolve_stack` (on an inference/analyze miss, fetch
`get_authenticated_emails`, extend, retry once) plus an `[identity]`-pointing
hint when a bookmark is present but unowned. Note: Tier 2 auto-fetch needs the
token's `user` scope; without it (e.g. a `repo`-only gh token) it degrades to
the config backstop, which the new hint guides the user to. Remaining:

- **Laziness assertion (M).** E2E now covers the login match (Tier 1, real
  forge), the config backstop, and Tier 2 when the token has `user` scope
  (`tests/e2e.rs`). Still unproven by an automated test: that the happy path
  fetches NEITHER endpoint — needs a recording forge stub (a 16-method impl), so
  left as a lower-priority unit follow-up.

Docs: nothing *needs* changing — the feature is automatic, and the `[identity]`
backstop is surfaced by the "unowned bookmark" error. Optional judgment call:
add an `[identity]` entry to `configuration.md` for reference completeness.


## Status whole-stack redesign — DONE (jjpr 0.32.0)

Shipped: author-agnostic discovery for `status` (`::(@ | mine()) ~ trunk()`)
via `get_status_bookmarks`/`build_status_graph`, `submit`/`watch`/`merge` still
`mine()`-scoped; `PullRequest.author`; foreign `PR open/merged by @user` rows
with base mergeability + `jjpr won't submit or merge it`; merged/stale cleanup
hint; Scenario-1 recognition. Merged-PR lookup is FOREIGN-ONLY (a `find_merged_pr`
name match on your own fresh bookmark could otherwise mislabel live unpushed work
as "merged, clean up").

### Deferred follow-ups (from the adversarial review)

- **Positional lookup of a sibling coworker branch.** `jjpr status <branch>`
  where `<branch>` is neither in `@`'s ancestry nor in `mine()`'s ancestry isn't
  found (discovery is scoped to `::(@ | mine())`), and the "not found" hint
  points at `--all`, which also won't show it, and "every local stack" is now
  inaccurate. Either broaden discovery for an explicit positional (needs a
  revset-injection-safe way to anchor on the named bookmark) or fix the hint.
- **Foreign-open over-fetch.** `fetch_segment_status` pulls mergeability + checks
  + reviews for every open PR in a shown segment, but a foreign-open segment only
  renders mergeability — two of the three calls are wasted. Fetch only what a
  foreign segment shows.

### From the second adversarial review (2026-07-12)

Two Medium items DONE (jjpr 0.32.0): the diamond `@`-as-merge misinference
(`select_stacks_to_show` now prefers a mine-containing overlapping stack, via
`overlapping_stacks`; submit/watch/merge unaffected — they use a mine-only
graph), and remote-only foreign-base enrichment (`render_base` now attributes
the `(based on X)` footer with the base's PR + author + link). Remaining:

- **`find_merged_pr` blip degrades silently (LOW).** A transient error is
  swallowed (`if let Ok(Some(pr))`), so a merged coworker branch renders as a
  bare unsubmitted-looking segment with no notice. Distinguish "lookup failed"
  from "no PR".
- **Foreign no-PR segment has no ownership cue (LOW).** A coworker segment with
  no open/merged PR renders as a plain `name (N changes)` line — under `--all` or
  in a mixed stack the reader can't tell it's someone else's.

## `--json` structured output (follow-up commit, after the redesign)

Global `--json` flag (clap `global = true`) for machine-readable output.

- Honored by report-and-exit commands: `status`, `submit`, `merge`,
  `auth test`, `config get`.
- **Rejected by `watch`** — it streams progress, not a single object. A
  final-completion summary could come later, but not per-poll JSON.
- Each command needs a serializable result struct (`status` gets one from the
  redesign; `submit`/`merge` mostly already have one).
- Under `--json`, stdout must be pure JSON: every human line (`Fetching
  remotes...`, hints, stale-bookmark warnings) moves to stderr or is
  suppressed. This stdout audit is the real work, not the serde.
- Origin: user suggestion on GitHub issue #5.
