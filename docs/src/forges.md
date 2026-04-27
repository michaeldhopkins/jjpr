# Forge support

jjpr auto-detects the forge from your remote URL and talks directly to
the forge's API over HTTP. Neither `gh` nor `glab` is required, but
jjpr picks up their stored credentials when they're present.

| Forge | Token env var | CLI fallback |
|---|---|---|
| GitHub | `GITHUB_TOKEN` or `GH_TOKEN` | `gh auth login` (reads stored credentials) |
| GitLab | `GITLAB_TOKEN` | `glab auth login` (reads stored credentials) |
| Forgejo / Codeberg | `FORGEJO_TOKEN` | none |

Auto-detection recognizes `github.com`, `gitlab.com`, and
`codeberg.org`, plus Enterprise subdomains for GitHub and GitLab. For
self-hosted instances, set `forge` in `.jj/jjpr.toml`. See
[Repo-local config](configuration.md#repo-local-config).

## GitHub

If you already use `gh`, jjpr finds your credentials automatically.
Otherwise, export `GITHUB_TOKEN` (or `GH_TOKEN`) with at least `repo`
scope.

GitHub Enterprise is auto-detected from the host
(`*.github.enterprise.example.com`). Credentials come from `gh` if it's
configured for that host.

## GitLab

If you use `glab`, jjpr picks up your credentials. Otherwise, export
`GITLAB_TOKEN` with `api` scope.

Self-hosted GitLab is auto-detected from the host. No extra config is
needed for gitlab.com or for any GitLab instance with a recognizable
URL pattern.

## Forgejo and Codeberg

Generate an API token with `repo` scope from your instance's settings.
For Codeberg, that's at
`https://codeberg.org/user/settings/applications`. Export it:

```
export FORGEJO_TOKEN=your_token_here
```

For self-hosted Forgejo, set the forge type in `.jj/jjpr.toml`:

```toml
forge = "forgejo"
```

Auto-detection only recognizes `codeberg.org`. Other Forgejo hosts
need the explicit `forge` setting.

## Verifying

```
jjpr auth test
```

Reports the detected forge, the token source, and the authenticated
user. See the [auth command](commands/auth.md) for details.
