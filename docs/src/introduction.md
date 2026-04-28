# jjpr

jjpr manages stacked pull requests for [Jujutsu](https://jj-vcs.github.io/jj/)
repositories. It pushes bookmarks, creates and updates PRs/MRs, merges
them, and syncs the stack on GitHub, GitLab, and Forgejo.

See [Installation](installation.md) to get started.

> Stacked PRs are chains of small pull requests that branch off of
> each other. They are better structured and easier to review than
> one large PR, and they let the developer focus on one feature by
> working ahead of the reviewer.

## Commands

`jjpr submit` and `jjpr watch` are what most users want. `submit`
pushes up the current stack of bookmarks as a stack of PRs to the
forge. `watch` runs in a loop, creating PRs for new bookmarks,
promoting drafts when CI passes, and merging from the bottom up once
each PR is approved.

Other lower-level commands are there for manual control and debugging.

- [merge](commands/merge.md): merge a stack from the bottom up, one-shot.
- [status](commands/status.md): show the current stack and PR state.
- [auth](commands/auth.md): test or set up forge authentication.
- [config](commands/config.md): manage config files.

## Reference

- [Configuration](configuration.md)
- [Forge support](forges.md)
- [How it works](how-it-works.md)
- [Troubleshooting](troubleshooting.md)
