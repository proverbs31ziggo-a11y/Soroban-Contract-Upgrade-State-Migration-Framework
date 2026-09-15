//! The example contract's key space.
//!
//! Soroban gives each contract one flat storage namespace, so the framework's
//! bookkeeping keys have to live *somewhere* inside it. Rather than reserving a symbol
//! prefix and documenting it, the contract names the namespace: one variant wraps
//! [`MigrationKey`], and `#[derive(Keyspace)]` teaches the framework to reach storage
//! through it. A collision between the framework's keys and the contract's own becomes
//! a compile-time impossibility rather than a convention someone has to remember.

use soroban_migrate::key::MigrationKey;
use soroban_migrate_macros::Keyspace;
use soroban_sdk::{contracttype, Address};

/// Every key this contract uses.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq, Keyspace)]
pub enum DataKey {
    /// Reserved for the migration framework's own bookkeeping: the schema version, the
    /// in-flight migration's cursor, and the pages of the key index. Marked with
    /// `#[migration]` so the derive can find it regardless of what it is called.
    #[migration]
    Migration(MigrationKey),

    /// One account's balance, as of [`crate::schema::BalanceV1`] or
    /// [`crate::schema::BalanceV2`].
    Balance(Address),

    /// The contract-wide counter, as of [`crate::schema::StatsV2`].
    Stats,

    /// The administrator allowed to drive migrations and fleet upgrades.
    Admin,
}
