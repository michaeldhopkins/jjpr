# GitHub native pull request stacks

A reference on the state of GitHub's native stacked-PR feature and what it
means for third-party stack management tools (Graphite, jjpr, ghstack, spr,
Sapling, etc.). Updated periodically; see [Changelog](#changelog) for the
delta between visits.

This file lives in the [jjpr](https://github.com/michaeldhopkins/jjpr) repo
but the content is tool-agnostic.

## Status — verified 2026-07-30

**Three of the four blockers from the previous visit are gone.** GitHub
[announced public preview on 2026-07-30](https://github.blog/changelog/2026-07-30-stacked-pull-requests-are-now-in-public-preview/).
The waitlist, the plan gating, and — decisively — the merge block have all
been lifted, and the REST schema has been published to the canonical
`docs.github.com` reference. The CLI still ships as a separately-installed
`gh` extension (`github/gh-stack`, now v0.1.0), not a built-in `gh pr`
subcommand.

What changed since 2026-07-22, each verified live (see
[Live verification 2026-07-30](#live-verification--public-preview-2026-07-30)):

1. **Merging via public API now works.** `gh-stack` v0.1.0 (2026-07-29) added
   `gh stack merge`, backed by a new public **asynchronous merge API**:
   `PUT /repos/{o}/{r}/pulls/{n}/merge-async` + `GET .../merge-async/{uuid}`.
   Verified end-to-end with an ordinary `repo`-scoped OAuth token. This
   retires the "merge is browser-only" finding entirely.
2. **Gating is gone.** `/stacks` returns `200` on a **personal Free-plan
   private repo** — no waitlist, no Team/Enterprise requirement. A stack was
   created and merged there. (The roadmap issue still says "Preview" and
   still lists only Team/Enterprise; it is stale — trust the live API.)
3. **Canonical publication happened.** `docs.github.com/en/rest/pulls/stacks`
   is live (was a 404), the async merge endpoints are in
   `docs.github.com/en/rest/pulls/pulls`, and the stacks endpoints appear on
   the GitHub App permissions reference under the ordinary **Pull requests**
   permission — no dedicated scope. This was the stability signal the
   previous entry was waiting for.

What did **not** change — and these are now the whole story for jjpr:

4. **Linear-only.** Diamonds/siblings still rejected (`422`). jjpr's diamond
   support remains a genuine differentiator with no native representation.
5. **The server-side cascade rebase still diverges remote branches from what
   jj pushed.** Re-verified: after a partial merge, the surviving PR's branch
   was force-moved to a GitHub-created commit. This is unchanged, and with
   merge no longer the blocker, **it is now the sharpest remaining problem**
   for a jj tool.

The single most actionable consequence, independent of whether jjpr ever
*creates* native stacks: because every repo now has the feature, a user can
stack jjpr's PRs with `gh-stack` at any time, and jjpr's own
`PUT /pulls/{n}/merge` will `403` on them. jjpr should detect stack
membership (`stack != null` on the PR payload) and route to `merge-async`.
That is a small, well-specified change and it is no longer blocked on
anything. See [Potential route for jjpr](#potential-route-for-jjpr).

## Roadmap and rollout

| Field | Value | Source |
|---|---|---|
| Announcement | [Public preview, 2026-07-30](https://github.blog/changelog/2026-07-30-stacked-pull-requests-are-now-in-public-preview/) | GitHub Changelog |
| Roadmap issue | [github/roadmap#1218](https://github.com/github/roadmap/issues/1218) | GitHub Roadmap |
| Status | **Public preview** (not GA) | Changelog |
| Rollout | "rolling out to all repositories over the coming days" | Changelog |
| Merge queue | Rolling out "progressively over the coming weeks" — **not yet everywhere** | Changelog |
| Waitlist | **Gone** — no signup required | Verified live |
| Plans | Works on **personal Free** repos (verified live) | Live probe |
| Docs | `gh.io/stacks`; feedback at `gh.io/stacks-feedback` | Changelog |
| Stack size limit | **100 PRs** per stack | gh-stack FAQ |

**The roadmap entry is stale.** As of 2026-07-30 it still reads "Feature
phase: Preview" with only GitHub Team and GitHub Enterprise plan labels, and
is listed "Up Next". The live API contradicts it: a personal **Free**-plan
private repo returns `200` from `/stacks` and merged a stack successfully.
The canonical docs page is published under `free-pro-team@latest` (plus
Enterprise Cloud and Enterprise Server 3.17–3.21), which corroborates Free/Pro
availability. Trust the API and the docs versioning over the roadmap labels.

The [community feedback discussion](https://github.com/orgs/community/discussions/201439)
is the liveliest source of real-world breakage. Recurring reports as of this
visit: merge-queue repos still getting `404`s (consistent with the staged
rollout), conflicts not surfacing in the UI until after a rebase, and merge
buttons staying green when a rebase is actually required. GitHub staff
engagement in the thread is minimal. Open user requests with no official
answer include CLI merge visibility of approvals, forced sequential merging,
cross-repository (fork) stacks, and PAT compatibility.

## CLI: `github/gh-stack` extension

Distributed as a `gh` extension, not bundled into `gh`. The
[cli/cli](https://github.com/cli/cli/releases) core (latest v2.96.0,
2026-07-02 — unchanged since the last visit) has **not** absorbed a built-in
`gh pr stack` / `gh stack` subcommand; `pkg/cmd/pr/` contains no `stack`
directory. The extension is still the only CLI surface.

`gh extension search stack` also surfaces unrelated third-party extensions
(`boneskull/gh-stack`, `VladimirAnaniev/gh-stack`, `ThePlenkov/gh-stackx`,
`134130/gh-domino`, `harsh183/gh-chain`) that predate the native feature and
implement their own stacking. Only `github/gh-stack` drives the native API.

```
gh extension install github/gh-stack
```

| Field | Value | Source |
|---|---|---|
| Repository | [github/gh-stack](https://github.com/github/gh-stack) | GitHub (not archived, actively maintained) |
| Latest release | v0.0.8 (2026-07-15) | [Releases API](https://api.github.com/repos/github/gh-stack/releases) |
| Documentation | https://github.github.com/gh-stack/ | GitHub |

### Release trail since the last visit

| Version | Date | Notable |
|---|---|---|
| v0.0.5 | 2026-05-26 | Multi-platform builds |
| v0.0.6 | 2026-06-15 | Rebase without trunk flag; PR-URL args; **PAT-auth warning** |
| v0.0.7 | 2026-06-30 | Interactive submit TUI; light/dark theming; adopt existing remote stacks |
| v0.0.8 | 2026-07-15 | **Migrated to the public Stacks REST API**; stack numbers are primary IDs; `link` appends to a stack by number; append-only updates |
| v0.1.0 | 2026-07-29 | **`gh stack merge` — the merge block is lifted.** Atomic partial-stack merge via the new async merge API; interactive wizard + `--yes`/`--merge`/`--squash`/`--rebase` for headless use; auto-routes to direct merge or merge queue. Also fixes: `link` ignored the repo default branch; `rebase`/`sync` could report success on a stale trunk; amended parent commits replayed into child branches |

### Subcommands

Verify against the extension's `--help` output and source on each
re-research; this list lags the repo. The documented verb set has grown to
include `init`, `add`, `checkout`, `modify`, `sync`, `rebase`, `unstack`,
and navigation verbs alongside the originals.

| Subcommand | Purpose | State (v0.0.8) |
|---|---|---|
| `gh stack submit` | Create or update PRs and link them as a Stack | Implemented |
| `gh stack view` | Show the current stack and its PR states | Implemented |
| `gh stack link` | Append existing PRs into a stack (by stack number) | Implemented |
| `gh stack rebase` / `sync` | Rebase/sync the stack against trunk | Implemented |
| `gh stack unstack` | Remove unmerged PRs from a stack | Implemented |
| `gh stack merge` | Merge a stack, or part of one | **Implemented (v0.1.0)** — see [Async merge API](#async-merge-api--the-merge-block-is-lifted) |

## API

The REST API is now published in the **canonical**
[docs.github.com/en/rest/pulls/stacks](https://docs.github.com/en/rest/pulls/stacks)
reference (it 404'd at the last visit), alongside the
`github.github.com/gh-stack` Pages site
([reference/rest-api](https://github.github.com/gh-stack/reference/rest-api/),
[reference/merge-api](https://github.github.com/gh-stack/reference/merge-api/)).
The async merge endpoints are documented under
[rest/pulls/pulls](https://docs.github.com/en/rest/pulls/pulls) ("Merge a pull
request asynchronously", "Get the result of an asynchronous merge").

GraphQL still exposes read-only stack *types* only — `PullRequestStack`,
`PullRequestStackEntry`, `PullRequestStackEntryConnection`,
`PullRequestStackEntryEdge` — and **no** stack or async-merge mutation
(schema introspection of `Mutation` for `/[Ss]tack|MergeAsync/` returns
nothing). REST is the only write surface.

**GitHub App permission**: resolved. The stacks endpoints are listed on the
[App permissions reference](https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps)
under the ordinary repository **"Pull requests"** permission. There is no
dedicated stacks scope.

### Endpoints

| Method | Path | Purpose | Success |
|---|---|---|---|
| GET | `/repos/{owner}/{repo}/stacks` | List stacks (newest first; `pull_request`, `per_page`, `page` query params) | 200 |
| GET | `/repos/{owner}/{repo}/stacks/{stack_number}` | Get one stack | 200 |
| POST | `/repos/{owner}/{repo}/stacks` | Create from an ordered PR-number list (bottom → top) | 201 |
| POST | `/repos/{owner}/{repo}/stacks/{stack_number}/add` | Append PRs to the top | 200 |
| POST | `/repos/{owner}/{repo}/stacks/{stack_number}/unstack` | Remove unmerged PRs (200 if stack survives, 204 if it dissolves) | 200 / 204 |
| GET | `/repos/{owner}/{repo}/pulls[/{n}]` | Existing PR endpoints, now embedding a `stack` object | 200 |

`404 Not Found` is returned when the feature is disabled for the repo — this
doubles as the capability check. Since public preview this should be rare;
every repo probed on 2026-07-30 returned `200`.

`POST /stacks` is **strictly typed**: `{"pull_requests": ["220"]}` (strings)
is rejected `422` with `` `"220"` is not of type `integer` ``. Send real JSON
integers.

### Async merge API — the merge block is lifted

The headline change of 2026-07-30. Merging a stacked PR is now possible
through a **public, canonically-documented REST API**. Because a stack merge
can span several PRs and take minutes, it runs in the background: you submit,
then poll.

| Method | Path | Purpose |
|---|---|---|
| PUT | `/repos/{o}/{r}/pulls/{n}/merge-async` | Submit a merge of PR `n` **and everything below it in the stack** |
| GET | `/repos/{o}/{r}/pulls/{n}/merge-async/{uuid}` | Poll the result |

This is the **required** path for stacked PRs — the legacy synchronous
`PUT /pulls/{n}/merge` and the `mergePullRequest` GraphQL mutation still
refuse them (re-verified: `403`, message unchanged and now stale, still
saying "Use the web interface instead").

Submit body — all fields optional:

| Field | Values | Notes |
|---|---|---|
| `merge_method` | `merge` \| `squash` \| `rebase` | Defaults to a merge commit. Not supported with `merge_queue` |
| `merge_action` | `default` \| `direct_merge` \| `merge_queue` | `default` (= omitted) auto-routes: direct merge, or the base branch's queue if it requires one |
| `commit_title`, `commit_message` | string | Not supported with `merge_queue` |
| `sha` | string | **Optimistic-concurrency guard** — the merge is rejected unless the PR head still matches |

Submit responses: `202` pending (carries the `uuid` to poll), `200` already
merged, `409` a request already exists (**the body carries the existing
`uuid`**), `400` not mergeable (closed/draft), `404` unavailable, `422` body
validation failure.

Poll responses are always `200` for a valid uuid, carrying
`status` ∈ `pending` | `merged` | `enqueued` | `failed` and a polymorphic
`details` object (`uuid`/`merge_method`/`merge_action`/`expected_head_sha`
while pending; `sha` when merged; `message` always). Results are retained
**24 hours**, then the uuid `404`s. GitHub's docs suggest polling once a
second.

Semantics that matter:

- **Atomic — on a direct merge.** Either every PR up to and including the
  target lands, or none does. **Not so under a merge queue**: the CLI
  reference says queued members "may land in separate groups rather than all
  at once", and an explicit merge method is ignored (the queue picks).
- **Partial merges are first-class.** Targeting the middle PR of a 3-stack
  merges the bottom two and leaves the top — verified.
- **Only basic PR state is checked at submit** (open, not draft). Branch
  protection and rulesets are evaluated when the merge *runs*, so a rule
  failure arrives as a `failed` **poll result**, not a submit error.
- **Bypassing merge requirements is not supported** — admin override cannot
  force a stack through its rules.
- **Auto-merge is not supported** on a stacked PR.

### Schema

Stack resource:
`id`, `number`, `node_id`, `url`, `base.ref`, `open` (bool),
`created_at`, `pull_requests[]`. Each PR entry: `number`, `state`,
`draft`, `merged_at`, `head.ref`, `head.sha`.

`stack` object embedded on a pull request:
`stack.id`, `stack.number`, `stack.size`, `stack.position` (1-based,
1 = bottom), `stack.base.ref` (the stack's ultimate target),
`stack.base.sha`.

### Live verification (2026-07-21, read-only)

Probed the live endpoints against a preview-enabled org (`MerchantsBonding`,
read-only GETs, no writes). Findings that the Pages docs don't state:

- **Auth**: a standard OAuth token with the `repo` scope works (200). The
  response carries `X-Accepted-Oauth-Scopes:` **empty** — no stack-specific
  scope is required; ordinary repo read access is sufficient.
- **PAT auth is accepted by the endpoint** (the CLI's anti-PAT warning is a
  CLI-flow choice, not an API restriction). Tested a fine-grained PAT: it
  authenticates (`GET /user` → the owning user) and reads a repo it has
  access to (`GET /repos/{owner}/{repo}` → 200). On that repo's `/stacks` it
  returns the **same 404 as OAuth** on a non-preview repo — *not* a 401/403.
  A categorical PAT rejection would surface as 401/403 on a PAT-visible repo;
  the identical not-enabled 404 shows the endpoint processes the PAT the same
  as OAuth. **Not yet confirmed**: a literal 200 from `/stacks` with a PAT on
  a *preview-enabled* repo — no PAT on hand could see an enabled repo (fine-
  grained PATs are per-owner and need the org to approve access to the
  enabled repo). jjpr's token path (ureq + `Authorization: Bearer`) is the
  exact mechanism exercised here.
- **API version**: served on the standard `X-Github-Api-Version-Selected:
  2022-11-28`; no special version header or `Accept` media type needed
  (`application/json` works; `github.v3` returned).
- **Rate limit**: the ordinary `core` bucket (5000/hr) — no separate pool.
- **Enablement granularity**: the entire `MerchantsBonding` org returned
  `200` (every repo probed), suggesting org-level, not per-repo, enablement
  in practice — though the roadmap still describes it as per-repo.
- **404 body cites a canonical docs path**: a disabled repo returns
  `{"message":"Not Found", "documentation_url":
  "https://docs.github.com/rest/pulls/stacks#list-pull-request-stacks", ...}`.
  That `docs.github.com/rest/pulls/stacks` page 404'd at the time — the
  prediction that publication was imminent proved correct; it went live by
  2026-07-30.
- **Single-stack GET is much richer than the list.** `GET /stacks/{n}`
  embeds near-full PR objects per entry: `url, id, number, node_id, title,
  state, draft, merged_at, html_url, user{login,...}`, and crucially
  **`head.{ref,sha,repo}` and `base.{ref,sha,repo}` per PR**. Because each
  PR's `base.ref` is the previous PR's `head.ref`, the entire linear chain
  (and its trunk target) is derivable from this one call — no need to fetch
  each PR separately. The list endpoint (`GET /stacks`) omits per-PR `base`
  and the user object; use it for the cheap capability probe, the single-GET
  for graph reconstruction.
- **Embedded `stack` on a PR** confirmed exactly as documented:
  `stack.position` is 1-based bottom-to-top (bottom PR = 1), `stack.size` is
  the PR count, `stack.base.{ref,sha}` is the ultimate trunk target. It omits
  `node_id`/`created_at`/`url` (leaner than the resource).
- **Stack numbers share the repo's issue/PR sequence** (observed stack
  `16305`/`16306` interleaved with PRs `16298`–`16304`); a created stack
  consumes a number. `node_id` prefix is `PRS_` (Pull Request Stack).

### Write endpoints — verified live (2026-07-21)

Exercised the write side end-to-end in a dedicated preview-enabled sandbox
(`MerchantsBonding/stacks-testing`), building a linear 3–4 PR stack via the
Git Data + Pulls APIs, then driving the stack endpoints. Request bodies and
behaviors, all confirmed against live responses:

- **`POST /stacks`** — body `{"pull_requests": [<numbers, bottom→top>]}`.
  Returns `201` with the stack (its `number` is drawn from the repo's
  issue/PR sequence — e.g. PRs 1–3 yielded stack 4).
- **`POST /stacks/{n}/add`** — same body `{"pull_requests": [...]}`. Returns
  `200`, appends to the top.
- **`POST /stacks/{n}/unstack`** — the `pull_requests` body is **ignored**;
  the operation always tries to remove every member. Naming only the top PR
  of a 3-PR stack returned `204` and removed all three. It does **not**
  retarget PR bases. "Remove one PR" is not available.
  (Re-verified and refined 2026-07-30 — see
  [Unstack semantics](#unstack-semantics--re-verified-2026-07-30) for the
  `200` vs `204` distinction and the merged-PR case, which is what the docs'
  "200 if the stack survives" line refers to.)

> **SUPERSEDED 2026-07-30.** The merge block described in the rest of this
> section was lifted at public preview. `PUT /pulls/{n}/merge-async` now
> merges stacked PRs through the public API — see
> [Async merge API](#async-merge-api--the-merge-block-is-lifted). What is
> still accurate here: the *legacy* `PUT /pulls/{n}/merge` continues to `403`
> on a stacked PR, and the cascade-rebase description below still holds
> (it now also follows an API merge, not only a web/queue one). Retained as a
> dated record.

**The critical finding (as of 2026-07-21) — merging a stacked PR via the API
is blocked.**
`PUT /repos/{o}/{r}/pulls/{n}/merge` on a PR that belongs to a native stack
returns `403`:

> `Merging stacked PRs via this API is not supported. Use the web interface instead.`

The block is tied to stack membership: after `unstack`, the same
`PUT /pulls/{n}/merge` succeeded (`{"merged": true}`).

**No public API can merge a stacked PR, by any route** (verified 2026-07-21,
with branch protection + a merge queue configured on the sandbox):

- Direct PR merge (`PUT /pulls/{n}/merge`) → `403` (above).
- Merge queue: `enqueuePullRequest` (GraphQL) on a stacked PR is rejected with
  *"This pull request is part of a stack and must be merged sequentially using
  the stack merge API."* So the earlier "merge queue is compatible" note (from
  GitHub's FAQ) does not mean a tool can enqueue a stacked PR directly.
- No stack-merge REST endpoint: `POST /stacks/{n}/merge` and variants → `404`;
  the public reference documents only 5 endpoints (list/get/create/add/unstack).
- No GraphQL stack-merge mutation: schema introspection finds stack *types*
  (`PullRequestStack`, `PullRequestStackEntry`) but no merge mutation.
- The official `gh-stack` extension has no `merge` command, and its API client
  (`internal/github/github.go`) calls only those same 5 endpoints.

The "stack merge API" the error names is GitHub's **internal, web-UI-only**
endpoint (what the merge button calls), not part of the public surface.
Merging a stacked PR is browser-only during the preview. For jjpr this is
final: native-stack support cannot include merge; a tool can push/create/read
and must hand the merge to the web UI.

**Merging the bottom PR triggers a cascading server-side rebase of the whole
remaining stack.** Verified via two web-UI merges. Merging the bottom PR:

- **Retargets the next PR's base** to trunk (`feat-a` → `main`), and
- **rebases every remaining branch** onto the new trunk tip. Both descendants
  got new head commits (`feat-b` `9fc37bb`→`c020b2f`, `feat-c` rebased so its
  parent is the new `feat-b`), and every PR stayed `mergeable_state: clean`
  with only its own one-file diff. The cascade goes all the way up, not just
  one level; the stack never goes stale.

This is a real rebase, not just a base-pointer move, and it happens
**regardless of merge method**. The bottom PR here merged as a *merge commit*,
which left its commit an ancestor of `main` — a pure retarget would have
sufficed — yet GitHub still rebased the descendants to fresh SHAs. (An earlier
note claimed a "clean pointer move, no rebase"; that was an under-observation
— head SHAs were not checked. The behavior is a rebase.)

- The merged PR **stays a member of the stack** (as `closed`/merged). The
  stack does **not** shrink or reindex — `size` stayed 3 and the remaining
  PRs kept their `position` values. The stack stays `open: true` until every
  member merges, then flips to `open: false`.
- **Review decisions survive the rebase — with no dismiss-stale rule.** With a
  real second reviewer and no branch protection: `APPROVED` on the middle PR
  and `CHANGES_REQUESTED` on the top PR were both preserved across the
  merge-and-rebase (`reviewDecision` unchanged); the approval even
  re-associated to the rebased head commit. Under a
  `dismiss_stale_reviews_on_push` rule the outcome instead depends on the
  bottom PR's merge method — see "Rebase-heavy stacking is hard on reviews" in
  the Lessons section.

Two catches for a tool:

1. This only follows a **web-UI or merge-queue** merge. API merge is blocked
   for stacked PRs (above), so a tool cannot trigger any of it through its own
   merge call.
2. **The server-side rebase diverges the remote branches from what was
   pushed.** After the merge, `feat-b`/`feat-c` point at SHAs GitHub created,
   not the ones jjpr/jj pushed. This collides with jj's model, where the local
   tool owns and rewrites commits: the next `jj git fetch` sees the bookmarks
   moved to commits jj doesn't have, and jjpr would have to reconcile (adopt
   GitHub's rebased commits, or re-push its own and undo GitHub's rebase). This
   reconciliation burden may matter more than the merge block, and needs a
   deliberate design answer before native linking is viable for jjpr.

### Stack behaviors — verified live (2026-07-21, sandbox)

Ran a batch of edge-case probes in `MerchantsBonding/stacks-testing`. All
directly shape a jjpr integration:

- **Force-push / commit rewrite is tracked — per branch.** Amended a member
  branch to a new SHA and force-updated the ref. After a few seconds'
  propagation, the PR head and the stack's entry for it both followed to the
  new SHA; membership and order held. Constant commit rewriting and jjpr's
  force-pushes are **compatible** with native stacks — the stack tracks each
  branch's head.
- **But the stack does NOT auto-rebase descendants.** Rewriting a *middle*
  commit (`feat-b`) left the branch above it (`feat-c`) based on the now-orphaned
  old commit — PR went `mergeable_state: dirty`, yet stayed a stack member. The
  server tracks head SHAs; it does not rebuild descendants. Keeping the stack
  coherent through a rewrite is the tool's job: rebasing `feat-c` onto the new
  `feat-b` (what `jj rebase` + jjpr's force-push of every affected branch does
  automatically) cleared the conflict and kept the stack intact. This is a
  point in jjpr's favor — jj rebases descendants for you; the native feature
  alone does not, and `gh-stack`'s own `rebase`/`sync` verbs are client-side
  git for exactly this reason.
- **Order is not inferred — the caller must sort.** `POST /stacks` with PRs out
  of base-chain order (`[13,11,12]`) is rejected `422` ("each PR's base ref is
  the previous PR's head ref"). jjpr must pass PRs bottom→top, already
  topologically sorted.
- **Non-linear sets are rejected.** Two sibling PRs both based on `main` → the
  same `422`. Diamonds/siblings cannot be registered; the API enforces a strict
  linear chain. jjpr's diamond shapes have no native representation.
- **Creating a stack is footprint-clean.** After `POST /stacks`, the member PRs
  had unchanged bodies, and no added labels, reviewers, comments, reviews, or
  checks — only `updated_at` bumped. The stack link is server-side metadata
  rendered by the UI, **not** written into PR bodies, so it does not collide
  with jjpr's own PR-body navigation comments (they are orthogonal; jjpr would
  simply choose to stop writing its comment).
- **Non-default base works.** A stack based on a `release` branch (not `main`)
  created fine, `base.ref: release`. Stacks can target any trunk.
- **Review/reviewer endpoints are NOT stack-blocked** (unlike merge). On a
  stacked PR, `GET .../reviews`, `GET/POST .../requested_reviewers`, and
  submitting a `COMMENT` review all behave normally (a request for the author
  returns the ordinary author-rejection `422`, not a stack `403`). The stack
  payload carries no review/approval fields, so jjpr keeps reading reviews
  per-PR exactly as today. The merge `403` is a narrow special case, not a
  general restriction on stacked-PR operations.

Since resolved with a second reviewer: approval survival across the merge
rebase (depends on merge method under dismiss-stale — see Lessons) and per-PR
review gating (confirmed, below). Still open: CODEOWNERS interaction, and
whether the web/queue merge refuses to start until the whole stack is green.

### Live verification — public preview (2026-07-30)

Built and merged a real 3-PR linear stack in `michaeldhopkins/forge-e2e-sandbox`
(a **personal, Free-plan, private** repo — itself the proof that gating is
gone) using an ordinary `gho_` OAuth token with the `repo` scope. Branches
`ns0730-a/b/c`, PRs 220/221/222, stack **223**. Everything below is a live
observation, and the sandbox was cleaned up afterwards.

**Setup.** `POST /stacks {"pull_requests":[220,221,222]}` → `201`, stack
number 223 drawn from the repo's issue/PR sequence, `node_id` prefix `PRS_`,
`open: true`, per-PR bases chained `main` ← `ns0730-a` ← `ns0730-b`. Matches
the previously-recorded shape exactly.

**Legacy merge paths still refuse stacked PRs.**
`PUT /pulls/220/merge` → `403 "Merging stacked PRs via this API is not
supported. Use the web interface instead."` The message is now misleading —
the web interface is no longer the only option — but the block itself is
intact and is what jjpr will hit today.

**Partial stack merge, end to end.** `PUT /pulls/221/merge-async` (the
**middle** PR) with `{"merge_method":"merge","merge_action":"default"}` →
`202 pending`. Polling returned `merged` on the second poll, roughly four
seconds later. Result:

- PRs **220 and 221 both merged**, `merged_at` one second apart — an atomic
  two-PR landing from a single request.
- Trunk received **one merge commit**, not one per PR: `main` went
  `c1438a3b` → `c42b50a9`, whose parents are the old trunk and `ns0730-b`'s
  tip. Because `ns0730-b` already contained `ns0730-a`'s commit, a single
  merge commit credits both PRs.
- PR **222 was retargeted straight to `main`**, skipping the intermediate
  merged branch entirely (`base: ns0730-b` → `base: main`) — the retarget
  jumps levels rather than walking down one.
- PR 222 was **rebased server-side**: head `19f7eb4c` → `fa1081de`, a commit
  GitHub authored (same author and author-date, new committer date),
  reparented onto the new trunk tip.
- The stack kept `size` 3 and every `position`; merged members stay in it.

**The divergence is re-confirmed, and it is the live problem for jjpr.** The
original pushed commit `19f7eb4c` still exists as a git object but is no
longer what `refs/heads/ns0730-c` points at — GitHub force-moved the branch to
its own rebased commit. This is exactly the reconcile burden flagged
previously, and it fires on the *merge-commit* path where the descendant did
not strictly need rebasing (its parent `0d136902` is an ancestor of the new
trunk). GitHub rebases regardless of merge method.

**`merge-async` also works on non-stacked PRs.** PR 224 (`stack: null`,
plain PR targeting `main`) submitted fine and merged with `merge_method:
squash`. The endpoint is a general asynchronous merge, not a stack-only one —
which means jjpr *could* use a single merge path for both cases, though it
does not have to.

**Validation and failure behavior** (probed against PR 222):

| Probe | Result |
|---|---|
| `merge_method: "fast-forward"` | `422` — "Must be one of: merge, squash, rebase" |
| `merge_action: "teleport"` | `422` — "Must be one of: default, direct_merge, merge_queue" |
| `sha` set to a stale head | `400`, body `{"status":"failed","details":{"message":"Pull request head branch was modified."}}` |
| `merge_action: "merge_queue"` on a repo with no queue | **`202` accepted**, then `failed` at poll: "Cannot perform a merge queue merge for a branch with no merge queue" |
| Second submit while one is pending | `409`, body carries the **existing** request's `uuid`, `merge_method` and `expected_head_sha` |
| Poll a terminal uuid again | Same terminal result — idempotent |
| Poll a bogus uuid | `404` |

Two corrections to `gh-stack`'s own source comments, both of which a jjpr
implementation should not inherit:

1. `merge_async.go` claims "the server rejects `merge_queue` on a branch with
   no queue rather than silently merging directly." It does reject, but
   **asynchronously** — submit returns `202`, and the rejection only appears
   at poll time. A caller cannot learn the outcome from the submit status
   code; it must poll. (Nothing was merged, so atomicity held.)
2. `classifyAsyncMergeError` notes the `409`'s existing uuid "isn't recovered
   here" because go-gh discards non-2xx bodies. That is a limitation of
   `gh-stack`'s HTTP client, not the API — **the `409` body does contain the
   uuid**. jjpr reads bodies with `ureq` and can recover and resume the
   in-flight merge instead of erroring.

**Lifecycle close.** Merging the last PR (222, `squash`) flipped stack 223 to
`open: false`. It remains listed by `GET /stacks` as a closed stack.

**Merge queue** could not be exercised — the sandbox has no queue configured
(`enqueuePullRequest` → "Merge queues are not enabled"), and per the changelog
queue support is still rolling out. Community reports of `404`s on
queue-enabled repos suggest it is genuinely not everywhere yet.

### Unstack semantics — re-verified (2026-07-30)

The `gh stack unstack` CLI docs describe *partial* removal ("when some pull
requests remain stacked, the stack is kept"), which appeared to contradict the
2026-07-21 API finding that unstack dissolves everything. Both are correct;
they describe different mechanisms. Verified with two fresh stacks.

**The `pull_requests` request body is ignored.** On a 4-PR stack (225–228),
`POST /stacks/{n}/unstack` with `{"pull_requests":[228]}` — naming only the
top PR — returned `204` and removed **all four**; `GET /stacks/{n}` then
`404`s and every PR's `stack` is `null`. The original "all-or-nothing" note
was right. The canonical reference now documents the body as *None*, matching
this; an earlier revision of the Pages docs showed a `pull_requests` body.

**Merged PRs are pinned and cannot be removed** — this is the partial case.
On a 3-PR stack (230–232) with the bottom PR merged first, unstack returned
**`200`** (not `204`), and:

- the merged PR **stayed** in the stack (`#230`, now reported `position 1/1`),
- the stack **survived** with `open: false`,
- both open PRs (`#231`, `#232`) became `stack: null` — freed.

So the status code is meaningful and worth branching on:

| Code | Meaning |
|---|---|
| `204` | Every member removed; stack dissolved and now `404`s |
| `200` | Stack survives — it still holds PRs that cannot be removed (merged/merging/queued); the body is the surviving stack |

**Bases are still not retargeted by unstack.** After the run, `#232` kept
`base: um0730-f`. (`#231` showed `base: main`, but that was GitHub's cascade
retarget when `#230` merged, not an unstack effect.)

**Consequence for the Tier-1 remedy**: unstacking works even mid-merge. If
jjpr has already landed part of a stack and then the user unstacks, the
remaining open PRs are freed and jjpr's ordinary `PUT /pulls/{n}/merge`
works on them. The merged PRs staying pinned is cosmetic.

### No web URL for a stack

The stack resource carries only an **API** `url`
(`api.github.com/repos/{o}/{r}/stacks/{n}`) — there is **no `html_url`**. The
`stack` object embedded on a pull request is leaner still: exactly
`base{ref,sha}`, `id`, `number`, `position`, `size` — no URL of any kind.

`gh-stack` never constructs a stack web URL either: `/stacks/` appears in its
Go sources only in API paths and tests. So there is no documented
`github.com/{owner}/{repo}/stacks/{n}` page to link to, and inventing one
would be a guess that may 404.

For jjpr this means: **name the stack by number, and link the PR** (which
does have `html_url`). Do not synthesize a stack URL.

### Which write operations a native stack blocks (2026-07-30)

Probed every mutation jjpr performs, against a live 3-PR stack (240). Only one
is refused, which is a much narrower blast radius than expected:

| Operation | Stacked PR | Result |
|---|---|---|
| Force-push the branch | ✅ | `200`; the stack follows the new head |
| Create a PR based on a stacked branch | ✅ | `201`; the new PR lands *outside* the stack |
| `PATCH` body | ✅ | `200` |
| `PATCH` title | ✅ | `200` |
| Post an issue comment (stack nav) | ✅ | `201` |
| `PATCH` **base** (retarget) | ❌ | **`422`** |
| `PUT .../merge` | ❌ | `403` (see above) |

The retarget refusal:

> `422` — "Cannot change the base branch because the pull request is part of a
> stack."

Two details that matter for a tool:

- **It fires even for a no-op.** Setting the base to the value it already has
  is rejected the same way, so the block is on the field, not on the change.
  A tool cannot sidestep it by diffing first.
- **It is why `gh stack unstack` + `gh stack init` is the documented way to
  restructure.** While the PRs are stacked, the chain is immutable.
- **But GitHub itself can still retarget — the 422 binds API callers, not the
  server.** Two ways of removing the branch a stack is rooted on give opposite
  results, and the difference matters:
  - **A raw `DELETE /git/refs/heads/{branch}`** closes the stacked PR above it
    (`state: closed`, `dirty`). Nothing retargets it.
  - **GitHub's own `delete_branch_on_merge` cleanup**, run as part of merging
    the PR below the stack, **auto-retargets the stacked PR to trunk and leaves
    it open** (verified: base `dbm0730-x` → `main`, PR open, still stack member
    `1/2`, stack still open). The merge flow knows to move dependent PRs; a
    bare ref deletion does not.

  So a tool merging the PR beneath a native stack does **not** destroy it, even
  on a `delete_branch_on_merge` repo. (One cosmetic artifact: the stack
  resource's own `base.ref` still names the deleted branch afterwards.)

A native stack can be **rooted on a branch that is not itself in the stack**
(a legal non-default base). So one chain of PRs can be part unstacked, part
stacked, and a single chain can even span two separate native stacks. Any tool
reasoning about "is this stack native?" must handle per-PR membership rather
than assuming the whole chain matches.

For jjpr this is the whole submit story. Pushing rewritten commits to a
stacked PR is *allowed* and tracked, so ordinary submits (amend, re-push) work
untouched. Only a **shape change** — reorder, insert, or drop a bookmark —
makes jjpr want to retarget, and that is exactly when GitHub refuses. jjpr
detects it at plan time and refuses before pushing, because submit pushes in
phase 1 and retargets in phase 3: discovering it late would leave rewritten
commits pushed under a stack order that contradicts them.

### The cascade rebase, from jj's side — RESOLVED (2026-07-30)

The open design question was how jjpr should reconcile GitHub's server-side
cascade rebase, and what provenance it would have to track to know whether it
should be updating the server or the server updating it. Tested both directions
against a live stack. **jj already answers it; jjpr needs no provenance store.**

**Case 1 — no local changes (the normal partial merge).** Local bookmarks sat
exactly where they were last pushed. Merged a 3-PR stack up to the middle PR,
then `jj git fetch`:

```
bookmark: csc0730-c@origin [updated] tracked
Abandoned 1 commits that are no longer reachable:
  trwomyrs f8035b52 csc0730-c@git | feat: csc0730 c

csc0730-c: mrswzqso f31ba55f     (local, @git and @origin all agree)
```

jj **adopted the server's rebased commit and abandoned the local original, with
no conflict**. The change ID changed (`trwomyrs` → `mrswzqso`), because GitHub's
commit is new to jj, but nothing needed resolving. This is exactly "let the
cascade come to us", and it is the default behavior of a plain fetch.

**Case 2 — local unpushed changes (the collision).** Amended the top commit
locally without pushing, then triggered the same cascade and fetched:

```
col0730-c (conflicted):
  - ypottmow/2 831bef20 (hidden)                     base: what we last pushed
  + ypottmow  389c38ed  ...(locally amended)         ours
  + mspxrllt  d2832013                               theirs (server rebase)
  @origin: mspxrllt d2832013
```

jj raises a **conflicted bookmark** carrying all three sides. That is precisely
the three-way information needed to choose a direction, and jj computes it.

**Case 3 — unpushed descendant work (the common real case). This one loses
work, silently.** Same 3-PR stack, but with an ordinary unpushed WIP commit on
top of `c` (`jj new c`, edit a file), then the same partial merge and fetch:

```
Abandoned 1 commits that are no longer reachable:
  sxsqpklx be77a4bc wip0731-c@git | feat: wip0731 c
Rebased 1 descendant commits
Working copy (@) now at: mtwmmmzr 4d067879 wip: local work not yet pushed
Parent commit (@-): yqvszouw 72a3666b wip0731-b | feat: wip0731 b
Added 0 files, modified 0 files, removed 1 files
```

jj abandoned the superseded `c` and reparented the WIP onto **`b`**, the
abandoned commit's *parent* — not onto GitHub's replacement `c`. Verified
after the fetch:

- `@`'s parent is `72a3666b (wip0731-b)`, the now-merged b.
- The bookmark `wip0731-c` points at GitHub's new `ebe120c5`.
- The new `c` is **not** an ancestor of `@`.
- `wip0731-c.txt` is **gone from the working copy**.

So the user's in-progress work is silently detached from the stack and loses
the content of the commit it was built on. jj's behavior is mechanically
correct (reparent a descendant onto the abandoned commit's parent) but
semantically wrong here, because a replacement commit *does* exist and jj has
no way to know that GitHub's new `c` is the same logical change as the old one.

**This invalidates any "adoption is free" reading of Cases 1 and 2.** It is
free only when nothing local descends from the rewritten commit. jjpr has to
detect descendant work before or after the fetch and re-parent it onto the
adopted commit itself.

**Also: the cascade is asynchronous and lags the merge.** Immediately after
`merge-async` reported `merged`, `GET /pulls/{c}` still showed the *old* head;
the rebase appeared several seconds later. jjpr's post-merge reconcile fetches
right after merging, so it can easily fetch before the cascade lands, see
nothing changed, and then push its own rebase — racing GitHub. Any
implementation must wait for or re-check the cascade rather than assume the
merge result implies settled refs.

**What this means for jjpr:**

| jj state after fetch | Who changed it | jjpr's job |
|---|---|---|
| local == `@origin` | nobody | nothing |
| local ahead, fast-forward | us | push (today's behavior) |
| bookmark moved, old commit abandoned, **nothing descends from it** | the server | nothing — fetch adopted it correctly |
| bookmark moved, **local work descended from the old commit** | the server | **re-parent the stranded work onto the adopted commit** (Case 3) |
| bookmark **conflicted** | both | surface the three sides; do not guess |

Two jobs, then, not one:

1. **Stop jjpr overwriting the adoption.** The post-merge reconcile runs
   `jj rebase -s <root> -d <trunk>` and force-pushes unconditionally, which
   would rewrite the server's commit back to jj's own, undo the cascade, and
   dismiss any approval it preserved.
2. **Repair what the adoption strands.** Case 3 is the common shape (you keep
   working while the stack lands) and jj's default handling silently detaches
   and de-contents it. jjpr must notice descendants of the rewritten commit and
   rebase them onto the adopted one.

Job 2 is the part with no free lunch, and it is why "just let the cascade come
to us" is not a one-line change.

jjpr already parses conflicted bookmarks (`jj/templates.rs` has
`test_parse_bookmark_conflicted_skipped`), so the detection primitive exists.

### PAT verification — RESOLVED (2026-07-30)

The question open since 2026-07-21 — *does a personal access token get a `200`
from `/stacks` on an enabled repo?* — is **answered: yes.**

A fine-grained PAT (`github_pat_…`) against `michaeldhopkins/jjpr`:

```
GET /repos/michaeldhopkins/jjpr/stacks   ->  HTTP 200   []
x-accepted-github-permissions: pull_requests=read
x-github-api-version-selected: 2022-11-28
x-ratelimit-limit: 5000   x-ratelimit-resource: core
```

Same status, same API version, same `core` rate-limit bucket as the OAuth
token. The CLI's anti-PAT warning is a `gh-stack` flow choice, not an API
restriction. **jjpr's PAT-authenticating users are fine for the read path.**

A false lead worth recording so it isn't re-chased: the first probe used
`forge-e2e-sandbox` and returned `404` — but that PAT's repo selection does
not include the sandbox, and `GET /repos/{owner}/{repo}` on it *also* `404`s.
A `404` from `/stacks` means "no access to this repo **or** feature not
enabled"; the two are indistinguishable. Always confirm the token can see the
repo itself before reading anything into a `/stacks` 404.

**The `x-accepted-github-permissions` response header is the authoritative
permission source** — better than the docs page, and it comes back even on
error responses:

| Call | Required permission | Observed |
|---|---|---|
| `GET /stacks` | `pull_requests=read` | `200` with a read PAT |
| `POST /stacks` (create) | `pull_requests=write` | `403` — this PAT lacks it |
| `PUT /pulls/{n}/merge-async` | **`contents=write`** | `404` (nonexistent PR; no side effect) |

Two things to carry into an implementation:

- **`merge-async` requires `contents=write`, not a pull-request permission.**
  Non-obvious, and it differs from every other stacks endpoint. It is the same
  shape as the ordinary merge endpoint (merging writes to a branch), and any
  jjpr user who can push already has it — but a token minted for read-only PR
  work will fail, and the failure mode is a bare `404`, not a clear `403`.
- The `403` on `POST /stacks` is **this PAT's missing scope, not a categorical
  PAT block** — the message is GitHub's standard "Resource not accessible by
  personal access token" for a fine-grained token lacking a declared
  permission. Not proven to the same standard as the read path: it would take
  a PAT holding `pull_requests: write` on a throwaway repo to close properly.

### Still undocumented / unverified

Most of the original list is now resolved: a `repo`-scoped OAuth token works
for both read and merge, the API version is standard `2022-11-28`, rate limits
are the ordinary `core` bucket, the GitHub App permission is plain "Pull
requests", and the schema is canonically published. What remains:

- **PAT `200` on an enabled repo**: still not literally observed. All live
  work has used a `gho_` OAuth token; no PAT was available in this session.
  The evidence remains indirect (a PAT authenticates and gets the same
  not-enabled `404` as OAuth rather than a `401`/`403`). Users in the
  [community discussion](https://github.com/orgs/community/discussions/201439)
  are asking about PAT support for agent/automation use with no official
  answer, so treat it as genuinely open. This matters for jjpr: many users
  authenticate with a PAT via `GITHUB_TOKEN`. **Verify before shipping any
  native-stack code path.**
- **Merge queue interaction**: unexercised (no queue on the sandbox; feature
  still rolling out). The routing contract is documented but unverified, and
  the `enqueued` terminal status means the caller must track the queue
  separately for the final outcome.
- **Approval survival under the async merge.** The 2026-07-22 finding — that a
  merge-commit landing preserves an upstream approval while a squash landing
  dismisses it under `dismiss_stale_reviews_on_push` — was verified against a
  **web-UI** merge with a second reviewer. Whether the async merge API behaves
  identically is untested (no second reviewer available this session). It
  probably does, since it is the same server-side machinery, but the whole
  "jjpr gap" argument rests on it.
- **CODEOWNERS interaction** and whether a stack merge refuses to start until
  every member is green — still unverified.
- **Schema stability**: canonical publication is a real signal, but the
  feature is still labelled preview. Keep tolerating unknown/extra fields.

## Operations supported by the native feature

| Operation | Native today (preview) | Public API today | Notes |
|---|---|---|---|
| Create stack of related PRs | Yes (`gh stack submit`) | Yes (`POST /stacks`) | Each PR still represented individually; "stack" is a server-side grouping keyed by number |
| List / visualize stack | Yes (`gh stack view`, web UI map) | Yes (`GET /stacks`) | UI remains the primary consumption surface |
| Append PR to existing stack | Yes (`gh stack link`) | Yes (`POST /stacks/{n}/add`) | Append-only; adds to the top |
| Remove PR from stack | Yes (`gh stack unstack`) | Yes (`POST /stacks/{n}/unstack`) | Only unmerged PRs |
| Cascading rebase of the stack on merge | Yes — verified | Follows any merge, incl. the async API | Retargets the surviving PR's base straight to trunk (skipping levels) AND rebases remaining branches to fresh SHAs, regardless of merge method. Rebased SHAs diverge from what jj pushed |
| Merge a PR that's in a stack | Yes | **Yes — `merge-async`** | `PUT /pulls/{n}/merge-async` + poll (verified live). The legacy `PUT /pulls/{n}/merge` still `403`s on a stacked PR |
| Atomic partial-stack merge | Yes (`gh stack merge <pr>`) | Yes | Merges everything up to and including the target PR; all-or-nothing **on a direct merge** (verified live). Under a merge queue members may land in separate groups |
| Merge a *non*-stacked PR asynchronously | — | Yes | `merge-async` is a general endpoint, not stack-only (verified on a `stack: null` PR) |
| Stack-aware CI semantics | Yes (CI against final target) | — | Per the stacked-PRs guide |
| Auto-merge on a stacked PR | **No** | No | Explicitly unsupported |
| Bypass merge requirements as admin | **No** | No | Explicitly unsupported for stacks |
| Diamond / non-linear stack support | **No** | No | FAQ: "There must be a fully linear history between each of the branches in the stack" |
| Stack size | up to **100 PRs** | — | FAQ; split into multiple stacks beyond that |

## Merge queue and auto-merge interaction

Merge queue is documented as compatible with native stacks per the
[gh-stack FAQ](https://github.github.com/gh-stack/faq/): all PRs in the stack
enter the queue together in the correct order and are evaluated individually
from the bottom up. A tool **can** now drive this — not via
`enqueuePullRequest` (still rejected for stacked PRs) but through
`merge-async` with `merge_action: "merge_queue"`, or `"default"`, which routes
to the queue automatically when the base branch requires one. A queued stack
returns the terminal status `enqueued`; the caller must then track the merge
queue itself for the final outcome.

Caveats, both unresolved as of 2026-07-30: queue support is explicitly still
"rolling out progressively over the coming weeks", and users on queue-enabled
repos are reporting `404`s in the
[community discussion](https://github.com/orgs/community/discussions/201439).
This path is also **unverified locally** — the sandbox has no queue. Forcing
`merge_action: "merge_queue"` on a branch without a queue is accepted at
submit and fails only at poll time (verified).

PR auto-merge (GA, GraphQL `enablePullRequestAutoMerge`) is not
stack-aware. Enabling it on individual PRs in a stack delegates the merge
wait to GitHub but produces no stack-level orchestration.

Both are usable today without the native stacks preview, by any tool that
doesn't mind the side effect: enabling auto-merge changes the PR's state on
GitHub. The PR's `auto_merge` field becomes non-null, the UI shows an
auto-merge banner, and the PR will merge whether the originating tool is
running or not. That is a real behavior change a reviewer can see, not an
internal optimization.

## Webhooks and App permissions

- **Webhooks**: the public
  [webhook-events page](https://docs.github.com/en/webhooks/webhook-events-and-payloads)
  now lists **`stacked`** as a `pull_request` action ("a PR is added to a
  stack"), and the `pull_request` payload embeds the `stack` object. There
  is **no** dedicated top-level `stack.*` event type.
  ([gh-stack webhooks reference](https://github.github.com/gh-stack/reference/webhooks/).)
- **GitHub App permissions**: **no** dedicated "stacks" permission scope on
  the
  [permissions reference](https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps).
  Access presumably rides on existing pull-request permissions, but this is
  not documented.

## Plan and access gating

Rewritten 2026-07-30 — the previous content (Team/Enterprise only, per-repo
enablement, `gh.io/stacksbeta` waitlist) is obsolete.

- **Plans**: works on **personal Free-plan** repos — verified live by creating
  and merging a stack in a private Free-plan repo. The canonical docs page is
  published under `free-pro-team@latest`, Enterprise Cloud, and Enterprise
  Server 3.17–3.21. The roadmap issue's Team/Enterprise labels are stale.
- **Waitlist**: none. Removed at public preview.
- **Enablement**: rolling out to all repositories. The `404` from the stacks
  endpoints remains the correct capability check — cheap, safe on any repo,
  and still meaningful for GitHub Enterprise Server and during the tail of the
  rollout. Keep it; just expect `200` far more often than not.
- **Merge queue**: the one genuinely staged piece, "rolling out progressively
  over the coming weeks". Do not assume queue routing works yet.

## Implications for third-party stacking tools

What tools can stop doing once the feature ships publicly with a
stable API:

- **Custom stack-comment generation**. The native UI renders a stack map.
  PR-body navigation comments become redundant.
- **Cascading rebase of the stack after a merge** — *verified*. GitHub
  retargets the surviving PR's base straight to trunk and rebases every
  remaining branch onto the new trunk tip, keeping the stack clean. The
  earlier catch that this only followed a web/queue merge is **obsolete**: the
  async merge API triggers the same cascade. The remaining catch is the sharp
  one for jjpr: the rebase rewrites remote branches to SHAs jj did not push,
  so jj sees divergence on the next fetch and jjpr must reconcile. For a jj
  tool this "GitHub rebases for you" is as much a problem as a convenience.
- **Stack-wide merge sequencing.** `merge-async` lands a whole stack, or any
  bottom-up prefix of it, atomically in one call. jjpr's own sequential
  merge loop is more code for a weaker guarantee on repos that have this.

What tools keep owning even after GA:

- **Local repo state and graph discovery**. The native feature operates
  on PRs that already exist; it does not push commits or manage
  branches/bookmarks for you.
- **Non-`git` VCS support**. jj, Sapling, hg-via-bridge etc. are out of
  scope for GitHub's CLI.
- **Pre-submission stack shaping** (rebase, split, fold, fixup) prior to
  pushing.
- **Multi-forge support**. GitLab, Forgejo, Bitbucket users get nothing
  from this.
- **Diamond / non-linear stacks**. GitHub requires fully linear history;
  jjpr's supported diamond shapes have no native representation.
- **Reconciling jj's local commits with GitHub's server-side rebase.** Nothing
  in the native feature helps here; it is the source of the problem.
- **Merge on repos without native stacks, and on every other forge.** GitLab,
  Forgejo and Bitbucket get nothing from any of this.

(Removed from this list on 2026-07-30: "stack-wide merge sequencing until a
`gh stack merge` / merge endpoint exists" — both now exist.)

## Potential route for jjpr

A phased path, cheapest-and-safest first. **This is now buildable** — the
gating, auth and merge blockers are gone and the schema is canonically
published. Step 0 is the one that should happen regardless of whether jjpr
ever supports native stacks at all.

0. **Stop `jjpr merge` breaking on someone else's stack. (Do this first.)**
   Every repo now has native stacks, so any user can run `gh stack submit` on
   jjpr's PRs — or click the UI — and jjpr's `PUT /pulls/{n}/merge` will
   `403` with a message telling them to use the web interface. That is a
   live, user-visible break that costs jjpr nothing to have caused. The PR
   payload already carries `stack` (null when unstacked), so detection is
   free on data jjpr may already fetch. Then either (a) route to
   `merge-async` and poll — the honest fix, and jjpr's `watch` already owns a
   poll loop to model it on — or (b) at minimum, explain precisely what
   happened instead of surfacing a raw 403. Note this changes merge
   semantics: merging a stacked PR lands **every PR below it too**, so jjpr
   must say so before doing it, per our defensive-design stance.

1. **Capability probe (no commitment).** Add a `Forge` method that does a
   `GET /repos/{owner}/{repo}/stacks` and treats `404` as "native stacks
   unavailable." This is one cheap request, safe on any repo, and is the
   gate every later step checks first. It also lets `jjpr status` note
   "this repo supports native stacks" without changing behavior.

2. **Read-only enrichment.** When native stacks are enabled, read the
   `stack` object embedded on each PR (`stack.number`, `stack.position`,
   `stack.size`, `stack.base.ref`) to cross-check jjpr's own graph-derived
   ordering, and surface the native stack in `status` output. Read-only:
   no mutation, no PR-state change, degrades cleanly when absent.

3. **Opt-in native linking — the tradeoff is now much smaller.** Behind a
   config flag (default off), after jjpr pushes and opens its PRs, call
   `POST /stacks` (body `{"pull_requests": [bottom→top]}`) or
   `POST /stacks/{n}/add` to register them as a native stack, replacing
   jjpr's PR-body navigation comments with GitHub's native map. Previously
   this was near-disqualifying because it disabled API merge; with
   `merge-async` available that objection is gone. What remains is the
   reconciliation cost in step 4 and the linear-only restriction.

4. **The unsolved problem is reconciliation, not merge.** GitHub's cascade
   rebase force-moves remote branches to commits **jj never created**
   (re-verified 2026-07-30). After a native stack merge, `jj git fetch` sees
   bookmarks pointing at unknown commits and jjpr must decide: adopt
   GitHub's rebased commits (abandoning the local ones, and with them jj's
   change identity), or re-push its own and undo GitHub's rebase. Neither is
   free, and jjpr's existing post-merge reconcile (`jj rebase -s <root> -d
   <trunk>` in `src/merge/execute.rs`) assumes jjpr owns the rewrite. **This
   now deserves the design attention that merge used to absorb.** Note the
   already-implemented `is_rooted_in` skip (v0.35.0) is the right instinct
   but solves a different case — there jjpr chooses not to rewrite; here
   GitHub rewrites without asking.

Hard constraints to respect throughout:

- **A stacked PR's base is immutable.** `PATCH /pulls/{n}` with a `base` is
  `422` for any stack member, even a no-op. Any jjpr flow that retargets
  (submit's phase 3, merge's post-merge reconcile) must check first rather than
  discover it from the error, because both do it *after* pushing.
- **Merging a stacked PR merges everything below it.** This is the semantic
  jjpr must surface loudly. `merge-async` on the middle of a stack lands the
  bottom two PRs atomically. A user asking jjpr to merge one PR must not
  silently land three.
- **Poll, don't trust the submit.** Rule failures, conflicts, and bad
  `merge_action` routing all return `202` at submit and only surface as
  `failed` at poll. Any jjpr implementation must treat the submit as
  provisional and drive the poll to a terminal state.
- **Use the `sha` guard.** jjpr force-pushes constantly, so a merge submitted
  against a head that has since moved is a real race. Passing `sha` turns
  that into a clean `400` instead of merging something unintended — this is
  exactly the defensive posture jjpr already takes elsewhere.
- **Recover the `409` uuid.** A duplicate submit returns the in-flight
  request's uuid in the body; resume polling it rather than erroring out.
  (`gh-stack` throws this away; jjpr's `ureq` client need not.)
- **`unstack` ignores its body and frees every removable PR.** It cannot
  remove a single PR, and does not retarget bases. Merged PRs stay pinned
  (`200`, stack survives); with none pinned the stack dissolves (`204`).
  Usable as an escape hatch even mid-merge, but it destroys the user's whole
  native grouping — never do it on jjpr's own initiative.
- **Linear-only.** The native API cannot represent jjpr's diamond stacks.
  The native path must be gated to linear segments and never be the only
  way to submit — our multi-shape support is a differentiator, not a
  fallback.
- **Auth — settled.** A `repo`-scoped OAuth token is proven for read *and*
  merge; a fine-grained **PAT gets `200`** on the read path; the GitHub App
  permission is plain "Pull requests". The one trap: `merge-async` requires
  **`contents=write`**, unlike every other stacks endpoint, and a token
  missing it fails with an opaque `404`. If jjpr ever calls `merge-async`, a
  `404` should be reported as "the endpoint is unavailable **or** your token
  lacks `contents: write`", never as a bare not-found.
- **Forge-abstraction fit.** Any native-stack calls live behind the
  `Forge` trait as GitHub-only methods with no-op defaults, so GitLab and
  Forgejo backends are unaffected.
- **Still preview.** Canonically documented now, but not GA. Tolerate
  unknown fields; don't serialize rigidly.

## Lessons for jjpr's own design

Separate from whether jjpr ever *supports* native stacks: watching how GitHub
built theirs surfaces things jjpr can apply to its own implementation.

- **The stack link is metadata, not PR-body text.** GitHub stores a stack as
  a separate object rendered by the UI and never edits PR bodies. jjpr has no
  UI, so its navigation comment is its only channel — but this validates
  keeping that footprint minimal and clearly delimited, and it confirms a
  native map and a jjpr comment would not collide if both ever coexist.
- **A legible positional model.** GitHub exposes `position` (1-based from the
  bottom), `size`, and the ultimate `base`. jjpr already computes the graph;
  presenting the same three facts plainly in `status` is a clean target.
- **Validate stack shape and reject with a precise message.** GitHub enforces
  a linear chain and rejects violations with an exact reason ("each PR's base
  ref must be the previous PR's head ref"). jjpr can pre-validate shape and
  give an actionable error rather than letting a bad push fail obscurely.
- **Diamonds are hard enough that GitHub declined them.** Native stacks are
  linear-only. jjpr's diamond support is a genuine differentiator with no
  native fallback, worth keeping robust and calling out as jjpr-only.
- **Cascading descendant rebase is the heart of stacking, and jj does it
  better.** GitHub reimplemented server-side descendant rebasing because it is
  essential; jj already does it locally, with conflict handling and non-linear
  shapes GitHub's linear-only version cannot match. Because jj owns the
  commits, jjpr should keep owning the rebase rather than delegate it.
- **Rebase-heavy stacking is hard on reviews — and there is a real jjpr gap,
  scoped to merge-commit landings.** Whether GitHub's cascade rebase keeps an
  upstream approval turns on the bottom PR's merge method, verified live with a
  real reviewer under `dismiss_stale_reviews_on_push: true`:
  - **Merge-commit landing (stack 26): approval SURVIVED**, re-associated to the
    rebased head SHA (`APPROVED @ new-sha`). The bottom commit stays in trunk,
    so the descendant rebase is cosmetic; GitHub re-points the approval to the
    new head, so it is never "stale" and dismiss-stale has nothing to dismiss.
  - **Squash landing (stack 22): approval DISMISSED**, pinned to the old SHA
    (`DISMISSED @ old-sha`). The bottom commit is gone from trunk, so the rebase
    is substantive; GitHub does not re-associate, the approval is stale, and
    dismiss-stale dismisses it.
  - (Baseline, stack 14, dismiss-stale OFF, merge-commit: survived + re-associated.)

  Implication: on repos with dismiss-stale ON, **native stacks preserve upstream
  approvals across a merge-commit landing that jjpr's rebase-then-force-push
  would dismiss** — a tool force-push always lands a fresh SHA with no
  re-association, so the approval goes stale and is dismissed. jjpr cannot make
  GitHub treat its push as a re-associating rebase (that is internal to the
  stack machinery). Two mitigations, both partial — **both now implemented**
  (v0.35.0), across GitHub, GitLab, and Forgejo:

  - **(a) Don't rewrite descendants that don't need it.** ✅ Implemented via
    `Jj::is_rooted_in` + a skip in `reconcile_local_state`
    (`src/merge/execute.rs`): a merge-commit/rebase-merge landing skips the
    rebase and force-push and only retargets the next PR's base. After a
    merge-commit landing the descendant's parent stays in trunk, so the
    descendant is already a clean commit on trunk — only its PR base needs retargeting (`update_pr_base`,
    a PATCH, no push). jjpr's default post-merge reconcile instead runs
    `jj rebase -s <root> -d <trunk>` (`src/merge/execute.rs`), reparenting every
    descendant onto the trunk *tip*, which rewrites SHAs and dismisses the
    approval. Skipping the rebase for parent-in-trunk landings would preserve it.
    jjpr's `submit` flow already skips unchanged bookmarks (`is_synced`,
    `src/submit/plan.rs`); the reconcile path does not. Tradeoff is small: the
    local stack sits one commit behind the trunk tip until the next natural
    rebase; the remote PR is clean either way. Does not help squash landings,
    where the descendant genuinely must be rebased (parent gone from trunk).
  - **(b) Warn before a dismissing push.** ✅ Implemented as
    `Forge::base_dismisses_stale_approvals` (GraphQL classic +
    `dismissesStaleReviews`, REST `rules/branches` fallback for rulesets; GitLab
    `reset_approvals_on_push`; Forgejo `dismiss_stale_approvals`) surfaced three
    ways: a forward-looking `status` note ("a squash-landing of #N would dismiss
    N approvals"), and a report at the actual push in `submit` and the merge
    reconcile. The detection call is spent only when an approval is genuinely at
    risk. The dismiss-stale setting is in GraphQL: classic protection on
    `pullRequest.baseRef.branchProtectionRule.dismissesStaleReviews`; rulesets
    (which the classic field reports as `null` — verified) on
    `repository.rulesets` (repo-level, fetched once, matched client-side to the
    base branch via its `ref_name` conditions). jjpr already runs a batch
    GraphQL query carrying review state, so both fields fold in there. Combined
    with the approval state it already reads, jjpr can warn that a push will
    dismiss an approval.

  No gap for squash landings — there both native and jjpr lose the approval.
- **Merged PRs stay in the stack; the stack closes only when all land.** A
  clean lifecycle model for representing a partially-landed stack, which jjpr's
  `status` and `merge` can mirror.

## Open questions

Items that need verification on re-research, with the source to check:

Answered on 2026-07-30 and struck from this list: API publication to
docs.github.com (done), plan gating (Free works), `gh stack merge` / a merge
endpoint (both shipped), GitHub App permission (plain "Pull requests"),
`X-GitHub-Api-Version` and rate limits (standard `2022-11-28`, `core` bucket).

Still open:

- **PAT merge-async write path.** Read is now proven (see
  [PAT verification](#pat-verification--resolved-2026-07-30)); a PAT actually
  *driving* a merge is not, because no available PAT had both
  `pull_requests: write` and access to a sandbox repo. The required permission
  is known (`contents=write`), so this is a scoping exercise rather than an
  unknown.
- **Approval survival under the async merge API.** The merge-commit-preserves
  / squash-dismisses finding was established against a **web-UI** merge. Re-run
  it through `merge-async` with a second reviewer and
  `dismiss_stale_reviews_on_push` on. The jjpr "gap" argument depends on it.
- **Merge queue routing.** Exercise `merge_action: "merge_queue"` and
  `"default"` on a queue-enabled repo once the rollout reaches one. Confirm
  the `enqueued` terminal status and how to track the eventual outcome.
- **CODEOWNERS interaction**, and whether a stack merge refuses to start until
  every member PR is green.
- **Behavior when a PR in a stack is closed (not merged) manually.** What
  happens to the members above it?
- **GA date and whether anything changes at GA.** Still labelled preview.
- **`gh stack` becoming a built-in subcommand.** The
  [cli/cli](https://github.com/cli/cli) repo would be the source; no movement
  as of v2.96.0.

## Re-research checklist

URLs to re-fetch and what to scan for:

1. [github/roadmap#1218](https://github.com/github/roadmap/issues/1218)
   — status field (still "Preview"?), plan list, target date, comments
   mentioning new capabilities or a GA date.
2. [github/gh-stack releases](https://api.github.com/repos/github/gh-stack/releases)
   — new versions past v0.0.8, changelog entries (especially any `merge`),
   pre-release tags.
3. [github/gh-stack repo](https://github.com/github/gh-stack) — README
   diff, new docs sections, archived/locked status (would signal a pivot
   into `gh` proper).
4. [gh-stack docs site](https://github.github.com/gh-stack/) — new guide
   pages, FAQ entries, and especially the
   [REST reference](https://github.github.com/gh-stack/reference/rest-api/)
   for schema changes and any newly-documented auth.
5. [docs.github.com/en/rest/pulls/stacks](https://docs.github.com/en/rest/pulls/stacks)
   and [rest/pulls/pulls](https://docs.github.com/en/rest/pulls/pulls) (async
   merge), plus the
   [GraphQL changelog](https://docs.github.com/en/graphql/overview/changelog)
   — search for `stack`, `Stack`, `pullRequestStack`, `mergeAsync`. Watch for
   a GraphQL merge mutation appearing (none as of 2026-07-30).
   Also check which doc versions the stacks page is published under — that
   list is a reliable plan-availability signal.
6. [GitHub Changelog](https://github.blog/changelog/) — filter on "stack"
   or "stacked"; look for a GA announcement.
7. [cli/cli releases](https://github.com/cli/cli/releases) — check for
   built-in `pr stack` subcommand absorption.
8. [Webhook events](https://docs.github.com/en/webhooks/webhook-events-and-payloads)
   — changes to the `stacked` action or the embedded `stack` payload.
9. [GitHub App permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps)
   — the stacks endpoints now sit under "Pull requests"; watch for a split-out
   scope.
10. `gh extension search stack` — surface competing or successor
    extensions.
11. [community discussion 201439](https://github.com/orgs/community/discussions/201439)
    (`gh.io/stacks-feedback`) — the best source of real-world breakage and the
    only place GitHub staff respond. Scan for merge-queue rollout status, PAT
    support, and fork/cross-repo stacks.

Live checks worth repeating (all cheap, and the previous run's script shape is
in the 2026-07-30 entry): probe `/stacks` on a repo to confirm the capability
`200`; build a 3-PR linear stack in `michaeldhopkins/forge-e2e-sandbox` and
run a partial `merge-async` to re-confirm the cascade rebase and divergence.
Clean up branches and files afterwards — the sandbox is shared.

When you re-research, verify each existing claim in this file against
its cited source and add a Changelog entry below.

## Verification methodology

Entries assembled from primary sources only:

- GitHub Roadmap entries on github.com/github/roadmap.
- The `github/gh-stack` GitHub repository and Releases API.
- The `github.github.com/gh-stack` documentation site (overview,
  stacked-PRs guide, FAQ, REST reference, webhooks reference).
- `docs.github.com` REST, GraphQL, webhook, and App-permission references
  searched for stack terminology.
- GitHub API release dates verified via
  `api.github.com/repos/github/gh-stack/releases`.

Third-party tutorials, blog posts, and community write-ups were
excluded. Any claim not citable to a github.com or github.github.com URL
should be marked unverified or removed.

## Changelog

Newest entries on top. Each entry: date, short theme, concrete bullets
naming what changed since the prior entry. Cite a primary source for
every claim.

### 2026-07-30 — Public preview: merge unblocked, gating gone, docs canonical

GitHub [announced public preview](https://github.blog/changelog/2026-07-30-stacked-pull-requests-are-now-in-public-preview/).
Three of the four standing blockers fell in one day. Full re-research against
every checklist source, plus a live build-and-merge of stack 223 (PRs 220–222)
in `michaeldhopkins/forge-e2e-sandbox`.

- **Merge via public API works.** `gh-stack` v0.1.0 (2026-07-29) shipped
  `gh stack merge`, backed by `PUT /repos/{o}/{r}/pulls/{n}/merge-async` and
  `GET .../merge-async/{uuid}`. Submit-then-poll, atomic, supports partial
  merges (everything up to and including the target PR). Verified end to end
  with an ordinary `repo`-scoped OAuth token: a merge targeting the **middle**
  PR of a 3-stack landed the bottom two atomically in ~4 seconds. This retires
  the 2026-07-21 finding "No public API can merge a stacked PR".
- **The legacy sync merge still `403`s** on a stacked PR, with the now-stale
  message "Use the web interface instead". This is what jjpr hits today.
- **Gating is gone.** `/stacks` returns `200` on a personal **Free**-plan
  private repo; a stack was created and merged there. No waitlist. The
  roadmap issue is stale (still "Preview", still Team/Enterprise-only labels);
  the docs are published under `free-pro-team@latest`.
- **Canonical publication happened** — `docs.github.com/en/rest/pulls/stacks`
  is live, async merge is in `rest/pulls/pulls`, and the App-permissions
  reference lists the stacks endpoints under the ordinary **"Pull requests"**
  permission (no dedicated scope). Resolves two long-standing open questions.
- **`merge-async` is not stack-only** — it merged a plain `stack: null` PR.
- **The cascade rebase is unchanged and now the sharpest issue.** After the
  partial merge, the surviving PR was retargeted straight to `main` (skipping
  the intermediate merged branch) and its branch force-moved from the pushed
  `19f7eb4c` to GitHub's own `fa1081de`. It rebases even on a merge-commit
  landing where the descendant did not need it. With merge no longer blocking,
  reconciling this against jj's local commits is the remaining design problem.
- **Trunk got one merge commit for the two-PR landing**, not one per PR.
- **Two corrections to `gh-stack`'s own source comments**: forcing
  `merge_action: "merge_queue"` on a queueless branch is accepted (`202`) and
  fails only at **poll** time, not at submit; and the `409` duplicate-submit
  response **does** carry the existing uuid — go-gh discards it, but `ureq`
  need not, so jjpr can resume an in-flight merge.
- **`sha` guard confirmed** as optimistic concurrency: a stale head yields
  `400 "Pull request head branch was modified."` — directly useful given how
  often jjpr force-pushes.
- Stack size limit is **100 PRs**. Auto-merge and admin bypass are explicitly
  unsupported for stacked PRs. Merge queue support is still rolling out and
  users report `404`s. GraphQL still has read-only types and no merge mutation.
  `cli/cli` still has not absorbed a built-in `stack` subcommand.
- **PAT question closed.** A fine-grained PAT returns `200` from `/stacks` on
  an enabled repo (`michaeldhopkins/jjpr`) — same API version and `core` rate
  bucket as OAuth. Open since 2026-07-21. Also captured the authoritative
  `x-accepted-github-permissions` header per endpoint: `GET /stacks` needs
  `pull_requests=read`, `POST /stacks` needs `pull_requests=write`, and
  **`merge-async` needs `contents=write`** — the odd one out, and it fails
  with an opaque `404` when missing.
- **Unstack semantics re-verified** (stacks 229 and 233): the `pull_requests`
  body is ignored and unstack always removes every removable member, but
  **merged PRs are pinned** and stay — which is when it returns `200` (stack
  survives) instead of `204` (dissolved). Reconciles the apparent conflict
  between our 2026-07-21 "all-or-nothing" finding and the CLI docs' partial
  removal; both were right about different mechanisms.
- **No web URL for a stack.** Only an API `url` on the resource; the embedded
  `stack` on a PR has no URL at all, and `gh-stack` never builds one. Name the
  stack by number and link the PR instead of synthesizing a stack URL.
- **Correction (same day): merging beneath a native stack is safe.** An
  earlier probe deleted a stack's root branch with `DELETE /git/refs` and saw
  the stacked PR close, which was written up as "a `delete_branch_on_merge`
  repo will silently close a stacked PR after a tool merges below it." That
  inference was wrong. Re-tested through the actual path — merge the PR below
  the stack on a repo with `delete_branch_on_merge: true` — GitHub's merge
  cleanup **auto-retargets** the stacked PR to trunk and leaves it open. Only a
  bare ref deletion closes it. No tool-caused data loss here.
- **Merge queue weakens atomicity.** Per the CLI reference, queued stack
  members "may land in separate groups rather than all at once", and an
  explicit merge method is ignored with a warning. The all-or-nothing
  guarantee holds for direct merges only.
- Sandbox cleaned up (branches, files, and probe PRs closed) after the run.

### 2026-07-22 — Merge-commit landing preserves the approval (real jjpr gap)

- Completed the dismiss-stale investigation with the discriminating cell:
  merge-commit + dismiss-stale ON. Same setup as the squash test, only the
  merge method changed. The upstream approval **SURVIVED** and re-associated to
  the rebased head SHA — the opposite of the squash result.
- So the merge method decides it: merge-commit keeps the bottom commit in trunk
  (cosmetic rebase → re-association → survives); squash discards it (substantive
  rebase → no re-association → dismiss-stale dismisses). dismiss-stale only
  dismisses when re-association fails.
- Conclusion: a **real jjpr gap exists for merge-commit landings under
  dismiss-stale** — native stacks keep an upstream approval that jjpr's
  force-push would dismiss. jjpr cannot replicate the re-associating rebase via
  the public API; mitigation is to skip no-op force-pushes and warn before a
  push dismisses reviews. No gap for squash. Recorded in the lessons section.

### 2026-07-22 — Dismiss-stale dismisses a squash-landed native rebase

- Tested the dismiss-stale question directly: a 2-PR stack under a ruleset with
  `dismiss_stale_reviews_on_push: true` + 1 approval, a real reviewer's approval
  on the top PR, then squash-merged the bottom PR. The cascade rebased the top
  PR (new head SHA) and its approval was **DISMISSED** — not re-associated,
  pinned to the old SHA. So for squash + dismiss-stale, native stacks lose the
  approval just like jjpr's force-push; **no jjpr gap for that (common) case.**
- Contrast with stack 14 (merge-commit, dismiss-stale off), where the approval
  survived and re-associated. Two variables differ, so the squash dismissal is
  not cleanly attributable to dismiss-stale vs the substantive rebase. The
  discriminating follow-up is merge-commit + dismiss-stale ON.
- Confirmed dismiss-stale was genuinely active: an unapproved probe PR targeting
  the protected branch read `BLOCKED`/`REVIEW_REQUIRED`.

### 2026-07-21 — No public API can merge a stacked PR (merge queue too)

- Configured a merge queue + required check on the sandbox and tried to merge
  the ready, approved bottom PR through it. `enqueuePullRequest` (GraphQL) is
  rejected: "must be merged sequentially using the stack merge API." So the
  merge queue is not a public API merge path for stacked PRs either.
- Chased the "stack merge API" the error names: no `POST /stacks/{n}/merge`
  (404), no GraphQL stack-merge mutation (introspection: stack types exist, no
  mutation), and the official `gh-stack` client calls only the 5 documented
  endpoints with no merge. Conclusion: it is GitHub's internal web-UI-only
  endpoint. Merging a stacked PR is browser-only in the preview — final for
  jjpr's merge story. Corrected the earlier "web-UI or merge-queue" wording.

### 2026-07-21 — Branch-protection review gating applies per-PR

- Added a ruleset requiring 1 approval on all branches, with the existing
  reviews in place. Result is per-PR, matching normal PRs: the approved PR
  reported `mergeable_state: clean` / `mergeStateStatus: CLEAN`; the
  changes-requested PR reported `blocked` / `BLOCKED` (with
  `mergeable: MERGEABLE`, so the gate is the review, not a conflict). The stack
  does not bypass protection or gate globally; each PR's own review state gates
  its own merge.
- Implication: the gating surfaces in the standard `mergeable_state` /
  `mergeStateStatus` signals jjpr already reads, so `watch`/`merge` gating logic
  needs no stack-specific changes.
- Not yet tested (needs a required status check + merge queue configured):
  whether enqueuing a stacked PR via the merge queue is an API-reachable merge
  path (the direct merge endpoint stays blocked), and whether the queue gates
  on the whole stack being green.

### 2026-07-21 — Merge is a cascading rebase; reviews survive it

- Corrected the earlier "clean pointer move" claim. Merging the bottom PR
  triggers a **cascading server-side rebase** of the whole remaining stack:
  every descendant branch gets a fresh SHA (`feat-b`, `feat-c` both rebased,
  `feat-c`'s parent = new `feat-b`), all stayed clean. Happens even under a
  merge-commit merge where a pure retarget would have sufficed.
- **Review decisions survive** the rebase: a real reviewer's `APPROVED`
  (middle PR) and `CHANGES_REQUESTED` (top PR) were both preserved; the
  approval re-associated to the rebased head. Landing a lower PR does not force
  re-review above.
- New jjpr risk flagged: the rebase rewrites remote branches to SHAs jj did not
  push, so jj sees divergence on fetch and jjpr must reconcile. Possibly a
  bigger obstacle than the API merge block. Updated the operations table,
  implications, and merge-behavior section.

### 2026-07-21 — Sandbox edge cases: force-push, ordering, footprint, reviews

- **Force-push tracked**: rewriting a member's commit to a new SHA is followed
  by the PR and the stack entry (after brief propagation); membership/order
  hold. jj commit-rewriting is compatible with native stacks — the headline
  win for a jjpr integration.
- **Order not inferred**: `POST /stacks` rejects out-of-order PR lists `422`;
  jjpr must sort bottom→top. **Non-linear** sibling sets rejected the same way
  (linear-only enforced).
- **Create is footprint-clean**: no body/label/reviewer/comment/check mutation,
  only `updated_at`. Native link is UI-rendered metadata, orthogonal to jjpr's
  nav comments.
- **Non-default base** stacks work (`base: release`).
- **Reviews not stack-blocked**: reviews/requested_reviewers endpoints behave
  normally on stacked PRs (merge `403` is a narrow special case). Stack payload
  has no review fields — jjpr keeps per-PR review reads.
- Pending a second reviewer: approval-survival-on-retarget, merge gating on
  approvals, CODEOWNERS.

### 2026-07-21 — Auto-retarget on merge verified (web-UI merge)

- Merged the bottom PR of a fresh 3-PR stack via the web UI, then re-read the
  API. The next PR's base **auto-retargeted** from the merged branch to trunk
  (`part-1`→`main`) — a clean pointer move (1 file vs main, mergeable, no
  rebase). Only the immediately-following PR retargeted.
- The merged PR **stays in the stack** as a closed member; `size`/`position`
  do not reindex; the stack stays `open` until all members merge. Reversed the
  prior "unverified" marks in the operations table and implications.
- Reinforced the catch: the retarget follows a web/queue merge only, since API
  merge is blocked for stacked PRs — a tool can't trigger it via its own merge.

### 2026-07-21 — Write API exercised; API merge is blocked for stacked PRs

- Built and drove a real stack in a preview-enabled sandbox
  (`MerchantsBonding/stacks-testing`). Verified request bodies:
  `POST /stacks` and `POST /stacks/{n}/add` take
  `{"pull_requests": [bottom→top]}`; create returns `201`, the stack number
  comes from the repo's PR sequence.
- **`unstack` dissolves the whole stack** (verified twice): naming only the
  top PR removed all PRs and emptied the list (`204`). It does not retarget
  bases. Not a per-PR removal in the preview.
- **API merge of a stacked PR is blocked**: `PUT /pulls/{n}/merge` → `403`
  "Merging stacked PRs via this API is not supported." The block is
  stack-membership-based — after `unstack`, the same merge succeeded. No REST
  stack-merge endpoint exists. Recorded as the top jjpr integration risk:
  jjpr's existing `merge` breaks on any natively-stacked PR, not just ones it
  creates. Updated the operations table, implications, and route accordingly.
- Auto-retarget-on-merge stays **unverified** — it can only follow a web/merge-
  queue merge, which the blocked API path prevents us from triggering.

### 2026-07-21 — PAT auth confirmed accepted (200-on-enabled still pending)

- Tested a fine-grained PAT against the live API. It authenticates
  (`GET /user`) and reads accessible repos (`GET /repos/{o}/{r}` → 200), and
  on a non-preview repo's `/stacks` returns the **same 404 as OAuth** — not a
  401/403. Conclusion: the Stacks endpoint **accepts PAT auth**; the gh-stack
  CLI's anti-PAT stance is a CLI choice, not an API rule. This is jjpr's exact
  token path (ureq + `Bearer`).
- Still pending: a literal 200 from `/stacks` with a PAT, which needs a PAT
  scoped to a preview-enabled repo (none on hand — the tested PAT reaches only
  personal repos, and `mbc` requires org-approved access).
- Added a reusable read-only probe at
  `~/.runner-scripts/github/stacks-pat-probe.sh` (reads `STACKS_PAT`, never
  prints it).

### 2026-07-21 — Live API probe + jjpr read-only prototype

- **Probed the live endpoints** (read-only) against a preview-enabled org.
  Verified: `repo`-scoped OAuth token works with **no** stack-specific scope
  (`X-Accepted-Oauth-Scopes` empty); standard API version `2022-11-28`;
  ordinary `core` rate-limit bucket; disabled repos return `404` whose body
  cites the (still-404) canonical `docs.github.com/rest/pulls/stacks` URL.
- **Single-stack GET is richer than documented** — embeds per-PR
  `base.{ref,sha}` + `head.{ref,sha}` + user, so the whole linear chain is
  reconstructable from one call. Recorded in the Live-verification section.
- **Started the jjpr prototype**: added `Stack`/`StackPr` types and a
  read-only `Forge::native_stacks()` (default `Ok(None)`; GitHub impl maps
  `404` → `None` as the capability probe). Unit test parses a live payload.
  Not wired into any command yet — the capability-probe step of the route.

### 2026-07-21 — Public REST API appears; still preview

- **Public Stacks REST API now documented** at
  [github.github.com/gh-stack/reference/rest-api](https://github.github.com/gh-stack/reference/rest-api/),
  introduced by `gh-stack` v0.0.8 (2026-07-15, "Migrated to the public
  Stacks REST API"). Endpoints: list/get/create/add/unstack under
  `/repos/{owner}/{repo}/stacks`, plus a `stack` object embedded on PR
  payloads. Captured the full endpoint table and field schema. Not yet in
  the canonical `docs.github.com` REST reference; no GraphQL surface.
- **Still private preview.** Roadmap #1218 remains in "Preview" (Team +
  Enterprise); the Q2 2026 broad-rollout target has slipped with no new
  date. No GA announcement on the GitHub Changelog.
- **Release trail** filled in: v0.0.5–v0.0.8 (multi-platform, PAT-auth
  warning, interactive submit TUI, remote-stack adoption, REST migration).
  `gh stack merge` still **not implemented** — no CLI command and no REST
  merge endpoint. Merge remains the standard cascade-on-PR-merge.
- **Diamond stacks explicitly unsupported** — FAQ requires "fully linear
  history between each of the branches in the stack." Confirms jjpr's
  diamond support stays a differentiator.
- **Webhooks**: new `pull_request` action `stacked` + embedded `stack`
  payload object. **App permissions**: no dedicated stacks scope.
- **Auth for the raw endpoints remains undocumented** (scope,
  `X-GitHub-Api-Version`, PAT acceptance) — the main blocker for a jjpr
  integration, flagged in Open Questions.
- Added a **"Potential route for jjpr"** section: capability probe →
  read-only enrichment → opt-in native linking → defer merge, with
  linear-only / auth / forge-abstraction constraints.

### 2026-04-27 — Initial entry

- Created file. Captured baseline state of the native PR stacks
  feature: private preview, waitlist-gated, Team/Enterprise plans,
  Q2 2026 broader rollout target.
- Documented `gh extension install github/gh-stack` distribution model.
  Latest release v0.0.2 published 2026-04-20; v0.0.1 published
  2026-04-10. `gh stack merge` confirmed unimplemented at v0.0.2.
- Recorded server-side stack object exists (`gh stack submit` "creates
  a Stack on GitHub") but no public REST or GraphQL documentation.
- Confirmed merge-queue compatibility per official FAQ.
- Listed open questions (diamond stacks, API publication date,
  `gh stack merge` semantics, plan gating at GA, webhooks, App
  permissions, manual-close behavior) for future re-research.
