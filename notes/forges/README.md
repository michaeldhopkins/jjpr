# Forge notes

Working notes on forges: deep dives on features of forges jjpr already
supports, and evaluations of forges it might support. Internal research, not
user documentation. User-facing forge support lives in `docs/src/forges.md`.

Each note states its own verification date and cites primary sources. Re-check
a note against its sources before relying on it; forge APIs move.

## Contents

- [github-native-stacks.md](github-native-stacks.md) — GitHub's native
  stacked-PR feature (preview): API surface, verified behaviors, and the route
  for a jjpr integration.
- [buzz.md](buzz.md) — evaluation of Buzz (Block's Nostr/NIP-34 workspace) as a
  candidate forge. Verdict: not a near-term target.

## Adding a note

- Evaluating a candidate forge: what it is, its API and auth model, how it maps
  to the `Forge` trait, and a verdict with the conditions that would change it.
- Researching a feature of a supported forge: the API, behaviors verified
  against a live instance, and what it means for jjpr.
