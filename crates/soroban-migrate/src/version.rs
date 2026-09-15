//! The on-chain schema version of a contract's storage.
//!
//! The version lives in instance storage as a plain `u32` rather than being
//! derived from the entry shapes. Deriving it would mean decoding every entry to
//! answer "what version am I?", and the whole point of tolerant decoding
//! (CAP-86) is that entries of *several* shapes can coexist during a migration.
//! A single explicit number is the only thing that can be answered in constant
//! time while a migration is half-applied.

use soroban_sdk::{Env, Val};

use crate::error::MigrationError;
use crate::key::{storage_key, Keyspace, MigrationKey};

/// The version a freshly deployed contract starts at.
///
/// Versions are `u32` and count up. Nothing about them is semantic: `3` does not
/// mean "third release", it means "the third shape this contract's entries have
/// had". Deciding what a version *contains* is the job of the schema
/// declaration, not the number.
pub const INITIAL_VERSION: u32 = 1;

/// Reads the stored schema version.
///
/// `None` means the contract has never been initialized. That is different from
/// any version number, and the two are never conflated: a contract that was
/// deployed before the framework existed is *adopted* (see
/// [`initialize`]) rather than assumed to be at
/// [`INITIAL_VERSION`], because assuming would silently skip that deployment's
/// real, unmigrated state.
pub fn read<K: Keyspace>(env: &Env) -> Option<u32> {
    let key = storage_key::<K>(env, MigrationKey::Version);
    env.storage().instance().get::<Val, u32>(&key)
}

/// Writes the schema version without any validation.
///
/// Reserved for the migration executor, which must be able to move the version
/// forward in the same transaction that finishes the last batch. Contracts
/// should call [`initialize`] instead.
pub fn write<K: Keyspace>(env: &Env, version: u32) {
    let key = storage_key::<K>(env, MigrationKey::Version);
    env.storage().instance().set::<Val, u32>(&key, &version);
}

/// Sets the schema version the first time a contract is deployed.
///
/// Idempotent when `version` matches what is already stored, so it is safe to
/// call from a constructor that may run more than once, and safe to call from
/// both a constructor and an explicit `adopt` entry point.
///
/// # Errors
///
/// * [`MigrationError::InvalidVersion`] if `version` is zero.
/// * [`MigrationError::VersionMismatch`] if a *different* version is already
///   stored. Overwriting a recorded version is how a real migration gets skipped,
///   so it is refused rather than allowed.
pub fn initialize<K: Keyspace>(env: &Env, version: u32) -> Result<u32, MigrationError> {
    if version == 0 {
        return Err(MigrationError::InvalidVersion);
    }
    match read::<K>(env) {
        Some(existing) if existing == version => Ok(existing),
        Some(_) => Err(MigrationError::VersionMismatch),
        None => {
            write::<K>(env, version);
            Ok(version)
        }
    }
}

/// Records a version for a contract that predates the framework.
///
/// This is the one sanctioned way to set a version other than
/// [`INITIAL_VERSION`] or the result of a migration, and it exists because
/// retrofitting the framework onto a live contract is the common case: the
/// entries already have a shape, and the operator needs to *declare* which
/// version that shape is. It is deliberately separate from [`initialize`] so
/// that adopting a live contract is a visible, intentional act in the diff.
///
/// # Errors
///
/// [`MigrationError::InvalidVersion`] if `version` is zero, or
/// [`MigrationError::MigrationAlreadyActive`] if a migration is in flight —
/// adopting mid-migration would rewrite history the executor is still walking.
pub fn adopt<K: Keyspace>(env: &Env, version: u32) -> Result<u32, MigrationError> {
    if version == 0 {
        return Err(MigrationError::InvalidVersion);
    }
    if crate::migration::read_state::<K>(env).is_some_and(|s| s.is_active()) {
        return Err(MigrationError::MigrationAlreadyActive);
    }
    write::<K>(env, version);
    Ok(version)
}

/// Fails unless the stored version is exactly `expected`.
///
/// This is the strict gate. Use it on entry points that read state with the
/// *new* schema and cannot tolerate the old shape — admin functions, or any
/// read that is not written through the tolerant decoding path.
///
/// # Errors
///
/// * [`MigrationError::NotInitialized`] — no version recorded.
/// * [`MigrationError::StateNewerThanContract`] — stored version is *above*
///   `expected`, meaning this build is stale (typically after a code rollback).
/// * [`MigrationError::VersionMismatch`] — stored version is *below*
///   `expected`, meaning a migration has not run yet.
pub fn require<K: Keyspace>(env: &Env, expected: u32) -> Result<(), MigrationError> {
    match read::<K>(env) {
        None => Err(MigrationError::NotInitialized),
        Some(actual) if actual == expected => Ok(()),
        Some(actual) if actual > expected => Err(MigrationError::StateNewerThanContract),
        Some(_) => Err(MigrationError::VersionMismatch),
    }
}

/// [`require`], as a panic, for putting at the top of an entry point.
///
/// Panics rather than returning, because a failed gate means the caller has
/// already made an assumption the contract cannot satisfy — there is no
/// meaningful value to return from the middle of a function whose precondition
/// just failed.
///
/// # Panics
///
/// Panics with the [`MigrationError`] returned by [`require`].
pub fn require_or_panic<K: Keyspace>(env: &Env, expected: u32) {
    if let Err(e) = require::<K>(env, expected) {
        soroban_sdk::panic_with_error!(env, e);
    }
}

/// Whether the contract's state is older than `expected`, i.e. a migration is
/// outstanding.
///
/// Note that "outstanding" is not the same as "needs work right now": a
/// tolerant contract reads older entries happily and migrates them lazily, so
/// this answers "is there anything for the executor to do", which is what the
/// CLI's status output wants.
pub fn is_behind<K: Keyspace>(env: &Env, expected: u32) -> bool {
    matches!(read::<K>(env), Some(actual) if actual < expected)
}
