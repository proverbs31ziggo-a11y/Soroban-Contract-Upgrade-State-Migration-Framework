//! Executor behaviour: batched progress, resumption, and the lifecycle edges.
//!
//! These tests run the whole migration inside one host frame, so they exercise
//! the *logical* resumption path — cursor arithmetic, index growth, carried-forward
//! keys. The property that a failed batch reverts its work comes from the host's
//! transaction semantics, not from this crate, and is tested where it can be
//! observed: against a real deployed contract in `examples/vault`.

mod support;

use soroban_sdk::Env;

use soroban_migrate::executor::{self, MAX_KEYS_PER_BATCH};
use soroban_migrate::index;
use soroban_migrate::migration::MigrationStatus;
use soroban_migrate::{version, MigrationError};

use support::{
    count_v2, host, is_v2, seed_v1, seed_v1_from, AddFrozen, DataKey, Jumping, SkipsEverything,
    V2ToV3,
};

fn with_env<T>(f: impl FnOnce(&Env) -> T) -> T {
    let env = Env::default();
    env.mock_all_auths();
    let id = host(&env);
    env.as_contract(&id, || f(&env))
}

#[test]
fn begin_requires_an_initialized_contract() {
    with_env(|env| {
        assert_eq!(
            executor::begin::<AddFrozen>(env),
            Err(MigrationError::NotInitialized)
        );
    });
}

#[test]
fn begin_requires_the_contract_to_be_at_the_migrations_from_version() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 2).unwrap();
        assert_eq!(
            executor::begin::<AddFrozen>(env),
            Err(MigrationError::StateNewerThanContract)
        );
    });
}

#[test]
fn begin_refuses_a_migration_that_jumps_versions() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        // Rollback walks the same index in reverse and needs `to` to be exactly
        // one increment away; a jump would make the reverse walk ambiguous.
        assert_eq!(
            executor::begin::<Jumping>(env),
            Err(MigrationError::NotSequential)
        );
    });
}

#[test]
fn begin_records_the_key_count_at_the_moment_it_starts() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 40);
        let state = executor::begin::<AddFrozen>(env).unwrap();
        assert_eq!(state.from, 1);
        assert_eq!(state.to, 2);
        assert_eq!(state.cursor, 0);
        assert_eq!(state.total, 40);
        assert_eq!(state.status, MigrationStatus::Running);
        assert_eq!(state.remaining(), 40);
        assert_eq!(state.progress_basis_points(), 0);
    });
}

#[test]
fn begin_resumes_rather_than_restarting() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 40);
        executor::begin::<AddFrozen>(env).unwrap();
        executor::batch::<AddFrozen>(env, 10).unwrap();

        // An operator who is unsure whether their last batch landed calls
        // `begin` again. It must report where the cursor actually is.
        let resumed = executor::begin::<AddFrozen>(env).unwrap();
        assert_eq!(resumed.cursor, 10);
    });
}

#[test]
fn a_migration_that_already_finished_cannot_be_started_again() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 5);
        executor::run_to_completion::<AddFrozen>(env, 5, 5).unwrap();
        assert_eq!(
            executor::begin::<AddFrozen>(env),
            Err(MigrationError::StateNewerThanContract),
            "the version gate catches a re-run before the record does"
        );
    });
}

#[test]
fn a_finished_record_for_an_earlier_migration_does_not_block_the_next_one() {
    with_env(|env| {
        // Migration 1 -> 2 finishes and its record is left in place.
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 5);
        executor::run_to_completion::<AddFrozen>(env, 5, 5).unwrap();
        assert_eq!(version::read::<DataKey>(env), Some(2));

        // Migration 2 -> 3 must be able to begin. Treating the older record as
        // "a migration is already done, refuse" would wedge every contract on
        // its second upgrade.
        let state = executor::begin::<V2ToV3>(env).unwrap();
        assert_eq!(state.from, 2);
        assert_eq!(state.to, 3);
        assert_eq!(state.cursor, 0);
        assert_eq!(state.total, 5, "the version-2 index is what it walks");
    });
}

#[test]
fn batch_guards_its_arguments() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 5);
        executor::begin::<AddFrozen>(env).unwrap();

        // A zero batch would spin forever without advancing the cursor.
        assert_eq!(
            executor::batch::<AddFrozen>(env, 0),
            Err(MigrationError::BatchSizeZero)
        );
        // Anything above the per-transaction budget cannot execute, so it is
        // refused up front rather than failing on chain mid-batch.
        assert_eq!(
            executor::batch::<AddFrozen>(env, MAX_KEYS_PER_BATCH + 1),
            Err(MigrationError::BatchTooLarge)
        );
    });
}

#[test]
fn batch_requires_a_migration_to_be_in_flight() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 5);
        assert_eq!(
            executor::batch::<AddFrozen>(env, 10),
            Err(MigrationError::NoActiveMigration)
        );
    });
}

#[test]
fn a_partial_batch_does_not_advance_the_version() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 40);
        executor::begin::<AddFrozen>(env).unwrap();

        let outcome = executor::batch::<AddFrozen>(env, 12).unwrap();
        assert_eq!(outcome.visited, 12);
        assert_eq!(outcome.migrated, 12);
        assert_eq!(outcome.cursor, 12);
        assert_eq!(outcome.total, 40);
        assert!(!outcome.done);
        assert!(outcome.made_progress());

        // The version is the gate every read goes through. Moving it before the
        // last batch would leave the contract claiming a shape its entries do not
        // all have.
        assert_eq!(version::read::<DataKey>(env), Some(1));
        assert_eq!(count_v2(env, 40), 12);
    });
}

#[test]
fn batched_progress_tiles_the_keyspace_and_completes() {
    with_env(|env| {
        let total = 100u32;
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, total);
        executor::begin::<AddFrozen>(env).unwrap();

        let mut cursors = Vec::new();
        loop {
            let outcome = executor::batch::<AddFrozen>(env, 32).unwrap();
            cursors.push(outcome.cursor);
            if outcome.done {
                break;
            }
        }
        // 32, 64, 96, then a short final batch to 100.
        assert_eq!(cursors, vec![32, 64, 96, 100]);

        assert_eq!(version::read::<DataKey>(env), Some(2));
        assert_eq!(count_v2(env, total), total);
        assert_eq!(
            executor::status::<AddFrozen>(env).unwrap().status,
            MigrationStatus::Completed
        );
        // Every key was carried into the version-2 index, so the *next*
        // migration has something to walk.
        assert_eq!(index::len::<DataKey>(env, 2), total);
    });
}

#[test]
fn a_completed_batch_is_idempotent_to_replay() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 10);
        executor::run_to_completion::<AddFrozen>(env, 5, 10).unwrap();

        // An operator who re-sends the final batch after a timeout must not
        // corrupt anything or double-apply the migration.
        let replay = executor::batch::<AddFrozen>(env, 5).unwrap();
        assert!(replay.done);
        assert_eq!(replay.visited, 0);
        assert_eq!(count_v2(env, 10), 10);
        assert_eq!(version::read::<DataKey>(env), Some(2));
    });
}

#[test]
fn rerunning_a_batch_over_migrated_keys_reports_them_as_current() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 10);

        // Abandon a migration after five keys, then start over from zero.
        executor::begin::<AddFrozen>(env).unwrap();
        executor::batch::<AddFrozen>(env, 5).unwrap();
        executor::abort::<DataKey>(env).unwrap();

        let state = executor::begin::<AddFrozen>(env).unwrap();
        assert_eq!(state.cursor, 0, "abort clears the cursor, not the work");
        let outcome = executor::batch::<AddFrozen>(env, 10).unwrap();
        assert_eq!(outcome.migrated, 5);
        assert_eq!(
            outcome.already_current, 5,
            "keys reached twice must be recognised, not rewritten"
        );
        assert_eq!(count_v2(env, 10), 10);
    });
}

#[test]
fn keys_registered_while_the_migration_runs_are_absorbed() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 5);
        executor::begin::<AddFrozen>(env).unwrap();

        // A live contract keeps accepting writes. The keys it registers land in
        // the version-1 index after the cursor has already been set.
        seed_v1_from(env, 5, 7);

        let outcome = executor::batch::<AddFrozen>(env, 100).unwrap();
        assert_eq!(
            outcome.total, 12,
            "the batch must absorb keys registered since begin, or they are \
             silently left in the old shape"
        );
        assert!(outcome.done);
        assert_eq!(count_v2(env, 12), 12);
    });
}

#[test]
fn skipped_keys_are_carried_into_the_new_index() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 6);
        executor::begin::<SkipsEverything>(env).unwrap();
        let outcome = executor::batch::<SkipsEverything>(env, 100).unwrap();

        assert_eq!(outcome.skipped, 6);
        assert_eq!(outcome.migrated, 0);
        assert!(outcome.done);
        // Carrying skipped keys forward costs index space but guarantees a key
        // can never fall out of the framework's view: the alternative loses them
        // for every later migration that does care about them.
        assert_eq!(index::len::<DataKey>(env, 2), 6);
        assert_eq!(version::read::<DataKey>(env), Some(2));
    });
}

#[test]
fn an_empty_index_completes_immediately() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        // The executor is honest about an empty keyspace: there is nothing to do,
        // so it is done. Refusing is `preflight::assert_upgradeable`'s job, and it
        // must be called before `begin` precisely because this path looks like
        // success.
        let outcome = executor::run_to_completion::<AddFrozen>(env, 10, 5).unwrap();
        assert_eq!(outcome.len(), 1);
        assert!(outcome.get(0).unwrap().done);
        assert_eq!(version::read::<DataKey>(env), Some(2));
    });
}

#[test]
fn abort_clears_the_cursor_and_clear_discards_finished_record() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 5);

        assert_eq!(
            executor::abort::<DataKey>(env),
            Err(MigrationError::NoActiveMigration)
        );

        executor::begin::<AddFrozen>(env).unwrap();
        // A finished migration cannot be abandoned — there is nothing in flight.
        assert_eq!(
            executor::clear::<DataKey>(env),
            Err(MigrationError::MigrationNotComplete)
        );
        executor::abort::<DataKey>(env).unwrap();
        assert!(executor::status::<AddFrozen>(env).is_none());

        executor::run_to_completion::<AddFrozen>(env, 10, 5).unwrap();
        executor::clear::<DataKey>(env).unwrap();
        assert!(executor::status::<AddFrozen>(env).is_none());
        // The version survives: it is what reads are gated on.
        assert_eq!(version::read::<DataKey>(env), Some(2));
        // And with the migration gone, a re-run is refused because the contract
        // is no longer at the `from` version.
        assert_eq!(
            executor::begin::<AddFrozen>(env),
            Err(MigrationError::StateNewerThanContract)
        );
    });
}

#[test]
fn progress_reaches_full_only_when_every_key_is_visited() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 8);
        let state = executor::begin::<AddFrozen>(env).unwrap();
        assert_eq!(state.progress_basis_points(), 0);

        executor::batch::<AddFrozen>(env, 4).unwrap();
        let half = executor::status::<AddFrozen>(env).unwrap();
        assert_eq!(half.progress_basis_points(), 5_000);

        executor::batch::<AddFrozen>(env, 4).unwrap();
        let done = executor::status::<AddFrozen>(env).unwrap();
        assert_eq!(done.progress_basis_points(), 10_000);
    });
}

#[test]
fn a_batch_that_only_skips_still_advances_the_cursor() {
    with_env(|env| {
        // Guard against a stall: if a batch visited keys but migrated none, the
        // cursor must still move, otherwise the CLI's loop makes no progress.
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 20);
        executor::begin::<SkipsEverything>(env).unwrap();
        let first = executor::batch::<SkipsEverything>(env, 7).unwrap();
        assert_eq!(first.cursor, 7);
        assert!(first.made_progress());
        assert!(!is_v2(env, 0));
    });
}
