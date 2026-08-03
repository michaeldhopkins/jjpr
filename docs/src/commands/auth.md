# auth

`jjpr auth` checks or explains forge authentication. Use it when a
forge call returns 401 or 403, or when you're setting up a new
machine.

```
jjpr auth test                        # test forge authentication for the current repo
jjpr auth test --remote origin        # pick a remote when the repo has several
jjpr auth setup                       # show auth setup instructions
```

## test

Detects the forge from your remote URL, resolves the token (env var
or CLI fallback), makes an authenticated API call, and reports the
result. On success, prints the authenticated user. On failure, prints
the error and a hint about which env var to set.

When detection itself fails, the error says which of the reasons it
was rather than a single catch-all: not a jj repo, unreadable config,
no supported remote, or more than one. In the last case it names the
remotes it found:

```
Error: multiple forge remotes found: origin, mirror. Use --remote to specify one.
```

### `--remote <NAME>`

Use a specific git remote instead of detecting one. Only needed when a
repo has more than one remote pointing at a supported forge, which is
otherwise ambiguous. Setting `forge` in `.jj/jjpr.toml` also resolves
it, and takes precedence.

## setup

Prints setup instructions for the detected forge: which env var the
token reads from, how to scope it, and how stored credentials from
`gh` or `glab` are picked up automatically. If no forge is detected
(running outside a jj repo, for instance), prints instructions for
all supported forges — `setup` is what you run *before* a repo is
configured, so it falls back rather than failing.

It accepts the same `--remote <NAME>` flag as `test`.
