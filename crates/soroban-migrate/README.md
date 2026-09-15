# soroban-migrate

The on-chain half of the [`soroban-migrate`](https://github.com/soroban-migrate/soroban-migrate)
framework: everything a *contract* needs to version its storage and migrate it.

`no_std`, no `alloc`-dependent surprises, and no host function newer than protocol 26
in its own code — the protocol-28 requirement comes from the `#[contracttype]`
decoding it depends on. See [`docs/cap-85-86.md`](https://github.com/soroban-migrate/soroban-migrate/blob/main/docs/cap-85-86.md).

## The modules

| module | what it is |
| --- | --- |
| `version` | the schema version, in instance storage, and the reads gated on it |
| `index` | an append-only, paged list of the keys the contract has written |
| `executor` | batched execution, cursors, resumption — the production path |
| `rollback` | the same index walked backwards, newest first |
| `preflight` | refuses a migration that would leave live entries behind |
| `fleet` | CAP-85 executable references: atomic fleet upgrades and cheap rollback |
| `store` | the application's write path, which keeps the index in step with the data |
| `schema` | the shape metadata a deployed contract reports about itself |
| `migration` | the `Migration` trait, and the state a run is in |

## Using it

```rust
use soroban_migrate::migration::{EntryOutcome, Migration, MigrationState};
use soroban_migrate::{executor, preflight, MigrationError};
use soroban_sdk::{contractimpl, Env};

#[contractimpl]
impl Vault {
    pub fn begin_migration(env: Env) -> Result<MigrationState, MigrationError> {
        preflight::assert_upgradeable::<AddFreezeFlag>(&env)?;
        executor::begin::<AddFreezeFlag>(&env)
    }

    /// One call is one transaction.
    pub fn migrate_batch(env: Env, limit: u32) -> Result<BatchOutcome, MigrationError> {
        executor::batch::<AddFreezeFlag>(&env, limit)
    }
}
```

`#[derive(Migration)]` generates `AddFreezeFlag` for additive changes; renames and type
changes need the tolerant shadow struct that `soroban-migrate generate` writes.

## Two invariants you own

1. `Migration::up` **must be idempotent** — a key can be visited more than once.
2. Application writes during a migration **must produce the new shape** — route them
   through `store::put`.

Both fail silently if you get them wrong, which is why they are documented on the
items rather than only here. The full explanation is in the
[root README](https://github.com/soroban-migrate/soroban-migrate#readme).

## License

Apache-2.0.
