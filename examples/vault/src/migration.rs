//! The V1 to V2 migration.
//!
//! This file is what `soroban-migrate generate` writes for an additive change, and it
//! is written out here by hand so that the example shows the whole of what the
//! framework does rather than hiding it behind a derive. The `#[derive(Migration)]`
//! form below is equivalent; the generated form is preferred when a change also
//! renames or retypes a field, because those need a tolerant shadow struct that the
//! generator has both schemas to synthesize.

use soroban_migrate::migration::{EntryOutcome, Migration};
use soroban_migrate::MigrationError;
use soroban_migrate_macros::Migration;
use soroban_sdk::{Env, Val};

use crate::keys::DataKey;
use crate::schema::BalanceV2;

/// Adds the freeze flag to every balance entry.
///
/// # Why this is safe to run against a live contract
///
/// The new field is `Option<bool>`, so a version-1 entry decodes as
/// `frozen: None` under CAP-86's relaxed unpacking. The version-2 code therefore
/// keeps serving reads throughout the migration, and the migration can be batched over
/// hours without any window in which the contract is unusable.
///
/// # Why this is safe to run twice
///
/// `up` fills `frozen` only when it is empty. An entry that already carries a value —
/// whether from an earlier batch or because the application wrote it under version 2 —
/// is reported as [`EntryOutcome::AlreadyCurrent`] and left alone. The framework may
/// visit a key more than once (the index is a log, not a set, and a batch that fails
/// is retried whole), so this property is what makes the migration correct rather than
/// merely usually-correct.
#[derive(Migration)]
#[migration(
    from = 1,
    to = 2,
    entry = BalanceV2,
    keys = DataKey,
    init(frozen = false),
    name = "add freeze flag",
    reversible,
)]
pub struct AddFreezeFlag;

/// The same change, written out by hand.
///
/// Kept in the example because it is what a reader needs in order to understand what
/// the derive is doing, and because `soroban-migrate generate` emits this shape. The
/// contract uses [`AddFreezeFlag`]; this struct exists to be read.
///
/// Nothing here needs the shadow struct that renames and retypes require, because the
/// change consumes no old key: it only fills one that is absent.
pub struct AddFreezeFlagByHand;

#[allow(dead_code)]
impl AddFreezeFlagByHand {
    /// The body of `up`, as the generator writes it.
    ///
    /// Not wired into [`Migration`] so that the example has exactly one live
    /// implementation; the tests compare the two behaviours instead.
    pub(crate) fn migrate_one(env: &Env, key: &Val) -> Result<EntryOutcome, MigrationError> {
        let storage = env.storage().persistent();
        if !storage.has::<Val>(key) {
            return Ok(EntryOutcome::Skipped);
        }
        // Decoding with the *new* type is what makes this work: a key the new shape
        // declares but the entry lacks arrives as `None` because the field is an
        // `Option`, and keys the new shape does not declare are discarded.
        let mut entry: BalanceV2 = match storage.get::<Val, BalanceV2>(key) {
            Some(e) => e,
            None => return Ok(EntryOutcome::Skipped),
        };
        let mut changed = false;
        // Optional addition: fill only when empty, so a second visit is a no-op.
        if entry.frozen.is_none() {
            entry.frozen = Some(false);
            changed = true;
        }
        if !changed {
            return Ok(EntryOutcome::AlreadyCurrent);
        }
        storage.set::<Val, BalanceV2>(key, &entry);
        Ok(EntryOutcome::Migrated)
    }
}

impl Migration for AddFreezeFlagByHand {
    type Keys = DataKey;

    const FROM: u32 = 1;
    const TO: u32 = 2;

    fn up(env: &Env, key: &Val) -> Result<EntryOutcome, MigrationError> {
        Self::migrate_one(env, key)
    }

    fn supports_down() -> bool {
        false
    }

    fn name() -> &'static str {
        "add freeze flag (hand-written)"
    }
}
