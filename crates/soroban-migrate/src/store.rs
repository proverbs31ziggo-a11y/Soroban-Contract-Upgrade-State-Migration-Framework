//! The version-aware write path.
//!
//! A migration that runs against a live contract has one invariant to maintain,
//! and it is an invariant the *application* has to maintain, not the framework:
//!
//! > While a migration is in flight, every write must produce the new shape and
//! > register its key under the new version.
//!
//! Break it and the failure is quiet. A key written in the old shape after the
//! cursor passed it is never revisited, so it stays old-shaped forever; a key
//! written without being registered is invisible to the next migration. Neither
//! shows up as an error — the migration still reports success.
//!
//! [`put`] exists so the invariant is maintained by construction rather than by
//! discipline. It writes the entry and registers the key under the version that
//! entry should conform to: the in-flight target if a migration is running, and
//! the stored version otherwise. There is no way to call it and get the index out
//! of step with the data.
//!
//! ```ignore
//! use soroban_migrate::store;
//!
//! // Instead of `env.storage().persistent().set(&key, &value)`:
//! store::put::<DataKey, _>(&env, &DataKey::Balance(owner).into_val(&env), &value);
//! ```
//!
//! # The escape hatch
//!
//! Not every persistent write should be registered. Derived caches, or entries
//! that are rewritten on every call, only inflate the index. Those should keep
//! writing through [`soroban_sdk::storage::Persistent::set`] directly and be
//! excluded from migration by returning [`crate::migration::EntryOutcome::Skipped`].
//! What must never happen is an unregistered write of a *migrated* key.

use soroban_sdk::{Env, IntoVal, Val};

use crate::index;
use crate::key::Keyspace;
use crate::migration::{read_state, MigrationStatus};
use crate::version;

/// The version an entry written right now should conform to.
///
/// The in-flight target version while a migration is running — because new and
/// migrated data must agree — and the stored version otherwise. Falls back to
/// [`version::INITIAL_VERSION`] for a contract that has not been initialized,
/// which is the only sensible answer for a write that happens before the
/// constructor recorded a version.
pub fn current_version<K: Keyspace>(env: &Env) -> u32 {
    if let Some(state) = read_state::<K>(env) {
        if state.status == MigrationStatus::Running {
            return state.to;
        }
    }
    version::read::<K>(env).unwrap_or(version::INITIAL_VERSION)
}

/// Writes a persistent entry under the current version and indexes its key.
///
/// # Footprint
///
/// One write for the entry, plus whatever [`index::register`] costs. The index
/// write is the price of the entry being findable later; see that function for
/// the accounting.
pub fn put<K: Keyspace, V: IntoVal<Env, Val>>(env: &Env, key: &Val, value: &V) {
    env.storage().persistent().set::<Val, V>(key, value);
    index::register::<K>(env, current_version::<K>(env), key);
}

/// Writes a persistent entry without indexing it.
///
/// For entries that are not migrated — caches, aggregates recomputed from other
/// entries, anything a migration would only ever skip. Prefer [`put`]: the cost
/// of an unnecessary index entry is a few bytes, and the cost of a missing one is
/// data that survives an upgrade in the wrong shape.
pub fn put_unindexed<V: IntoVal<Env, Val>>(env: &Env, key: &Val, value: &V) {
    env.storage().persistent().set::<Val, V>(key, value);
}

/// Removes a persistent entry.
pub fn remove(env: &Env, key: &Val) {
    // The index is append-only and is deliberately not pruned here. Removing from
    // a log while it is being read positionally would shift every later position
    // and break a resumable cursor; a removed key simply reads as absent when the
    // migration reaches it, and the migration returns
    // [`crate::migration::EntryOutcome::Skipped`]. Compaction is a separate,
    // explicit operation performed after a migration completes.
    env.storage().persistent().remove::<Val>(key);
}
