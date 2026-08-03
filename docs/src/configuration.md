# Configuration

jjpr reads configuration from two optional TOML files.

| Location | Created by | Purpose |
|---|---|---|
| `~/.config/jjpr/config.toml` (or `$XDG_CONFIG_HOME/jjpr/config.toml`) | `jjpr config init` | Global defaults |
| `.jj/jjpr.toml` (inside the repo's `.jj/` directory) | `jjpr config init --repo` | Repo-local overrides |

If neither file exists, jjpr uses built-in defaults. CLI flags override
config files. Repo-local config overrides global config.

## Global config

```toml
merge_method = "squash"
required_approvals = 1
require_ci_pass = true
reconcile_strategy = "rebase"
stack_nav = "comment"
```

## Repo-local config

Repo-local config goes in `.jj/jjpr.toml`. Because `.jj/` is gitignored,
the file is per-clone.

```toml
forge = "forgejo"
forge_token_env = "FORGEJO_TOKEN"
stack_nav = "description"
```

## Field reference

### `merge_method`

How the forge combines the PR when it lands.

- `squash` (default): all commits in the PR collapse into one commit on
  the target branch. Linear history.
- `merge`: a merge commit is created. The individual commits from the
  PR branch are preserved.
- `rebase`: commits are rebased onto the target branch individually
  with no merge commit. Linear history, each commit kept separately.

### `required_approvals`

Number of approving reviews required before merging. Default `1`.

### `require_ci_pass`

If `true` (default), CI checks must pass before merging. Override with
`--no-ci-check` on a single invocation.

### `reconcile_strategy`

How the remaining stack is synced after a PR is merged.

- `rebase` (default): rebases downstream commits onto the new base.
  Rewrites history. Pushes become force-pushes.
- `merge`: creates merge commits on downstream branches that
  incorporate the updated base. Pushes stay fast-forward (no force-push
  events on GitHub) but the history grows merge commits.

### `stack_nav`

Where to show the stack navigation block.

- `comment` (default): a separate comment on each PR.
- `description`: embedded in the PR body. More visible to reviewers.
  Updates the body on each `submit`.

### `forge`

Forge type. One of `github`, `gitlab`, or `forgejo`. When set,
auto-detection is skipped. Use this for self-hosted instances that
auto-detection can't recognize. Repo-local only.

### `forge_token_env`

Name of the environment variable that holds the API token. When
unset, jjpr falls back to the forge's default (`GITHUB_TOKEN`,
`GITLAB_TOKEN`, or `FORGEJO_TOKEN`). Repo-local only.

## `[identity]`

Which commits count as yours. jjpr scopes `submit`, `merge`, and
`watch` to your own work, so it has to decide who authored a commit.
It already knows your local `user.email`, and it will ask the forge
for the verified emails on your account when the local one isn't
enough. This section covers what neither of those finds.

```toml
[identity]
emails = ["old@employer.example", "me@personal.example"]
logins = ["my-second-account"]
```

- `emails` — author emails that are yours, beyond `user.email` and
  anything fetched from the forge.
- `logins` — forge logins that are yours, for a second account jjpr
  can't enumerate from the authenticated one.

Both default to empty and are additive: entries here are unioned with
what jjpr discovers, never a replacement for it.

You need this in two situations. The first is commits authored under
an email you no longer use — a machine still configured with an old
address, or history carried over from another job. The second is a
token without the `user` scope: fetching your verified emails needs
it, and a `repo`-only token (which is what `gh` stores by default for
some setups) can't. jjpr degrades to this config rather than guessing,
and says so:

```
A bookmark in the working copy isn't recognized as yours —
likely authored under a different email. Add it with
`[identity] emails = ["..."]` in the jjpr config, or name it explicitly.
```

"Name it explicitly" is the alternative: pass the bookmark to the
command (`jjpr submit my-bookmark`) instead of letting jjpr infer it
from the working copy.

## Configuring forges

Forge-specific authentication and self-hosted instance setup live in
[Forge support](forges.md).
