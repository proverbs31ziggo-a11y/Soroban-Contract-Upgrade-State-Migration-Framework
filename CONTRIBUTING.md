# Contributing to soroban-migrate

Thanks for helping. This project rewrites live contract state, so the bar for
correctness is high — but the bar for *getting started* is low. Most open issues are
scoped so that one person can finish one in a sitting.

## Finding something to work on

* **[`good first issue`](https://github.com/proverbs31ziggo-a11y/Soroban-Contract-Upgrade-State-Migration-Framework/issues?q=is%3Aopen+label%3A%22good+first+issue%22)** —
  small, self-contained, with the files to touch named in the issue.
* Every issue carries a **complexity** label:

  | label | what it usually means |
  | --- | --- |
  | `complexity: trivial` | docs, a doctest, a single flag; under a day |
  | `complexity: medium` | one module, new tests; a few days |
  | `complexity: high` | a design decision or a change across crates; discuss first |

* Every issue carries an **area** label (`area: runtime`, `area: schema`, `area: cli`,
  `area: macros`, `area: docs`, `area: ci`) naming the crate it lives in.

Comment on an issue before you start so two people do not do the same work. If you have
not opened a PR within 7 days of being assigned, the issue may be reassigned — just say
so if you need longer.

For `complexity: high` issues, post a short plan (a few bullet points) on the issue and
wait for a maintainer to agree before writing code.

## Setting up

You need Rust **1.91.0** or later.

```console
$ git clone https://github.com/proverbs31ziggo-a11y/Soroban-Contract-Upgrade-State-Migration-Framework
$ cd Soroban-Contract-Upgrade-State-Migration-Framework
$ cargo test --workspace
```

That is all you need for every crate except the contract build. To build the example
contract to Wasm you also need [`stellar-cli`](https://developers.stellar.org/docs/tools/cli/stellar-cli)
>= 25.2.0, the `wasm32v1-none` target, and a compiler **other than 1.91.0** (see
"Verifying it" in the [README](README.md#verifying-it) for why):

```console
$ rustup target add wasm32v1-none
$ stellar contract build --package vault-example
```

## Before you open a PR

CI runs exactly these, and a PR must pass all of them:

```console
$ cargo fmt --all --check
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo test --workspace
$ RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items
```

If you changed anything under `examples/vault/src/`, also run the adoption gate from
`examples/vault/`:

```console
$ cargo run -q -p soroban-migrate-cli -- check
$ cargo run -q -p soroban-migrate-cli -- generate 1 2 --force && git diff --exit-code -- migrations/
```

## What a good PR looks like

* **One issue per PR**, with `Closes #N` in the description.
* **Tests for behaviour, not just coverage.** A bug fix comes with a test that fails
  without the fix. A new `check` rule comes with a test for the case it refuses *and*
  the nearest case it must still allow.
* **Comments explain why.** The codebase documents the reason behind each decision
  (look at `executor.rs` or `diff.rs` for the house style). A comment that restates the
  code is not needed; one that explains a non-obvious constraint is.
* **A `CHANGELOG.md` entry** under `[Unreleased]` for anything a user would notice. Read
  the note at the top of the changelog: a stricter `check` verdict, or different
  `generate` output for the same plan, is a breaking change even if no API changed.

## Rules that are not negotiable

These protect people's deployed contracts. A PR that breaks one will not be merged
however good the rest is.

1. **`check` refuses by default.** A new kind of change is denied until there is a test
   proving it is safe.
2. **A batch must fit Mainnet's limits.** `tests/resource_limits.rs` fails if a change
   makes a batch heavier; do not raise its thresholds to make it pass.
3. **No secret keys.** The CLI delegates signing to `stellar`. It must never read,
   store, or log a secret.
4. **The runtime crate stays `no_std`** and adds no dependencies without discussion.

## Commit messages

Write the subject in the imperative ("Add `--json` to `status`"), under about 70
characters. In the body, say what was wrong and why the change fixes it.

## Releasing (maintainers)

1. Move the `[Unreleased]` changelog entries under a new version heading.
2. Bump `version` in the workspace `Cargo.toml` and the `workspace.dependencies` paths.
3. Tag `vX.Y.Z` on `main` and publish the crates in dependency order:
   `soroban-migrate-schema`, `soroban-migrate-macros`, `soroban-migrate`,
   `soroban-migrate-cli`.

## Conduct

Everyone taking part is expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Questions

Open an issue with the `question` label. There are no silly questions about Soroban
storage — the platform's behaviour here is subtle, and that is why this project exists.
