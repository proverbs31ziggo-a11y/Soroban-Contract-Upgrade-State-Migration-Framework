#![no_std]
#![forbid(unsafe_code)]
// `missing_docs` is not enabled: `#[contracttype]` emits `impl` blocks and spec
// statics from the annotated item's span, and no attribute reachable from this
// crate silences the lint for generated items. Every public item here is
// documented by hand instead, and CI runs `cargo doc -D warnings` to keep
// intra-doc links honest.
#![warn(clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::cast_possible_truncation,
    clippy::doc_markdown
)]

//! Versioned storage-schema migration for Soroban contracts.
//!
//! # The problem
//!
//! Soroban lets a contract replace its Wasm. Nothing migrates the state
//! underneath. Change a struct's shape and every existing entry becomes
//! unreadable — and because Soroban has no way to enumerate a contract's storage,
//! you cannot even find out *how many* entries you just broke. Teams ship a
//! hand-written migration contract per upgrade, or they get the struct right the
//! first time.
//!
//! # What this does
//!
//! Five mechanisms, each addressing a mechanical obstacle rather than a stylistic
//! one:
//!
//! * **[`version`]** records which shape an entry is in, as a number in instance
//!   storage, and gates reads on it. Constant-time, so it works while a migration
//!   is half-applied.
//! * **[`index`]** is an append-only paged list of the keys a contract has
//!   written, because Soroban cannot enumerate them. Registration costs one extra
//!   read-modify-write per application write and turns enumeration into a bounded
//!   operation.
//! * **[`executor`]** applies a migration in batches that fit the network's
//!   200-read / 200-write caps, checkpointing a cursor on-chain. A batch is one
//!   atomic transaction, so a failed batch leaves no partial work to clean up.
//! * **[`rollback`]** walks the same index backwards, and **[`preflight`]**
//!   refuses migrations that would leave live entries behind.
//!
//! [`fleet`] adds the CAP-85 half: because an executable reference entry can be
//! repointed in one write, a fleet's code can be swapped atomically, which gives
//! an upgrade a cheap and permanent rollback target — and lets state migrate
//! while the old code is still serving.
//!
//! # Using it
//!
//! The `soroban-migrate-macros` crate generates all of the boilerplate below from
//! a schema declaration. Written out by hand, the pieces are:
//!
//! ```ignore
//! use soroban_migrate::key::{Keyspace, MigrationKey};
//! use soroban_migrate::{executor, migration::{EntryOutcome, Migration}, version};
//! use soroban_sdk::{contracttype, Env, Val};
//!
//! // 1. Name the framework's key space inside the contract's own.
//! #[contracttype]
//! pub enum DataKey {
//!     Migration(MigrationKey),
//!     Balance(Address),
//! }
//!
//! impl Keyspace for DataKey {
//!     fn wrap(env: &Env, key: MigrationKey) -> Val {
//!         DataKey::Migration(key).into_val(env)
//!     }
//! }
//!
//! // 2. Describe one version change.
//! pub struct AddFrozen;
//!
//! impl Migration for AddFrozen {
//!     type Keys = DataKey;
//!     const FROM: u32 = 1;
//!     const TO: u32 = 2;
//!
//!     fn up(env: &Env, key: &Val) -> Result<EntryOutcome, MigrationError> {
//!         // Read the entry, add the new field, write it back. Must be idempotent.
//!         # todo!()
//!     }
//! }
//!
//! // 3. Expose it. Each of these is one transaction.
//! #[contractimpl]
//! impl Vault {
//!     pub fn begin_migration(env: Env) -> Result<MigrationState, MigrationError> {
//!         soroban_migrate::preflight::assert_upgradeable::<AddFrozen>(&env)?;
//!         executor::begin::<AddFrozen>(&env)
//!     }
//!
//!     pub fn migrate_batch(env: Env, limit: u32) -> Result<BatchOutcome, MigrationError> {
//!         executor::batch::<AddFrozen>(&env, limit)
//!     }
//! }
//! ```
//!
//! # Invariants you own
//!
//! Two things the framework cannot check for you, both stated here because they
//! are where a migration actually goes wrong:
//!
//! 1. **[`migration::Migration::up`] must be idempotent.** Keys can be visited
//!    more than once. The trait documentation lists the three independent reasons.
//! 2. **Application writes during a migration must produce the new shape.**
//!    Route them through [`store::put`], which registers the key under the
//!    in-flight target version so the index can never fall out of step with the
//!    data.
//!
//! # Protocol requirements
//!
//! **Protocol 28 is required**, and it is worth being precise about which half needs
//! it, because the two halves fail differently.
//!
//! * [`fleet`] needs CAP-85: executable references, `ExecutableRefs`, and
//!   `update_current_contract(ContractExecutable::ExternalRef(..))`. This is the
//!   developer-visible half of the CAP.
//! * The *rest of the crate* calls no host function newer than protocol 26 —
//!   [`version`], [`index`], [`executor`], [`rollback`], [`preflight`], and
//!   [`store`] would run on an older protocol. What needs protocol 28 is the
//!   property they are built around: decoding an entry written by an *older* schema.
//!   `#[contracttype]` on `soroban-sdk` 28 implements that with CAP-86's sparse map
//!   functions — the derive emits
//!   `env.sparse_map_unpack_to_slice` — and `sparse_map_unpack_to_linear_memory`
//!   declares `min_supported_protocol: 28`.
//!
//! The distinction matters operationally: without protocol 28 the framework still
//! compiles and its bookkeeping still works, but an upgrade *does* have a window in
//! which the new code cannot read the old entries. That window is the whole reason to
//! use this crate, so the requirement is stated rather than left to be discovered.
//!
//! See `docs/cap-85-86.md` for which framework module depends on which clause.

pub mod error;
pub mod executor;
pub mod fleet;
pub mod index;
pub mod key;
pub mod migration;
pub mod preflight;
pub mod rollback;
pub mod schema;
pub mod store;
pub mod version;

#[cfg(any(test, feature = "testutils"))]
pub mod testing;

pub use error::MigrationError;
pub use executor::{DEFAULT_KEYS_PER_BATCH, MAX_KEYS_PER_BATCH};
pub use key::{KeyIndexInfo, Keyspace, MigrationKey};
pub use migration::{BatchOutcome, EntryOutcome, Migration, MigrationState, MigrationStatus};
pub use schema::{FieldSpec, StorageSchema};
pub use version::INITIAL_VERSION;
