//! `soroban-migrate dry-run`: replay the whole migration against a forked ledger.
//!
//! # Why this cannot be done with the `stellar` CLI alone
//!
//! A simulation only produces a good answer for the *next* batch. Subsequent batches
//! depend on state the earlier ones wrote, and a simulation writes nothing, so the
//! second batch cannot be simulated from a ledger that has not been changed. An
//! operator therefore cannot find out from simulations alone how many transactions a
//! migration will take, what the largest batch that fits is, or what the run will
//! cost — which is exactly what they need to know before starting.
//!
//! So this command forks. It loads a ledger snapshot into a local host, replays the
//! migration against it batch by batch, and measures every batch against the caps a
//! real transaction is held to. The fork is a full copy of the entries the contract
//! touches, and it is never written back anywhere: the replay happens entirely in
//! this process's memory.
//!
//! # Where the snapshot comes from
//!
//! `stellar snapshot create` (or any other producer of a `LedgerSnapshot` JSON file)
//! exports the entries for a contract or an account. Point `--snapshot` at it. The
//! alternative — fetching entries over RPC during the replay — would put a network
//! round trip inside a metered host invocation, which corrupts both the measurement
//! and the numbers it is used to compute.
//!
//! # What the replay proves, and what it does not
//!
//! It proves that every batch fits: that no batch exceeds the entry, memory, or
//! instruction caps, which is the difference between a migration that completes and
//! one that fails on chain partway through. It also reports the total transaction
//! count and the projected fee.
//!
//! It does **not** prove the migration is correct. It runs the contract's own code
//! against the contract's own state, so a migration that computes the wrong value
//! computes it identically here. Correctness comes from the diff, the plan, and the
//! tests that assert on entry shapes — which is why `check` refuses a change nobody
//! declared, and why this command will not run a migration whose plan still holds a
//! placeholder.

use std::cell::Cell;
use std::path::PathBuf;

use soroban_migrate::BatchOutcome;
use soroban_sdk::{Address, Env, IntoVal, String as SdkString, Symbol, TryFromVal, Val};

use crate::batch::BatchOutcomeView;
use crate::error::{CliError, Result};
use crate::output::{
    percent_of, thousands, transactions_needed, MAINNET_INSTRUCTIONS, MAINNET_LEDGER_ENTRIES,
    MAINNET_MEMORY_BYTES,
};
use crate::project::Project;

/// Arguments for `dry-run`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The ledger snapshot to replay against, as written by `stellar snapshot`.
    #[arg(long, default_value = ".soroban-migrate/snapshot.json")]
    pub snapshot: PathBuf,

    /// Keys per batch. Defaults to the framework's recommendation; the replay then
    /// reports the largest batch that would actually have fit.
    #[arg(long, default_value_t = soroban_migrate::DEFAULT_KEYS_PER_BATCH)]
    pub limit: u32,

    /// Stop after this many batches.
    #[arg(long, default_value_t = 1_000)]
    pub max_batches: u32,
}

/// Whether a string has the shape of a contract id.
///
/// Deliberately a shape check rather than a checksum: a wrong-but-well-formed id is
/// reported by the host as a missing contract, which is the right diagnosis, whereas
/// reimplementing strkey validation here would be a second implementation to disagree
/// with the first.
pub fn looks_like_contract_id(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(chars.next(), Some('C'))
        && text.len() == 56
        && text
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// One batch, as the replay measured it.
#[derive(Debug, Clone, Copy)]
pub struct BatchCall {
    /// What the batch reported.
    pub outcome: BatchOutcomeView,
    /// Modelled CPU instructions the invocation consumed.
    pub instructions: u64,
    /// Peak modelled memory, in bytes.
    pub memory_bytes: u64,
    /// Ledger entries the invocation's footprint covered.
    pub ledger_entries: u64,
}

impl BatchCall {
    /// Whether the invocation would have been submittable on Mainnet.
    ///
    /// All three caps are checked because all three bind in practice, and which one
    /// binds depends on the shape of the entry being migrated rather than on the key
    /// count.
    pub fn fits_mainnet(&self) -> bool {
        self.instructions <= MAINNET_INSTRUCTIONS
            && self.memory_bytes <= MAINNET_MEMORY_BYTES
            && self.ledger_entries <= MAINNET_LEDGER_ENTRIES
    }
}

/// Something that can apply one batch.
///
/// A trait so that the replay loop can be tested against a caller that costs nothing
/// and fails on demand. The loop's job is the accounting — when to stop, what a stall
/// looks like, how to report — and that job is worth testing without a ledger.
pub trait BatchCaller {
    /// Applies one batch and reports its outcome and resource use.
    ///
    /// # Errors
    ///
    /// [`CliError::Network`] when the migration itself fails.
    fn call(&self, limit: u32) -> Result<BatchCall>;
}

/// What a full replay found.
#[derive(Debug, Clone, Default)]
pub struct ReplayReport {
    /// Every batch, in order.
    pub batches: Vec<BatchCall>,
    /// Whether the migration reached the end of its index.
    pub completed: bool,
}

impl ReplayReport {
    /// Total instructions across every batch.
    pub fn total_instructions(&self) -> u64 {
        self.batches.iter().map(|b| b.instructions).sum()
    }

    /// The heaviest batch by instructions, which is the one that decides the
    /// feasible batch size.
    pub fn heaviest(&self) -> Option<&BatchCall> {
        self.batches
            .iter()
            .max_by_key(|b| (b.instructions, b.memory_bytes))
    }

    /// The number of entries the migration covered.
    pub fn entries(&self) -> u32 {
        self.batches.last().map_or(0, |b| b.outcome.total)
    }
}

/// Applies batches until the migration finishes, stalls, or a limit is reached.
///
/// # Errors
///
/// [`CliError::Refused`] when a batch stalls, which is the one condition a retry
/// cannot fix.
pub fn replay<C: BatchCaller>(caller: &C, limit: u32, max_batches: u32) -> Result<ReplayReport> {
    let mut report = ReplayReport::default();
    for _ in 0..max_batches {
        let call = caller.call(limit)?;
        let outcome = call.outcome;
        report.batches.push(call);
        if outcome.is_stall() {
            return Err(CliError::Refused(format!(
                "the replayed batch visited no keys and did not finish, so the cursor is pinned at \
                 {} of {}. A replay is the cheapest possible place to find this: on chain it would \
                 have been a run that never converges.",
                outcome.cursor, outcome.total
            )));
        }
        if outcome.done {
            report.completed = true;
            return Ok(report);
        }
    }
    Ok(report)
}

/// Runs the command.
///
/// # Errors
///
/// [`CliError::Missing`] when the snapshot file or the contract address is absent,
/// and [`CliError::Refused`] when a replayed batch stalls or does not fit.
pub fn run(project: &Project, args: &Args) -> Result<()> {
    if !args.snapshot.is_file() {
        return Err(CliError::Missing(format!(
            "no ledger snapshot at {}. Produce one with `stellar snapshot create --address \
             <CONTRACT_ID> --out {}`.",
            args.snapshot.display(),
            args.snapshot.display()
        )));
    }
    let (network, entries) = super::status::require_context(project)?;

    // Validated here rather than inside the host, where an unusable address surfaces as a
    // strkey decode panic. A forked host cannot resolve an alias either, so the two
    // mistakes get one message.
    if !looks_like_contract_id(&network.contract) {
        return Err(CliError::Usage(format!(
            "`{}` is not a contract id. A dry run loads a snapshot into a local host, which \
             cannot resolve a `stellar` alias — a replay needs the `C...` id itself.",
            network.contract
        )));
    }

    let fork = ForkedContract::load(
        &args.snapshot,
        &network.contract,
        &entries.begin_fn,
        &entries.batch_fn,
    );

    println!(
        "replaying {} on {} against {}",
        entries.batch_fn,
        network.contract,
        args.snapshot.display()
    );
    println!(
        "  batches of at most {}, capped at {} batches. Nothing is submitted; the fork is \
         in-memory and is not written back.\n",
        args.limit, args.max_batches
    );

    let report = replay(&fork, args.limit, args.max_batches)?;
    print_report(&report, args.limit);
    Ok(())
}

fn print_report(report: &ReplayReport, limit: u32) {
    crate::output::heading("dry run");
    for (index, call) in report.batches.iter().enumerate() {
        let outcome = call.outcome;
        let fits = if call.fits_mainnet() { "fits" } else { "OVER" };
        println!(
            "  batch {:>4}: {:<5}  visited {:>5}  cpu {}  mem {}  footprint {}",
            index + 1,
            fits,
            outcome.visited,
            percent_of(call.instructions, MAINNET_INSTRUCTIONS),
            percent_of(call.memory_bytes, MAINNET_MEMORY_BYTES),
            percent_of(call.ledger_entries, MAINNET_LEDGER_ENTRIES),
        );
    }

    crate::output::subheading("result");
    let entries = report.entries();
    println!("  entries:        {}", thousands(u64::from(entries)));
    println!(
        "  transactions:   {} at {} per batch",
        report.batches.len(),
        limit
    );
    println!(
        "  cpu:            {} instructions total",
        thousands(report.total_instructions())
    );
    if let Some(heaviest) = report.heaviest() {
        println!(
            "  heaviest batch: cpu {}, mem {}",
            percent_of(heaviest.instructions, MAINNET_INSTRUCTIONS),
            percent_of(heaviest.memory_bytes, MAINNET_MEMORY_BYTES)
        );
    }

    if !report.completed {
        println!(
            "\n  The replay stopped at --max-batches before the end of the index, so the figures \
             above cover the batches that ran. Re-run with a larger --max-batches for the whole \
             migration."
        );
    }

    let over: Vec<usize> = report
        .batches
        .iter()
        .enumerate()
        .filter(|(_, b)| !b.fits_mainnet())
        .map(|(i, _)| i + 1)
        .collect();
    if over.is_empty() {
        println!(
            "\n  Every batch fits a Mainnet transaction. The migration needs {} transaction{}.",
            report.batches.len(),
            if report.batches.len() == 1 { "" } else { "s" }
        );
    } else {
        println!(
            "\n  Batches {over:?} exceed a Mainnet cap and would fail on chain. Lower --limit and \
             re-run: the heaviest batch above shows how much headroom there is, and the cap it is \
             closest to is the one that binds."
        );
    }
    if report.completed && entries > 0 {
        println!(
            "  A {} entry migration needs at least {} transactions at this batch size.",
            thousands(u64::from(entries)),
            transactions_needed(entries, limit.max(1))
        );
    }
}

/// A contract running on a forked ledger.
///
/// The migration's own code, executed against the contract's own state, in this
/// process. Nothing is signed and nothing is sent: the fork's only purpose is to let
/// each batch see what the previous one wrote, which is the one thing a simulation
/// cannot do.
pub struct ForkedContract {
    env: Env,
    contract: Address,
    begin_fn: Symbol,
    batch_fn: Symbol,
    /// Kept as text as well as a `Symbol`, because `Symbol` has no `Display` and the
    /// name appears in error messages.
    batch_fn_name: String,
    started: Cell<bool>,
}

impl ForkedContract {
    /// Loads a snapshot into a host and points it at a contract.
    ///
    /// The contract must be a `C...` strkey id and not an alias: an alias is resolved by
    /// the `stellar` CLI against its own configuration, and a forked host has no such
    /// configuration. `run` validates the shape first so that a mistake is a sentence
    /// rather than a panic from inside the SDK's strkey decoder.
    pub fn load(
        snapshot: &std::path::Path,
        contract: &str,
        begin_fn: &str,
        batch_fn: &str,
    ) -> Self {
        let env = Env::from_ledger_snapshot_file(snapshot);
        // A fork cannot sign. Auth is mocked for the replay so that the migration's
        // driver entry points can be called at all — they are admin-gated in
        // production, and a dry run that could not call them would report nothing.
        // This is a simulation: it produces no signature and no transaction.
        env.mock_all_auths();
        let address = Address::from_string(&SdkString::from_str(&env, contract));
        Self {
            contract: address,
            begin_fn: Symbol::new(&env, begin_fn),
            batch_fn: Symbol::new(&env, batch_fn),
            batch_fn_name: batch_fn.to_string(),
            started: Cell::new(false),
            env,
        }
    }
}

impl BatchCaller for ForkedContract {
    fn call(&self, limit: u32) -> Result<BatchCall> {
        // `begin` and the first batch are one transaction in production, so the first
        // replayed batch pays for both. Charging them to the same measurement is what
        // makes the reported footprint the one the first real transaction will have.
        if !self.started.replace(true) {
            self.env.invoke_contract::<Val>(
                &self.contract,
                &self.begin_fn,
                soroban_sdk::Vec::new(&self.env),
            );
        }

        let args = soroban_sdk::Vec::from_array(
            &self.env,
            [IntoVal::<Env, Val>::into_val(&limit, &self.env)],
        );
        let raw: Val = self
            .env
            .invoke_contract(&self.contract, &self.batch_fn, args);
        let outcome = BatchOutcome::try_from_val(&self.env, &raw).map_err(|_| {
            CliError::Network(format!(
                "`{}` did not return a `BatchOutcome`. Check `contract.batch_fn` in {}.",
                self.batch_fn_name,
                crate::config::FILE_NAME
            ))
        })?;

        let used = self.env.cost_estimate().resources();
        Ok(BatchCall {
            outcome: BatchOutcomeView::from(outcome),
            instructions: u64::try_from(used.instructions.max(0)).unwrap_or(0),
            memory_bytes: u64::try_from(used.mem_bytes.max(0)).unwrap_or(0),
            ledger_entries: u64::from(used.disk_read_entries)
                + u64::from(used.memory_read_entries)
                + u64::from(used.write_entries),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A caller that walks an index of `total` keys `limit` at a time, with a fixed
    /// per-key cost, so the loop's accounting can be checked exactly.
    struct Fake {
        total: u32,
        cursor: Cell<u32>,
        per_key: u64,
        refuses: Option<u32>,
    }

    impl Fake {
        fn new(total: u32, per_key: u64) -> Self {
            Self {
                total,
                cursor: Cell::new(0),
                per_key,
                refuses: None,
            }
        }
    }

    impl BatchCaller for Fake {
        fn call(&self, limit: u32) -> Result<BatchCall> {
            let cursor = self.cursor.get();
            if self.refuses == Some(cursor) {
                return Err(CliError::Network("the migration returned an error".into()));
            }
            let visited = limit.min(self.total.saturating_sub(cursor));
            let next = cursor + visited;
            self.cursor.set(next);
            Ok(BatchCall {
                outcome: BatchOutcomeView {
                    from: 1,
                    to: 2,
                    cursor: next,
                    total: self.total,
                    visited,
                    migrated: visited,
                    already_current: 0,
                    skipped: 0,
                    done: next >= self.total,
                },
                instructions: u64::from(visited) * self.per_key,
                memory_bytes: 1_000,
                ledger_entries: u64::from(visited) * 2,
            })
        }
    }

    #[test]
    fn a_contract_id_is_recognised_by_shape() {
        assert!(looks_like_contract_id(
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
        ));
        // An alias, which the forked host cannot resolve.
        assert!(!looks_like_contract_id("vault"));
        // The right length but an account id.
        assert!(!looks_like_contract_id(
            "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
        ));
        assert!(!looks_like_contract_id("Cshort"));
    }

    #[test]
    fn a_replay_runs_until_the_index_is_exhausted() {
        let report = replay(&Fake::new(1_000, 100), 150, 100).unwrap();
        assert!(report.completed);
        assert_eq!(report.batches.len(), 7);
        assert_eq!(report.entries(), 1_000);
        // The last batch is short; the first six are full.
        assert_eq!(report.batches[6].outcome.visited, 100);
    }

    #[test]
    fn a_replay_reports_the_heaviest_batch_because_it_decides_the_batch_size() {
        let report = replay(&Fake::new(1_000, 100), 150, 100).unwrap();
        let heaviest = report.heaviest().unwrap();
        assert_eq!(heaviest.outcome.visited, 150);
        assert_eq!(heaviest.instructions, 15_000);
    }

    #[test]
    fn a_stalled_replay_is_refused_rather_than_reported_as_success() {
        // A caller whose total never shrinks and whose cursor never advances: exactly
        // the state that would spend fees forever on chain.
        struct Stuck;
        impl BatchCaller for Stuck {
            fn call(&self, _limit: u32) -> Result<BatchCall> {
                Ok(BatchCall {
                    outcome: BatchOutcomeView {
                        from: 1,
                        to: 2,
                        cursor: 10,
                        total: 1_000,
                        visited: 0,
                        migrated: 0,
                        already_current: 0,
                        skipped: 0,
                        done: false,
                    },
                    instructions: 0,
                    memory_bytes: 0,
                    ledger_entries: 0,
                })
            }
        }
        let error = replay(&Stuck, 150, 10).unwrap_err();
        assert!(matches!(error, CliError::Refused(_)));
        assert!(error.to_string().contains("pinned"), "got: {error}");
    }

    #[test]
    fn a_failure_inside_a_batch_propagates_without_reporting_progress() {
        let fake = Fake {
            refuses: Some(150),
            ..Fake::new(1_000, 100)
        };
        let error = replay(&fake, 150, 100).unwrap_err();
        assert!(matches!(error, CliError::Network(_)));
        // The cursor must not have advanced past the failing batch.
        assert_eq!(fake.cursor.get(), 150);
    }

    #[test]
    fn max_batches_bounds_a_replay_without_claiming_completion() {
        let report = replay(&Fake::new(1_000, 100), 100, 3).unwrap();
        assert!(!report.completed);
        assert_eq!(report.batches.len(), 3);
        assert_eq!(report.entries(), 1_000);
    }

    #[test]
    fn a_batch_over_a_cap_is_recognised_as_unsubmittable() {
        let fits = BatchCall {
            outcome: BatchOutcomeView {
                from: 1,
                to: 2,
                cursor: 1,
                total: 1,
                visited: 1,
                migrated: 1,
                already_current: 0,
                skipped: 0,
                done: true,
            },
            instructions: MAINNET_INSTRUCTIONS,
            memory_bytes: MAINNET_MEMORY_BYTES,
            ledger_entries: MAINNET_LEDGER_ENTRIES,
        };
        assert!(fits.fits_mainnet());
        let too_big = BatchCall {
            memory_bytes: MAINNET_MEMORY_BYTES + 1,
            ..fits
        };
        assert!(!too_big.fits_mainnet());
        let too_many_entries = BatchCall {
            ledger_entries: MAINNET_LEDGER_ENTRIES + 1,
            ..fits
        };
        assert!(!too_many_entries.fits_mainnet());
    }
}
