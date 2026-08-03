# jjpr TODO

## Fixed: watch never gave up on a failing submit, refresh, or merge (2026-08-03)

Found while writing tests for the mutation misses below. Measured, not theorised:
a `run_watch_loop` with a healthy graph scan and a forge whose `list_open_prs`
always failed ran **1,000,548 submit attempts in 10 seconds** and never gave up.
It should stop after `MAX_CONSECUTIVE_ERRORS` (10).

`consecutive_errors` is one counter shared by four phases, and each phase reset it
on its OWN success (scan, submit, PR refresh, and `run_merge_phase` internally).
So with an earlier phase succeeding and a later one failing, it oscillated
0 -> 1 -> 0 -> 1 and the `>=` guard was unreachable. Only the scan arm could ever
fire, because nothing reset before it in an iteration. The user-visible symptom
was `Submit error (1/10)` printed on every poll — a countdown that never advanced.

Fixed by making "consecutive" mean *iterations*, not *the last thing that
happened*: the counter is snapshotted at the top of an iteration and reset at the
bottom only if nothing incremented it. Failing phases `continue` and so never
reach that reset, which is what lets the count accumulate; the snapshot
comparison also captures the increments `run_merge_phase` makes internally, which
a plain "reset at the bottom" would have wiped.

This is also why 783/788 and 805/810 came back MISSED and resisted a
straightforward retry test — the code they guard was unreachable. The mutation
misses were a symptom of the bug, not merely a coverage gap.

Verified: submit attempts went 1,000,548 -> 11 (ten retries plus the one
pre-loop `print_initial_watch_status` listing), and the three submit-arm mutants
(`+=`->`-=`, `+=`->`*=`, `>=`->`<`) are each killed, checked by applying them by
hand rather than inferring it.

**The merge-phase eval arm turned out to be a second instance of the same bug.**
It increments `consecutive_errors` and breaks its segment loop but returns Ok, so
the outer loop's per-phase threshold checks never saw it — nothing in the loop
ever tested that increment against the budget. Added a check at the bottom of the
iteration for increments that did not exit the iteration themselves.

**But two safety nets overlap, and the other one wins.** `no_progress_count >= 5`
(watch.rs) fires strictly before `MAX_CONSECUTIVE_ERRORS = 10` whenever the
errors make no progress — which repeated eval errors do by definition. So for a
persistently failing eval it is the STALL detector that stops watch, at 5
iterations, and the error budget never gets there. The new threshold only bites
when errors INTERLEAVE with progress: a merge succeeding resets
`no_progress_count` while `consecutive_errors` keeps climbing.

That interleaved path is now covered. `StackJj` builds a real two-segment stack
(`auth` under `profile`) where `auth` merges every iteration and `profile` fails
to evaluate: the merge counts as progress, `no_progress_count` resets, and the
error budget becomes the only thing that can stop the loop. It exits at
`MAX_CONSECUTIVE_ERRORS`, and the long-surviving `*=` mutant on the eval
increment is finally killed — with `*= 1` the counter pins at 0 and the loop runs
to the deadline.

Verified in BOTH directions, which matters given how the previous test fooled
itself. The three mutants that should die do (`+=`->`*=`, `+=`->`-=`, and the new
`>=` threshold), and changing the stall limit from 5 to 50 does NOT affect this
test — proving it isolates the error budget instead of quietly measuring the
stall detector again.

All four retry arms are now covered by five tests, and each was confirmed to
exercise its OWN arm rather than a sibling. That check matters because the arms
are structurally identical — same increment, same threshold, same message shape —
so an iteration count looks the same whichever one fired. Mutating each arm's
threshold separately is what distinguishes them: the PR-refresh test kills the
refresh threshold and leaves the scan and submit thresholds alive, which is proof
the alternating-failure fixture lands where it claims. Twelve mutants killed
across the four arms.

Worth recording how that was found, because the first version of the test passed
for the wrong reason. It asserted `MAX_CONSECUTIVE_ERRORS` calls to
`mark_pr_ready` and went green — but `mark_pr_ready` is called TWICE per
iteration (once promoting the draft, once inside `evaluate_segment`), and the
stall detector exits after 5, so 5 x 2 = 10 hit the budget's value by arithmetic
accident. The test now asserts ITERATIONS instead, which names the real mechanism
and cannot be satisfied that way; changing the stall limit from 5 to 50 kills it.

## Open: `run_watch_loop` — TESTS FIRST, then decompose

Carries three suppressed complexity lints (`too_many_arguments`,
`cognitive_complexity`, and `too_many_lines` added in the 2026-08-03 reformat,
which pushed it to 284/275 by splitting argument lists without adding a statement
or a branch). Three lints on one function is the signal; the length is not.

**Measured 2026-08-03 — do not re-derive.** First whole-file mutation run in this
repo, `cargo mutants --file src/watch.rs`, 102 of 105 mutants completed:

| outcome | count |
|---|---|
| caught | 32 |
| **missed** | **52** |
| timeout | 6 |
| unviable | 12 |

**32 caught against 52 missed — a 38% catch rate**, versus 95% measured on
`forge/remote.rs`. The distribution is what matters: **24 of the 52 misses are
inside `run_watch_loop` itself**, plus 10 more in `run_merge_phase`, which it
calls. They are not cosmetic — they are the state machine's mechanics:

- `702` `consecutive_errors += 1` survives `-=` and `*=`
- `707` `if consecutive_errors >= MAX_CONSECUTIVE_ERRORS` survives `>=` → `<`
- `783`, `805`, `936` the other progress/retry counters, same operators
- `788`, `810` their thresholds, same comparison flip
- `835`, `840`, `841`, `864`, `910` negated guards survive `delete !`
- `872` a compound condition survives `||` → `&&`

Concretely: you can invert the error counter, or flip the give-up threshold so
watch either never gives up on a broken repo or gives up on the first error, and
the entire suite still passes.

**So the order is the reverse of what "decompose it" implies.** The tests cannot
detect changes to this function's control flow, which means extraction would be
an unguarded rewrite of the least-verified code in the crate. Sequence:

1. **DONE.** Tests targeting the misses — counters, thresholds, guards. Five
   tests, twelve mutants killed across the four retry arms. Re-measured:
   **38% -> 61%** (57 caught / 37 missed), and misses inside `run_watch_loop`
   fell **24 -> 9**, none of them in the retry or exit control flow. That was the
   specific thing that made extraction unsafe, so the blocker is cleared.
2. **STARTED.** First extraction: `handle_phase_error`. The three phase arms
   (scan, submit, PR refresh) were byte-identical apart from a label, so they
   collapse to one helper plus a `PhaseError` decision — the caller still owns
   the `break`/`continue` because a helper cannot drive its caller's loop.
3. **PARTLY DONE.** `too_many_lines` is off: the extraction brought the function
   back under the limit, so the lint passes rather than being suppressed. The
   other two allows stand and are honest — 13 parameters is a real seam problem
   and the loop genuinely branches hard.

The duplication had already drifted, which is the argument for having done it:
only the graph-scan arm printed "Too many consecutive errors; giving up."
Submit and PR refresh exited SILENTLY after ten failures, so the last thing a
user saw was an error line indistinguishable from the nine before it. Three
copies of a block cannot be kept in step by intention; one helper fixes it by
construction.

Remaining work on this function is the nine non-retry misses: the `delete !`
guards, the arithmetic at 646/860/871, and the `||` -> `&&` at 872. Note the
misses have moved OUT of the loop — the file's remaining 37 cluster in reporting
functions (`report_orphaned_prs`, `report_reconcile_failure`,
`print_initial_watch_status`) where a flipped comparison changes which message
prints rather than what jjpr does. Those are candidates for judging equivalent,
not for more tests.

Two caveats on the numbers. The 6 timeouts are ambiguous — an injected infinite
loop inside a polling loop is arguably detected rather than missed, so the true
rate may be a little better than 38%. And 3 mutants never ran, so this is 102/105.

Why there is no direct coverage today: `run_watch_loop` has exactly one caller
(`main.rs`). The 41 unit tests in `watch.rs` exercise helpers and predicates
around it, and the only binary-level watch tests are 2 in `tty_watch.rs` (spinner
rendering) and 1 in `watch_heartbeat.rs` (second watch exits when one is
running). All three test peripheral concerns.

## Fixed: `auth test` misdiagnosed ambiguous remotes (2026-08-03)

Found while reviewing the empty-path-components fix. With two recognised forge
remotes, `submit`/`status` (via `resolve_remote`) correctly said
`multiple forge remotes found: origin, second. Use --remote to specify one.`,
while `auth test` (via `detect_forge_for_cwd`) said `could not detect forge`.

The second was actively wrong: it reported *no* supported remote when the
problem was *two*, and the advice it gave could not help. It mattered more than
a stray message because `auth test` is what jjpr's other errors tell you to run
— the forge failure path prints ``try `jjpr auth test` `` — so the diagnostic of
last resort was the one that lied. Verified pre-existing rather than caused by
the parser change: two *clean* GitLab remotes reproduced it identically.

The root cause was broader than the symptom. `detect_forge_for_cwd` discarded
every error with `.ok()?`, so five distinct failures — not a jj repo, unreadable
config, jj itself failing, no supported remote, more than one — all collapsed to
the same sentence. It now returns `Result` and each surfaces its own.

Propagating alone would not have been enough: the message names `--remote`, and
`auth` had no such flag (`submit` and `merge` did). Fixing only the message would
have offered advice the command could not take, so `test` and `setup` both gained
it. `setup` still falls back to printing help for every forge rather than
erroring, because it is what you run *before* a repo is configured.

Verified by reverting the fix and re-running the new test rather than by mutation
testing: the diff yields only 2 mutants, one unviable, so a green mutants run
would not have proven the test could fail.

## Fixed: empty path components in remote URLs (2026-08-02)

Filed as "`parse_gitlab_path` keeps a leading slash" — `https://gitlab.com//sub/repo`
gave `owner = "/sub"`, which jjpr encodes into `%2Fsub%2Frepo`. Judged garbage
input and low priority.

Measuring the neighbouring cases before fixing it changed that assessment. The
leading slash was the rare symptom; the same missing normalisation made GitLab
reject a **trailing** slash outright — `https://gitlab.com/group/repo/`, which is
what you get copying a URL out of a browser address bar. GitHub's parser already
accepted it, so the same remote worked on one forge and reported "no supported
forge remotes found" on the other. That is the case a user actually hits.

Fixed by dropping empty path components before splitting, rather than by
rejecting them: a remote URL always names a repository, never a group page, so a
trailing slash is punctuation. `group/sub/` therefore reads as repo `sub` in
group `group`, not as an empty-project subgroup.

Fixing it surfaced a third defect in both parsers: `.git` was stripped from the
whole path before the slashes were normalised, so `owner/repo.git/` kept the repo
named `repo.git` and every API call 404d. GitHub had this one too. Both now strip
`.git` from the repo component.

All forges now share one rule. Leaving GitHub stricter than GitLab was the first
plan, on the grounds that its prevalent case already worked — but that is the
cross-forge split this fix exists to remove, reproduced in miniature, and two
parsers reading the raw path independently is how they drifted apart to begin
with. Both now go through `path_components` and `strip_git_suffix`.

**This is not purely additive, which an early draft of this entry claimed.** The
reasoning was "every shape whose behaviour changed was previously failing, so
nothing that worked before works differently" — false. `resolve_remote` errors
when it finds MORE than one forge remote, so a repo whose second remote was
previously ignored *because its URL was malformed* now has two recognised
remotes and starts failing with `multiple forge remotes found ... use --remote`.
A mirror added with a trailing slash is enough. Narrow, but it is a real way a
working setup breaks, and it is why this went out as a minor rather than a patch.

Verified these are functional remotes rather than merely parseable strings, which
is the premise the whole fix rests on. `git ls-remote` fetches all of
`gitlab.com/gitlab-org/gitlab-runner/`, `gitlab.com//gitlab-org/gitlab-runner`,
and `github.com/rust-lang/log/`. So a user can have any of them configured, `jj
git push` works, and jjpr was the only thing refusing. GitLab redirects the first
two to `…/gitlab-runner.git/` — `.git` followed by a trailing slash, i.e. the
third defect is not hypothetical; the forge's own redirect emits that form.

## Fixed: the change graph keyed by change id (2026-08-01)

`build_change_graph_from` and `traverse_and_discover_segments` keyed
`adjacency_list`, the segment map and `fully_collected` by **change id**. A
divergent change is one id on two commits, and both copies can occupy a single
ancestry chain, so two genuinely distinct segments collapsed onto one key.

Measured before the fix, on `cx2(change_a) -> cx1(change_b) -> cy1(change_a)`:

```
ADJ:    {"change_a": "change_b"}
STACKS: 1 stack
  segments: [["bm_y"], ["bm_a1", "bm_x"]]
```

`bm_a1` (on `cy1`) and `bm_x` (on `cx2`) were reported as one segment despite
sitting at opposite ends of the chain.

**Fixed by keying on commit id** throughout the graph and traversal. The two
keyings are isomorphic whenever change ids are unique, so for every
non-divergent repo this is a no-op — the whole suite passed unchanged across the
switch, which is the evidence for that. It also preserves the case change-id
keying was really serving: two bookmarks on one commit still form one segment,
because they share a commit id.

`analyze.rs` matched the submit target by change id too, and now matches by
bookmark **name** (unique) and working-copy ancestry by **commit** id.

Policy, decided rather than inherited: a divergent change with both copies in
the stack now makes `submit` and `watch` refuse before pushing anything, scoped
to the stack being submitted (an unrelated divergent change elsewhere does not
block). `merge` already refused via its repo-wide gate. `status` never refuses —
it marks the segment `?? divergent`, since it is where a user diagnoses this.

**Reachability, measured.** The shape needs two copies whose diffs DIFFER but do
not conflict. Identical diffs cannot stack at all: jj answers a rebase of one
onto the other with "Abandoned 1 divergent commits that were already present in
the destination". So a racing restack — which produces identical diffs — yields
sibling divergence that never forms this shape, and an earlier note here saying
otherwise was wrong. Same-file/different-content copies do stack but conflict,
and jjpr's conflict check refuses first (found by driving the real binary in
e2e). What reaches the divergence refusal is the clean case: one copy adding a
file the other does not.

Covered end to end by `divergent_change_refuses_before_pushing_all_forges`
(`tests/forge_e2e.rs`), which drives the real binary against all three sandbox
forges and asserts nothing was pushed.

`would_close_cycle` is retained as defence in depth: with unique commit ids and
DAG ancestry a cycle should be unreachable, and the guard makes that a checked
property rather than an assumption.

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

**Caveat found 2026-08-01, low priority: `is_rooted_in` cannot answer when the
rebase root is divergent.** It builds `parents({root}) ~ ::{base}` with the root
written as a bare change ID, and jj refuses to resolve a divergent symbol
(`Error: Change ID <x> is divergent` — verified against a real divergent repo).
The call site swallows that with `.unwrap_or(false)`, so an *unanswerable* query
becomes a confident "not rooted" — the destructive direction, since that branch
rebases and force-pushes.

**Not reachable today**, and the first draft of this entry wrongly said it was.
`reconcile_local_state` opens with a repo-wide `divergent()` gate that fails
safe and returns before `is_rooted_in` is ever called
(`preexisting_divergence_short_circuits_before_any_later_jj_call` proves it by
panicking on every later jj call). So the headline result above stands as
written; this is only a latent sharp edge.

It matters for merge-async because the cascade design wants to call
`is_rooted_in` in situations the current entry gate does not cover. Two things
to decide then, both design calls rather than tidying:

- Wrapping the root in `change_id()` (as `fix(merge)` now does for the conflict
  screen) makes the query answerable, but with divergence it answers from
  *both* copies — which may not be the semantics the cascade wants.
- `.unwrap_or(false)` conflates "definitely not rooted" with "couldn't tell".
  For the cascade these want different handling: the first is "rebase and
  repair", the second is "stop and tell the user".

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

- ~~Wire up `Forge::native_stacks`~~ **DONE, differently.** `status` now flags
  native-stack membership from the `stack` object embedded in each PR payload,
  which is free, so it needed no extra call. `native_stacks` itself was
  replaced by `get_stack(owner, repo, stack_number)`: the merge pre-flight
  wants one stack, not a listing, and already knows which from
  `PullRequest::stack`. The wrong 404 comment is corrected too.
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
