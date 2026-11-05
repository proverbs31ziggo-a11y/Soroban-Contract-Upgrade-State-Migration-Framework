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

You need Rust **1.91.0** or later, with `rustfmt` and `clippy`.

```console
$ git clone https://github.com/proverbs31ziggo-a11y/Soroban-Contract-Upgrade-State-Migration-Framework
$ cd Soroban-Contract-Upgrade-State-Migration-Framework
$ cargo test --workspace
```

That is all you need for every crate except the contract build. The test suite needs
**no network, no `stellar` CLI, and no Wasm target**: every test runs against the
in-process host from `soroban-sdk`'s `testutils`, and the CLI's RPC and `stellar` calls
are tested by checking the command lines they build, not by running them. The slowest
part is `crates/soroban-migrate/tests/resource_limits.rs`, which deliberately pushes a
ledger past its budget. Running against a live network is a manual step, recorded in
[`docs/TESTNET.md`](docs/TESTNET.md); it is not part of `cargo test` and not part of CI.

### Two toolchains, on purpose

CI uses two compilers, and this looks like an inconsistency. It is not.

| what | compiler | why |
| --- | --- | --- |
| fmt, clippy, tests, docs, adoption gate | **1.91.0** exactly | the workspace `rust-version`; CI pins it so the MSRV claim is actually tested |
| building the deployable contract | **1.98.1** (any accepted stable) | `stellar contract build` refuses 1.91.0, and 1.82–1.83, because they miscompile Wasm |

So do not "fix" the `contract` job to use `rust-version`, and do not raise
`rust-version` to match the contract job. To reproduce the `check` job exactly, run the
commands below with `cargo +1.91.0`. To build the example contract to Wasm you also need
[`stellar-cli`](https://developers.stellar.org/docs/tools/cli/stellar-cli) >= 25.2.0
(CI pins 28.0.0) and the `wasm32v1-none` target:

```console
$ rustup target add wasm32v1-none
$ stellar contract build --package vault-example
```

Plain `cargo build --target wasm32v1-none` is not a substitute: it skips the contract
spec step, so the result compiles but is not deployable. The
[README](README.md#verifying-it) and the comment above `[profile.contract]` in
`Cargo.toml` give the details.

## Before you open a PR

CI runs exactly these, and a PR must pass all of them:

```console
$ cargo fmt --all --check
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo test --workspace
$ RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items
```

**Clippy pedantic lints are errors.** `soroban-migrate`, `soroban-migrate-schema`, and
`soroban-migrate-cli` each set `#![warn(clippy::pedantic)]` at the crate root, and
`-D warnings` turns every warning into a failure. Expect lints such as `doc_markdown`,
`cast_possible_truncation`, and `needless_pass_by_value` on code that looks fine. Fix
the code where you can. If a lint is wrong for one item, put `#[allow(clippy::...)]` on
that item with a comment saying why. Do not add crate-wide allows to get a PR through.

If you changed anything under `examples/vault/src/`, also run the adoption gate from
`examples/vault/`:

```console
$ cargo run -q -p soroban-migrate-cli -- check
$ cargo run -q -p soroban-migrate-cli -- generate 1 2 --force && git diff --exit-code -- migrations/
```

## Where code runs: `no_std` and Wasm

Two kinds of crate live in this workspace, and the difference matters more than anything
else here.

| crate | runs where | `std`? |
| --- | --- | --- |
| `soroban-migrate` (runtime) | inside the deployed contract, as Wasm | **no** — `#![no_std]` |
| `examples/vault` | inside the deployed contract, as Wasm | **no** — `#![no_std]` |
| code emitted by `soroban-migrate-schema`'s `codegen.rs` | inside the user's contract | **no** |
| `soroban-migrate-schema`, `soroban-migrate-macros` | on the developer's machine, at build time | yes |
| `soroban-migrate-cli` | on the developer's machine | yes |

The two most likely first contributions both cross this line:

* **A new schema change kind.** The classification (`ChangeKind` and the `check`
  verdict in `crates/soroban-migrate-schema/src/diff.rs`) is host code, and `std` is
  fine there. But if the change kind needs a new generated migration, the code
  `codegen.rs` emits compiles into a `no_std` contract. It may use only `soroban_sdk`
  and `soroban_migrate`: no `String`, `Vec`, `format!`, or `std::` paths. Use
  `soroban_sdk::{String, Vec, Map, Bytes}` instead. If it needs runtime support, that
  goes in `soroban-migrate`, with the same restriction.
* **A new CLI subcommand.** It goes in `crates/soroban-migrate-cli/src/commands/`, where
  `std` is fine. But the CLI reuses the runtime crate's types and constants on purpose
  (see the comment in its `Cargo.toml`). If you need a new shared type, add it to
  `soroban-migrate` in `no_std` form. Do not copy it into the CLI, and do not give the
  runtime a `std` feature.

`cargo test` will not catch a `std` leak into generated code, because tests compile it
for the host. Build the example with `stellar contract build --package vault-example`
before opening a PR that changes `codegen.rs` or `soroban-migrate`. The `contract` CI
job will also catch it.

For the protocol background (why fleets, executable references, and the batch limits
exist), read [`docs/cap-85-86.md`](docs/cap-85-86.md).

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
   Neither does generated migration code (see "Where code runs" above).

## Commit messages

Write the subject in the imperative ("Add `--json` to `status`"), under about 70
characters. In the body, say what was wrong and why the change fixes it.

## Releasing (maintainers)

Four crates are published. `vault-example` has `publish = false` and is never
released.

1. Make sure `main` is green on all three CI jobs (`check`, `example`, `contract`).
2. Move the `[Unreleased]` changelog entries under a new version heading. Decide the
   version bump using the note at the top of the changelog: a stricter `check` or
   different `generate` output counts as breaking.
3. Bump `version` under `[workspace.package]`, and the matching `version` of each
   internal crate under `[workspace.dependencies]`. They must agree, or `cargo publish`
   resolves the wrong version.
4. Commit, then tag `vX.Y.Z` on `main`.
5. Publish in this order, waiting for each crate to appear on crates.io before the
   next:

   ```console
   $ cargo publish -p soroban-migrate-schema
   $ cargo publish -p soroban-migrate
   $ cargo publish -p soroban-migrate-macros
   $ cargo publish -p soroban-migrate-cli
   ```

   `soroban-migrate` comes before `soroban-migrate-macros` because the macros crate
   has a versioned dev-dependency on it, and `cargo publish` must resolve that from
   crates.io. Run `cargo publish --dry-run -p <crate>` first if you are unsure.

## Conduct

Everyone taking part is expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Questions

Open an issue with the `question` label. There are no silly questions about Soroban
storage — the platform's behaviour here is subtle, and that is why this project exists.
