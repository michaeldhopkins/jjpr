# GitHub native pull request stacks

A reference on the state of GitHub's native stacked-PR feature and what it
means for third-party stack management tools (Graphite, jjpr, ghstack, spr,
Sapling, etc.). Updated periodically; see [Changelog](#changelog) for the
delta between visits.

This file lives in the [jjpr](https://github.com/michaeldhopkins/jjpr) repo
but the content is tool-agnostic.

## Status — verified 2026-07-22

GitHub's native PR-stack feature is still in **private preview**,
waitlist-gated (`gh.io/stacksbeta`), restricted to GitHub Team and
Enterprise plans. The Q2 2026 broader-rollout target has slipped: it is now
Q3 and the roadmap issue is still in the "Preview" phase with no committed
GA date. The CLI ships as a separately-installed `gh` extension
(`github/gh-stack`), not a built-in `gh pr` subcommand.

As of `gh-stack` v0.0.8 (2026-07-15) there is a **documented REST API**: five
endpoints under `/repos/{owner}/{repo}/stacks`
(list/get/create/add/unstack), plus a `stack` object embedded in
pull-request payloads. This is the first time a third-party tool could
integrate against a described schema rather than reverse-engineering. The
write side has now been exercised end-to-end against a live preview-enabled
repo (the sections below record what the Pages docs don't state).

Verified live that the docs omit: a `repo`-scoped token works (no
stack-specific scope), a fine-grained PAT is accepted, the API version is the
standard `2022-11-28`, and rate limits are the ordinary `core` bucket.
GraphQL exposes read-only stack *types* (`PullRequestStack`,
`PullRequestStackEntry`) but no stack mutation.

The findings that bound a jjpr integration:

1. **No public API can merge a stacked PR** — not the PR merge endpoint
   (403), not the merge queue, no stack-merge endpoint or mutation. Merging
   is web-UI-only in the preview (an internal API behind the merge button).
2. **Merging cascade-rebases the whole stack server-side**, rewriting the
   remote branches to SHAs jj never pushed — a reconcile burden for a jj tool.
3. **Linear-only** — diamonds/siblings are rejected (`422`).
4. **Preview-grade and gated** — schema on a Pages site, not the canonical
   docs; endpoints `404` unless GitHub enabled the repo; Team/Enterprise only.

Net: worth prototyping the read/capability path against; not shippable while
merge is browser-only, the schema is preview-grade, and access is gated.

## Roadmap and rollout

| Field | Value | Source |
|---|---|---|
| Roadmap issue | [github/roadmap#1218](https://github.com/github/roadmap/issues/1218) | GitHub Roadmap |
| Title | "Pull request stacks [Preview]" | Roadmap |
| Status | Private preview (open, not GA) | Roadmap |
| Plans listed | GitHub Team, GitHub Enterprise | Roadmap |
| Broader rollout target | Q2 2026 target has slipped; no new date | Roadmap |
| Waitlist | `gh.io/stacksbeta` | Roadmap |
| Repo enablement | Per-repo, by GitHub | gh-stack README |

GitHub Free is not listed on the roadmap entry. Plan availability after
GA is not committed. No GA announcement has appeared on the
[GitHub Changelog](https://github.blog/changelog/) as of the verification
date.

## CLI: `github/gh-stack` extension

Distributed as a `gh` extension, not bundled into `gh`. The
[cli/cli](https://github.com/cli/cli/releases) core (latest v2.96.0,
2026-07-02) has **not** absorbed a built-in `gh pr stack` / `gh stack`
subcommand; the extension is still the only surface.

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
| `gh stack merge` | Merge a stack | **Not implemented** (no CLI command, no REST endpoint) |

## API

There is now a **documented REST API**, but only on the
`github.github.com/gh-stack` Pages site
([reference/rest-api](https://github.github.com/gh-stack/reference/rest-api/)).
It is **absent** from the canonical
[docs.github.com/en/rest/pulls](https://docs.github.com/en/rest/pulls)
reference (`/en/rest/pulls/stacks` 404s), and there are **no** stack
additions in the
[GraphQL changelog](https://docs.github.com/en/graphql/overview/changelog)
(no `pullRequestStack`, `linkedPullRequest`, or `stack` schema entries).

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
doubles as the capability check.

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
  That `docs.github.com/rest/pulls/stacks` page still 404s today, but the API
  already links it — official REST-reference publication looks imminent.
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
- **`POST /stacks/{n}/unstack`** — body `{"pull_requests": [...]}`, but the
  operation **dissolves the whole stack**: naming only the top PR of a 3-PR
  stack returned `204` and removed all three (`GET /stacks` → `[]`, every
  PR's `stack` field `null`). It does **not** retarget PR bases — each PR
  keeps the chained base it had (PR2 stayed `base: change-1`, etc.). Treat
  unstack as all-or-nothing in the preview; "remove one PR" is not available.
  (The docs' "200 if the stack survives" was never observed.)

**The critical finding — merging a stacked PR via the API is blocked.**
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

### Still undocumented / unverified

Live probing (above) answered most of the original blockers — a `repo`-scoped
token works, the API version is standard `2022-11-28`, and rate limits are the
ordinary `core` bucket. What remains:

- **PAT 200 on an enabled repo**: PAT auth is accepted by the endpoint (shown
  by an identical not-enabled 404, not a 401/403 — see Live verification), but
  a literal 200 from `/stacks` with a PAT still needs a PAT that can reach a
  preview-enabled repo. Confirm before shipping, e.g. once the org approves a
  fine-grained PAT (Pull requests: read) for the enabled repo.
- **GitHub App permission**: no dedicated scope documented; presumably rides
  on pull-request read permission, untested.
- **Canonical publication**: the schema still lives only on the Pages site
  (the `docs.github.com/rest/pulls/stacks` URL the API cites 404s). Until it
  lands there, the shape is preview-grade — tolerate unknown/extra fields.

## Operations supported by the native feature

| Operation | Native today (preview) | Public API today | Notes |
|---|---|---|---|
| Create stack of related PRs | Yes (`gh stack submit`) | Yes (`POST /stacks`) | Each PR still represented individually; "stack" is a server-side grouping keyed by number |
| List / visualize stack | Yes (`gh stack view`, web UI map) | Yes (`GET /stacks`) | UI remains the primary consumption surface |
| Append PR to existing stack | Yes (`gh stack link`) | Yes (`POST /stacks/{n}/add`) | Append-only; adds to the top |
| Remove PR from stack | Yes (`gh stack unstack`) | Yes (`POST /stacks/{n}/unstack`) | Only unmerged PRs |
| Cascading rebase of the stack on merge | Yes — verified | Follows a web/queue merge only | Merging the bottom PR retargets the next PR's base to trunk AND rebases every remaining branch to fresh SHAs (all descendants, not one level); review decisions survive. NOT triggered by an API merge (blocked). Rebased SHAs diverge from what jj pushed |
| Merge a PR that's in a stack | Web UI / merge queue only | **Blocked** | `PUT /pulls/{n}/merge` returns `403` for a stacked PR (verified live); unstack first and it merges. No REST merge path for stacked PRs |
| Stack-aware CI semantics | Yes (CI against final target) | — | Per the stacked-PRs guide |
| Diamond / non-linear stack support | **No** | No | FAQ: "There must be a fully linear history between each of the branches in the stack" |

## Merge queue and auto-merge interaction

Merge queue (GA) is documented as compatible with native stacks per the
[gh-stack FAQ](https://github.github.com/gh-stack/faq/): a stack can be
merged via merge queue, all PRs are queued in order, and ejection cascades
upward. **But a tool cannot drive this via the public API.** Verified live:
with a merge queue configured, `enqueuePullRequest` on a stacked PR is
rejected ("must be merged sequentially using the stack merge API"). Whatever
queue integration exists is orchestrated by GitHub's internal stack merge API
behind the web UI, not by public `enqueuePullRequest`. See "No public API can
merge a stacked PR" above.

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

- **Plans on the roadmap**: Team, Enterprise. Free users will not get the
  preview. GA plan availability is not committed.
- **Per-repo enablement**: Stacks are toggled on per repository by GitHub
  during preview. Tools cannot assume a repo supports stacks; the `404`
  from the stacks endpoints is the capability check.
- **Waitlist**: Access requires individual sign-up at `gh.io/stacksbeta`.

## Implications for third-party stacking tools

What tools can stop doing once the feature ships publicly with a
stable API:

- **Custom stack-comment generation**. The native UI renders a stack map.
  PR-body navigation comments become redundant.
- **Cascading rebase of the stack after a merge** — *verified*, with two
  catches. On a web-UI/merge-queue merge, GitHub retargets the next PR's base
  to trunk and rebases every remaining branch onto the new trunk tip, keeping
  the stack clean, and preserves review decisions across the rebase. Catch
  one: it only follows a web/queue merge, not an API merge (blocked for stacked
  PRs). Catch two, and it is the sharp one for jjpr: the rebase rewrites the
  remote branches to SHAs jj did not push, so jj sees divergence on the next
  fetch and jjpr must reconcile. For a jj tool this "GitHub rebases for you" is
  as much a problem as a convenience.

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
- **Stack-wide merge sequencing** until a `gh stack merge` / merge endpoint
  exists.

## Potential route for jjpr

A phased path, cheapest-and-safest first. Nothing here is buildable to
production today (preview-gated, undocumented auth), but this is the shape
to prototype toward.

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

3. **Opt-in native linking — with a hard tradeoff.** Behind a config flag
   (default off), after jjpr pushes and opens its PRs, call `POST /stacks`
   (body `{"pull_requests": [bottom→top]}`) or `POST /stacks/{n}/add` to
   register them as a native stack. This replaces our PR-body navigation
   comments with GitHub's native map. But registering a native stack
   **disables API merge for those PRs** (see below): jjpr's own `merge`/
   `watch` would 403 on them until the stack is dissolved. So native linking
   and jjpr-driven merge are mutually exclusive per-PR. Opt-in and off by
   default, and clearly surfaced, per our defensive-design stance.

4. **Merge stays entirely jjpr's — and native stacks actively break it.**
   Verified live: `PUT /pulls/{n}/merge` returns `403` for any PR in a native
   stack ("Merging stacked PRs via this API is not supported"); there is no
   REST stack-merge endpoint. Two consequences: (a) jjpr cannot delegate
   merge to the native feature, and (b) jjpr's *existing* merge breaks on a
   PR someone stacked via `gh-stack` even if jjpr never opts in. `jjpr merge`
   should therefore detect stack membership (the PR's `stack` field is
   non-null) and, rather than emitting a raw 403, either explain it or offer
   to `unstack` first (unstack, then `PUT /merge` succeeds — proven).

Hard constraints to respect throughout:

- **Native stacks break API merge — the top integration risk.** Detect stack
  membership before any merge and handle it deliberately. This is true
  regardless of whether jjpr ever creates stacks, because users can create
  them independently with `gh-stack`.
- **`unstack` is all-or-nothing.** It dissolves the whole stack (verified),
  not a single PR, and does not retarget bases. If jjpr unstacks to merge, it
  tears down the entire native grouping.
- **Linear-only.** The native API cannot represent jjpr's diamond stacks.
  The native path must be gated to linear segments and never be the only
  way to submit — our multi-shape support is a differentiator, not a
  fallback.
- **Auth — mostly solved.** A `repo`-scoped token needs no special scope, and
  a fine-grained PAT is accepted by the endpoint (both verified). jjpr's
  `ureq` + `Bearer` path is exactly what was tested. Remaining: a PAT 200 on
  an enabled repo (needs a PAT scoped to one) and GitHub App permissions.
- **Forge-abstraction fit.** Any native-stack calls live behind the
  `Forge` trait as GitHub-only methods with no-op defaults, so GitLab and
  Forgejo backends are unaffected.
- **Preview instability.** The schema lives on a Pages site, not the
  canonical REST reference, and is append-only/preview. Don't serialize
  against it rigidly; tolerate unknown fields.

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
  stack machinery). Two mitigations, both partial:

  - **(a) Don't rewrite descendants that don't need it.** After a merge-commit
    landing the descendant's parent stays in trunk, so the descendant is already
    a clean commit on trunk — only its PR base needs retargeting (`update_pr_base`,
    a PATCH, no push). jjpr's default post-merge reconcile instead runs
    `jj rebase -s <root> -d <trunk>` (`src/merge/execute.rs`), reparenting every
    descendant onto the trunk *tip*, which rewrites SHAs and dismisses the
    approval. Skipping the rebase for parent-in-trunk landings would preserve it.
    jjpr's `submit` flow already skips unchanged bookmarks (`is_synced`,
    `src/submit/plan.rs`); the reconcile path does not. Tradeoff is small: the
    local stack sits one commit behind the trunk tip until the next natural
    rebase; the remote PR is clean either way. Does not help squash landings,
    where the descendant genuinely must be rebased (parent gone from trunk).
  - **(b) Warn before a dismissing push — from GraphQL, no extra REST calls.**
    The dismiss-stale setting is in GraphQL: classic protection on
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

- **Auth — mostly answered** (see Live verification): a `repo`-scoped OAuth
  token works with no special scope, and a **fine-grained PAT is accepted by
  the endpoint** (not rejected like the CLI warns). Still open: a literal 200
  from `/stacks` with a PAT on an enabled repo (needs a PAT with access to
  one), and whether a **GitHub App** permission works.
- **API publication to docs.github.com**. When does the schema move from the
  Pages site into the canonical REST reference (a stability signal)? Watch
  [docs.github.com/en/rest/pulls](https://docs.github.com/en/rest/pulls).
- **Plan gating at GA**. Does Free get access? Check the roadmap entry
  and the GA announcement.
- **`gh stack merge` / merge endpoint**. Does one ever ship, and with what
  semantics (squash vs merge vs rebase, partial-stack, queue interaction)?
  Check [gh-stack releases](https://github.com/github/gh-stack/releases) and
  the REST reference.
- **`X-GitHub-Api-Version`** value and rate limits for the stacks endpoints.
- **`gh stack` becoming a built-in subcommand**. The
  [cli/cli](https://github.com/cli/cli) repo would be the source.
- **Behavior when a PR in a stack is closed manually**. What happens to the
  rest? Likely in the stacked-PRs guide once expanded.

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
5. [docs.github.com/en/rest/pulls](https://docs.github.com/en/rest/pulls)
   and the [GraphQL changelog](https://docs.github.com/en/graphql/overview/changelog)
   — search for `stack`, `Stack`, `pullRequestStack`, `linkedPullRequest`.
   Publication here is the key stability signal.
6. [GitHub Changelog](https://github.blog/changelog/) — filter on "stack"
   or "stacked"; look for a GA or public-preview announcement.
7. [cli/cli releases](https://github.com/cli/cli/releases) — check for
   built-in `pr stack` subcommand absorption.
8. [Webhook events](https://docs.github.com/en/webhooks/webhook-events-and-payloads)
   — changes to the `stacked` action or the embedded `stack` payload.
9. [GitHub App permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps)
   — any new "stacks" scope.
10. `gh extension search stack` — surface competing or successor
    extensions.

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
