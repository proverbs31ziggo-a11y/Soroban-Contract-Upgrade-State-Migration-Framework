//! The rollback path: undoing a completed migration.
//!
//! # Rollback is not the inverse of batching
//!
//! Forward migration walks the old version's index from the start and appends to
//! the new version's index, so the walk is stable: registering keys ahead of the
//! cursor cannot shift a position the cursor has not reached yet.
//!
//! Rollback has to walk the *new* index — that is the only index that describes
//! the current shape of the data — and it must do so from the end, because the
//! old index is being rebuilt from position zero. Walking an index backwards
//! while something appends to it is only well defined if nothing appends. The
//! executor therefore pins `total` when rollback begins and **fails the batch**
//! if the new index grew since, rather than silently leaving those keys in the
//! new shape. An operator who sees that error has learned something real: their
//! fleet is writing new-version data and a code rollback is not going to be
//! clean.
//!
//! # Rollback pairs with a code rollback
//!
//! Reverting state without reverting code leaves a contract that reads the new
//! shape from old-shaped data. The supported sequence is: publish the previous
//! Wasm to the fleet tag, then run the rollback, then confirm. Because CAP-85
//! executable references make the code flip a single persistent write, the two
//! halves are close enough in time that the gap is a handful of ledgers — but
//! they are still two steps, and `soroban-migrate rollback` prints the sequence
//! rather than performing half of it.

use soroban_sdk::Env;

use crate::error::MigrationError;
use crate::executor::{extend_instance_ttl, MAX_KEYS_PER_BATCH};
use crate::index;
use crate::migration::{
    read_state, write_state, BatchOutcome, EntryOutcome, Migration, MigrationState, MigrationStatus,
};
use crate::version;

/// Begins a rollback of a completed migration.
///
/// # Errors
///
/// * [`MigrationError::NotSequential`] — the migration does not advance exactly one
///   version, so its inverse is ambiguous.
/// * [`MigrationError::NoActiveMigration`] — nothing is recorded, so there is no
///   completed migration to undo.
/// * [`MigrationError::MigrationNotComplete`] — the migration is still running, or was
///   abandoned. Neither leaves a state that a reverse walk can be defined against.
/// * [`MigrationError::WrongMigration`] — a different version pair is recorded.
/// * [`MigrationError::DownNotSupported`] — the migration declares no compensating
///   transformation, so the only rollback available is restoring a state backup.
/// * [`MigrationError::StateNewerThanContract`] — the recorded version is ahead of
///   what this migration produced, so this is not the migration to undo.
pub fn begin<M: Migration>(env: &Env) -> Result<MigrationState, MigrationError> {
    if M::TO != M::FROM.saturating_add(1) {
        return Err(MigrationError::NotSequential);
    }
    if !M::supports_down() {
        return Err(MigrationError::DownNotSupported);
    }

    // The record is consulted before the version gate, and the order is load-bearing.
    // An unfinished migration leaves the version at `FROM`, so checking the version
    // first would report `VersionMismatch` — "the contract is behind" — when what is
    // actually true is "the migration you are trying to undo has not finished". The
    // two need opposite operator responses: finish the migration, versus start one.
    let state = read_state::<M::Keys>(env).ok_or(MigrationError::NoActiveMigration)?;
    if state.from != M::FROM || state.to != M::TO {
        return Err(MigrationError::WrongMigration);
    }
    if state.status == MigrationStatus::RollingBack {
        return Ok(state);
    }
    if state.status != MigrationStatus::Completed {
        return Err(MigrationError::MigrationNotComplete);
    }
    // Only now, with a finished migration in hand, is the version meaningful: it must
    // be the version that migration produced.
    version::require::<M::Keys>(env, M::TO)?;

    // Pinned for the duration of the rollback; see the module documentation.
    let total = index::len::<M::Keys>(env, M::TO);
    // The new index is a superset of the old one, because the forward migration
    // carried every key forward. Rebuilding the old index by re-registering as we
    // revert therefore reproduces it exactly; appending instead would double it,
    // permanently. See `index::reset`.
    index::reset::<M::Keys>(env, M::FROM);
    let rolling = MigrationState {
        from: state.from,
        to: state.to,
        cursor: 0,
        total,
        status: MigrationStatus::RollingBack,
        started_ledger: env.ledger().sequence(),
    };
    write_state::<M::Keys>(env, &rolling);
    Ok(rolling)
}

/// Reverts up to `limit` keys, walking the new-version index backwards.
///
/// A key at rollback position `i` is index position `total - 1 - i` in the `to`
/// index, so the newest registrations are undone first and the walk never needs
/// to renumber.
///
/// # Errors
///
/// Every error from [`crate::executor::batch`] applies, plus:
///
/// * [`MigrationError::OrphanedEntries`] — the `to` index grew since rollback
///   began, which means something is still writing new-version data. Refusing
///   here is the point: continuing would leave those keys in the new shape while
///   the version claims otherwise.
/// * [`MigrationError::NoActiveMigration`] — no rollback in flight.
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
    if state.status == MigrationStatus::RolledBack {
        return Ok(finished_outcome(&state));
    }
    if state.status != MigrationStatus::RollingBack {
        return Err(MigrationError::NoActiveMigration);
    }

    let observed = index::len::<M::Keys>(env, M::TO);
    if observed > state.total {
        return Err(MigrationError::OrphanedEntries);
    }

    extend_instance_ttl(env);

    // Reverse window: rollback positions [cursor, end) map to the last entries of
    // the `to` index, in descending order.
    let end = core::cmp::min(state.cursor.saturating_add(limit), state.total);
    let descending_start = state.total - end;
    let span = end - state.cursor;
    let keys = index::range::<M::Keys>(env, M::TO, descending_start, span)?;

    let mut reverted = 0u32;
    let mut already_current = 0u32;
    let mut skipped = 0u32;
    let mut visited = 0u32;

    // `range` returns ascending; walk it in reverse so the newest key is undone
    // first, matching the documented order.
    let mut i = keys.len();
    while i > 0 {
        i -= 1;
        let Some(key) = keys.get(i) else {
            return Err(MigrationError::CorruptIndex);
        };
        let outcome = M::down(env, &key)?;
        index::register::<M::Keys>(env, M::FROM, &key);
        match outcome {
            EntryOutcome::Migrated => reverted += 1,
            EntryOutcome::AlreadyCurrent => already_current += 1,
            EntryOutcome::Skipped => skipped += 1,
        }
        visited += 1;
    }

    state.cursor = end;
    let done = state.cursor >= state.total;
    if done {
        state.status = MigrationStatus::RolledBack;
        version::write::<M::Keys>(env, M::FROM);
    }
    write_state::<M::Keys>(env, &state);

    Ok(BatchOutcome {
        from: state.from,
        to: state.to,
        cursor: state.cursor,
        total: state.total,
        visited,
        migrated: reverted,
        already_current,
        skipped,
        done,
    })
}

/// Reverts batches until the rollback finishes or `max_batches` is reached.
///
/// For tests and local simulation, where every batch can run inside one host
/// invocation.
///
/// # Errors
///
/// Propagates every error from [`begin`] and [`batch`].
pub fn run_to_completion<M: Migration>(
    env: &Env,
    limit: u32,
    max_batches: u32,
) -> Result<soroban_sdk::Vec<BatchOutcome>, MigrationError> {
    begin::<M>(env)?;
    let mut out = soroban_sdk::Vec::new(env);
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

/// Reads the rollback bookkeeping.
pub fn status<M: Migration>(env: &Env) -> Option<MigrationState> {
    read_state::<M::Keys>(env)
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
