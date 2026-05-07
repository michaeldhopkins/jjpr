# Parity scenarios

Data-driven E2E tests for `jjpr submit`, `jjpr merge`, and `jjpr watch`.
Each `.toml` file describes a stack to build, optional setup steps, the
jjpr subcommand to run, and assertions about the resulting forge state.

## Running

```
JJPR_E2E=1 cargo test --test parity -- --nocapture
```

Single scenario:

```
JJPR_E2E=1 PARITY_SCENARIO=01-submit-creates-stack \
    cargo test --test parity -- --nocapture
```

Without `JJPR_E2E=1` the harness takes a skip path and exits clean — that's
what runs in normal `cargo test`.

## Schema

```toml
name = "..."
description = "..."

[[stack]]                       # build commits + bookmarks, base→top
bookmark = "auth"
file     = "auth.rs"
content  = "// auth\n"
message  = "Add authentication"

[[setup]]                       # optional, run in order
type = "submit"                 # or "external_admin_merge"
extra_args = []

[[setup]]
type     = "external_admin_merge"
bookmark = "auth"
method   = "squash"             # or "merge" / "rebase"

[run]                           # the command-under-test
command    = "submit"           # "merge" | "watch"
extra_args = ["--no-ci-check"]
timeout_minutes = 1             # only for watch; defaults to 1

[expect]
exit_status        = "success"  # or "failure"
stderr_contains    = []
stderr_not_contains = []

[[expect.pr]]
bookmark         = "profile"
state            = "open"       # open | merged | closed | absent
base             = "main"       # bookmark name (auto-prefixed) or "main"
commit_count_max = 1            # bloated-diff guard
diff_lines_max   = 5            # bloated-diff guard

[[expect.comment]]
bookmark     = "profile"
contains     = ["<!-- jjpr:stack-info -->"]
not_contains = []
```

## Bookmark prefix

Each test run mints a unique prefix (e.g. `p1a2b3c-4d5e6f-`) so concurrent
runs don't collide. Scenarios reference bookmarks by their unprefixed name
(`auth`, `profile`); the harness applies the prefix everywhere it matters.

Inside `expect.comment.contains` / `not_contains` you can use the
`{{bookmark:NAME}}` placeholder if you need to assert against the prefixed
name in the comment body.

## Approvals

The testing repo is single-account, so scenarios cannot rely on PR review
approvals. Two consequences:

- `jjpr watch` end-to-end is not yet exercisable (it requires
  `required_approvals >= 1`). Scenarios for watch should test its
  submit/promote phases or behavior under timeout, not the full merge loop.
- `jjpr merge` scenarios can use default `required_approvals = 1` to leave
  later PRs blocked-after-reconcile, which is the right shape for testing
  retarget/sync without actually merging the whole stack.

## Adding a scenario

1. Drop a new `NN-<slug>.toml` here. Numbering controls run order.
2. Run `JJPR_E2E=1 PARITY_SCENARIO=<slug> cargo test --test parity -- --nocapture`.
3. The harness cleans up PRs and remote branches on Drop, even on failure,
   so you can iterate freely.
