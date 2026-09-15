//! Pre-flight checks: the refusals that stop an upgrade from orphaning entries.

mod support;

use soroban_sdk::{Env, Vec};

use soroban_migrate::executor;
use soroban_migrate::index;
use soroban_migrate::preflight;
use soroban_migrate::rollback;
use soroban_migrate::{version, MigrationError};

use support::{entry_key, host, seed_v1, AddFrozen, DataKey, V2ToV3};

fn with_env<T>(f: impl FnOnce(&Env) -> T) -> T {
    let env = Env::default();
    env.mock_all_auths();
    let id = host(&env);
    env.as_contract(&id, || f(&env))
}

#[test]
fn inspect_reports_an_uninitialized_contract_without_failing() {
    with_env(|env| {
        // A contract that predates the framework is a valid thing to ask about —
        // it is what the adopt flow is for — so this reports rather than errors.
        let report = preflight::inspect::<DataKey>(env);
        assert_eq!(report.version, None);
        assert_eq!(report.index_len, 0);
        assert_eq!(report.index_entries, 0);
        assert!(!report.migration_active);
        assert!(!report.rollback_available);
    });
}

#[test]
fn inspect_reports_index_footprint_and_migration_state() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 100);
        let report = preflight::inspect::<DataKey>(env);
        assert_eq!(report.version, Some(1));
        assert_eq!(report.index_len, 100);
        // 100 keys across 4 pages, plus the summary entry.
        assert_eq!(report.index_entries, 5);

        executor::begin::<AddFrozen>(env).unwrap();
        assert!(preflight::inspect::<DataKey>(env).migration_active);
        assert!(!preflight::inspect::<DataKey>(env).rollback_available);

        executor::run_to_completion::<AddFrozen>(env, 100, 5).unwrap();
        assert!(!preflight::inspect::<DataKey>(env).migration_active);
        assert!(
            preflight::inspect::<DataKey>(env).rollback_available,
            "a completed migration is still undoable until it is cleared"
        );
    });
}

#[test]
fn an_uninitialized_contract_is_not_upgradeable() {
    with_env(|env| {
        // Without this refusal, `begin` would compute a key count from an empty
        // index, migrate nothing, and advance the version — the single most
        // dangerous outcome in the framework, because it looks like success.
        assert_eq!(
            preflight::assert_upgradeable::<AddFrozen>(env),
            Err(MigrationError::NotInitialized)
        );
    });
}

#[test]
fn a_contract_at_the_wrong_version_is_not_upgradeable() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 3).unwrap();
        seed_v1(env, 10);
        assert_eq!(
            preflight::assert_upgradeable::<AddFrozen>(env),
            Err(MigrationError::StateNewerThanContract)
        );
    });
}

#[test]
fn a_contract_with_an_empty_index_is_not_upgradeable() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        // The contract is at version 1 and has a recorded version, but the
        // framework has never seen a key. Either the application never registered
        // keys, or its data lives somewhere the framework cannot reach. Both are
        // reasons to stop, not to proceed.
        assert_eq!(
            preflight::assert_upgradeable::<AddFrozen>(env),
            Err(MigrationError::OrphanedEntries)
        );
    });
}

#[test]
fn pre_flighting_a_migration_that_is_already_running_is_a_resume_not_a_conflict() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 10);
        executor::begin::<AddFrozen>(env).unwrap();
        // An operator who does not know whether their last call landed must be able to
        // call the same entry point again. A pre-flight that refused here would make
        // `begin` incompatible with itself.
        let report = preflight::assert_upgradeable::<AddFrozen>(env).expect("resume is allowed");
        assert!(report.migration_active);
    });
}

#[test]
fn a_rollback_in_flight_blocks_a_pre_flight_for_the_next_upgrade() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 10);
        executor::run_to_completion::<AddFrozen>(env, 10, 5).unwrap();
        rollback::begin::<AddFrozen>(env).unwrap();

        // The version is 2 and a 2 -> 3 migration would otherwise be eligible, but a
        // reverse walk of the same index is in progress. Two version changes must not
        // run against one index at the same time, and this is the case where a
        // pre-flight has to say so.
        assert_eq!(
            preflight::assert_upgradeable::<V2ToV3>(env),
            Err(MigrationError::MigrationAlreadyActive)
        );
    });
}

#[test]
fn a_ready_contract_reports_its_shape() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 40);
        let report = preflight::assert_upgradeable::<AddFrozen>(env).unwrap();
        assert_eq!(report.version, Some(1));
        assert_eq!(report.index_len, 40);
        assert!(!report.migration_active);
    });
}

#[test]
fn a_finished_migration_does_not_block_a_preflight_for_the_next_one() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 10);
        executor::run_to_completion::<AddFrozen>(env, 10, 5).unwrap();
        // Version is now 2, so a preflight for a 2 -> 3 migration must pass the
        // version gate and ignore the finished 1 -> 2 record. A framework that
        // refused here would be single-use.
        assert!(preflight::assert_upgradeable::<V2ToV3>(env).is_ok());
    });
}

#[test]
fn orphan_detection_finds_keys_the_index_has_never_seen() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, 10);

        // Keys the index knows about.
        let mut known = Vec::new(env);
        known.push_back(entry_key(env, 0));
        known.push_back(entry_key(env, 9));
        assert_eq!(preflight::count_orphans::<DataKey>(env, 1, &known), Ok(0));
        assert_eq!(
            preflight::assert_keys_indexed::<DataKey>(env, 1, &known),
            Ok(())
        );

        // A live entry written before the framework was adopted: it exists in
        // storage but was never registered, so a migration would walk past it and
        // leave it in the old shape forever.
        let unknown = entry_key(env, 5_000);
        env.storage()
            .persistent()
            .set::<soroban_sdk::Val, support::EntryV1>(&unknown, &support::EntryV1 { amount: 1 });

        let mut mixed = Vec::new(env);
        mixed.push_back(entry_key(env, 0));
        mixed.push_back(unknown);
        assert_eq!(preflight::count_orphans::<DataKey>(env, 1, &mixed), Ok(1));
        assert_eq!(
            preflight::assert_keys_indexed::<DataKey>(env, 1, &mixed),
            Err(MigrationError::OrphanedEntries)
        );
    });
}

#[test]
fn orphan_detection_scales_past_one_page() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        seed_v1(env, index::PAGE_SIZE * 5);

        // Verifying a manifest composes across chunks, which is what lets a
        // contract with thousands of entries be checked within transaction input
        // limits.
        let mut chunk = Vec::new(env);
        let mut n = 0u32;
        while n < index::PAGE_SIZE * 5 {
            chunk.push_back(entry_key(env, n));
            if chunk.len() == 64 {
                assert_eq!(preflight::count_orphans::<DataKey>(env, 1, &chunk), Ok(0));
                chunk = Vec::new(env);
            }
            n += 1;
        }
        assert_eq!(preflight::count_orphans::<DataKey>(env, 1, &chunk), Ok(0));
    });
}
