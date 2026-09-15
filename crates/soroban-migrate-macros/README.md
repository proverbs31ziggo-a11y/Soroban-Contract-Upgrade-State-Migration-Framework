# soroban-migrate-macros

The derives for [`soroban-migrate`](https://github.com/proverbs31ziggo-a11y/Soroban-Contract-Upgrade-State-Migration-Framework).
Each one removes boilerplate that is easy to get subtly wrong and impossible to notice
when it is.

## `StorageSchema`

Declares a struct as one version of a storage shape, and emits the metadata a deployed
contract reports about itself.

```rust
#[derive(StorageSchema)]
#[storage_schema(version = 2, name = "Balance")]
#[contracttype]
pub struct BalanceV2 {
    pub owner: Address,
    pub amount: i128,
    pub frozen: Option<bool>,
}
```

`name` is the shape's identity across versions, and it is what makes `BalanceV1` and
`BalanceV2` two versions of one shape rather than two unrelated ones. Omit it and there
is no version pair to diff — `soroban-migrate check` detects exactly that and refuses.

## `Keyspace`

Implements `soroban_migrate::key::Keyspace` for a contract's `DataKey` enum, so the
framework's bookkeeping cannot collide with the contract's own keys.

```rust
#[derive(Keyspace)]
#[contracttype]
pub enum DataKey {
    #[migration]
    Migration(MigrationKey),
    Balance(Address),
}
```

## `Migration`

Implements a migration for an **additive** change.

```rust
#[derive(Migration)]
#[migration(from = 1, to = 2, entry = BalanceV2, init(frozen = false), reversible)]
pub struct AddFreezeFlag;
```

It covers additions and optionality changes: the cases whose idempotency can be
*proved* rather than argued, because a field is filled only when it is empty and a
second visit therefore cannot change anything.

It **refuses** renames and type changes with a compile error pointing at
`soroban-migrate generate`. Those consume an old key, so being re-runnable requires
reading the old shape through a tolerant shadow struct — a second type the derive
cannot synthesize from one struct, and one the generator can write because it has both
schemas. Accepting them here, and generating something that only works the first time,
is the failure this refusal prevents.

## License

Apache-2.0.
