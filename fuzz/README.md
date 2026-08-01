# Fuzzing jjpr

Coverage-guided fuzzing with [`cargo-fuzz`] / libFuzzer. The general method — the two
budgets, the target-kind catalog, the seeding ladder, the CI shape, the gotchas — is in
the **`rust-fuzzing` skill**. This file is the operator's guide; what is specific to
jjpr's *targets* is in [`../AGENTS.md`](../AGENTS.md).

This directory is a **standalone workspace** (note the empty `[workspace]` in
`Cargo.toml`), so the root `cargo build` / `test` / `clippy` / `deny` never see it. It
builds only under `cargo fuzz`, which needs nightly for `-Zsanitizer`; jjpr itself stays
on stable.

## One-time setup

```sh
rustup toolchain install nightly --component rust-src
cargo install cargo-fuzz
```

## Run

Always select nightly with `+nightly`. cargo-fuzz invokes the inner build from the
**repo root**, and rustup resolves `rust-toolchain.toml` by the *current directory*, so
a toolchain file inside `fuzz/` would not be picked up; the `+nightly` override
propagates via `RUSTUP_TOOLCHAIN`.

```sh
# from the repo root
cargo +nightly fuzz build                       # all targets: do they compile + link?
cargo +nightly fuzz run jj_output               # until a crash or Ctrl-C
cargo +nightly fuzz run jj_output -- -max_total_time=600
cargo +nightly fuzz run jj_output -- -dict=fuzz/dict/jj_output.dict
```

Replay the saved corpus without mutating — the deterministic regression gate CI runs on
every push:

```sh
cargo +nightly fuzz run jj_output -- -runs=0 fuzz/corpus/jj_output
```

## Targets

| Target | Kind | Asserts |
|---|---|---|
| `jj_output` | never-panics | `parse_log_output` / `parse_bookmark_output` survive any subprocess bytes |
| `remote_url` | invariant | a credential in a remote URL never reaches owner/repo/host; detection and config-pinned parsing agree |
| `comment_roundtrip` | roundtrip | a nav comment jjpr generates is one jjpr can parse back, faithfully and first |
| `forge_payload` | schema-resilience | any subset of a real stack payload's keys may go missing |
| `config_load` | never-panics + determinism | `.jjpr.toml` parsing never panics and is deterministic |
| `graph_invariants` | invariant | a graph built from fuzzed jj output has no cycle in `adjacency_list` |

## A crash

libFuzzer writes the reproducing input to `fuzz/artifacts/<target>/crash-<hash>`.

```sh
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/crash-<hash>   # reproduce
cargo +nightly fuzz fmt <target> fuzz/artifacts/<target>/crash-<hash>   # show as a value
```

Then add the minimized input as a **unit test** next to the code it broke, fix the bug,
and keep the input as a `seed-*` so the replay gate holds it down forever.

Read the artifact kind before assuming a bug: `crash-*` is a panic/abort and is real;
`timeout-*` may be sanitizer slowness, so replay it on a normal build first;
`slow-unit-*` is the perf/DoS signal, most interesting when the input is *small*.

## Corpus and seeds

The canonical corpus lives in the nightly's Actions cache, not in git. Only `seed-*`
files are committed — they are what a fresh clone and a cold cache fuzz from. Shrink an
overgrown local corpus with `cargo +nightly fuzz cmin <target>`.

The dictionaries in `dict/` are derived from the template constants in
`src/jj/templates.rs`; regenerate them when a template changes:

```sh
grep -oE '"[a-zA-Z]+":' src/jj/templates.rs | sort -u | sed 's/"/\\"/g; s/^/"/; s/$/"/' \
  > fuzz/dict/jj_output.dict
```

## CI

- `.github/workflows/fuzz-replay.yml` — every push/PR. Replays each target's corpus
  (`-runs=0`). Deterministic, minutes, read-only on the corpus cache.
- `.github/workflows/fuzz.yml` — nightly. Builds once, fans out to shards, merges each
  target's corpus back, then reports coverage. Budget is 4h/shard, under GitHub's 6h
  hard cap so the job **completes** — a cancelled job swallows the crash signal.

## Reproducibility

`+nightly` uses whatever nightly is installed. If a churny nightly ever breaks a run,
pin a dated one (`rustup toolchain install nightly-2026-08-01`, and set the CI's
`dtolnay/rust-toolchain` to the same date) and bump it deliberately.

[`cargo-fuzz`]: https://github.com/rust-fuzz/cargo-fuzz
