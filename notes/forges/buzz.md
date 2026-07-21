# Buzz (Block) as a candidate forge

Evaluation of [Buzz](https://github.com/block/buzz) for jjpr `Forge` support.

## Verdict — evaluated 2026-07-21

Not a near-term target. Buzz's git layer is NIP-34, git over Nostr, not an HTTP
REST forge. Supporting it means adding a second transport (signed Nostr events
over relay WebSockets) and a second auth model (keypairs), not mapping REST
calls the way GitLab and Forgejo did. The platform is early-stage, and jjpr's
central stacking operation, retargeting a PR's base branch, has no clear NIP-34
representation. Revisit only if Nostr/NIP-34 git gains real developer adoption,
Buzz matures, and users ask for it.

## What it is

Two faces of one Block, Inc. project (Apache-2.0):

- **buzz.xyz** — hosted product, in testing. Positioned as a unified workspace
  for "your people, your agents, your project."
- **github.com/block/buzz** — the open-source relay: "a workspace where humans
  and agents build together, on a relay you own." Described as a hive-mind
  communication platform. Rust and TypeScript. ~1.4k stars, actively developed.

Architecture is a Nostr relay: one community, one identity model, one event
log. Git is one facet, layered on via NIP-34. The relay exposes REST endpoints
for channel/DM/media/workflow/git plus WebSocket, and agents drive it through
`buzz-cli`.

## Git model (NIP-34)

Event kinds from `crates/buzz-core/src/kind.rs`:

| Kind | NIP-34 event | Rough forge analog |
|---|---|---|
| 30617 | Repository announcement | repo |
| 30618 | Repository state (branch/tag refs) | refs |
| 1617 | Patch (git format-patch) | commits |
| 1618 | Pull request | PR |
| 1619 | Pull request update (tip commit change) | force-push |
| 1621 | Issue | issue |
| 1630–1633 | Status: Open / Merged / Closed / Draft | PR state |

The concepts jjpr needs mostly exist: repos, PRs, PR updates on tip change,
issues, and status events for merge/close/draft. A patch-series/PR model is
also a natural fit for stacking.

## Fit against the `Forge` trait

Three structural blockers, in order of weight:

1. **Transport.** jjpr's `Forge` backends are HTTP request/response via `ureq`
   with a bearer token. Buzz is Nostr: secp256k1/schnorr-signed events
   published over relay WebSockets, with subscriptions and EOSE. A Buzz backend
   needs a Nostr client and event signing, a new I/O layer rather than another
   REST backend.
2. **Auth.** Nostr keypairs (`nsec`/`npub`), not tokens. jjpr's token
   resolution (env vars, `gh`/`glab` CLI) does not apply.
3. **No base-retargeting primitive.** jjpr builds stacks by retargeting a PR's
   base branch. NIP-34's PR-update event (1619) is defined as a tip-commit
   change, the head, not the base. jjpr's core stack mechanic has no clear
   native event and would rely on convention.

A fourth consideration is scope: "support Buzz" is really "support NIP-34."
Buzz is one relay implementation of a general protocol that other clients also
use. Any integration belongs behind the `Forge` trait as a NIP-34 backend, with
Buzz as one instance.

## What would change the verdict

- Nostr/NIP-34 git reaches real developer adoption.
- Buzz ships past its testing phase.
- Users ask for it.

If those hold, scope the work as a NIP-34 backend (Nostr transport plus
keypair auth), and first confirm how NIP-34 expects a dependent/stacked PR to
express its base.

## Sources

- Repo: https://github.com/block/buzz (README, `NOSTR.md`,
  `crates/buzz-core/src/kind.rs`, `crates/buzz-relay/src/api/git/`)
- Landing: https://buzz.xyz/
- Protocol: NIP-34, https://github.com/nostr-protocol/nips
