//! The batched executor: applying a migration across more entries than fit in
//! one transaction.
//!
//! # Why batching is not optional
//!
//! Mainnet caps a single invocation at 200 ledger-entry reads and 200 writes.
//! A contract with five thousand entries needs at least `5000 / 200 = 25`
//! transactions, and more in practice once the framework's own bookkeeping is
//! counted. Any framework that offers a one-shot `migrate()` is offering
//! something that works on test data and fails on production data.
//!
//! # How resumption stays correct
//!
//! The executor's correctness rests on one property of Soroban: **a contract
//! invocation is atomic**. If a batch fails for any reason — an `Err` from the
//! migration, a panic, exhausting a resource limit — the network discards every
//! write the batch made, including the cursor. There is no half-applied batch to
//! detect and no compensating transaction to write. Retrying the same batch is
//! therefore always safe, and "exactly once" is achieved by making the batch
//! itself the unit of atomicity rather than trying to make individual entries
//! individually resumable.
//!
//! The consequence is that [`batch`] is idempotent as a whole *given* that
//! [`Migration::up`] is idempotent per key — which is why that requirement is
//! documented as a hard one on the trait rather than as a nicety.
//!
//! # Live contracts
//!
//! A migration does not require the contract to stop. The `from` index grows
//! whenever the application registers a key, and the executor absorbs that
//! growth by re-reading the index length before deciding it is finished. A
//! migration therefore completes when the cursor catches up with the index, not
//! at a fixed key count decided at `begin`.
//!
//! What the contract *must* do while a migration is running is write the new
//! shape. Old-shape writes after a key was migrated would be silently
//! overwritten by the next batch's inverse, or worse, left alone. Route writes
//! through [`crate::store::put`], which registers keys under the in-flight
//! target version and keeps the index consistent with what is actually on disk.

use soroban_sdk::{Env, Vec};

use crate::error::MigrationError;
use crate::index;
use crate::key::Keyspace;
use crate::migration::{
    read_state, write_state, BatchOutcome, EntryOutcome, Migration, MigrationState, MigrationStatus,
};
use crate::version;

/// Largest batch the framework will execute in one transaction.
///
/// Measured, not chosen for roundness. Mainnet caps a single invocation at five
/// things simultaneously: 200 entry reads, 200 entry writes, 400 footprint
/// entries, 400M cpu instructions, and 41.9 MiB of memory.
///
/// The reasoning most people reach for — "150 keys means about 150 reads and 150
/// writes, so it fits" — is right but not binding. Measured against enforced
/// Mainnet limits on a 600-key contract, in a batch that includes `begin`:
///
/// | keys | cpu instructions | memory |
/// | ---- | ---------------- | ------ |
/// | 1    | 1.3M   (0.3%)    | 0.5 MiB (1.2%) |
/// | 32   | 16.3M  (4.1%)    | 4.2 MiB (10.0%) |
/// | 100  | 52.4M  (13.1%)   | 12.9 MiB (30.8%) |
/// | 150  | 80.3M  (20.1%)   | 19.4 MiB (46.1%) |
///
/// Entry counts are nowhere near their caps at 150 keys. **Memory is the binding
/// constraint**, at roughly 127 KiB per key plus a fixed baseline, and it is the
/// one to watch: the SDK warns that memory metering on the host tends to
/// *under*estimate what the Wasm contract will actually use. 46% of the budget is
/// the largest this constant can safely be, and a migration that decodes bigger
/// entries than these — a struct with a map of balances, say — will hit the memory
/// cap well before 150.
///
/// So this constant is a *guard*, not a recommendation: [`batch`] refuses anything
/// larger. The CLI's dry run measures the real footprint against the real contract
/// and reports the largest batch that fits, which is the number an operator should
/// actually use. `crates/soroban-migrate/tests/resource_limits.rs` asserts every
/// figure above and fails if a change to the framework makes a batch heavier.
pub const MAX_KEYS_PER_BATCH: u32 = 150;

/// Suggested batch size for operators who have not measured their own.
///
/// Two thirds of [`MAX_KEYS_PER_BATCH`], chosen so that a migration decoding entries
/// roughly half again as expensive as the framework's test fixture — see the
/// measurements on that constant — still fits. The CLI's dry run replaces this
/// guess with a measurement against the contract being migrated.
pub const DEFAULT_KEYS_PER_BATCH: u32 = 100;

/// Instance-entry TTL threshold for the framework's own bookkeeping during a
/// long migration.
pub const INSTANCE_TTL_THRESHOLD: u32 = 100_000;

/// Instance-entry TTL target, in ledgers. Roughly 30 days at five seconds per
/// ledger.
///
/// A migration over a large contract can run for many hours of operator
/// attention. If the instance entry — which holds the version, the cursor, and
/// the index summary — archives partway through, every subsequent batch fails
/// until the entry is restored, and the failure looks like a missing version
/// rather than an expired entry. Extending on each batch keeps the whole run
/// alive.
pub const INSTANCE_TTL_EXTEND_TO: u32 = 518_400;

/// Starts a migration, or resumes the one already in flight.
///
/// Resuming is deliberately the same call as starting. An operator who is unsure
/// whether their previous invocation landed should call this, not guess: the
/// returned state says exactly where the cursor is, and a migration that is
/// already in flight at the same version pair is returned rather than rejected.
///
/// # Errors
///
/// * [`MigrationError::NotSequential`] — `M::TO` is not `M::FROM + 1`.
/// * [`MigrationError::NotInitialized`] — the contract has no recorded version.
/// * [`MigrationError::StateNewerThanContract`] / [`MigrationError::VersionMismatch`]
///   — stored version is not `M::FROM`.
/// * [`MigrationError::AlreadyCompleted`] — this migration already finished.
/// * [`MigrationError::WrongMigration`] — a *different* migration is in flight.
pub fn begin<M: Migration>(env: &Env) -> Result<MigrationState, MigrationError> {
    if M::TO != M::FROM.saturating_add(1) {
        return Err(MigrationError::NotSequential);
    }
    version::require::<M::Keys>(env, M::FROM)?;

    if let Some(existing) = read_state::<M::Keys>(env) {
        let same_migration = existing.from == M::FROM && existing.to == M::TO;
        if same_migration {
            return match existing.status {
                MigrationStatus::Completed => Err(MigrationError::AlreadyCompleted),
                MigrationStatus::Running => Ok(existing),
                // A rollback of this migration is in flight. Finishing it is
                // `rollback::batch`'s job, so starting a forward run now would
                // have the two walk the same index in opposite directions.
                MigrationStatus::RollingBack | MigrationStatus::RolledBack => {
                    Err(MigrationError::MigrationAlreadyActive)
                }
            };
        }
        if existing.is_active() {
            return Err(MigrationError::WrongMigration);
        }
        // A finished record for an *earlier* migration. It does not block this
        // one: the version gate above already proved the contract is at
        // `M::FROM`, which a completed earlier migration is how the contract
        // got to. Treating the stale record as "a migration is already done,
        // refuse" would wedge every contract on its second upgrade.
    }

    let state = MigrationState {
        from: M::FROM,
        to: M::TO,
        cursor: 0,
        total: index::len::<M::Keys>(env, M::FROM),
        status: MigrationStatus::Running,
        started_ledger: env.ledger().sequence(),
    };
    write_state::<M::Keys>(env, &state);
    Ok(state)
}

/// Applies up to `limit` keys, then checkpoints.
///
/// # Errors
///
/// * [`MigrationError::BatchSizeZero`] — batches must make progress.
/// * [`MigrationError::BatchTooLarge`] — `limit` exceeds
///   [`MAX_KEYS_PER_BATCH`].
/// * [`MigrationError::NoActiveMigration`] — nothing in flight, or the state is
///   mid-rollback.
/// * [`MigrationError::WrongMigration`] — the in-flight migration is a different
///   version pair.
/// * [`MigrationError::CorruptIndex`] — the index does not describe itself
///   consistently.
///
/// Any error from [`Migration::up`] is returned unchanged after the transaction
/// reverts, leaving the cursor untouched.
pub fn batch<M: Migration>(env: &Env, limit: u32) -> Result<BatchOutcome, MigrationError> {
    if limit == 0 {
        return Err(MigrationError::BatchSizeZero);
    }
    if limit > MAX_KEYS_PER_BATCH {
        return Err(MigrationError::BatchTooLarge);
    }

    let mut state = read_state::<M::Keys>(env).ok_or(MigrationError::NoActiveMigration)?;
    if state.from != M::FROM || state.to != M::TO {
        return Err(MigrationError::WrongMigration);
    }
    if state.status == MigrationStatus::Completed {
        return Ok(finished_outcome(&state));
    }
    if state.status != MigrationStatus::Running {
        // A rollback is in flight. Advancing it is `rollback::batch`'s job.
        return Err(MigrationError::NoActiveMigration);
    }

    extend_instance_ttl(env);

    let start = state.cursor;
    let keys = index::range::<M::Keys>(env, M::FROM, start, limit)?;

    let mut migrated = 0u32;
    let mut already_current = 0u32;
    let mut skipped = 0u32;
    let mut visited = 0u32;

    let mut i = 0u32;
    while i < keys.len() {
        let Some(key) = keys.get(i) else {
            return Err(MigrationError::CorruptIndex);
        };
        let outcome = M::up(env, &key)?;
        // Every visited key is carried into the new version's index, including
        // keys this migration does not manage. Carrying `Skipped` keys forward
        // costs index space but guarantees a key can never fall out of the
        // framework's view: the alternative — dropping keys a migration did not
        // touch — loses them for every *later* migration that does care.
        index::register::<M::Keys>(env, M::TO, &key);
        match outcome {
            EntryOutcome::Migrated => migrated += 1,
            EntryOutcome::AlreadyCurrent => already_current += 1,
            EntryOutcome::Skipped => skipped += 1,
        }
        visited += 1;
        i += 1;
    }

    state.cursor = start.saturating_add(visited);

    // Absorb keys the live contract registered while this batch was prepared.
    // Without this, a busy contract would finish "at a key count decided at
    // begin" and leave everything written since then unmigrated.
    let observed = index::len::<M::Keys>(env, M::FROM);
    if observed > state.total {
        state.total = observed;
    }

    let done = state.cursor >= state.total;
    if done {
        state.status = MigrationStatus::Completed;
        // Advance the version in the same transaction that finishes the last
        // batch, so no observer can see a fully migrated contract that still
        // claims to be at the old version.
        version::write::<M::Keys>(env, M::TO);
    }
    write_state::<M::Keys>(env, &state);

    Ok(BatchOutcome {
        from: state.from,
        to: state.to,
        cursor: state.cursor,
        total: state.total,
        visited,
        migrated,
        already_current,
        skipped,
        done,
    })
}

/// Applies batches until the migration finishes or `max_batches` is reached.
///
/// Every batch is a separate transaction on a real network, so this exists for
/// tests and for local simulation, where the whole migration can run inside one
/// host invocation. It is not the production path: `max_batches` is a guard
/// against a migration that never converges, and hitting it returns the state it
/// stopped at rather than an error, because "not finished yet" is a normal
/// outcome for a caller that is looping.
///
/// # Errors
///
/// Propagates every error from [`begin`] and [`batch`].
pub fn run_to_completion<M: Migration>(
    env: &Env,
    limit: u32,
    max_batches: u32,
) -> Result<Vec<BatchOutcome>, MigrationError> {
    begin::<M>(env)?;
    let mut out = Vec::new(env);
    let mut n = 0u32;
    while n < max_batches {
        let outcome = batch::<M>(env, limit)?;
        let done = outcome.done;
        out.push_back(outcome);
        if done {
            break;
        }
        n += 1;
    }
    Ok(out)
}

/// Reads the migration bookkeeping without touching storage twice.
pub fn status<M: Migration>(env: &Env) -> Option<MigrationState> {
    read_state::<M::Keys>(env)
}

/// Abandons an in-flight migration, leaving entries in whichever shape they
/// reached.
///
/// Aborting does **not** undo work. It clears the cursor so a later `begin`
/// starts from zero, which only makes sense because [`Migration::up`] is
/// idempotent: the second pass re-visits already-migrated keys and reports them
/// as [`EntryOutcome::AlreadyCurrent`].
///
/// The contract is therefore left relying on tolerant decoding until a migration
/// is run to completion. Use [`crate::rollback`] instead when the goal is to get
/// back to a consistent old-shaped state.
///
/// # Errors
///
/// [`MigrationError::NoActiveMigration`] if nothing is in flight.
pub fn abort<K: Keyspace>(env: &Env) -> Result<(), MigrationError> {
    let state = read_state::<K>(env).ok_or(MigrationError::NoActiveMigration)?;
    if !state.is_active() {
        return Err(MigrationError::NoActiveMigration);
    }
    crate::migration::clear_state::<K>(env);
    Ok(())
}

/// Discards the record of a completed migration.
///
/// Called once the operator has confirmed the contract is healthy at the new
/// version. The schema version itself is left alone — it is what reads are gated
/// on and must outlive the bookkeeping.
///
/// # Errors
///
/// [`MigrationError::MigrationNotComplete`] if the migration is still in flight.
pub fn clear<K: Keyspace>(env: &Env) -> Result<(), MigrationError> {
    match read_state::<K>(env) {
        None => Ok(()),
        Some(state) if state.is_active() => Err(MigrationError::MigrationNotComplete),
        Some(_) => {
            crate::migration::clear_state::<K>(env);
            Ok(())
        }
    }
}

fn finished_outcome(state: &MigrationState) -> BatchOutcome {
    BatchOutcome {
        from: state.from,
        to: state.to,
        cursor: state.cursor,
        total: state.total,
        visited: 0,
        migrated: 0,
        already_current: 0,
        skipped: 0,
        done: true,
    }
}

/// Keeps the instance entry — version, cursor, index summary — alive across a
/// migration that spans many hours of operator attention.
pub(crate) fn extend_instance_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
}
