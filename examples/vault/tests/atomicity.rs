//! The two properties the executor's correctness rests on, measured rather than
//! asserted.
//!
//! 1. **A failed batch leaves nothing behind.** The framework resumes from a cursor
//!    stored on chain, and that is only sound if a batch that fails part-way through
//!    also loses its cursor. This comes from Soroban's own invocation semantics, not from
//!    anything in this crate — which is exactly why it has to be measured here, against a
//!    real deployed contract, instead of being assumed. The framework's unit tests run
//!    several batches inside one host frame, so they cannot observe a transaction
//!    boundary at all.
//!
//! 2. **An unbatched migration is impossible.** Mainnet caps a single invocation at 200
//!    entry reads, 200 writes, and 400 footprint entries. A migration over a thousand
//!    entries has to be batched; a framework that offers a one-shot `migrate()` works on
//!    test data and fails on production data.
//!
//! Both are measured against a purpose-built probe contract rather than the vault,
//! because the vault's migration is deliberately infallible and always succeeds — the
//! right property for an example, and useless for testing failure.

use soroban_env_host::InvocationResourceLimits;
use soroban_sdk::testutils::cost_estimate::NetworkInvocationResourceLimits;
use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, IntoVal, Val};

use soroban_migrate::key::MigrationKey;
use soroban_migrate::migration::{BatchOutcome, EntryOutcome, Migration, MigrationState};
use soroban_migrate::{executor, index, version, MigrationError};
use soroban_migrate_macros::Keyspace;

mod harness;
use harness::contract_error;

/// The amount that makes the probe migration fail, so that a batch can be made to fail
/// *after* it has already rewritten part of its range.
///
/// Deliberately a value [`Fixture::seed`] never produces. A sentinel woven into the
/// seeded amounts would make every full-size batch fail, leaving no way to measure what a
/// batch costs when it *succeeds* — so which key is poisoned, if any, is chosen per test.
const SENTINEL: u32 = u32::MAX;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemV1 {
    pub amount: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemV2 {
    pub amount: u32,
    pub frozen: Option<bool>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq, Keyspace)]
pub enum ProbeKey {
    Migration(MigrationKey),
    Item(u32),
}

fn item_key(env: &Env, n: u32) -> Val {
    ProbeKey::Item(n).into_val(env)
}

/// Migrates from v1 to v2, and refuses to migrate the entry whose amount is
/// [`SENTINEL`].
///
/// A failure in the middle of a range is the interesting case: the entries before it have
/// already been rewritten by the time the batch gives up, so whether they survive is
/// exactly the question.
pub struct FailsOnSentinel;

impl Migration for FailsOnSentinel {
    type Keys = ProbeKey;
    const FROM: u32 = 1;
    const TO: u32 = 2;

    fn up(env: &Env, key: &Val) -> Result<EntryOutcome, MigrationError> {
        let storage = env.storage().persistent();
        if !storage.has::<Val>(key) {
            return Ok(EntryOutcome::Skipped);
        }
        let current: ItemV2 = match storage.get::<Val, ItemV2>(key) {
            Some(e) => e,
            None => return Ok(EntryOutcome::Skipped),
        };
        if current.amount == SENTINEL {
            return Err(MigrationError::OrphanedEntries);
        }
        if current.frozen.is_some() {
            return Ok(EntryOutcome::AlreadyCurrent);
        }
        storage.set::<Val, ItemV2>(
            key,
            &ItemV2 {
                amount: current.amount,
                frozen: Some(false),
            },
        );
        Ok(EntryOutcome::Migrated)
    }

    fn name() -> &'static str {
        "fails on the sentinel entry"
    }
}

#[contract]
pub struct Probe;

#[contractimpl]
impl Probe {
    /// Writes `count` version-1 entries and registers their keys under version 1.
    pub fn seed(env: Env, count: u32) {
        version::initialize::<ProbeKey>(&env, 1).unwrap();
        let mut n = 0u32;
        while n < count {
            let key = item_key(&env, n);
            env.storage()
                .persistent()
                .set::<Val, ItemV1>(&key, &ItemV1 { amount: n });
            index::register::<ProbeKey>(&env, 1, &key);
            n += 1;
        }
    }

    /// Starts the migration and applies one batch, in one invocation.
    pub fn migrate_batch(env: Env, limit: u32) -> Result<BatchOutcome, MigrationError> {
        executor::begin::<FailsOnSentinel>(&env)?;
        executor::batch::<FailsOnSentinel>(&env, limit)
    }

    /// The migration bookkeeping, if any survived.
    pub fn cursor(env: Env) -> Option<MigrationState> {
        executor::status::<FailsOnSentinel>(&env)
    }

    /// How many of the first `count` entries carry the new shape.
    pub fn migrated_count(env: Env, count: u32) -> u32 {
        let mut done = 0u32;
        let mut n = 0u32;
        while n < count {
            let key = item_key(&env, n);
            let entry: Option<ItemV2> = env.storage().persistent().get(&key);
            if entry.is_some_and(|e| e.frozen.is_some()) {
                done += 1;
            }
            n += 1;
        }
        done
    }

    /// Rewrites every entry in one invocation, the way a framework without batching
    /// would have to.
    ///
    /// Exists to be measured against Mainnet's caps, and to fail there. It is not part of
    /// the framework: it is the counter-example the framework exists to avoid.
    pub fn naive_migrate_all(env: Env, count: u32) -> Result<u32, MigrationError> {
        let mut n = 0u32;
        while n < count {
            let key = item_key(&env, n);
            let mut entry: ItemV2 = env
                .storage()
                .persistent()
                .get(&key)
                .ok_or(MigrationError::CorruptIndex)?;
            entry.frozen = Some(false);
            env.storage().persistent().set::<Val, ItemV2>(&key, &entry);
            n += 1;
        }
        Ok(count)
    }
}

/// A registered probe with `count` version-1 entries, and limits relaxed while they were
/// written.
///
/// Building a thousand-key keyspace is not something a single transaction has to do, so
/// the limits are only enforced by the tests that measure against them.
struct Fixture {
    env: Env,
    id: Address,
}

impl Fixture {
    fn new(count: u32) -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.cost_estimate().disable_resource_limits();
        let id = env.register(Probe, ());
        let fixture = Self { env, id };
        fixture.client().seed(&count);
        fixture
    }

    fn client(&self) -> ProbeClient<'_> {
        ProbeClient::new(&self.env, &self.id)
    }

    /// Overwrites entry `index` with the amount the migration refuses, so that a batch
    /// reaching it fails only *after* rewriting the entries before it.
    fn poison_entry(&self, index: u32) {
        self.env.as_contract(&self.id, || {
            let key = item_key(&self.env, index);
            self.env
                .storage()
                .persistent()
                .set::<Val, ItemV1>(&key, &ItemV1 { amount: SENTINEL });
        });
    }

    /// Holds every subsequent invocation to the limits a real transaction is held to.
    fn enforce_mainnet_limits(&self) {
        self.env
            .cost_estimate()
            .enforce_resource_limits(InvocationResourceLimits::mainnet());
    }

    /// The contract's recorded schema version.
    fn schema_version(&self) -> Option<u32> {
        let env = &self.env;
        env.as_contract(&self.id, || version::read::<ProbeKey>(env))
    }
}

#[test]
fn a_batch_that_fails_partway_leaves_neither_entries_nor_cursor() {
    let fixture = Fixture::new(10);
    fixture.poison_entry(5);

    // The batch rewrites entries 0 through 4 and then fails on entry 5. On a real network
    // the whole invocation reverts; here the host enforces the same boundary, which is
    // what makes this test meaningful rather than a restatement of the code.
    let error = contract_error(fixture.client().try_migrate_batch(&10));
    assert_eq!(error, MigrationError::OrphanedEntries);

    assert_eq!(
        fixture.client().migrated_count(&10),
        0,
        "the entries the batch had already rewritten must have been reverted with it"
    );
    assert_eq!(
        fixture.client().cursor(),
        None,
        "and the cursor must not have advanced, or a retry would skip the range the \
         failed batch had covered"
    );
    assert_eq!(
        fixture.schema_version(),
        Some(1),
        "the version must not have moved either"
    );
}

#[test]
fn a_retry_after_a_failed_batch_starts_from_the_same_place() {
    let fixture = Fixture::new(10);
    fixture.poison_entry(5);

    // Two identical failures in a row. If the first had advanced the cursor, the second
    // would start further along and entry 5 — the one that fails — would be skipped
    // forever, leaving it in the old shape while the version moved on.
    assert_eq!(
        contract_error(fixture.client().try_migrate_batch(&10)),
        MigrationError::OrphanedEntries
    );
    assert_eq!(
        contract_error(fixture.client().try_migrate_batch(&10)),
        MigrationError::OrphanedEntries
    );
    assert_eq!(fixture.client().cursor(), None);
    assert_eq!(fixture.client().migrated_count(&10), 0);
}

#[test]
fn a_batch_that_stops_before_the_failure_commits_normally() {
    let fixture = Fixture::new(10);
    fixture.poison_entry(5);

    // Five entries, none of them the sentinel: the batch succeeds and checkpoints.
    let outcome = fixture.client().migrate_batch(&5);
    assert_eq!(outcome.visited, 5);
    assert_eq!(outcome.migrated, 5);
    assert!(!outcome.done);
    assert_eq!(fixture.client().migrated_count(&5), 5);

    // The cursor is committed, which is what makes the next batch resumable.
    assert_eq!(fixture.client().cursor().unwrap().cursor, 5);

    // And the next batch is the one that fails, proving the boundary is exactly where
    // the framework says it is.
    assert_eq!(
        contract_error(fixture.client().try_migrate_batch(&5)),
        MigrationError::OrphanedEntries
    );
    assert_eq!(
        fixture.client().cursor().unwrap().cursor,
        5,
        "the entries before the sentinel are still migrated and the cursor has not moved"
    );
    assert_eq!(fixture.client().migrated_count(&10), 5);
}

#[test]
fn a_migration_over_a_thousand_entries_cannot_run_unbatched() {
    // The counter-example, *measured* rather than enforced. Enforcing Mainnet's limits
    // here would abort the host with a panic that no test can catch — a panic is not a
    // contract error, so it never reaches the client — which would leave the point
    // untestable. The invocation instead runs with the limits relaxed and its footprint is
    // compared against them afterwards.
    //
    // Asserting on the *entry counts* rather than on memory also keeps this test
    // deterministic: memory metering on the host is an estimate that the SDK warns tends
    // to undershoot, while "this wrote a thousand entries" is not.
    let fixture = Fixture::new(1_000);

    let written = fixture.client().naive_migrate_all(&1_000);
    assert_eq!(written, 1_000);

    let used = fixture.env.cost_estimate().resources();
    let limits = InvocationResourceLimits::mainnet();
    let footprint = used
        .disk_read_entries
        .saturating_add(used.memory_read_entries)
        .saturating_add(used.write_entries);
    println!(
        "an unbatched 1000-key migration uses {written} writes (cap {}) and {footprint} \
         footprint entries (cap {}), so it cannot be submitted as one transaction",
        limits.write_entries, limits.ledger_entries,
    );
    assert!(
        used.write_entries > limits.write_entries,
        "an unbatched thousand-entry migration must exceed Mainnet's write cap; if this \
         passed, either the cap has changed or the migration is not doing the work it \
         claims (measured {} writes against a cap of {})",
        used.write_entries,
        limits.write_entries
    );
    assert!(
        footprint > limits.ledger_entries,
        "and it must exceed the total footprint cap too"
    );
}

#[test]
fn the_same_migration_batched_fits_comfortably() {
    let fixture = Fixture::new(1_000);
    fixture.enforce_mainnet_limits();

    // `begin` plus a full-size batch, in one invocation, because that is one transaction.
    let outcome = fixture
        .client()
        .migrate_batch(&soroban_migrate::MAX_KEYS_PER_BATCH);
    assert_eq!(outcome.visited, soroban_migrate::MAX_KEYS_PER_BATCH);
    assert_eq!(outcome.migrated, soroban_migrate::MAX_KEYS_PER_BATCH);
    assert!(!outcome.done, "a thousand entries do not fit in one batch");

    println!(
        "a batch of {} keys over a 1000-key contract fits Mainnet's caps; \
         the migration needs {} transactions",
        soroban_migrate::MAX_KEYS_PER_BATCH,
        1_000u32.div_ceil(soroban_migrate::MAX_KEYS_PER_BATCH)
    );
}

#[test]
fn a_batch_larger_than_the_budget_is_refused_before_anything_is_written() {
    let fixture = Fixture::new(10);
    fixture.enforce_mainnet_limits();

    // Refused by the framework, not by the network. The difference matters: a batch that
    // is refused costs nothing and names the problem, while a batch that exceeds a cap on
    // chain consumes its fee and reverts, leaving the operator to work out why.
    assert_eq!(
        contract_error(
            fixture
                .client()
                .try_migrate_batch(&(soroban_migrate::MAX_KEYS_PER_BATCH + 1))
        ),
        MigrationError::BatchTooLarge
    );
    // `begin` ran in the same invocation and registered the migration; its write went
    // down with the refusal, which is the property the whole executor is built on.
    assert_eq!(fixture.client().cursor(), None);
    assert_eq!(fixture.schema_version(), Some(1));
}
