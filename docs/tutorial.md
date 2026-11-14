# Your First Migration

This tutorial walks through a complete storage schema upgrade from scratch using `soroban-migrate`: starting with a minimal `Counter` contract, adopting the framework's key index and schema snapshots, evolving the storage shape from version 1 to version 2, verifying the migration in a `soroban_sdk::Env::default()` test, and seeing how `soroban-migrate check` refuses an unsafe upgrade before it can ship.

---

## 1. Start with a Minimal Contract

Suppose you have a Soroban contract that stores a `Counter` struct in persistent storage:

```rust
#![no_std]
use soroban_sdk::{contract, contractimpl, contracttype, Env};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Counter {
    pub value: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Counter(u32),
}

#[contract]
pub struct CounterContract;

#[contractimpl]
impl CounterContract {
    pub fn increment(env: Env, id: u32) -> Counter {
        let key = DataKey::Counter(id);
        let current = env
            .storage()
            .persistent()
            .get::<_, Counter>(&key)
            .unwrap_or(Counter { value: 0 });
        let next = Counter {
            value: current.value + 1,
        };
        env.storage().persistent().set(&key, &next);
        next
    }
}
```

If you later add a field to `Counter` and upgrade the contract's Wasm, two problems arise immediately:

1. **Decoding:** Any existing entry written by the old code lacks the new field.
2. **Enumeration:** Soroban cannot list all keys in storage, so a migration function cannot discover which `DataKey::Counter(id)` entries exist unless they were indexed on write.

---

## 2. Adopt `soroban-migrate`

Add `soroban-migrate` and `soroban-migrate-macros` to your contract's `Cargo.toml`:

```toml
[dependencies]
soroban-sdk = "28.0.0-rc.2"
soroban-migrate = { path = "../../crates/soroban-migrate" }
soroban-migrate-macros = { path = "../../crates/soroban-migrate-macros" }
```

Next, wire three hooks into the contract:

1. **Keyspace (`#[derive(Keyspace)]`):** Add a `#[migration] Migration(MigrationKey)` variant to `DataKey` so the framework's version marker, cursor, and paged key index live in their own collision-free namespace.
2. **Versioned Schema (`#[derive(StorageSchema)]`):** Annotate `CounterV1` with `#[storage_schema(version = 1, name = "Counter")]`.
3. **Indexed Writes (`store::put`):** Initialize the schema version on deploy with `version::initialize::<DataKey>(&env, 1)` and write entries via `store::put::<DataKey, CounterV1>(&env, &key, &next)` so every written key is registered in the append-only key index.

```rust
#![no_std]
use soroban_migrate::key::MigrationKey;
use soroban_migrate::{store, version, MigrationError};
use soroban_migrate_macros::{Keyspace, StorageSchema};
use soroban_sdk::{contract, contractimpl, contracttype, Env, IntoVal, Val};

#[derive(StorageSchema)]
#[storage_schema(version = 1, name = "Counter")]
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CounterV1 {
    pub value: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq, Keyspace)]
pub enum DataKey {
    #[migration]
    Migration(MigrationKey),
    Counter(u32),
}

#[contract]
pub struct CounterContract;

#[contractimpl]
impl CounterContract {
    pub fn initialize(env: Env) -> Result<u32, MigrationError> {
        version::initialize::<DataKey>(&env, 1)
    }

    pub fn increment(env: Env, id: u32) -> CounterV1 {
        let key: Val = DataKey::Counter(id).into_val(&env);
        let current = env
            .storage()
            .persistent()
            .get::<Val, CounterV1>(&key)
            .unwrap_or(CounterV1 { value: 0 });
        let next = CounterV1 {
            value: current.value + 1,
        };
        store::put::<DataKey, CounterV1>(&env, &key, &next);
        next
    }
}
```

Now initialize the migration project from your contract directory:

```console
$ soroban-migrate init
wrote soroban-migrate.toml
wrote migrations/counter_1.json
wrote migrations/keyspace.json
```

`soroban-migrate init` creates `soroban-migrate.toml` and commits baseline snapshots of both `Counter` v1 (`migrations/counter_1.json`) and `DataKey` (`migrations/keyspace.json`).

---

## 3. Add `last_updated: Option<u64>` and Generate the v1 -> v2 Migration

Now evolve `Counter` to record the ledger timestamp when the counter was last updated. Keep `CounterV1` in source so the migration can transform v1 entries into v2, and add `CounterV2` with `version = 2` and the same `name = "Counter"` identity:

```rust
#[derive(StorageSchema)]
#[storage_schema(version = 2, name = "Counter")]
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CounterV2 {
    pub value: u32,
    /// `None` for entries written at v1 before timestamps were tracked.
    pub last_updated: Option<u64>,
}
```

Export the v2 snapshot, verify compatibility with `check`, and generate the migration module:

```console
$ soroban-migrate schema export
wrote migrations/counter_2.json

$ soroban-migrate check
schema changes
==============

Counter v1 -> v2
----------------
  INFO  added optional field `last_updated`: Option<u64>
        Un-migrated v1 entries unpack `last_updated` as `None` under CAP-86.

  verdict: safe

$ soroban-migrate generate 1 2
wrote migrations/counter_1_to_2.rs
```

Because `last_updated` is `Option<u64>`, CAP-86 sparse map unpacking allows v2 contract code to read un-migrated v1 entries immediately (receiving `last_updated: None`) while background batches rewrite the entries to v2.

---

## 4. Run the Migration in a `soroban_sdk::Env::default()` Test

Wire the generated migration into `executor::begin`, `executor::batch`, and `preflight::assert_upgradeable` (just like `examples/vault/src/lib.rs`), then test the entire v1-to-v2 upgrade in memory with `soroban_sdk::Env::default()`:

```rust
use soroban_migrate::migration::{BatchOutcome, Migration, MigrationState};
use soroban_migrate::{executor, preflight, store, version, MigrationError};
use soroban_sdk::{Env, IntoVal, TryFromVal, Val};

pub struct AddLastUpdated;

impl Migration for AddLastUpdated {
    type Keyspace = DataKey;

    const FROM_VERSION: u32 = 1;
    const TO_VERSION: u32 = 2;

    fn up(env: &Env, key: &Val) -> Result<bool, MigrationError> {
        let storage = env.storage().persistent();
        let Some(raw) = storage.get::<Val, Val>(key) else {
            return Ok(false);
        };
        let Ok(mut entry) = CounterV2::try_from_val(env, &raw) else {
            return Ok(false);
        };
        if entry.last_updated.is_none() {
            entry.last_updated = Some(0);
            storage.set(key, &entry);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[test]
fn migrates_v1_counters_to_v2_and_advances_version_on_final_batch() {
    let env = Env::default();
    let contract_id = env.register(CounterContract, ());

    // 1. Seed three v1 entries through the indexed store.
    env.as_contract(&contract_id, || {
        version::initialize::<DataKey>(&env, 1).unwrap();
        for id in 1..=3 {
            let key: Val = DataKey::Counter(id).into_val(&env);
            store::put::<DataKey, CounterV1>(&env, &key, &CounterV1 { value: id * 10 });
        }
    });

    // 2. Run preflight, begin the v1 -> v2 migration, and apply in batches of 2.
    env.as_contract(&contract_id, || {
        let report = preflight::assert_upgradeable::<AddLastUpdated>(&env).unwrap();
        assert_eq!(report.version, Some(1));
        assert_eq!(report.index_len, 3);

        let state: MigrationState = executor::begin::<AddLastUpdated>(&env).unwrap();
        assert_eq!(state.total, 3);

        let batch_1: BatchOutcome = executor::batch::<AddLastUpdated>(&env, 2).unwrap();
        assert!(!batch_1.done);
        assert_eq!(version::read::<DataKey>(&env), Some(1));

        let batch_2: BatchOutcome = executor::batch::<AddLastUpdated>(&env, 2).unwrap();
        assert!(batch_2.done);
        assert_eq!(version::read::<DataKey>(&env), Some(2));

        // 3. Verify every entry now carries the migrated v2 shape.
        for id in 1..=3 {
            let key: Val = DataKey::Counter(id).into_val(&env);
            let migrated: CounterV2 = env.storage().persistent().get(&key).unwrap();
            assert_eq!(migrated.value, id * 10);
            assert_eq!(migrated.last_updated, Some(0));
        }
    });
}
```

---

## 5. What Happens When You Add a Required Field Instead of `Option`

Suppose you had declared `last_updated: u64` instead of `Option<u64>`:

```rust
#[derive(StorageSchema)]
#[storage_schema(version = 2, name = "Counter")]
#[contracttype]
pub struct CounterV2 {
    pub value: u32,
    pub last_updated: u64,
}
```

When you export the snapshot and run `soroban-migrate check`, the compatibility gate refuses the upgrade and exits with code `2`:

```console
$ soroban-migrate schema export
wrote migrations/counter_2.json

$ soroban-migrate check
schema changes
==============

Counter v1 -> v2
----------------
  DENIED  added required field `last_updated`: u64
          A version-1 entry has no `last_updated` key in its map, so unpacking it as
          version 2 traps inside the host until the entry is rewritten. Make the field
          `Option<u64>`, or run `soroban-migrate plan 1 2` and declare an `init`
          expression for `last_updated`.

  verdict: refused (1 denied change)
```

Why this refusal matters:

- Without an `init` plan or `Option<u64>`, any read of an un-migrated v1 entry while the migration is in flight traps at the host level.
- To proceed safely, either keep the field `Option<u64>` so un-migrated entries decode as `None`, or run `soroban-migrate plan 1 2` to declare an explicit `init` value for `last_updated` before generating the migration.
