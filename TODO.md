# jjpr TODO

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

- **Seed the `watch` flow.** `cmd_watch` and the watch loop build their own `jj`
  instances (main.rs ~962, watch.rs ~1018) that don't yet call `set_identity`;
  wire them like `resolve_stack` so watch honors `[identity]`/Tier 2.
- **Command-level laziness tests (D/L/M).** Recording forge stubs proving: the
  beancounter path fixes the label with `get_authenticated_emails` never called;
  submit auto-recovers via Tier 2; the happy path fetches neither endpoint.

### Docs to write (hand-edited prose — facts to cover)

- **`configuration.md` — `[identity]` section.** Fields: `emails` (extra author
  emails beyond local `user.email`), `logins` (extra forge logins). What it
  affects: the ownership set every command uses. When you need it: second
  account, an email not registered on your forge account, offline. When you
  don't: single account is auto-recognized (login match + fetched verified
  emails). TOML example with both keys.
- **`how-it-works.md` — "which commits are yours".** Ownership = local
  `user.email`, plus PR author-login match against your authenticated login,
  plus auto-fetched verified account emails, plus `[identity]` config. Note
  cross-machine same-account works without config.
- **`status.md`** — a line that cross-machine work under one account is
  recognized as yours (no longer "someone else's").


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
