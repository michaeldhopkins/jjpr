# auth

`jjpr auth` checks or explains forge authentication. Use it when a
forge call returns 401 or 403, or when you're setting up a new
machine.

```
jjpr auth test                        # test forge authentication for the current repo
jjpr auth setup                       # show auth setup instructions
```

## test

Detects the forge from your remote URL, resolves the token (env var
or CLI fallback), makes an authenticated API call, and reports the
result. On success, prints the authenticated user. On failure, prints
the error and a hint about which env var to set.

## setup

Prints setup instructions for the detected forge: which env var the
token reads from, how to scope it, and how stored credentials from
`gh` or `glab` are picked up automatically. If no forge is detected
(running outside a jj repo, for instance), prints instructions for
all supported forges.
