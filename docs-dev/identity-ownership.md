# Spec: multi-identity ownership (lazy)

Status: **Tier 1 shipped in 0.33.0** — the login match (fixes the reported
status label with no `/user/emails` fetch, verified in beancounter) plus the
`owned()` email union (Identity/JjRunner plumbing, config `[identity]`, seeded
in status + submit + merge). Remaining: Tier 2 lazy `/user/emails`
auto-augmentation on a discovery miss; seeding the `watch` flow; command-level
laziness tests (D/L/M) with recording forge stubs. Details below. Recognize all of a user's identities so every command
treats their work as theirs — the same GitHub account with different commit
emails per machine, or (via config) a second account.

## The problem
Ownership today is jj's `mine()` = exact match on the single local `user.email`.
A commit authored under another of your emails (a different machine) reads as
"someone else's" and is invisible to `submit`/`watch`/`merge`.

## Two tiers, both automatic and lazy
| Tier | Key | Fixes | Fetches only when |
|---|---|---|---|
| 1 — login match | GitHub login | the "someone else's" label on a PR that's yours | a segment is foreign-by-email AND has a PR |
| 2 — email union | author email | mutating commands acting on your other-email branches; `owned()` discovery | ownership discovery comes up short against work clearly present |

Happy path (everything matches local email) fetches nothing. No user-facing
toggle — jjpr fetches `/user/emails` itself, automatically, only when it needs
that information, and caches it for the run.

## Identity model
`Identity { emails: Vec<String>, logins: Vec<String> }`
- emails (seed, free): local `user.email` + `[identity].emails`. Lazily extended
  with the account's verified `/user/emails` on a discovery miss.
- logins (lazy): authenticated login (`get_authenticated_user`) + `[identity].logins`.

## The ownership revset (SPIKE RESOLVED, jj 0.39; 0.36-compatible)
`owned()` := `author(exact:"e1") | author(exact:"e2") | …`
- `author(exact:X)` matches the email *component* exactly (verified: `me@x.com`
  does NOT match `notme@x.com`; matches the email in `Name <email>`). Exact is
  the guarantee; substring is unsafe in principle.
- `author_email(exact:…)` is cleaner but newer — adopt only if the jj floor bumps.
- Escape each email for a jj string literal (`\` and `"`) — `escape_revset_string`.
- One email → equivalent to today's `mine()`. Empty → emit literal `mine()`.

## The two lazy triggers
1. Login reclassification (status): after building the view, if a segment is
   foreign-by-email AND has a PR, fetch the authenticated login once and
   reclassify segments whose `pr.author.login ∈ logins` as yours. All-local →
   never fetched. (Fixes the beancounter case — the merged PR's `author.login`
   is already in hand from `find_merged_pr`.)
2. Email augmentation (submit/watch/merge): discover with `owned()` = local +
   config; if that owns nothing but `::@ ~ trunk()` is non-empty, fetch
   `/user/emails`, augment, rebuild, retry ONCE. A genuine coworker branch stays
   unrecognized (your verified emails won't include theirs).

## Plumbing
`JjRunner` holds `owned_revset: String` (default `"mine()"`) + `set_identity`.
`get_my_bookmarks`/`get_status_bookmarks` interpolate it — trait signatures
unchanged, submit/watch/merge/stubs untouched. Lazy augmentation = a second
`set_identity` + rebuild at the command layer. A command that never calls
`set_identity` falls back to `mine()` (today's behavior); never breaks.

## Forge
`get_authenticated_emails(&self) -> Result<Vec<String>>` (GitHub/GitLab/Forgejo
`/user/emails`, verified-only), best-effort — scope/offline error → empty.
`get_authenticated_user` already exists. Both called lazily.

## Config (backstop only — no toggle)
```toml
[identity]
emails = ["me@work.com"]        # optional; unioned into owned()
logins = ["my-other-account"]   # optional; the two-accounts case
```
Common single-account/multi-machine case needs zero config. Config covers a
second account, an unregistered email, or offline.

## Failure / fallback
| Situation | Result |
|---|---|
| Everything matches local email | nothing fetched, identical to today |
| Your merged/open PR under another email | Tier 1 login match → yours, no `/user/emails` |
| `submit` on your live other-email branch | Tier 2 auto-fetches `/user/emails` |
| Email not on any account | needs `[identity].emails` |
| Two GitHub accounts | authenticated one automatic; other via config |
| Token lacks `user:email` / offline | fetch errors → local + config only |
| Genuine coworker branch | never in your verified emails → stays "someone else's" |

## Affected surfaces → proving tests
| # | Surface | Test |
|---|---|---|
| A | `get_my_bookmarks` interpolates `owned()` | real-jj: emails A(local)+B; `{A}`→A only, `{A,B}`→both |
| B | `get_status_bookmarks` broad uses `owned()` | real-jj: `{A,B}` → broad graph includes B's stack |
| C | `build_change_graph` sees B (submit/merge/watch) | real-jj: `{A,B}` graph contains B |
| D | Tier 1 fixes the label with NO email fetch | stub-forge: `@` on B-authored merged branch, login=you → yours; assert `get_authenticated_emails` never called |
| L | Tier 2 lazy augmentation recovers submit | real-jj + stub forge: `{A}` empty while `::@` non-empty → auto-fetch `{A,B}` → graph gets B; retry-once bounded |
| M | Laziness: no fetch on the happy path | recording stub: all-local stack → neither endpoint called |
| E | Your other-email base becomes a segment | real-jj: B base under A stack → segment under `{A,B}` |
| F | Inference mine-preference broadened | D + diamond test with a B "mine" arm |
| G | `resolve_identity` union/dedup/lazy/error-fallback | unit |
| H | `owned_revset()` build + escaping + not-substring | unit |
| I | `get_authenticated_emails` per backend + real fetch | unit + `JJPR_E2E` |
| J | Login supplement in `StatusRender` | unit |
| K | Regression: single-identity byte-identical | real-jj golden |

## Docs & version
`how-it-works.md` (Identities section), `configuration.md` (`[identity]`),
`status.md` (cross-machine work recognized). Minor version bump.
