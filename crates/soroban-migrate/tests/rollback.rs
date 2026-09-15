//! Rollback: which migrations may be undone, in what order, and what makes the
//! framework refuse.

mod support;

use soroban_sdk::Env;

use soroban_migrate::executor;
use soroban_migrate::index;
use soroban_migrate::migration::MigrationStatus;
use soroban_migrate::rollback;
use soroban_migrate::{version, MigrationError};

use support::{count_v2, host, seed_entries_at, seed_v1, AddFrozen, AddFrozenNoDown, DataKey};

fn with_env<T>(f: impl FnOnce(&Env) -> T) -> T {
    let env = Env::default();
    env.mock_all_auths();
    let id = host(&env);
    env.as_contract(&id, || f(&env))
}

#[test]
fn a_migration_without_a_compensating_transformation_cannot_be_rolled_back() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 5);
        executor::run_to_completion::<AddFrozenNoDown>(env, 5, 5).unwrap();
        // Refused before a single batch is sent, rather than half-applied and
        // then discovered to be impossible.
        assert_eq!(
            rollback::begin::<AddFrozenNoDown>(env),
            Err(MigrationError::DownNotSupported)
        );
    });
}

#[test]
fn a_migration_that_has_not_finished_cannot_be_rolled_back() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 20);
        executor::begin::<AddFrozen>(env).unwrap();
        executor::batch::<AddFrozen>(env, 5).unwrap();

        // Rolling back a half-applied migration would need to know which of the twenty
        // keys are in which shape, and the answer would change while the rollback ran.
        // Finish it, or abandon it with `executor::abort`.
        //
        // The error must name the real problem. The version is still 1, so a check
        // ordered the other way round would report `VersionMismatch` — "this contract is
        // behind" — when what is true is "the migration you are undoing has not
        // finished". Those need opposite operator responses.
        assert_eq!(
            rollback::begin::<AddFrozen>(env),
            Err(MigrationError::MigrationNotComplete)
        );
    });
}

#[test]
fn rollback_requires_a_record_of_the_completed_migration() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 5);
        executor::run_to_completion::<AddFrozen>(env, 5, 5).unwrap();
        // Confirming a migration clears its record, and with it the ability to
        // undo it without restoring a backup.
        executor::clear::<DataKey>(env).unwrap();
        assert_eq!(
            rollback::begin::<AddFrozen>(env),
            Err(MigrationError::NoActiveMigration)
        );
    });
}

#[test]
fn rollback_restores_the_previous_shape_and_version() {
    with_env(|env| {
        let total = 40u32;
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, total);
        executor::run_to_completion::<AddFrozen>(env, 16, 10).unwrap();
        assert_eq!(count_v2(env, total), total);

        rollback::run_to_completion::<AddFrozen>(env, 16, 10).unwrap();

        assert_eq!(version::read::<DataKey>(env), Some(1));
        assert_eq!(
            count_v2(env, total),
            0,
            "every entry must be back in shape 1"
        );
        assert_eq!(
            rollback::status::<AddFrozen>(env).unwrap().status,
            MigrationStatus::RolledBack
        );
        // The version-1 index is rebuilt as the rollback walks, so it is exactly
        // the size it was — not doubled, which is what appending would have
        // produced and which would tax every future migration.
        assert_eq!(index::len::<DataKey>(env, 1), total);
    });
}

#[test]
fn rollback_can_be_run_after_a_partial_rollback_without_reapplying() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 10);
        executor::run_to_completion::<AddFrozen>(env, 10, 5).unwrap();

        rollback::begin::<AddFrozen>(env).unwrap();
        let first = rollback::batch::<AddFrozen>(env, 4).unwrap();
        assert_eq!(first.migrated, 4);
        assert_eq!(first.cursor, 4);
        assert!(!first.done);
        assert_eq!(version::read::<DataKey>(env), Some(2));

        // `begin` again resumes rather than restarting.
        let resumed = rollback::begin::<AddFrozen>(env).unwrap();
        assert_eq!(resumed.cursor, 4);
        rollback::batch::<AddFrozen>(env, 100).unwrap();
        assert_eq!(version::read::<DataKey>(env), Some(1));
        assert_eq!(count_v2(env, 10), 0);
    });
}

#[test]
fn rollback_unwinds_newest_first() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 10);
        executor::run_to_completion::<AddFrozen>(env, 10, 5).unwrap();
        rollback::begin::<AddFrozen>(env).unwrap();

        // A batch of 3 must undo keys 9, 8, 7 — the most recently registered —
        // so that a rollback interrupted by a restart continues downward from a
        // position that means something.
        rollback::batch::<AddFrozen>(env, 3).unwrap();
        assert_eq!(count_v2(env, 10), 7);
        assert_eq!(count_v2(env, 7), 7, "keys 0..6 must still be in shape 2");
        assert!(!support::is_v2(env, 7));
        assert!(!support::is_v2(env, 8));
        assert!(!support::is_v2(env, 9));
    });
}

#[test]
fn rollback_refuses_to_continue_when_new_data_appears() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 10);
        executor::run_to_completion::<AddFrozen>(env, 10, 5).unwrap();

        rollback::begin::<AddFrozen>(env).unwrap();
        rollback::batch::<AddFrozen>(env, 3).unwrap();

        // Something is still writing version-2 data — a mixed fleet mid-rollout, or a
        // second operator who did not check. It registers into the version-2 index,
        // which is where an application writing under the new code registers.
        //
        // Walking backwards over an index that is growing would leave those keys in the
        // new shape while the version claims they are in the old one, so the framework
        // refuses instead of silently producing that state.
        seed_entries_at(env, 100, 1, 2);
        assert_eq!(
            rollback::batch::<AddFrozen>(env, 3),
            Err(MigrationError::OrphanedEntries)
        );
    });
}

#[test]
fn rolling_back_twice_is_a_no_op() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 5);
        executor::run_to_completion::<AddFrozen>(env, 5, 5).unwrap();
        rollback::run_to_completion::<AddFrozen>(env, 5, 5).unwrap();

        let replay = rollback::batch::<AddFrozen>(env, 5).unwrap();
        assert!(replay.done);
        assert_eq!(replay.visited, 0);
        assert_eq!(version::read::<DataKey>(env), Some(1));
    });
}
