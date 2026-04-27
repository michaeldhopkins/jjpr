# GitHub native pull request stacks

A reference on the state of GitHub's native stacked-PR feature and what it
means for third-party stack management tools (Graphite, jjpr, ghstack, spr,
Sapling, etc.). Updated periodically; see [Changelog](#changelog) for the
delta between visits.

This file lives in the [jjpr](https://github.com/michaeldhopkins/jjpr) repo
but the content is tool-agnostic.

## Status — verified 2026-04-27

GitHub's native PR-stack feature is in **private preview**, waitlist-gated,
restricted to GitHub Team and Enterprise plans, with broader rollout
targeted for Q2 2026. The CLI surface ships as a separately-installed
`gh` extension (`github/gh-stack`), not as a built-in `gh pr` subcommand.
The REST and GraphQL APIs do not yet expose a documented stack object;
the server-side schema is reachable through `gh-stack` but not published
for third-party consumers. The `merge` operation in the official extension
is unimplemented as of v0.0.2.

Nothing in this feature set is integrable today by a third-party tool
that needs documented APIs and broad plan compatibility.

## Roadmap and rollout

| Field | Value | Source |
|---|---|---|
| Roadmap issue | [github/roadmap#1218](https://github.com/github/roadmap/issues/1218) | GitHub Roadmap |
| Status | Private preview | Roadmap |
| Plans listed | GitHub Team, GitHub Enterprise | Roadmap |
| Broader rollout target | Q2 2026 | Roadmap |
| Waitlist | `gh.io/stacksbeta` | Roadmap |
| Repo enablement | Per-repo, by GitHub | Inferred from gh-stack docs |

GitHub Free is not listed on the roadmap entry. Plan availability after
GA is not committed.

## CLI: `github/gh-stack` extension

Distributed as a `gh` extension, not bundled into `gh`.

```
gh extension install github/gh-stack
```

| Field | Value | Source |
|---|---|---|
| Repository | [github/gh-stack](https://github.com/github/gh-stack) | GitHub |
| Latest release | v0.0.2 (2026-04-20) | [Releases](https://github.com/github/gh-stack/releases) |
| Prior release | v0.0.1 (2026-04-10) | Releases |
| Documentation | https://github.github.com/gh-stack/ | GitHub |

### Subcommands

Verify against the extension's `--help` output and source on each
re-research; this list lags the repo.

| Subcommand | Purpose | State (v0.0.2) |
|---|---|---|
| `gh stack submit` | Create or update PRs on GitHub and link them as a Stack | Implemented |
| `gh stack view` | Show the current stack and its PR states | Implemented |
| `gh stack link` | Associate existing PRs into a stack | API-only per docs (no `--help` exposure verified) |
| `gh stack merge` | Merge a stack | **Not implemented** in v0.0.2 |

The repo and docs imply additional commands are planned; treat the
table above as a floor, not a ceiling.

## API

There is no public REST or GraphQL documentation for stack-related
endpoints as of the verification date. Searches against
[docs.github.com/en/rest](https://docs.github.com/en/rest) and
[docs.github.com/en/graphql](https://docs.github.com/en/graphql)
return no stack objects, mutations, or queries.

A server-side stack object exists. `gh stack submit` "creates a Stack on
GitHub" per the extension's
[overview](https://github.github.com/gh-stack/introduction/overview/),
and `gh stack link` is described as "API-only" in the
[stacked-PRs guide](https://github.github.com/gh-stack/guides/stacked-prs/).
The schema, endpoint URLs, request/response shapes, and authentication
scopes are not published. Third parties cannot integrate without
reverse-engineering, and the schema is liable to change during preview.

## Operations supported by the native feature

| Operation | Native today (preview) | Public API today | Notes |
|---|---|---|---|
| Create stack of related PRs | Yes (`gh stack submit`) | No | Each PR still represented individually; "stack" is a server-side grouping |
| List / visualize stack | Yes (`gh stack view`, web UI map) | No | UI is the primary consumption surface |
| Server-side auto-rebase after upstream merge | Yes | No | Per [stacked-PRs guide](https://github.github.com/gh-stack/guides/stacked-prs/) |
| Merge entire approved stack at once | No (extension `merge` unimplemented) | No | — |
| Auto-retarget downstream PR base on upstream merge | Yes (server-side) | No | Eliminates the manual base-retargeting that today's tools do |
| Stack-aware CI semantics | Yes (CI runs against final target) | No | Per the same guide |
| Diamond / non-linear stack support | Unverified | — | No source confirms; flag for re-research |

## Merge queue and auto-merge interaction

Merge queue (GA) is documented as compatible with native stacks per the
[gh-stack FAQ](https://github.github.com/gh-stack/faq/): a stack can be
merged via merge queue, all PRs are queued in order, and ejection
cascades upward (an ejected PR ejects the rest of its stack from the
queue).

PR auto-merge (GA, GraphQL `enablePullRequestAutoMerge`) is
not stack-aware. Enabling it on individual PRs in a stack delegates the
merge wait to GitHub but produces no stack-level orchestration.

Both are usable today without the native stacks preview, by any tool
that doesn't mind the side effect: enabling auto-merge changes the PR's
state on GitHub. The PR's `auto_merge` field becomes non-null, the UI
shows an auto-merge banner, and the PR will merge whether the
originating tool is running or not. That is a real behavior change a
reviewer can see, not an internal optimization.

## Plan and access gating

- **Plans on the roadmap**: Team, Enterprise. Free users will not get the
  preview. GA plan availability is not committed.
- **Per-repo enablement**: Stacks are toggled on per repository by GitHub
  during preview. Tools cannot assume a repo supports stacks; a
  capability check is required before delegation.
- **Waitlist**: Access requires individual sign-up at `gh.io/stacksbeta`.

## Implications for third-party stacking tools

What tools can stop doing once the feature ships publicly with a
documented API:

- **Server-side rebase orchestration**. GitHub auto-rebases the rest of
  the stack after a merge to trunk. Today every tool reimplements this.
- **Custom stack-comment generation**. The native UI renders a stack map.
  PR-body navigation comments become redundant.
- **Per-PR base-pointer retargeting**. Server handles it.
- **Stack-aware merge sequencing**. Pending `gh stack merge`
  implementation, with merge-queue integration as the likely substrate.

What tools likely keep owning even after GA:

- **Local repo state and graph discovery**. The native feature operates
  on PRs that already exist; it does not push commits or manage
  branches/bookmarks for you.
- **Non-`git` VCS support**. jj, Sapling, hg-via-bridge etc. are out of
  scope for GitHub's CLI.
- **Pre-submission stack shaping** (rebase, split, fold, fixup) prior to
  pushing.
- **Multi-forge support**. GitLab, Forgejo, Bitbucket users get nothing
  from this.

What's actionable today:

- Nothing stack-specific. The preview is gated, undocumented, and
  incomplete (`merge` unimplemented).
- Auto-merge / merge queue delegation is feasible per-PR but mutates PR
  state on GitHub and isn't stack-aware. Tools whose value proposition
  includes "we don't change your PR settings" should not opt in by
  default.

## Open questions

Items that need verification on re-research, with the source to check:

- **Diamond / non-linear stacks**. Does the native model support them?
  Check `gh-stack` docs and extension `--help` for branching primitives.
- **API documentation**. When does the schema get published? Watch
  [docs.github.com/en/rest/pulls](https://docs.github.com/en/rest/pulls)
  and the GraphQL schema diff.
- **Plan gating at GA**. Does Free get access? Check the roadmap entry
  and the GA announcement.
- **`gh stack merge` semantics**. Once implemented: squash vs merge vs
  rebase, partial-stack merge, queue interaction. Check
  [gh-stack releases](https://github.com/github/gh-stack/releases).
- **`gh stack` becoming a built-in subcommand**. The
  [cli/cli](https://github.com/cli/cli) repo would be the source.
- **Webhook / event surface**. Are there `stack.*` webhook events for
  CI providers and bots? Check
  [docs.github.com/en/webhooks](https://docs.github.com/en/webhooks).
- **Bot / app permissions**. Does a GitHub App need a new permission
  scope to read or modify stacks? Check the Apps permissions reference.
- **Behavior when a PR in a stack is closed manually**. What happens to
  the rest? Likely in the stacked-PRs guide once expanded.

## Re-research checklist

URLs to re-fetch and what to scan for:

1. [github/roadmap#1218](https://github.com/github/roadmap/issues/1218)
   — status field, plan list, target date, comments mentioning new
   capabilities or scope cuts.
2. [github/gh-stack releases](https://github.com/github/gh-stack/releases)
   — new versions, changelog entries (especially any mention of `merge`),
   pre-release tags.
3. [github/gh-stack repo](https://github.com/github/gh-stack) — README
   diff, new docs sections, archived/locked status (would signal a pivot
   into `gh` proper).
4. [gh-stack docs site](https://github.github.com/gh-stack/) — scan for
   new guide pages, FAQ entries, API reference.
5. [docs.github.com/en/rest/pulls](https://docs.github.com/en/rest/pulls)
   and [GraphQL schema](https://docs.github.com/en/graphql/overview/changelog)
   — search for `stack`, `Stack`, `linkedPullRequest`, `pullRequestStack`.
6. [GitHub Changelog](https://github.blog/changelog/) — filter on "stack"
   or "stacked".
7. [cli/cli releases](https://github.com/cli/cli/releases) — check for
   built-in `pr stack` subcommand absorption.
8. `gh extension search stack` — surface competing or successor
   extensions.

When you re-research, verify each existing claim in this file against
its cited source and add a Changelog entry below.

## Verification methodology

Initial entry assembled from primary sources only:

- GitHub Roadmap entries on github.com/github/roadmap.
- The `github/gh-stack` GitHub repository and Releases API.
- The `github.github.com/gh-stack` documentation site (overview,
  stacked-PRs guide, FAQ).
- `docs.github.com` REST and GraphQL references searched for stack
  terminology.
- GitHub API release dates verified via
  `api.github.com/repos/github/gh-stack/releases`.

Third-party tutorials, blog posts, and community write-ups were
excluded. Any claim not citable to a github.com or github.github.com URL
should be marked unverified or removed.

## Changelog

Newest entries on top. Each entry: date, short theme, concrete bullets
naming what changed since the prior entry. Cite a primary source for
every claim.

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
