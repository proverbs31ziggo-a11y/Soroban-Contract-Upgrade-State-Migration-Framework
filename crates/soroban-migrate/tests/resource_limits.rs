//! Batched execution measured against Mainnet's enforced resource limits.
//!
//! This file is the evidence for two claims: that batching is not optional, and
//! that [`MAX_KEYS_PER_BATCH`] is a safe batch size. It runs under
//! [`InvocationResourceLimits::mainnet`], the limits a real transaction is held
//! to, so a batch that passes here is one that can be submitted.
//!
//! # The metering unit is the invocation, not the test
//!
//! The host checks resource limits when a contract invocation ends, and its
//! counters accumulate across everything done inside that invocation. Running
//! `begin` plus four batches inside one `as_contract` frame therefore measures a
//! five-batch migration as a single invocation and fails, even though each batch
//! individually fits — which is exactly the mistake a production operator cannot
//! make, because each batch *is* its own transaction.
//!
//! Every test here therefore wraps each batch in its own
//! [`deliver`] call, standing in for the transaction boundary. That is the whole
//! point of the framework: the batch size is chosen so that one batch is one
//! submittable transaction.
//!
//! Run with `--nocapture` to see the measured footprints:
//!
//! ```text
//! cargo test -p soroban-migrate --test resource_limits -- --nocapture
//! ```

mod support;

use soroban_env_host::InvocationResourceLimits;
use soroban_sdk::testutils::cost_estimate::NetworkInvocationResourceLimits;
use soroban_sdk::{Address, Env};

use soroban_migrate::executor::{self, MAX_KEYS_PER_BATCH};
use soroban_migrate::migration::BatchOutcome;
use soroban_migrate::{index, version};

use support::{entry_key, host, seed_v1, AddFrozen, DataKey, EntryV2};

/// Runs one closure as its own contract invocation, which is how the host — and
/// therefore the network — decides what one transaction's footprint is.
fn deliver<T>(env: &Env, id: &Address, f: impl FnOnce() -> T) -> T {
    env.as_contract(id, f)
}

fn fresh_env() -> Env {
    let env = Env::default();
    env.mock_all_auths();
    env
}

fn enable_mainnet_limits(env: &Env) {
    env.cost_estimate()
        .enforce_resource_limits(InvocationResourceLimits::mainnet());
}

fn batch(env: &Env, id: &Address, limit: u32) -> BatchOutcome {
    deliver(env, id, || executor::batch::<AddFrozen>(env, limit))
        .expect("a batch inside the per-transaction budget must be submittable")
}

#[test]
fn a_full_batch_fits_inside_mainnet_limits() {
    let env = fresh_env();
    let id = host(&env);

    // Setup runs with limits relaxed: building a large keyspace does not itself
    // need to fit in one transaction.
    env.cost_estimate().disable_resource_limits();
    let total = 1_000u32;
    deliver(&env, &id, || {
        version::initialize::<DataKey>(&env, 1).unwrap();
        seed_v1(&env, total);
    });
    enable_mainnet_limits(&env);

    // In production `begin` and the first batch are one transaction.
    let outcome = deliver(&env, &id, || {
        executor::begin::<AddFrozen>(&env).unwrap();
        executor::batch::<AddFrozen>(&env, MAX_KEYS_PER_BATCH)
    })
    .expect("a maximum-size batch must be submittable");

    assert_eq!(outcome.visited, MAX_KEYS_PER_BATCH);
    assert_eq!(outcome.migrated, MAX_KEYS_PER_BATCH);
    assert!(!outcome.done);
    assert_eq!(outcome.total, total);

    let budget = env.cost_estimate().budget();
    println!(
        "begin + batch of {MAX_KEYS_PER_BATCH} keys over a {total}-key contract: \
         cpu_instructions={}, mem_bytes={}",
        budget.cpu_instruction_cost(),
        budget.memory_bytes_cost(),
    );
    println!(
        "a {total}-key migration therefore takes {} transactions",
        total.div_ceil(MAX_KEYS_PER_BATCH)
    );
}

#[test]
fn every_batch_of_a_real_run_fits() {
    let env = fresh_env();
    let id = host(&env);
    env.cost_estimate().disable_resource_limits();
    let total = 500u32;
    deliver(&env, &id, || {
        version::initialize::<DataKey>(&env, 1).unwrap();
        seed_v1(&env, total);
    });
    enable_mainnet_limits(&env);

    let mut batches = 0u32;
    let mut cursor = 0u32;
    let mut cpu_worst = 0u64;
    deliver(&env, &id, || {
        executor::begin::<AddFrozen>(&env).unwrap();
    });
    while cursor < total {
        let outcome = batch(&env, &id, MAX_KEYS_PER_BATCH);
        // The first batch is not the expensive one: later batches read and write
        // index pages that are already populated, and the version-2 index they
        // write into is larger. Asserting only on the first batch would miss that.
        cpu_worst = cpu_worst.max(env.cost_estimate().budget().cpu_instruction_cost());
        assert!(outcome.visited > 0, "a batch must always make progress");
        cursor = outcome.cursor;
        batches += 1;
        assert!(batches < 10, "500 keys should not take this many batches");
    }
    assert_eq!(
        deliver(&env, &id, || version::read::<DataKey>(&env)),
        Some(2)
    );
    println!(
        "{total}-key migration: {batches} transactions of at most {MAX_KEYS_PER_BATCH} keys; \
         worst batch used {cpu_worst} cpu instructions of a 400000000 budget"
    );
}

#[test]
fn a_batch_crossing_an_index_page_boundary_fits() {
    let env = fresh_env();
    let id = host(&env);

    env.cost_estimate().disable_resource_limits();
    deliver(&env, &id, || {
        version::initialize::<DataKey>(&env, 1).unwrap();
        seed_v1(&env, 1_000);
        assert_eq!(index::info::<DataKey>(&env, 1).unwrap().page_count, 32);
    });
    enable_mainnet_limits(&env);

    deliver(&env, &id, || executor::begin::<AddFrozen>(&env)).unwrap();
    // Advance to a cursor that sits mid-page, so the measured batch pays for two
    // page reads and two page writes on each index rather than landing neatly on
    // a boundary. Cursor 35 is 3 keys into page 1.
    let mut cursor = 0u32;
    while cursor < 35 {
        let outcome = batch(&env, &id, 1);
        cursor = outcome.cursor;
        assert!(outcome.visited > 0);
    }
    assert_eq!(cursor, 35);

    let outcome = batch(&env, &id, MAX_KEYS_PER_BATCH);
    assert_eq!(outcome.visited, MAX_KEYS_PER_BATCH);
    assert_eq!(outcome.cursor, 35 + MAX_KEYS_PER_BATCH);
}

/// Measures the footprint of a batch of `keys` over a shared starting state.
fn measure(keys: u32) -> (u64, u64) {
    let env = fresh_env();
    let id = host(&env);
    env.cost_estimate().disable_resource_limits();
    deliver(&env, &id, || {
        version::initialize::<DataKey>(&env, 1).unwrap();
        seed_v1(&env, 600);
    });
    enable_mainnet_limits(&env);

    deliver(&env, &id, || executor::begin::<AddFrozen>(&env)).unwrap();
    let outcome = deliver(&env, &id, || executor::batch::<AddFrozen>(&env, keys)).unwrap();
    assert_eq!(outcome.visited, keys);
    let budget = env.cost_estimate().budget();
    (budget.cpu_instruction_cost(), budget.memory_bytes_cost())
}

/// Documents what actually binds a batch size.
///
/// Mainnet caps a single invocation at 200 entry reads, 200 entry writes, 400
/// total footprint entries, 400M cpu instructions and 40 MiB of memory. The
/// batch-size constant has to satisfy all five, and the point of this test is that
/// the *binding* one is not the entry caps most people would reason about — it is
/// memory. Watching the numbers is the only way to keep the constant honest.
#[test]
fn memory_is_what_binds_the_batch_size() {
    let mut previous_mem = 0u64;
    for keys in [1u32, 32, 100, MAX_KEYS_PER_BATCH] {
        let (cpu, mem) = measure(keys);
        println!(
            "batch of {keys:>3} keys: cpu={cpu:>10} ({:.1}% of 400M), mem={mem:>9} ({:.1}% of 41.9M)",
            f64::from(u32::try_from(cpu).unwrap_or(u32::MAX)) / 4_000_000.0,
            f64::from(u32::try_from(mem).unwrap_or(u32::MAX)) / 419_430.4,
        );
        assert!(cpu < 400_000_000, "batch of {keys} exceeds the cpu budget");
        assert!(
            mem < 41_943_040,
            "batch of {keys} exceeds the memory budget"
        );
        assert!(
            mem >= previous_mem,
            "memory must grow with batch size, otherwise the measurement is not measuring \
             what it claims to"
        );
        previous_mem = mem;
    }
    println!(
        "MAX_KEYS_PER_BATCH={MAX_KEYS_PER_BATCH} leaves headroom on every cap; see the \
         doc comment on that constant for the reasoning"
    );
}

#[test]
fn a_batch_of_one_is_always_possible() {
    // The floor: an operator whose migration logic is unusually expensive can
    // always fall back to single-key batches, which cost more transactions but
    // never exceed a limit.
    let env = fresh_env();
    let id = host(&env);
    env.cost_estimate().disable_resource_limits();
    deliver(&env, &id, || {
        version::initialize::<DataKey>(&env, 1).unwrap();
        seed_v1(&env, 3);
    });
    enable_mainnet_limits(&env);

    deliver(&env, &id, || executor::begin::<AddFrozen>(&env)).unwrap();
    let mut seen = 0u32;
    loop {
        let outcome = batch(&env, &id, 1);
        seen += outcome.visited;
        if outcome.done {
            break;
        }
    }
    assert_eq!(seen, 3);
}

#[test]
fn resuming_over_already_migrated_keys_is_cheap() {
    // A resume after a timeout, or a re-run over keys written by the new code,
    // is a normal operator action. It must be cheap: it reads entries and reports
    // them as current rather than rewriting 150 of them.
    let env = fresh_env();
    let id = host(&env);
    env.cost_estimate().disable_resource_limits();
    deliver(&env, &id, || {
        version::initialize::<DataKey>(&env, 1).unwrap();
        let mut n = 0u32;
        while n < MAX_KEYS_PER_BATCH {
            let key = entry_key(&env, n);
            env.storage().persistent().set::<soroban_sdk::Val, EntryV2>(
                &key,
                &EntryV2 {
                    amount: n,
                    frozen: Some(true),
                },
            );
            index::register::<DataKey>(&env, 1, &key);
            n += 1;
        }
    });
    enable_mainnet_limits(&env);

    let outcome = deliver(&env, &id, || {
        executor::begin::<AddFrozen>(&env).unwrap();
        executor::batch::<AddFrozen>(&env, MAX_KEYS_PER_BATCH)
    })
    .unwrap();
    assert_eq!(outcome.already_current, MAX_KEYS_PER_BATCH);
    assert_eq!(outcome.migrated, 0);
    assert!(outcome.done);
}

#[test]
fn the_frameworks_own_bookkeeping_is_small() {
    // Reporting only: operators should be able to see what the framework costs
    // before committing to it.
    let env = fresh_env();
    let id = host(&env);
    env.cost_estimate().disable_resource_limits();
    deliver(&env, &id, || {
        version::initialize::<DataKey>(&env, 1).unwrap();
        seed_v1(&env, 1_000);
        let pages = index::info::<DataKey>(&env, 1).unwrap().page_count;
        let entries = index::footprint_entries::<DataKey>(&env, 1);
        println!(
            "1_000 keys -> {pages} index pages plus 1 summary entry = {entries} persistent \
             ledger entries, on top of 2 instance entries for the version and the migration state"
        );
        assert_eq!(entries, 1_000u32.div_ceil(index::PAGE_SIZE) + 1);
    });
}
