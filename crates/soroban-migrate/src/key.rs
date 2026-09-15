//! Storage keys owned by the migration framework, and the key-space hook that
//! keeps them from colliding with the host contract's own keys.
//!
//! Soroban gives each contract one flat storage namespace. A framework that
//! writes bookkeeping into that namespace therefore has to answer a question
//! the protocol does not answer for it: *how do I know my keys are not the
//! host contract's keys?*
//!
//! Rather than reserving a magic [`Symbol`](soroban_sdk::Symbol) prefix and
//! hoping, the framework asks the host contract to name the namespace it lives
//! in. The host contract puts one variant wrapping [`MigrationKey`] into its own
//! `DataKey` enum, and the framework reaches storage exclusively through the
//! [`Keyspace`] implementation for that enum. Collisions become a compile-time
//! impossibility instead of a documented convention.
//!
//! ```ignore
//! use soroban_migrate::key::{Keyspace, MigrationKey};
//! use soroban_sdk::{contracttype, Env, Val};
//!
//! #[contracttype]
//! pub enum DataKey {
//!     /// Reserved for the migration framework. The variant *name* is up to you;
//!     /// only the inner [`MigrationKey`] values are fixed by the framework.
//!     Migration(MigrationKey),
//!     Balance(Address),
//! }
//!
//! impl Keyspace for DataKey {
//!     fn wrap(env: &Env, key: MigrationKey) -> Val {
//!         DataKey::Migration(key).into_val(env)
//!     }
//! }
//! ```
//!
//! `#[derive(Keyspace)]` generates exactly this implementation; see the
//! `soroban-migrate-macros` crate.
//!
//! # No lock, on purpose
//!
//! Earlier designs put a lock entry in this namespace to stop two operators from
//! advancing the same cursor concurrently. It was removed because it cannot do
//! anything: a batch is one atomic transaction, so a second operator's batch
//! always executes entirely before or entirely after the first, reads the cursor
//! it actually finds, and writes the cursor it actually computes. The only thing
//! two racing operators can waste is a fee on a redundant batch — and a redundant
//! batch is harmless precisely because [`crate::migration::Migration::up`] is
//! idempotent. A lock that can never be observed to be held is not a lock; it is
//! an entry that costs rent.

use soroban_sdk::{contracttype, Env, Val};

/// Every storage key the framework writes.
///
/// Entries are split across durabilities on purpose:
///
/// * Instance-scoped keys ([`MigrationKey::Version`], [`MigrationKey::State`],
///   [`MigrationKey::IndexInfo`]) are small, are read by nearly every contract
///   call, and must share the instance entry's lifetime. Reading them costs
///   nothing extra once the instance entry is already in the footprint of a
///   call.
/// * [`MigrationKey::IndexPage`] entries are unbounded in aggregate, so they are
///   persistent. A contract with ten thousand migrating keys would blow the
///   instance entry's size budget if the index lived there.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationKey {
    /// `u32`: the storage schema version the contract's entries conform to.
    Version,
    /// [`crate::migration::MigrationState`]: bookkeeping for the in-flight or
    /// last-completed migration.
    State,
    /// [`KeyIndexInfo`]: how many pages the index for a schema version has and
    /// how many keys those pages hold in total.
    IndexInfo(u32),
    /// The page at `page` of the key index for schema version `version`, holding
    /// up to [`crate::index::PAGE_SIZE`] keys.
    IndexPage(u32, u32),
}

/// Summary of a schema version's key index.
///
/// `len` is the number of keys across all pages. Because pages are fixed-size
/// and only the final page is partially filled, `len` and `page_count` together
/// are enough to compute any key's offset without touching the pages
/// themselves — which is what lets the executor resume from a cursor without
/// re-walking the index.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyIndexInfo {
    /// Number of allocated pages. Always at least 1 once the index exists.
    pub page_count: u32,
    /// Total keys across every page.
    pub len: u32,
}

/// Names the storage namespace the framework operates in.
///
/// Implement this for the host contract's own `DataKey` enum. The framework
/// names the variant; the host contract chooses its index inside the enum, so
/// adding a `Balance(Address)` variant later never shifts the framework's keys.
pub trait Keyspace {
    /// Wrap a framework key into the host contract's key type.
    fn wrap(env: &Env, key: MigrationKey) -> Val;
}

/// Builds the storage key for `key` in the key space of `K`.
///
/// This is the only sanctioned way to address framework storage. Everything in
/// this crate goes through it.
#[inline]
pub fn storage_key<K: Keyspace>(env: &Env, key: MigrationKey) -> Val {
    K::wrap(env, key)
}
