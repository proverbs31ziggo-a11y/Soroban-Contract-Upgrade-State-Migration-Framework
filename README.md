# soroban-migrate

Versioned storage-schema migrations for Soroban contracts.

A Soroban contract can replace its Wasm. **Nothing migrates the state underneath.**
Change a struct's shape and every existing persistent entry becomes unreadable — and
because Soroban cannot enumerate a contract's storage, you cannot even find out *how
many* entries you just broke. Teams handle this today with a hand-written migration
contract per upgrade and a lot of prayer.

Protocol 28 acknowledged the problem. [CAP-85](https://github.com/stellar/stellar-protocol/blob/master/core/cap-0085.md)
adds externally managed contract executables, so a fleet's code can be swapped
atomically; [CAP-86](https://github.com/stellar/stellar-protocol/blob/master/core/cap-0086.md)
adds sparse map unpacking, so a struct can be read from data written by an older shape.
CAP-86's own motivation says why it was needed: adding or removing a field "has
rendered contracts unusable after an update".

Those CAPs remove the blockers. This repository is the part that was left: the
developer-facing framework on top — versioned schemas, generated up/down migration
code, a compatibility checker that refuses unsafe upgrades, and a batched executor that
respects a transaction's resource limits. See [`docs/cap-85-86.md`](docs/cap-85-86.md)
for exactly which primitive each mechanism depends on.

---

## What it gives you

| | |
| --- | --- |
| **Versioned schemas** | A shape declares its version with `#[storage_schema(version = N)]`. Snapshots are committed one file per version, like Rails' `db/schema.rb` or Diesel's migration snapshots, so the diff that justified a migration stays checkable after the old struct is deleted. |
| **A compatibility gate** | `soroban-migrate check` diffs every version pair, and the contract's key space against its committed snapshot, refusing upgrades that would make entries unreadable or unreachable, or silently discard a field. It exits `2`, so CI can tell "do not ship this" from "the repository is broken". |
| **Generated migrations** | `soroban-migrate generate` writes the `Migration` implementation — including the tolerant shadow struct that renames and type changes need in order to be re-runnable. |
| **A key index** | Soroban cannot enumerate storage, so the framework keeps an append-only, paged list of the keys the contract has written. One extra read-modify-write per application write buys bounded enumeration. |
| **A batched executor** | A migration over thousands of entries cannot fit one transaction. Batches checkpoint a cursor on chain and resume after failure, and a batch is one atomic transaction, so a failed batch leaves nothing behind. |
| **Rollback** | Walking the same index backwards, plus CAP-85's ability to repoint a fleet at the previous build in a single write. |
| **A pre-flight check** | `preflight` refuses a migration that would leave live entries behind — the failure mode that otherwise looks like success. |
| **A CLI** | `init`, `schema`, `check`, `plan`, `generate` run offline and gate CI. `status`, `run`, and `dry-run` drive and simulate a deployed contract. |

---

## Quick start

```console
$ soroban-migrate init                       # config + snapshots of the current shapes
$ soroban-migrate check                      # exits 2 on anything unsafe
```

Change a struct. Bump its version.

```rust
#[derive(StorageSchema)]
#[storage_schema(version = 2, name = "Balance")]
#[contracttype]
pub struct BalanceV2 {
    pub owner: Address,
    pub amount: i128,
    /// `None` means "written before the freeze flag existed".
    pub frozen: Option<bool>,
}
```

```console
$ soroban-migrate schema export              # snapshot v2
$ soroban-migrate check
$ soroban-migrate generate 1 2               # writes migrations/balance_1_to_2.rs
```

Then drive it against the deployment:

```console
$ soroban-migrate status                     # where is the contract, and where is the migration
$ soroban-migrate run                        # simulate the next batch; sends nothing
$ soroban-migrate run --submit               # apply batches until the migration finishes
```

The worked example is [`examples/vault`](examples/vault). It is a real adoption of the
CLI: its `soroban-migrate.toml`, its committed snapshots, and its generated migration
are all in the tree, and `cargo test` runs the CLI against them.

---

## The four decisions the framework makes for you

### 1. A batch is the unit of atomicity, not an entry

A contract invocation is atomic. If a batch fails — an `Err`, a panic, an exhausted
resource limit — the network discards every write it made, **including the cursor**.
There is no half-applied batch to detect and no compensating transaction to write, and
retrying the same batch is always safe.

This is why there is no `Failed` state in the migration lifecycle: it would require a
second transaction to record, and any operator able to send that transaction can just
retry the batch instead.

### 2. Memory is what binds the batch size

Mainnet caps one invocation at 200 entry reads, 200 writes, 400 footprint entries,
400M CPU instructions and 41.9 MiB of memory. The reasoning most people reach for —
"150 keys means ~150 reads and ~150 writes, so it fits" — is right but not binding.
Measured against enforced Mainnet limits, in a batch that includes `begin`:

| keys | cpu instructions | memory |
| ---- | ---------------- | ------ |
| 1 | 1.3M (0.3%) | 0.5 MiB (1.2%) |
| 32 | 16.3M (4.1%) | 4.2 MiB (10.0%) |
| 100 | 52.4M (13.1%) | 12.9 MiB (30.8%) |
| 150 | 80.3M (20.1%) | 19.4 MiB (46.1%) |

`cargo test -p soroban-migrate --test resource_limits -- --nocapture` prints this
table and fails if a change makes a batch heavier. `MAX_KEYS_PER_BATCH` is `150`,
which is a *guard* and not a recommendation: decode larger entries than these and
memory binds sooner, which is why `dry-run` measures the real contract.

### 3. An unbatched migration is not merely slow

Measured on a 1000-entry contract, the one-shot version of the same migration:

```
1000 writes             (Mainnet cap: 200)
2001 footprint entries  (Mainnet cap: 400)
```

It cannot be submitted as one transaction. A framework that offers a one-shot
`migrate()` offers something that works on test data and fails on production data.
`examples/vault/tests/atomicity.rs` asserts both halves of this.

### 4. Refusing is the default

| change | verdict |
| --- | --- |
| add an `Option` field | safe — un-migrated entries decode it as `None` |
| add a required field | **denied** — every entry that predates it traps on read |
| remove a field | **denied** unless the plan declares `dropped` — the decoder discards it silently |
| change a field's type | **denied** unless the plan declares the conversion |
| make a field required | **denied** unless the plan declares how absent values are filled |
| rename a field | only recognised when the plan declares it; otherwise it is a removal plus an addition |
| `BalanceV1` / `BalanceV2` with no shared `name` | **denied** — two unrelated shapes means no version pair, and `check` would otherwise pass about a change it never looked at |
| rename a key-space variant | **denied** — a stored key's discriminant is the variant's *name*, so every key written under it stops decoding |
| change a key-space variant's payload | **denied** — for the same reason: the encoded vector after the discriminant changes |
| remove a key-space variant | **denied** — the entries written under it become unreachable, and so un-migratable |
| move the `#[migration]` marker | **denied** — the framework's version marker, cursor, and index move to a different variant |
| reorder key-space variants | safe — the generated case list and dispatch arms are both built from declaration order, so they move together |
| add a key-space variant | safe — no existing key refers to it |

Every one of those compiles. None of them is a build error.

The key-space rows are the ones worth reading twice: renaming a variant and reordering variants
look alike and are opposites. Reordering is the intuitive fear and is harmless; renaming is the
thing nobody worries about and is fatal.

---

## What the framework cannot check for you

Two invariants belong to the application, and both fail silently:

1. **`Migration::up` must be idempotent.** A key can be visited more than once: the
   index is a log rather than a set, a batch that fails is retried whole, and a
   rollback followed by a re-run revisits everything. The generated implementations
   are built from operations that cannot accumulate — filling a field only when it is
   empty, or consuming an old key that is *absent* on the second visit.
2. **Application writes during a migration must produce the new shape.** Route them
   through `store::put`, which registers the key under the in-flight target version.
   A write in the old shape after the cursor passed that key is never revisited, and a
   write whose key was never registered is invisible to the next migration.

---

## Honest limitations

* **The framework is not yet audited or released.** It is `0.1.0`, it is not on
  crates.io, and the version pins target `soroban-sdk 28.0.0-rc.1`.
* **Protocol 28 is required**, for the reason in
  [`docs/cap-85-86.md`](docs/cap-85-86.md#protocol-requirements-in-practice). Without
  it an upgrade still has the window this crate exists to remove.
* **`run` and `status` need the [`stellar` CLI](https://developers.stellar.org/docs/tools/cli/stellar-cli)
  and a `[network]` section.** They delegate transaction construction, signing and
  submission to it. This tool never handles a secret key, and it does not reimplement
  fee estimation or auth assembly.
* **`run`, `status` and `dry-run` have never been executed against a live endpoint.**
  What is tested is everything up to the wire: the `stellar` argument vectors, the
  parsing of its output, and the batch/resume loop replayed against a caller that
  returns scripted results. Nothing here has sent a transaction to Testnet or Mainnet,
  so treat the first real run as the first real run.
* **`dry-run` proves the batches fit; it does not prove the migration is correct.** It
  runs the contract's own code against the contract's own state in a forked host, so a
  migration that computes the wrong value computes it identically there. Correctness
  comes from the diff, the plan, and tests that assert on entry shapes.
* **`dry-run` needs a ledger snapshot** (`stellar snapshot create`). Fetching entries
  over RPC *during* a metered replay would corrupt both the measurement and the figures
  derived from it.
* **The executor does not enumerate storage; the application does.** A contract whose
  keys are not registered cannot be migrated by anything, and `preflight` can only
  refuse the upgrade, not fix it.

---

## Repository layout

```
crates/soroban-migrate/         the on-chain runtime: no_std, no allocation surprises
  version.rs                    the schema version, and the reads gated on it
  index.rs                      the append-only, paged key index
  executor.rs                   batched execution, checkpointing, resumption
  rollback.rs                   the same index walked backwards
  preflight.rs                  refuses migrations that would orphan live entries
  fleet.rs                      CAP-85 executable references
  store.rs                      the application's write path
  schema.rs                     schema metadata a deployed contract reports
crates/soroban-migrate-macros/  StorageSchema, Keyspace, Migration derives
crates/soroban-migrate-schema/  the off-chain half: model, parser, diff, codegen
crates/soroban-migrate-cli/     the `soroban-migrate` binary
examples/vault/                 a worked upgrade, adopted with the CLI
docs/cap-85-86.md               which protocol primitive each mechanism uses
```

---

## Verifying it

```console
$ cargo test --workspace                    # 248 tests
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo fmt --all --check
$ RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items
```

Building the example contract needs `stellar-cli` >= 25.2.0 and a Rust compiler other
than 1.91.0 — not just `cargo`:

```console
$ rustup target add wasm32v1-none
$ stellar contract build --package vault-example
```

Three traps make plain `cargo build` unusable here, and the toolchain enforces all three
rather than warning about them.

* **The compiler must be one `stellar contract build` accepts.** It refuses 1.91.0
  outright, and rejects 1.82 through 1.83, because those compilers miscompile Wasm. So
  the workspace's `rust-version = "1.91.0"` — a claim about what compiles the crates,
  which CI does test at exactly that version — is deliberately *not* the version that
  builds the artifact. CI builds the contract with 1.98.1.
* **The target must be `wasm32v1-none`.** `wasm32-unknown-unknown` is rejected on Rust
  1.82+, which enables `reference-types` and `multi-value` that the host does not
  support.
* **The spec is only correct once the build system has shaken it.** A build that skips
  that step links and exports correctly while carrying a wrong public interface, so
  `cargo build --target wasm32v1-none` only tells you the code compiles for the host.

`stellar contract build` builds the `release` profile rather than `[profile.contract]`;
pass `--profile contract` for the smaller, stripped artifact.

The tests that carry the most weight, and what they are evidence for:

| test | what it establishes |
| --- | --- |
| `soroban-migrate/tests/resource_limits.rs` | a full batch fits Mainnet's enforced limits, and memory is what binds |
| `soroban-migrate/tests/executor.rs` | resumption, idempotency, and the version advancing in the batch that finishes |
| `examples/vault/tests/atomicity.rs` | a failed batch leaves no entries and no cursor; the unbatched migration exceeds the caps |
| `examples/vault/tests/upgrade.rs` | the whole upgrade end to end on a deployed contract, with the application serving throughout |
| `soroban-migrate-cli/tests/example_project.rs` | the CLI on a real project: snapshots current, gate passing, generated file reproducible byte for byte |

## License

Apache-2.0.
