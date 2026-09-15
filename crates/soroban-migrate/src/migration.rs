//! The migration contract: what a version change *means*, and how far along we
//! are in applying it.

use soroban_sdk::{contracttype, Env, Val};

use crate::error::MigrationError;
use crate::key::{storage_key, Keyspace, MigrationKey};

/// Where a migration is in its lifecycle.
///
/// There is no `Failed` state. A batch is one transaction, so a batch that fails
/// is rolled back by the network and leaves the cursor exactly where it was.
/// Persisting a failure state would require a second transaction to record it,
/// and any operator able to send that transaction is equally able to just retry
/// the batch. Failure is therefore reported to the caller and to the CLI, never
/// stored on-chain where it would need its own recovery path.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationStatus {
    /// Batches remain to be applied.
    Running,
    /// Every indexed key was migrated and the schema version was advanced.
    Completed,
    /// A rollback is walking the new-version index in reverse.
    RollingBack,
    /// Every key was returned to the old shape and the version was reverted.
    RolledBack,
}

/// Bookkeeping for the in-flight or most recently finished migration.
///
/// Stored in instance storage, so it is read alongside the schema version at no
/// extra cost on calls that already touch the instance.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationState {
    /// Schema version the entries started at.
    pub from: u32,
    /// Schema version the entries are being moved to. Always `from + 1`.
    pub to: u32,
    /// Number of keys already visited, counted positionally in the `from` index.
    /// The next batch starts here.
    pub cursor: u32,
    /// How many keys the `from` index held when the batch last checked. Grows if
    /// the live contract registers new keys mid-migration.
    pub total: u32,
    /// Lifecycle position.
    pub status: MigrationStatus,
    /// Ledger sequence when the migration began, for operator forensics.
    pub started_ledger: u32,
}

impl MigrationState {
    /// True while the migration still needs batches applied.
    pub fn is_active(&self) -> bool {
        matches!(
            self.status,
            MigrationStatus::Running | MigrationStatus::RollingBack
        )
    }

    /// How many keys remain before the cursor reaches the end of the index as
    /// last observed.
    pub fn remaining(&self) -> u32 {
        self.total.saturating_sub(self.cursor)
    }

    /// Fraction of the migration complete, in basis points, for dashboards.
    ///
    /// Basis points rather than a float because Soroban has no floating point,
    /// and rather than a rounded percentage because an operator watching a
    /// ten-thousand-key migration needs the difference between 99% and 99.9%.
    /// Returns `10_000` for a migration with nothing left to do.
    pub fn progress_basis_points(&self) -> u32 {
        if self.total == 0 {
            return 10_000;
        }
        let done = u64::from(core::cmp::min(self.cursor, self.total));
        ((done * 10_000) / u64::from(self.total)) as u32
    }
}

/// What happened to one entry.
///
/// Distinguishing these matters for two reasons. Operators need to know that a
/// batch of 150 "succeeded" by skipping 148 entries that were already migrated
/// rather than by rewriting them, because the batch's cost is wildly different
/// in the two cases. And the executor needs to know whether to add a key to the
/// new version's index — a skipped key must not be, or the index would grow a
/// new version's worth of entries on every resume.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryOutcome {
    /// The entry was rewritten into the new shape.
    Migrated,
    /// The entry was already in the new shape and was left untouched. Correct
    /// for entries that were written by the new code after an earlier batch, and
    /// for entries reached twice because they appear twice in the index.
    AlreadyCurrent,
    /// The entry is not managed by this migration and was left alone.
    Skipped,
}

/// Result of one batch, returned to the operator or CLI.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchOutcome {
    /// Version the batch read from.
    pub from: u32,
    /// Version the batch wrote to.
    pub to: u32,
    /// Cursor position after the batch: where the next batch starts.
    pub cursor: u32,
    /// Total keys the batch believes it is working through.
    pub total: u32,
    /// Keys the batch looked at.
    pub visited: u32,
    /// Of those, how many were rewritten.
    pub migrated: u32,
    /// Of those, how many were already correct.
    pub already_current: u32,
    /// Of those, how many this migration does not manage.
    pub skipped: u32,
    /// True once there is nothing left to do and the version has been advanced.
    pub done: bool,
}

impl BatchOutcome {
    /// True when the batch examined at least one key.
    ///
    /// A batch that visited nothing while not being done means the cursor is
    /// pinned — the CLI treats it as a stall and stops rather than looping.
    pub fn made_progress(&self) -> bool {
        self.visited > 0 || self.done
    }
}

/// A single-version state transformation.
///
/// Implementations are generated by `#[derive(Migration)]`; hand-writing one is
/// supported but rarely necessary.
///
/// # Idempotency
///
/// [`Self::up`] **must be idempotent**. The same key can be visited more than
/// once, for three independent reasons:
///
/// 1. The key index is a log, not a set. A key registered under v1 and again
///    under v1 returns twice. See [`crate::index::register`].
/// 2. A rollback followed by a re-run revisits every key.
/// 3. A batch that completes its work but fails later in the same transaction —
///    an out-of-gas on an unrelated write, say — is rolled back entirely, so the
///    keys it "already migrated" are visited again on retry.
///
/// The framework cannot enforce this, which is exactly why the generated
/// implementations are built out of operations that are idempotent by
/// construction: setting a field that is already set, or filling in a field that
/// is still empty. Anything that *accumulates* — a counter, a running total — is
/// not idempotent and must be recomputed from the entry rather than added to.
pub trait Migration {
    /// The key space the host contract declared for the framework.
    type Keys: Keyspace;

    /// Version being migrated from. Must equal the contract's stored version
    /// when [`crate::executor::begin`] is called.
    const FROM: u32;

    /// Version being migrated to. Must be `FROM + 1`; see
    /// [`MigrationError::NotSequential`] for why jumps are refused.
    const TO: u32;

    /// Moves one entry forward.
    ///
    /// `key` is the encoded storage key, straight from the index. The
    /// implementation is responsible for all interpretation of it — including
    /// deciding that it does not manage this key at all and returning
    /// [`EntryOutcome::Skipped`], which is what lets one migration live next to
    /// unrelated data in the same contract.
    ///
    /// # Errors
    ///
    /// Returning `Err` aborts the whole batch. The transaction reverts, the
    /// cursor does not move, and the operator can retry or abort.
    fn up(env: &Env, key: &Val) -> Result<EntryOutcome, MigrationError>;

    /// Moves one entry back to the previous shape.
    ///
    /// Defaults to refusing, which is the honest default: most transformations
    /// are lossy, and a rollback that silently produces wrong data is worse than
    /// one that refuses. Only implement this when the previous shape is fully
    /// recoverable from the current one — adding an `Option` field is
    /// recoverable, discarding a field is not.
    ///
    /// # Errors
    ///
    /// Returning `Err` aborts the whole rollback batch, on the same terms as
    /// [`Self::up`]. The default returns [`MigrationError::DownNotSupported`].
    fn down(_env: &Env, _key: &Val) -> Result<EntryOutcome, MigrationError> {
        Err(MigrationError::DownNotSupported)
    }

    /// Whether [`Self::down`] is implemented. Gates the rollback entry points so
    /// that a missing compensating action is refused before any batch is sent.
    fn supports_down() -> bool {
        false
    }

    /// Human-readable name for CLI output.
    fn name() -> &'static str {
        "migration"
    }
}

/// Reads the migration bookkeeping, or `None` if there is neither an in-flight
/// nor a finished migration.
pub fn read_state<K: Keyspace>(env: &Env) -> Option<MigrationState> {
    let key = storage_key::<K>(env, MigrationKey::State);
    env.storage().instance().get::<Val, MigrationState>(&key)
}

/// Writes migration bookkeeping. Reserved for the executor.
pub fn write_state<K: Keyspace>(env: &Env, state: &MigrationState) {
    let key = storage_key::<K>(env, MigrationKey::State);
    env.storage()
        .instance()
        .set::<Val, MigrationState>(&key, state);
}

/// Removes migration bookkeeping, discarding the record of a finished run.
///
/// After this, nothing on-chain remembers that the migration happened — the
/// schema version is the only remaining evidence. That is intentional: the
/// version is what reads are gated on, and keeping a history of every past
/// migration would make the instance entry grow without bound. History belongs
/// in the deploy log and in the CLI's own records, not in the contract.
pub fn clear_state<K: Keyspace>(env: &Env) {
    let key = storage_key::<K>(env, MigrationKey::State);
    env.storage().instance().remove::<Val>(&key);
}
