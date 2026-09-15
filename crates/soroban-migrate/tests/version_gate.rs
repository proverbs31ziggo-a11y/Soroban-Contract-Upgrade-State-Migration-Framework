//! Version bookkeeping: what each error means and when it is raised.

mod support;

use soroban_sdk::Env;

use soroban_migrate::executor;
use soroban_migrate::version::{self, INITIAL_VERSION};
use soroban_migrate::MigrationError;

use support::{host, AddFrozen, DataKey};

fn with_env<T>(f: impl FnOnce(&Env) -> T) -> T {
    let env = Env::default();
    env.mock_all_auths();
    let id = host(&env);
    env.as_contract(&id, || f(&env))
}

#[test]
fn uninitialized_contract_has_no_version() {
    with_env(|env| {
        assert_eq!(version::read::<DataKey>(env), None);
    });
}

#[test]
fn initialize_records_the_initial_version() {
    with_env(|env| {
        assert_eq!(
            version::initialize::<DataKey>(env, INITIAL_VERSION),
            Ok(INITIAL_VERSION)
        );
        assert_eq!(version::read::<DataKey>(env), Some(INITIAL_VERSION));
    });
}

#[test]
fn initialize_is_idempotent_for_the_same_version() {
    with_env(|env| {
        version::initialize::<DataKey>(env, INITIAL_VERSION).unwrap();
        assert_eq!(
            version::initialize::<DataKey>(env, INITIAL_VERSION),
            Ok(INITIAL_VERSION)
        );
    });
}

#[test]
fn initialize_refuses_to_overwrite_a_recorded_version() {
    with_env(|env| {
        version::initialize::<DataKey>(env, INITIAL_VERSION).unwrap();
        // Silently accepting this would be how a real migration gets skipped:
        // the entries still hold version-1 shapes, but the contract now claims
        // version 2 and no gate would ever fail.
        assert_eq!(
            version::initialize::<DataKey>(env, 2),
            Err(MigrationError::VersionMismatch)
        );
        assert_eq!(version::read::<DataKey>(env), Some(INITIAL_VERSION));
    });
}

#[test]
fn initialize_rejects_version_zero() {
    with_env(|env| {
        assert_eq!(
            version::initialize::<DataKey>(env, 0),
            Err(MigrationError::InvalidVersion)
        );
        assert_eq!(version::read::<DataKey>(env), None);
    });
}

#[test]
fn adopt_declares_the_shape_of_a_contract_that_predates_the_framework() {
    with_env(|env| {
        // A live contract retrofitted onto the framework: its entries already
        // have a shape, and the operator declares which version that is.
        assert_eq!(version::adopt::<DataKey>(env, 7), Ok(7));
        assert_eq!(version::read::<DataKey>(env), Some(7));
    });
}

#[test]
fn adopt_is_refused_while_a_migration_is_in_flight() {
    with_env(|env| {
        version::initialize::<DataKey>(env, 1).unwrap();
        executor::begin::<AddFrozen>(env).unwrap();
        // Adopting mid-migration would rewrite the version the executor is
        // walking against.
        assert_eq!(
            version::adopt::<DataKey>(env, 4),
            Err(MigrationError::MigrationAlreadyActive)
        );
    });
}

#[test]
fn require_distinguishes_unset_stale_and_ahead() {
    with_env(|env| {
        assert_eq!(
            version::require::<DataKey>(env, 3),
            Err(MigrationError::NotInitialized)
        );

        version::initialize::<DataKey>(env, 1).unwrap();
        assert_eq!(version::require::<DataKey>(env, 1), Ok(()));
        // Behind: a migration has not run.
        assert_eq!(
            version::require::<DataKey>(env, 2),
            Err(MigrationError::VersionMismatch)
        );
        // Ahead: this build is stale, typically after a code rollback. The two
        // cases need opposite operator responses, so they must not collapse.
        version::write::<DataKey>(env, 5);
        assert_eq!(
            version::require::<DataKey>(env, 4),
            Err(MigrationError::StateNewerThanContract)
        );
        assert_eq!(
            version::require::<DataKey>(env, 5),
            Ok(()),
            "a stale build must still accept state at its own version"
        );
        assert!(!soroban_migrate::error::requires_code_rollback(
            &MigrationError::VersionMismatch
        ));
        assert!(soroban_migrate::error::requires_code_rollback(
            &MigrationError::StateNewerThanContract
        ));
    });
}

#[test]
fn is_behind_reports_an_outstanding_migration() {
    with_env(|env| {
        assert!(!version::is_behind::<DataKey>(env, 1));
        version::initialize::<DataKey>(env, 1).unwrap();
        assert!(version::is_behind::<DataKey>(env, 2));
        assert!(!version::is_behind::<DataKey>(env, 1));
    });
}

#[test]
fn require_or_panic_panics_with_the_framework_error() {
    let result = std::panic::catch_unwind(|| {
        with_env(|env| {
            version::require_or_panic::<DataKey>(env, 1);
        });
    });
    assert!(
        result.is_err(),
        "an uninitialized contract must not pass the gate"
    );
}
