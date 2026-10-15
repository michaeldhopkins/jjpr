# Upstream watch

Things in jj-vcs/jj that jjpr's recovery messages depend on. Read only: nothing here is posted, commented on or reacted to upstream.

## A reflog-style recovery for the operation log

No issue or discussion proposes a reflog or a per-bookmark history as such. Searches of issues and discussions for "reflog", bookmark history, partial `jj op restore` and recovery after a fetch found none (checked 2026-10-14). The nearest are below. jjpr's pause message sends people to `jj op log` and `jj op restore` because that is all jj offers today; a reflog-style command would let it name something narrower.

- [#7229](https://github.com/jj-vcs/jj/issues/7229), open: `jj workspace update-stale` discarded a workspace's uncommitted changes, with no trace left in the operation log. Matters because jjpr's fetch-rewrote-the-working-copy warning tells the user to run `jj workspace update-stale`. Checked 2026-10-14.
- [#7538](https://github.com/jj-vcs/jj/issues/7538), open: "The working copy is stale" errors that end in a conflicted or concurrent-modification state. The same situation that warning covers. Checked 2026-10-14.
- [#6846](https://github.com/jj-vcs/jj/issues/6846), open: `jj op noop`. Not about recovery; listed because it came up for operation-log search terms and is a place such a command would be discussed. Checked 2026-10-14.
