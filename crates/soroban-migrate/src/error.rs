//! Errors returned by the migration framework.
//!
//! The framework deliberately distinguishes a *stale* contract (state is older
//! than the code) from a *rolled back* contract (state is newer than the code).
//! The two cases need opposite operator responses, so they are never collapsed
//! into one "version mismatch" error.

use soroban_sdk::contracterror;

/// Errors surfaced by every migration entry point.
///
/// Contract authors are expected to propagate these unchanged rather than
/// mapping them onto their own error space, so that operators see the same
/// meaning regardless of which contract they are migrating. See
/// [`is_retryable`] for the operational classification the CLI uses when
/// deciding whether to resume a batch.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum MigrationError {
    /// The contract has never had a schema version written, so nothing is known
    /// about the shape of the entries it holds.
    ///
    /// Fix: call [`crate::version::initialize`] from the contract's constructor,
    /// or run the `soroban-migrate adopt` flow for a contract that predates the
    /// framework.
    NotInitialized = 1,

    /// Stored state is *newer* than the code understands: the entry shape
    /// belongs to a version this contract build does not know about.
    ///
    /// This happens after a rollback where the code was reverted but the state
    /// was not, or when two contract versions share one address. It is never
    /// safe to "fix" this by writing the old version over the new one; the
    /// compatible code build must be restored instead.
    StateNewerThanContract = 2,

    /// Stored state is *older* than the code expects, and no migration has been
    /// run to close the gap.
    ///
    /// Fix: run the missing migration with [`crate::executor::begin`] /
    /// `batch`, or have the contract decode through the tolerant path until it
    /// completes.
    VersionMismatch = 3,

    /// A migration is already in flight for this contract.
    MigrationAlreadyActive = 4,

    /// No migration is in flight, so there is nothing to resume or roll back.
    NoActiveMigration = 5,

    /// The in-flight migration is not the one the caller asked to advance.
    ///
    /// Finish or abort the in-flight migration before starting another one.
    WrongMigration = 6,

    /// The requested batch is larger than the per-transaction key budget the
    /// framework can execute within the network's resource limits.
    BatchTooLarge = 7,

    /// A batch of zero keys was requested. Batches must make progress, otherwise
    /// an operator loop can spin forever without advancing the cursor.
    BatchSizeZero = 8,

    /// A key index page could not be decoded, or an index entry was missing.
    /// The index is append-only bookkeeping; corruption means the framework's
    /// own writes are suspect and the migration must not continue.
    CorruptIndex = 9,

    /// `preflight` found live entries that the framework has never seen, so a
    /// migration would silently leave them behind.
    ///
    /// Fix: register the keys (see [`crate::index::register`]) or supply a key
    /// manifest that covers them, then re-run the pre-flight check.
    OrphanedEntries = 10,

    /// A rollback was requested before the migration finished. Rolling back a
    /// half-applied migration leaves entries in two shapes at once.
    MigrationNotComplete = 11,

    /// The migration declares no compensating (down) transformation, so it can
    /// only be rolled back by restoring a backup of the state.
    DownNotSupported = 12,

    /// Schema versions start at [`crate::version::INITIAL_VERSION`]; zero is
    /// not a valid version and is rejected rather than treated as "unset",
    /// because "unset" already has its own representation.
    InvalidVersion = 13,

    /// Migrations advance one version at a time. A migration that jumps
    /// versions would make rollback ambiguous, so it is rejected at `begin`.
    NotSequential = 14,

    /// The migration has already completed.
    AlreadyCompleted = 15,

    /// An executable reference tag has no entry, or the entry's owner has none.
    ///
    /// Raised by [`crate::fleet::promote`] before it writes anything, so a failed
    /// promotion leaves the live tag untouched rather than half-moved.
    UnknownExecutableTag = 16,
}

/// Whether retrying the same batch can plausibly succeed.
///
/// `soroban-migrate run` uses this to decide between resuming and aborting when
/// a batch fails: transient conditions are retried, structural problems are
/// surfaced to the operator.
pub fn is_retryable(error: &MigrationError) -> bool {
    matches!(
        error,
        MigrationError::MigrationAlreadyActive | MigrationError::WrongMigration
    )
}

/// Whether the error means the contract's state cannot be understood by this
/// build at all, which requires restoring a compatible build rather than running
/// a migration.
///
/// Note that [`MigrationError::NotInitialized`] is in this set for a *stale* build
/// but not for a fresh one: a contract that predates the framework reports the
/// same error and is fixed by adopting it, not by rolling code back. The error
/// cannot distinguish the two, so the CLI resolves it against the deploy history
/// rather than guessing.
pub fn requires_code_rollback(error: &MigrationError) -> bool {
    matches!(
        error,
        MigrationError::StateNewerThanContract | MigrationError::NotInitialized
    )
}
