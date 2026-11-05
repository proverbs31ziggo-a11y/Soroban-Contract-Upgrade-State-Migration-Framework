# Roadmap

Where soroban-migrate is going, in the order it gets there. Each item links to its
tracking issue; most are open for contributors. See [CONTRIBUTING.md](../CONTRIBUTING.md)
for how to pick one up.

## Done (0.1.0)

* Versioned schemas with `#[storage_schema(version = N)]` and committed snapshots
* `check`: a compatibility gate over every version pair and the key space, exiting `2`
  on anything unsafe
* `generate`: up-migrations, including the tolerant shadow struct for renames and type
  changes
* The paged key index, the batched and resumable executor, rollback, and `preflight`
* CAP-85 fleet references
* The CLI: `init`, `schema`, `check`, `plan`, `generate`, `status`, `run`, `dry-run`
* A worked example (`examples/vault`) adopted with the CLI and gated in CI

## Milestone 1 — proven on a live network

The framework is tested up to the network boundary. This milestone takes it across.

* ~~Run `status` and `run` against Testnet, and record the contract IDs and
  transaction hashes~~ — done, see [TESTNET.md](TESTNET.md) (#15)
* Run `dry-run` against Testnet — blocked on the protocol-30 SDK (#20)
* A version-1 build of the example, so a live run rewrites old-shaped entries — #34
* Compile generated migrations in CI, not only compare their text — #35
* `status` on a finished migration stops suggesting another batch — #33
* Test the claim that the framework runs on protocol 27 and only decoding needs 28 — #14
* Measure the key index's write amplification instead of asserting it — #13
* Move to the protocol-30 `soroban-sdk`, and settle the pinning policy — #20

## Milestone 2 — easy to adopt

* A guide for retrofitting the framework onto a contract that is already deployed — #19
* `--json` output on every subcommand, so CI and scripts never parse prose — #16
  (split into #24, #25, #26, #27, #28)
* Shell completions — #30
* A "Your first migration" tutorial — #32
* Compile the doc examples instead of marking them `ignore` — #11 (split into #21, #22, #23)
* Generate `down()` when a version change is reversible — #29
* `dry-run` checks that every migrated entry reads back under the new schema — #18

## Milestone 3 — covers every real schema

* ~~Follow shape changes nested inside a field's type~~ — done (#7)
* Enums as stored values — #8
* Temporary storage, and a defined outcome when an entry expires mid-migration — #9
* Changes of storage durability between versions — #10

## Milestone 4 — hardened

* Property and fuzz tests for the parser and the diff — #12
* Dependency advisories and license checks in CI — #31
* An external review of the runtime crate before 1.0
* Publish the crates to crates.io

## Non-goals

* **Signing or submitting transactions.** That stays with `stellar-cli`.
* **Enumerating storage the framework did not index.** Soroban cannot do it; the
  framework does not pretend to.
* **One-shot migrations.** A migration over production data cannot fit one transaction;
  see "An unbatched migration is not merely slow" in the README.
