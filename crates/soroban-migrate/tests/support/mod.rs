//! Shared fixtures for the framework's integration tests.
//!
//! The fixtures are deliberately shaped like a real contract rather than like a
//! unit test: a `DataKey` enum that mixes framework bookkeeping with application
//! payload keys, and migrations that read and write `#[contracttype]` structs
//! through the same tolerant-decoding path real contracts use.

#![allow(dead_code)]

use soroban_sdk::{contract, contractimpl, contracttype, Env, IntoVal, Val};

use soroban_migrate::key::MigrationKey;
use soroban_migrate::migration::{EntryOutcome, Migration};
use soroban_migrate::MigrationError;

/// A contract to hang a storage context off in tests.
///
/// Framework tests need *a* contract address so that `env.as_contract` has a
/// storage namespace to work in, and they need it to be a real registered
/// contract so that the instance entry exists. Its behaviour is irrelevant.
#[contract]
pub struct HostContract;

#[contractimpl]
impl HostContract {
    pub fn noop(_env: Env) -> u32 {
        0
    }
}

/// The key space a realistic host contract would declare.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Framework bookkeeping, wrapped rather than namespaced by convention.
    Migration(MigrationKey),
    /// Application payload, addressed by an integer in tests so failures name a
    /// single number instead of a 56-character address.
    Entry(u32),
}

impl soroban_migrate::Keyspace for DataKey {
    fn wrap(env: &Env, key: MigrationKey) -> Val {
        DataKey::Migration(key).into_val(env)
    }
}

/// A registered contract plus its address, so tests can enter a storage context.
pub fn host(env: &Env) -> soroban_sdk::Address {
    env.register(HostContract, ())
}

/// Storage key for payload entry `n`.
pub fn entry_key(env: &Env, n: u32) -> Val {
    DataKey::Entry(n).into_val(env)
}

/// Compares two storage keys by content.
///
/// `soroban_sdk::Val` deliberately does not implement `PartialEq`: a `Val` that
/// holds an object is a handle, so `==` on the raw representation would compare
/// handles rather than contents and would be wrong in a way that looks right.
/// Equality of keys is therefore a host question, answered by asking the host
/// whether a vector contains the value. Every key comparison in these tests goes
/// through here so that a test cannot accidentally assert handle identity.
pub fn keys_equal(env: &Env, a: &Val, b: &Val) -> bool {
    let mut probe = soroban_sdk::Vec::new(env);
    probe.push_back(*a);
    probe.contains(*b)
}

/// Asserts two lists of keys are equal, element for element, by content.
#[track_caller]
pub fn assert_keys_eq(env: &Env, actual: &soroban_sdk::Vec<Val>, expected: &soroban_sdk::Vec<Val>) {
    assert_eq!(actual.len(), expected.len(), "key lists differ in length");
    let mut i = 0u32;
    while i < actual.len() {
        let a = actual.get(i).unwrap();
        let b = expected.get(i).unwrap();
        assert!(keys_equal(env, &a, &b), "key at position {i} differs");
        i += 1;
    }
}

/// The shape a payload entry has at version 1.
///
/// Written without an `Option`, so an entry in this shape has no `frozen` key at
/// all in its serialized map — which is exactly the condition CAP-86's relaxed
/// unpacking exists to handle.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryV1 {
    pub amount: u32,
}

/// The shape the same entry has at version 2.
///
/// `frozen` is an `Option` on purpose. Making it `Option` is what allows the
/// version-2 code to read a version-1 entry without trapping, which is what makes
/// it possible to migrate state while the old code is still serving reads. A
/// non-optional field would make every un-migrated entry a panic.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryV2 {
    pub amount: u32,
    pub frozen: Option<bool>,
}

/// Version 1 to version 2: add an optional `frozen` flag.
///
/// The canonical additive migration, and the one the framework's codegen emits.
pub struct AddFrozen;

impl Migration for AddFrozen {
    type Keys = DataKey;
    const FROM: u32 = 1;
    const TO: u32 = 2;

    fn up(env: &Env, key: &Val) -> Result<EntryOutcome, MigrationError> {
        let storage = env.storage().persistent();
        if !storage.has::<Val>(key) {
            return Ok(EntryOutcome::Skipped);
        }
        let current = match storage.get::<Val, EntryV2>(key) {
            Some(v) => v,
            None => return Ok(EntryOutcome::Skipped),
        };
        // `frozen` is `None` for an entry still in the version-1 shape, and this
        // is where idempotency comes from: an entry that already has the field is
        // reported as already current and not rewritten.
        if current.frozen.is_some() {
            return Ok(EntryOutcome::AlreadyCurrent);
        }
        storage.set::<Val, EntryV2>(
            key,
            &EntryV2 {
                amount: current.amount,
                frozen: Some(false),
            },
        );
        Ok(EntryOutcome::Migrated)
    }

    fn down(env: &Env, key: &Val) -> Result<EntryOutcome, MigrationError> {
        let storage = env.storage().persistent();
        if !storage.has::<Val>(key) {
            return Ok(EntryOutcome::Skipped);
        }
        let current = match storage.get::<Val, EntryV2>(key) {
            Some(v) => v,
            None => return Ok(EntryOutcome::Skipped),
        };
        if current.frozen.is_none() {
            // Already in the version-1 shape. Rollback runs on partially
            // reverted state when a previous batch failed, so this is a normal
            // outcome, not an error.
            return Ok(EntryOutcome::AlreadyCurrent);
        }
        storage.set::<Val, EntryV1>(
            key,
            &EntryV1 {
                amount: current.amount,
            },
        );
        Ok(EntryOutcome::Migrated)
    }

    fn supports_down() -> bool {
        true
    }

    fn name() -> &'static str {
        "add frozen flag"
    }
}

/// Version 1 to version 2, but not reversible.
///
/// Used to prove that a migration without a compensating transformation is
/// refused before a rollback is attempted, rather than half-applied.
pub struct AddFrozenNoDown;

impl Migration for AddFrozenNoDown {
    type Keys = DataKey;
    const FROM: u32 = 1;
    const TO: u32 = 2;

    fn up(env: &Env, key: &Val) -> Result<EntryOutcome, MigrationError> {
        AddFrozen::up(env, key)
    }

    fn supports_down() -> bool {
        false
    }
}

/// A migration that never touches storage, for exercising version bookkeeping in
/// isolation.
pub struct Noop;

impl Migration for Noop {
    type Keys = DataKey;
    const FROM: u32 = 1;
    const TO: u32 = 2;

    fn up(_env: &Env, _key: &Val) -> Result<EntryOutcome, MigrationError> {
        Ok(EntryOutcome::Skipped)
    }
}

/// A migration whose `up` always fails, for proving that a failed batch leaves
/// the cursor untouched.
pub struct AlwaysFails;

impl Migration for AlwaysFails {
    type Keys = DataKey;
    const FROM: u32 = 1;
    const TO: u32 = 2;

    fn up(_env: &Env, _key: &Val) -> Result<EntryOutcome, MigrationError> {
        Err(MigrationError::OrphanedEntries)
    }
}

/// A migration that skips every key, for proving that a migration over keys it
/// does not manage does not advance the version.
pub struct SkipsEverything;

impl Migration for SkipsEverything {
    type Keys = DataKey;
    const FROM: u32 = 1;
    const TO: u32 = 2;

    fn up(_env: &Env, _key: &Val) -> Result<EntryOutcome, MigrationError> {
        Ok(EntryOutcome::Skipped)
    }
}

/// Version 2 to version 3: adds nothing, so tests can check that a second
/// upgrade in a contract's life is possible at all.
pub struct V2ToV3;

impl Migration for V2ToV3 {
    type Keys = DataKey;
    const FROM: u32 = 2;
    const TO: u32 = 3;

    fn up(_env: &Env, _key: &Val) -> Result<EntryOutcome, MigrationError> {
        Ok(EntryOutcome::Skipped)
    }
}

/// A migration that jumps more than one version, for proving `begin` refuses it.
pub struct Jumping;

impl Migration for Jumping {
    type Keys = DataKey;
    const FROM: u32 = 1;
    const TO: u32 = 3;

    fn up(_env: &Env, _key: &Val) -> Result<EntryOutcome, MigrationError> {
        Ok(EntryOutcome::Skipped)
    }
}

/// Writes `count` version-1 entries and indexes them under version 1.
pub fn seed_v1(env: &Env, count: u32) {
    seed_v1_from(env, 0, count);
}

/// Writes `count` version-1 entries starting at payload address `start` and indexes
/// them under version 1. Used to simulate a live contract registering keys while a
/// migration is in flight.
pub fn seed_v1_from(env: &Env, start: u32, count: u32) {
    seed_entries_at(env, start, count, 1);
}

/// Writes `count` version-1-shaped entries starting at `start` and registers their keys
/// under `version`.
///
/// Registration version and entry shape are separated deliberately: they are separate
/// facts, and several tests need one without the other. A key registered under version 2
/// while a rollback is in flight is what "something is still writing new-version data"
/// looks like on chain, because that is the index an application writing under the new
/// code registers into.
pub fn seed_entries_at(env: &Env, start: u32, count: u32, version: u32) {
    let mut n = start;
    let end = start + count;
    while n < end {
        let key = entry_key(env, n);
        env.storage()
            .persistent()
            .set::<Val, EntryV1>(&key, &EntryV1 { amount: n });
        soroban_migrate::index::register::<DataKey>(env, version, &key);
        n += 1;
    }
}

/// Reads payload entry `n` as the version-2 shape, or `None` if absent.
///
/// Reading through the version-2 type is what a real contract does: CAP-86's
/// relaxed unpacking means a version-1 entry decodes successfully with `frozen`
/// left as `None`, rather than trapping. That property is why a migration can run
/// while the old code is still serving reads.
pub fn read_v2(env: &Env, n: u32) -> Option<EntryV2> {
    env.storage()
        .persistent()
        .get::<Val, EntryV2>(&entry_key(env, n))
}

/// Whether payload entry `n` is in the version-2 shape.
pub fn is_v2(env: &Env, n: u32) -> bool {
    read_v2(env, n).is_some_and(|e| e.frozen.is_some())
}

/// Counts how many of the first `count` payload entries are in the version-2
/// shape.
pub fn count_v2(env: &Env, count: u32) -> u32 {
    let mut done = 0u32;
    let mut n = 0u32;
    while n < count {
        if is_v2(env, n) {
            done += 1;
        }
        n += 1;
    }
    done
}
