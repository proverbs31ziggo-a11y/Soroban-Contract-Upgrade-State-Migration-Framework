//! The whole upgrade, end to end, against a real deployed contract.
//!
//! This is the test that says the framework works: a contract with two hundred entries
//! in the old shape is migrated in batches, its version advances only at the end, its
//! reads keep working throughout, and the whole thing can be undone.

mod harness;

use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, IntoVal};

use soroban_migrate::migration::MigrationStatus;
use soroban_migrate::version::INITIAL_VERSION;
use soroban_migrate::MigrationError;

use harness::{contract_error, fixture};
use vault_example::keys::DataKey;
use vault_example::schema::{BalanceV1, BalanceV2};

/// The example's change, as the CLI sees it.
///
/// Kept next to the contract so that a reader comparing the two can see that they are
/// the same change described twice: once as Rust the contract runs, and once as a
/// schema diff the CLI reasons about.

#[test]
fn a_fresh_contract_is_at_the_first_version_with_an_empty_index() {
    let f = fixture();
    assert_eq!(f.client().schema_version(), Some(INITIAL_VERSION));
    assert_eq!(f.client().migration_status(), None);

    // No keys yet, so nothing is eligible for migration. This is the state `preflight`
    // refuses, and the reason is worth restating where a reader will meet it: an empty
    // index plus a version bump is what a migration that silently misses everything
    // looks like from the outside.
    assert_eq!(
        contract_error(f.client().try_preflight()),
        MigrationError::OrphanedEntries
    );
}

#[test]
fn seeded_version_one_entries_read_as_the_new_shape_before_any_migration() {
    let f = fixture();
    let owners = f.seed_v1(20);

    assert_eq!(f.client().schema_version(), Some(1));
    assert_eq!(f.migrated_count(&owners), 0);

    // The property the whole rollout depends on: the version-2 code reads a version-1
    // entry without trapping. If this failed, the code could not be promoted until the
    // migration finished, and a long migration would mean a long outage.
    let first = f.balance(&owners.get(0).unwrap()).expect("seeded");
    assert_eq!(first.amount, 1_000);
    assert_eq!(first.frozen, None);
}

#[test]
fn the_whole_upgrade_runs_in_batches_and_advances_the_version_last() {
    let f = fixture();
    let owners = f.seed_v1(200);
    let client = f.client();

    let report = client.preflight();
    assert_eq!(report.version, Some(1));
    assert_eq!(report.index_len, 200);
    assert!(!report.migration_active);

    let state = client.begin_migration();
    assert_eq!(state.cursor, 0);
    assert_eq!(state.total, 200);

    let mut cursors = Vec::new();
    loop {
        // Each call is one transaction on a real network.
        let outcome = client.migrate_batch(&80);
        cursors.push(outcome.cursor);
        assert!(
            outcome.migrated + outcome.already_current > 0,
            "a batch must make progress"
        );

        if outcome.done {
            break;
        }
        // The version is the gate every read goes through. Advancing it before the last
        // batch would leave the contract claiming a shape its entries do not all have.
        assert_eq!(client.schema_version(), Some(1));
    }

    assert_eq!(cursors, vec![80, 160, 200]);
    assert_eq!(client.schema_version(), Some(2));
    assert_eq!(f.migrated_count(&owners), 200);

    let status = client.migration_status().expect("recorded");
    assert_eq!(status.status, MigrationStatus::Completed);
    assert_eq!(status.progress_basis_points(), 10_000);

    // Values survive the migration. A migration that reshapes entries and loses their
    // contents is the failure this whole exercise is meant to prevent.
    let first = f.balance(&owners.get(0).unwrap()).unwrap();
    assert_eq!(first.amount, 1_000);
    assert_eq!(first.frozen, Some(false));
}

#[test]
fn a_migration_resumes_from_its_cursor_after_being_abandoned() {
    let f = fixture();
    let owners = f.seed_v1(50);
    let client = f.client();

    client.begin_migration();
    client.migrate_batch(&20);
    assert_eq!(f.migrated_count(&owners), 20);

    // Abandoning clears the cursor without undoing the work, which is only safe because
    // `up` is idempotent: the second pass re-visits the first twenty and reports them as
    // already current rather than rewriting them.
    client.abort_migration();
    assert_eq!(client.migration_status(), None);
    assert_eq!(client.schema_version(), Some(1));

    client.begin_migration();
    let outcome = client.migrate_batch(&50);
    assert_eq!(outcome.already_current, 20);
    assert_eq!(outcome.migrated, 30);
    assert!(outcome.done);
    assert_eq!(client.schema_version(), Some(2));
}

#[test]
fn replaying_the_final_batch_is_harmless() {
    let f = fixture();
    let owners = f.seed_v1(30);
    let client = f.client();

    client.begin_migration();
    client.migrate_batch(&30);
    assert_eq!(
        client.migration_status().unwrap().status,
        MigrationStatus::Completed
    );

    // An operator whose transaction timed out does not know whether it landed.
    // Re-sending it must be safe, and it must be cheap: no writes, and a report that
    // says so.
    let replay = client.migrate_batch(&30);
    assert!(replay.done);
    assert_eq!(replay.visited, 0);
    assert_eq!(replay.migrated, 0);
    assert_eq!(f.migrated_count(&owners), 30);
    assert_eq!(client.schema_version(), Some(2));
}

#[test]
fn the_application_keeps_working_throughout_the_migration() {
    let f = fixture();
    let owners = f.seed_v1(100);
    let client = f.client();

    client.begin_migration();
    client.migrate_batch(&40);

    // A deposit while the migration is in flight. It goes through `store::put`, which
    // registers the key under the in-flight target version, so the index stays in step
    // with what is actually on disk — and the entry is written in the new shape.
    let newcomer = Address::generate(&f.env);
    let balance = client.deposit(&newcomer, &500);
    assert_eq!(balance.frozen, Some(false));

    let outcome = client.migrate_batch(&soroban_migrate::MAX_KEYS_PER_BATCH);
    assert!(outcome.done);
    assert_eq!(client.schema_version(), Some(2));

    // The entry written during the migration is intact, and it was never wrongly
    // reported as unmigrated.
    assert_eq!(f.balance(&newcomer).unwrap().amount, 500);
    assert_eq!(f.migrated_count(&owners), 100);
}

#[test]
fn a_completed_migration_can_be_rolled_back_and_the_old_shape_restored() {
    let f = fixture();
    let owners = f.seed_v1(60);
    let client = f.client();

    client.begin_migration();
    client.migrate_batch(&60);
    assert_eq!(f.migrated_count(&owners), 60);

    let rolling = client.begin_rollback();
    assert_eq!(rolling.total, 60);

    let mut done = false;
    while !done {
        done = client.rollback_batch(&25).done;
    }

    assert_eq!(client.schema_version(), Some(1));
    assert_eq!(f.migrated_count(&owners), 0, "every entry back in shape 1");
    assert_eq!(
        client.migration_status().unwrap().status,
        MigrationStatus::RolledBack
    );

    // The rollback rebuilt the version-1 index rather than appending to it, so a re-run
    // of the forward migration walks exactly the right number of keys.
    assert_eq!(f.index_len(1), 60);
}

#[test]
fn rolling_back_a_migration_that_did_not_finish_is_refused() {
    let f = fixture();
    f.seed_v1(20);
    let client = f.client();

    client.begin_migration();
    client.migrate_batch(&5);

    // A rollback needs to know which of the twenty entries are in which shape, and the
    // answer is still changing. Finish it, or abandon it and re-run from zero.
    assert_eq!(
        contract_error(client.try_begin_rollback()),
        MigrationError::MigrationNotComplete
    );
}

#[test]
fn confirming_a_migration_discards_the_record_but_keeps_the_version() {
    let f = fixture();
    f.seed_v1(10);
    let client = f.client();

    client.begin_migration();
    client.migrate_batch(&10);

    client.confirm_migration();
    assert_eq!(client.migration_status(), None);
    // The version is what reads are gated on and must outlive the record of how it got
    // there. Losing it would make the contract look like a fresh deployment whose entries
    // happen to be in an unknown shape.
    assert_eq!(client.schema_version(), Some(2));

    // And with the record gone, the same migration cannot be started again. The error is
    // `StateNewerThanContract` rather than `AlreadyCompleted`, and that is the more
    // useful answer: the contract's *state* is ahead of the version the migration starts
    // from, which is true regardless of whether a record of how it got there survives.
    assert_eq!(
        contract_error(client.try_begin_migration()),
        MigrationError::StateNewerThanContract
    );
}

#[test]
fn a_contract_whose_keys_were_never_registered_is_refused_before_anything_runs() {
    let f = fixture();
    let client = f.client();

    // Live entries written without registration — a contract retrofitted onto the
    // framework, or one whose write path bypassed `store::put`. Nothing can enumerate
    // them, so the only safe response is to refuse.
    let owner = Address::generate(&f.env);
    f.env.as_contract(&f.id, || {
        let key = DataKey::Balance(owner.clone()).into_val(&f.env);
        f.env
            .storage()
            .persistent()
            .set::<soroban_sdk::Val, BalanceV1>(
                &key,
                &BalanceV1 {
                    owner: owner.clone(),
                    amount: 42,
                },
            );
    });

    assert_eq!(
        contract_error(client.try_preflight()),
        MigrationError::OrphanedEntries
    );
    assert_eq!(
        contract_error(client.try_begin_migration()),
        MigrationError::OrphanedEntries
    );
    assert_eq!(client.schema_version(), Some(1));
}

#[test]
fn beginning_again_resumes_rather_than_restarting() {
    let f = fixture();
    f.seed_v1(10);
    let client = f.client();

    client.begin_migration();
    client.migrate_batch(&4);

    // An operator who is unsure whether their last call landed calls `begin` again. It
    // must report where the cursor actually is, not start over — and it must not be
    // refused, because "resume" and "start" are the same operation from the operator's
    // point of view.
    let resumed = client.begin_migration();
    assert_eq!(resumed.cursor, 4);

    // The pre-flight composes with that: against a migration already running at the same
    // version pair it succeeds and says so, rather than refusing and leaving the operator
    // with no safe call to make.
    let report = client.preflight();
    assert!(report.migration_active);
    assert_eq!(report.version, Some(1));
}

#[test]
fn batches_above_the_transaction_budget_are_refused_rather_than_failing_on_chain() {
    let f = fixture();
    f.seed_v1(10);
    let client = f.client();
    client.begin_migration();

    // Refusing up front is the difference between an operator learning that their batch
    // size is wrong from a clear error, and learning it from a transaction that consumed
    // its fee and reverted halfway through a migration.
    assert_eq!(
        contract_error(client.try_migrate_batch(&(soroban_migrate::MAX_KEYS_PER_BATCH + 1))),
        MigrationError::BatchTooLarge
    );
    assert_eq!(
        contract_error(client.try_migrate_batch(&0)),
        MigrationError::BatchSizeZero
    );
}

#[test]
fn the_deployed_contract_reports_the_schema_its_source_declares() {
    // The contract's `StorageSchema` metadata is what lets a build be checked against the
    // schema snapshots in a repository before it is uploaded.
    let declared = <BalanceV2 as soroban_migrate::schema::StorageSchema>::JSON;
    assert!(declared.contains("\"version\": 2"));
    assert!(declared.contains("\"frozen\""));
    assert!(declared.contains("\"optional\": true"));

    let v1 = <BalanceV1 as soroban_migrate::schema::StorageSchema>::JSON;
    assert!(v1.contains("\"version\": 1"));
    assert!(!v1.contains("frozen"));

    // Both report the *same* shape name, which is what makes them two versions of one
    // shape rather than two unrelated ones. Without `name = "Balance"` on both, the
    // name would default to the struct name, there would be no version pair to diff, and
    // `soroban-migrate check` would report success about a change it never looked at.
    for json in [declared, v1] {
        assert!(
            json.contains("\"name\": \"Balance\""),
            "each version must declare the shape's shared identity, not its struct name: {json}"
        );
    }
}

#[test]
fn the_derives_metadata_matches_what_the_cli_parser_produces_from_source() {
    // The contract emits its schema as JSON at compile time; the CLI derives the same
    // schema by parsing the source. If those two disagreed, `soroban-migrate check` would
    // report a phantom change on every run and would stop being read.
    // Parsed from the real file rather than a copy of its text. A copy would pass
    // while the file drifted, which is precisely the failure this is meant to catch.
    let source = include_str!("../src/schema.rs");
    let parsed = soroban_migrate_schema::parse::parse_source("src/schema.rs", source)
        .expect("the example's schema module parses");

    for schema in &parsed {
        // Matched on the *declared* identity rather than the struct name, because that
        // is the pairing: two versions of `Balance`, and a `Stats` whose struct name
        // carries a version the schema identity does not.
        let emitted = match (schema.name.as_str(), schema.version) {
            ("Balance", 1) => <BalanceV1 as soroban_migrate::schema::StorageSchema>::JSON,
            ("Balance", 2) => <BalanceV2 as soroban_migrate::schema::StorageSchema>::JSON,
            ("Stats", 2) => {
                <vault_example::schema::StatsV2 as soroban_migrate::schema::StorageSchema>::JSON
            }
            other => panic!("unexpected schema {other:?} in the example"),
        };
        assert_eq!(
            schema.to_json(),
            emitted,
            "the derive's JSON for `{}` and the parser's differ, so every `check` would \
             report a change that is not there",
            schema.name
        );
    }
}

#[test]
fn the_cli_generator_produces_valid_rust_for_the_examples_own_change() {
    // The generator is pointed at the example's real schemas, so a change to the schema
    // model cannot silently stop the two agreeing.
    use soroban_migrate_schema::codegen::{generate, CodegenOptions};
    use soroban_migrate_schema::diff::diff_with_plan;
    use soroban_migrate_schema::model::Schema;
    use soroban_migrate_schema::plan::MigrationPlan;

    let parse = |json: &str| Schema::from_json(json).expect("derive emits valid JSON");
    let from = parse(<BalanceV1 as soroban_migrate::schema::StorageSchema>::JSON);
    let to = parse(<BalanceV2 as soroban_migrate::schema::StorageSchema>::JSON);

    let mut plan = MigrationPlan::new(1, 2);
    plan.init.insert("frozen".into(), "false".into());

    let diff = diff_with_plan(&from, &to, &plan).expect("the example's change is diffable");
    let generated = generate(
        &from,
        &to,
        &diff,
        &plan,
        &CodegenOptions::conventional(&from, &to, "crate::keys::DataKey"),
    )
    .expect("the example's change is generatable");

    syn::parse_file(&generated).expect("generated code is valid Rust");
    assert!(generated.contains("if entry.frozen.is_none()"));
    assert!(generated.contains("entry.frozen = Some(false);"));
}
