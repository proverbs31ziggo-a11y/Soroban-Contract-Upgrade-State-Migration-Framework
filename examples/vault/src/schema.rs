//! The example contract's storage shapes, one struct per version.
//!
//! Keeping every version's struct in the source is what makes the migration possible
//! to write *and* to test. A migration is a function from an old shape to a new one,
//! and deleting the old shape as soon as the new one lands is how teams end up unable
//! to write that function.

use soroban_migrate_macros::StorageSchema;
use soroban_sdk::contracttype;
use soroban_sdk::Address;

/// A single account's balance, as of schema version 1.
///
/// A minimal entry, deliberately: the point of the example is the *migration*, and a
/// shape small enough to read at a glance makes it obvious what the migration is
/// doing.
///
/// # Why `name` is stated explicitly
///
/// A shape's name defaults to the struct's name, and versioned structs cannot all be
/// called `Balance` — the Rust type names have to differ. Without `name`, `BalanceV1`
/// and `BalanceV2` would register as two *unrelated* shapes, each with one version, and
/// there would be no version pair to diff: `check` would report success about a change
/// it never looked at. `name = "Balance"` is what makes them versions of one shape, and
/// `soroban-migrate check` refuses a pair of structs whose names differ only by a
/// version suffix precisely so that this cannot be forgotten silently.
#[derive(StorageSchema)]
#[storage_schema(version = 1, name = "Balance")]
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BalanceV1 {
    /// The account this balance belongs to, duplicated from the storage key so that
    /// a single entry is self-describing when read out of context by an indexer.
    pub owner: Address,
    /// The balance, in the contract's smallest unit.
    pub amount: i128,
}

/// The same entry at schema version 2: a freeze flag is added.
///
/// `frozen` is `Option<bool>` rather than `bool`, and that choice is the whole
/// reason this upgrade can roll out to a live contract. CAP-86's relaxed unpacking
/// decodes a missing key as `None` for an `Option` field and *errors* for anything
/// else, so an optional field means the version-2 code can read a version-1 entry
/// without trapping — which is what lets the code ship before the state finishes
/// migrating.
///
/// Making it `bool` would compile, be denied by `soroban-migrate check`, and panic on
/// every account that had not been migrated yet.
#[derive(StorageSchema)]
#[storage_schema(version = 2, name = "Balance")]
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BalanceV2 {
    pub owner: Address,
    pub amount: i128,
    /// `None` means "written before the freeze flag existed"; `Some(false)` means
    /// "explicitly unfrozen". The distinction is load-bearing during the migration and
    /// disappears once it completes.
    pub frozen: Option<bool>,
}

/// The counter entry, added in version 2 to demonstrate a second, independent field
/// shape in the same contract.
///
/// Nothing migrates it: it is written for the first time at version 2, so the
/// version-2 index knows about it and no earlier version ever saw it.
#[derive(StorageSchema)]
#[storage_schema(version = 2, name = "Stats")]
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatsV2 {
    /// Total of every balance, maintained on write so that reads are O(1).
    pub total: i128,
}
