//! Test helpers for downstream contracts and for this crate's own tests.
//!
//! Enabled by the `testutils` feature, or automatically under `cfg(test)`.
//!
//! The one thing every test needs and cannot easily write is a [`Keyspace`]
//! implementation plus a way to create entries that the framework can see. A
//! contract under test has its own `DataKey`; this module provides a stand-in so
//! that framework behaviour can be exercised without pinning a contract's
//! key layout into the framework's test suite.

use soroban_sdk::{contracttype, Env, IntoVal, Val};

use crate::index;
use crate::key::{KeyIndexInfo, Keyspace, MigrationKey};

/// A complete key space for tests, covering framework bookkeeping and a plain
/// `u32`-addressed payload.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestKey {
    /// Framework bookkeeping. Present exactly as it would be in a real contract,
    /// because the framework's storage layout is part of what tests exercise.
    Migration(MigrationKey),
    /// A payload entry addressed by an integer. Real contracts use address- or
    /// symbol-keyed entries; an integer keeps test intent legible in failure
    /// output.
    Entry(u32),
}

impl Keyspace for TestKey {
    fn wrap(env: &Env, key: MigrationKey) -> Val {
        TestKey::Migration(key).into_val(env)
    }
}

/// The storage key for payload entry `n`.
pub fn entry_key(env: &Env, n: u32) -> Val {
    TestKey::Entry(n).into_val(env)
}

/// Writes payload entry `n` and registers it under `version`.
pub fn seed_entry<V: IntoVal<Env, Val>>(env: &Env, n: u32, version: u32, value: &V) {
    let key = entry_key(env, n);
    env.storage().persistent().set::<Val, V>(&key, value);
    index::register::<TestKey>(env, version, &key);
}

/// Writes `count` payload entries, each holding its own index, and registers
/// them under `version`.
///
/// Returns the keys, in registration order, so a test can assert on a specific
/// entry later without recomputing the encoding.
pub fn seed_entries(env: &Env, count: u32, version: u32) -> soroban_sdk::Vec<Val> {
    let mut keys = soroban_sdk::Vec::new(env);
    let mut n = 0u32;
    while n < count {
        let key = entry_key(env, n);
        env.storage().persistent().set::<Val, u32>(&key, &n);
        index::register::<TestKey>(env, version, &key);
        keys.push_back(key);
        n += 1;
    }
    keys
}

/// Reads payload entry `n`, or `None` if it is absent.
pub fn read_entry<V: soroban_sdk::TryFromVal<Env, Val>>(env: &Env, n: u32) -> Option<V> {
    env.storage().persistent().get::<Val, V>(&entry_key(env, n))
}

/// The index summary for `version`, for assertions about framework bookkeeping.
pub fn index_info(env: &Env, version: u32) -> Option<KeyIndexInfo> {
    index::info::<TestKey>(env, version)
}
