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
