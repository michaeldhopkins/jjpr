# Installation

Requires Rust 1.91+ when building from source. Runtime requires
[jj](https://jj-vcs.github.io/jj/) 0.36+ and a colocated jj/git
repository with a supported remote.

## Homebrew

```
brew tap michaeldhopkins/tap
brew install jjpr
```

## cargo-binstall

```
cargo binstall jjpr
```

Pulls a pre-built binary if one is published for your platform; falls
back to building from source.

## crates.io

```
cargo install jjpr
```

## From source

```
git clone https://github.com/michaeldhopkins/jjpr
cargo install --path jjpr
```

## Verifying

```
jjpr --version
```

Confirms the binary is on your `$PATH` and reports the installed
version.

## Next: authentication

jjpr needs an API token (or `gh` / `glab` credentials) to talk to the
forge. See [Forge support](forges.md) for token env vars and
self-hosted setup, and [auth](commands/auth.md) for verifying that
jjpr can authenticate from the current repo.

## Tell your coding agent about jjpr

Coding agents don't know jjpr is installed, so they reach for `git`,
`gh` or `jj undo` instead. Paste this into the instructions file your
agent reads: `AGENTS.md` at the root of the repository (read by most
agents), or `CLAUDE.md` for Claude Code, which can import `AGENTS.md`
with a first line of `@AGENTS.md`.

```markdown
## Pull requests: jjpr

This repo uses jj (Jujutsu) with jjpr for stacked pull requests. Use
jjpr for anything that touches pull requests; never `gh pr`, `git push`
or `jj git push` for a bookmark jjpr manages.

- `jjpr` shows each stack and its PRs (read-only).
- `jjpr submit` pushes the stack and creates or updates its PRs.
- `jjpr merge` merges a stack from the bottom up; `jjpr watch` drives
  it to merged.
- `jjpr undo` takes back the last jjpr command, locally and on the
  forge; `jjpr redo` puts it back. Never use `jj undo` or
  `jj op restore` for this: they don't touch the forge.
- Add `--dry-run` to see what a command would do first. When jjpr
  refuses or fails, read its last lines: they say what to do next.
- `jjpr <command> --help` lists every option, with examples.
  Docs: https://michaeldhopkins.com/docs/jjpr/
```
