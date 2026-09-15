# soroban-migrate-schema

The off-chain half of [`soroban-migrate`](https://github.com/soroban-migrate/soroban-migrate):
the schema model, the source parser, the compatibility diff, and the migration
generator. It never runs inside a contract.

Everything here works on *source* and on *schema files*, which is what makes it possible
to answer "is this upgrade safe?" before anything is deployed — and to answer it in CI
with no network access.

## The four pieces

| module | what it does |
| --- | --- |
| `model` | a schema as data: field names, declared types, and whether each field is `Option<..>` |
| `parse` | reads `#[storage_schema(version = N)]` declarations out of Rust source, so a schema can be snapshotted from a contract that has never been built for Wasm |
| `diff` | classifies every change between two versions and decides whether the upgrade is safe |
| `codegen` | emits an idempotent `Migration` implementation for a diff |

The diff and the generator share the parser with the proc macros, deliberately: the
derive emits a shape's JSON into the contract at compile time and this crate derives the
same JSON from the source, and if the two spellings disagreed then every `check` would
report a phantom change. `examples/vault/tests/upgrade.rs` asserts they agree, on a real
schema file.

## The property everything is organised around

Soroban's `#[contracttype]` structs serialize as symbol-keyed maps, and as of protocol 28
decoding is **tolerant**: an absent key decodes as `None` for an `Option` field and
errors for any other field, and a key the struct has no field for is discarded.

Tolerance gives, and it takes away:

* It makes it possible to upgrade a contract's code before its state, which is what makes
  a batched migration viable against a live contract at all.
* It makes it *quietly destructive* to remove a field, because the discarded key looks
  like nothing happened. So `diff` denies removals unless a plan declares them, and
  `codegen` will not generate code for a denied diff.

```rust
use soroban_migrate_schema::diff::{diff_with_plan, Verdict};
use soroban_migrate_schema::model::{Field, Schema};
use soroban_migrate_schema::plan::MigrationPlan;

let v1 = Schema::new(1, "Account", vec![Field::new("owner", "Address")]);
let v2 = Schema::new(2, "Account", vec![
    Field::new("owner", "Address"),
    Field::new("frozen", "Option<bool>"),
]);

let result = diff_with_plan(&v1, &v2, &MigrationPlan::new(1, 2)).unwrap();
assert_eq!(result.verdict, Verdict::SafeWithLazyMigration);
```

## License

Apache-2.0.
