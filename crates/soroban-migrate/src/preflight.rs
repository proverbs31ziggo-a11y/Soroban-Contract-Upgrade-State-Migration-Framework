//! Pre-flight checks: refusing upgrades that would orphan live entries.
//!
//! # Two kinds of compatibility, checked in two places
//!
//! Upgrading a Soroban contract changes two things independently, and each has
//! its own way of going wrong:
//!
//! 1. **Code compatibility.** The new Wasm reads entries with a different shape.
//!    This is a property of two type definitions, so it is checked *off-chain*,
//!    by diffing schema declarations before anything is deployed. See the CLI's
//!    `soroban-migrate schema diff` and `soroban-migrate check`.
//! 2. **Coverage.** The migration knows which entries exist. A contract with
//!    live entries that the framework has never seen will migrate the ones it
//!    knows about and silently leave the rest in the old shape, where the new
//!    code will either misread them or trap on them. This is a property of the
//!    actual ledger, so it is checked *on-chain*, here.
//!
//! Coverage is the failure mode that actually bites. Shape changes are caught by
//! a compiler and a diff; a missing key is caught by nothing at all, because
//! nothing in Soroban can enumerate what a contract holds. `orphan_scan` is the
//! primitive that turns "I hope we found them all" into a check that fails.
//!
//! # The bounded-work caveat
//!
//! [`assert_keys_indexed`] takes a list of keys, because enumerating them
//! on-chain is impossible. That list comes from the CLI, which built it from an
//! off-chain indexer or from the key index itself. It is bounded by the
//! transaction's input size, so a contract with ten thousand entries is verified
//! in chunks — which is fine, because the check is a *set membership* test, and
//! chunks of a set membership test compose. Verify every chunk, and every key is
//! verified.

use soroban_sdk::{contracttype, Env, Val, Vec};

use crate::error::MigrationError;
use crate::index;
use crate::key::Keyspace;
use crate::migration::{read_state, Migration, MigrationStatus};
use crate::version;

/// What the framework knows about a contract's migration state.
///
/// Returned by an on-chain `status` entry point so an operator can inspect a
/// deployed contract without a local toolchain, and so the CLI's dry run can
/// compare its own view against the chain's.
#[allow(missing_docs)] // macro-generated impl items cannot carry docs
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreflightReport {
    /// Recorded schema version. `None` when the contract was never initialized.
    pub version: Option<u32>,
    /// Keys known to the index for the recorded version.
    pub index_len: u32,
    /// Ledger entries the index occupies.
    pub index_entries: u32,
    /// Whether a migration is in flight.
    pub migration_active: bool,
    /// Whether a completed migration is recorded, and so is still undoable.
    ///
    /// This reports that the *state* is undoable, not that the code implements
    /// a compensating transformation. Only the migration type knows that, and
    /// [`Migration::supports_down`] answers it.
    pub rollback_available: bool,
}

/// Collects what the framework knows about a contract.
///
/// Never fails. An uninitialized contract is a valid thing to ask about — it is
/// exactly the state the `adopt` flow is for — so this reports rather than
/// errors, and leaves refusal to [`assert_upgradeable`].
pub fn inspect<K: Keyspace>(env: &Env) -> PreflightReport {
    let recorded = version::read::<K>(env);
    let index_len = recorded.map_or(0, |v| index::len::<K>(env, v));
    let index_entries = recorded.map_or(0, |v| index::footprint_entries::<K>(env, v));
    let state = read_state::<K>(env);
    PreflightReport {
        version: recorded,
        index_len,
        index_entries,
        migration_active: state
            .as_ref()
            .is_some_and(super::migration::MigrationState::is_active),
        rollback_available: state
            .as_ref()
            .is_some_and(|s| s.status == MigrationStatus::Completed),
    }
}

/// Refuses to let a migration start unless the contract is in a state where one can
/// make sense, and reports what the framework knows when it can.
///
/// Call this at the top of the upgrade entry point, before deploying new Wasm and
/// before `begin`. It is written to be composable with [`crate::executor::begin`],
/// which starts a migration *or resumes one already in flight*: a pre-flight against a
/// migration that is already running at the same version pair succeeds and reports
/// `migration_active: true`, so an operator who is unsure whether their last call
/// landed can call the same entry point again rather than having to guess which of two
/// calls is safe.
///
/// It answers the questions that are cheap to ask and expensive to get wrong:
///
/// * Is a version recorded at all? If not, the contract predates the framework
///   and must be adopted first — otherwise `begin` would compute a key count
///   from an empty index and "successfully" migrate nothing.
/// * Is a migration already in flight? Starting a second one would compute a
///   cursor against the wrong index.
/// * Does the index have any keys? An index of zero on a contract that
///   genuinely holds data means the application never registered keys, and the
///   migration would be a no-op that still advances the version. That is the
///   most dangerous outcome in the whole framework, because it looks like
///   success.
///
/// # Errors
///
/// * [`MigrationError::NotInitialized`] — no version recorded.
/// * [`MigrationError::MigrationAlreadyActive`] — a *different* migration is in
///   flight. Resuming `M` itself is not an error; see above.
/// * [`MigrationError::VersionMismatch`] — stored version is not `M::FROM`.
/// * [`MigrationError::OrphanedEntries`] — the `from` index is empty.
pub fn assert_upgradeable<M: Migration>(env: &Env) -> Result<PreflightReport, MigrationError> {
    version::require::<M::Keys>(env, M::FROM)?;

    if let Some(state) = read_state::<M::Keys>(env) {
        if state.is_active() {
            // Resuming is not a second migration. If the in-flight migration is *this*
            // one, there is nothing left for a pre-flight to check: the index was
            // verified before the first batch, and the cursor is the framework's own
            // record of what it has already done. Refusing here would make `begin`
            // incompatible with itself — an operator who is unsure whether their last
            // call landed would be unable to ask.
            if state.from == M::FROM && state.to == M::TO {
                return Ok(inspect::<M::Keys>(env));
            }
            // A *different* migration is active, which is a real conflict: two version
            // changes must not be applied against one index. The reachable case is a
            // rollback in flight while an operator tries to pre-flight the next
            // forward migration.
            return Err(MigrationError::MigrationAlreadyActive);
        }
    }

    if index::len::<M::Keys>(env, M::FROM) == 0 {
        return Err(MigrationError::OrphanedEntries);
    }

    Ok(inspect::<M::Keys>(env))
}

/// Verifies that every key in `keys` is present in the index for `version`.
///
/// The on-chain half of orphan detection. The CLI passes the keys it found by
/// enumerating the ledger, in chunks small enough to fit a transaction; any key
/// the index has never seen means the migration would leave real data behind, and
/// the check fails rather than proceeding.
///
/// # Errors
///
/// * [`MigrationError::OrphanedEntries`] — at least one key is not indexed.
/// * [`MigrationError::CorruptIndex`] — the index does not describe itself
///   consistently.
pub fn assert_keys_indexed<K: Keyspace>(
    env: &Env,
    version: u32,
    keys: &Vec<Val>,
) -> Result<(), MigrationError> {
    let mut i = 0u32;
    while i < keys.len() {
        let Some(key) = keys.get(i) else {
            return Err(MigrationError::CorruptIndex);
        };
        if !index::contains::<K>(env, version, &key)? {
            return Err(MigrationError::OrphanedEntries);
        }
        i += 1;
    }
    Ok(())
}

/// How many of `keys` the index has never seen.
///
/// The non-failing form of [`assert_keys_indexed`], for a CLI that wants to
/// report *which* keys are missing rather than just that some are. Returns the
/// count and leaves naming them to the caller, because returning the keys
/// themselves would duplicate the caller's input for no gain.
///
/// # Errors
///
/// [`MigrationError::CorruptIndex`] if the index does not describe itself
/// consistently.
pub fn count_orphans<K: Keyspace>(
    env: &Env,
    version: u32,
    keys: &Vec<Val>,
) -> Result<u32, MigrationError> {
    let mut orphans = 0u32;
    let mut i = 0u32;
    while i < keys.len() {
        let Some(key) = keys.get(i) else {
            return Err(MigrationError::CorruptIndex);
        };
        if !index::contains::<K>(env, version, &key)? {
            orphans += 1;
        }
        i += 1;
    }
    Ok(orphans)
}

/// Whether a completed migration is recorded and has not been rolled back.
pub fn rollback_available<K: Keyspace>(env: &Env) -> bool {
    match read_state::<K>(env) {
        Some(state) => state.status == MigrationStatus::Completed,
        None => false,
    }
}
