# jjpr

jjpr manages stacked pull requests for [Jujutsu](https://jj-vcs.github.io/jj/)
repositories. It pushes bookmarks, creates and updates PRs/MRs, merges
them, and syncs the stack on GitHub, GitLab, and Forgejo.

`jjpr watch` is the main driver. It creates draft PRs, promotes them
when CI passes, merges from the bottom up once approved, and keeps the
rest of the stack rebased onto the new base.

```
jj bookmark set auth
jj bookmark set profile
jjpr watch
```

## Install

```
brew install michaeldhopkins/tap/jjpr      # Homebrew
cargo binstall jjpr                         # cargo-binstall
cargo install jjpr                          # crates.io
```

Requires Rust 1.91+ to build from source and
[jj](https://jj-vcs.github.io/jj/) 0.36+ at runtime.

## Documentation

Full docs at
[**michaeldhopkins.com/docs/jjpr**](https://michaeldhopkins.com/docs/jjpr/):
quickstart, per-command reference, configuration, forge support, and
troubleshooting.

The doc sources are in [`docs/src/`](docs/src/) and are hand-edited.
Run `./generate-docs.sh` to rebuild the site and mirror it into the
`michaeldhopkins.com` repo.

## Development

```
cargo test               # unit + jj integration tests
cargo clippy --tests     # lint
JJPR_E2E=1 cargo test    # add E2E tests against a real forge
```

Test tiers:

- **Unit**: fast, no I/O, stub `Jj` and `Forge` traits.
- **jj integration**: real `jj` binary against temp repos, no network.
- **E2E**: real `jj` and a real forge against
  [jjpr-testing-environment](https://github.com/michaeldhopkins/jjpr-testing-environment),
  gated by `JJPR_E2E`.

Contributor conventions live in [`CLAUDE.md`](CLAUDE.md).

## License

MIT or Apache-2.0.
