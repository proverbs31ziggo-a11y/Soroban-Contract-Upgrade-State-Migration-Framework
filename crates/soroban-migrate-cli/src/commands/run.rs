//! `soroban-migrate run`: drive the batches.
//!
//! # The shape of a migration run
//!
//! A migration is a sequence of independent transactions, each of which is a batch.
//! The cursor lives on chain, so the driver holds no state: every iteration re-reads
//! what it is about to do, which is what makes the run resumable by a different
//! operator, on a different machine, after an arbitrary interruption. Nothing here is
//! bookkept locally, and that is deliberate — a local record would be a second source
//! of truth whose disagreement with the chain is not resolvable.
//!
//! # Why `--submit` is not the default
//!
//! Simulating is free and changes nothing; submitting is irreversible and costs XLM.
//! A tool whose default is "spend the operator's money" is a tool nobody should run
//! by accident, and `soroban-migrate run` typed from shell history is exactly the
//! accident to design against. The default therefore simulates the next batch and
//! prints what it would cost.
//!
//! # Why the loop can stop on its own
//!
//! Two conditions end a run without success, and both are reported as failures rather
//! than retried: a batch that visits nothing without finishing ([`BatchOutcomeView::is_stall`])
//! means the cursor is pinned, and a batch count that reaches `--max-batches` means
//! the migration is larger than the operator has authorised. Both would otherwise
//! spend XLM indefinitely.

use crate::batch::{BatchOutcomeView, MigrationStatusView};
use crate::commands::status::require_context;
use crate::config::{ContractCall, Network};
use crate::error::{CliError, Result};
use crate::output::transactions_needed;
use crate::project::Project;
use crate::stellar::{is_null, Invocation, Stellar};

/// Arguments for `run`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Keys to migrate per batch. The framework refuses more than its measured
    /// per-transaction budget, and so does this.
    #[arg(long, default_value_t = soroban_migrate::DEFAULT_KEYS_PER_BATCH)]
    pub limit: u32,

    /// Actually submit. Without this, the next batch is simulated and nothing is sent.
    #[arg(long)]
    pub submit: bool,

    /// Stop after this many batches.
    #[arg(long, default_value_t = 50)]
    pub max_batches: u32,

    /// Do not start a migration; require one to be in flight already.
    #[arg(long)]
    pub skip_begin: bool,
}

/// Runs the command.
///
/// # Errors
///
/// [`CliError::Usage`] for a batch size the executor would refuse, [`CliError::Refused`]
/// when a batch stalls or the migration has not finished within `--max-batches`, and
/// [`CliError::Network`] when a call fails.
pub fn run(project: &Project, args: &Args) -> Result<()> {
    if args.limit == 0 {
        return Err(CliError::Usage(
            "a batch must make progress, so --limit 0 is refused: an operator loop that advances \
             nothing spends fees forever."
                .into(),
        ));
    }
    if args.limit > soroban_migrate::MAX_KEYS_PER_BATCH {
        return Err(CliError::Usage(format!(
            "--limit {} exceeds the framework's per-transaction budget of {}. The executor would \
             refuse the batch; measuring the real contract with `soroban-migrate dry-run` gives \
             the largest batch that actually fits.",
            args.limit,
            soroban_migrate::MAX_KEYS_PER_BATCH
        )));
    }
    if args.max_batches == 0 {
        return Err(CliError::Usage("--max-batches 0 would run nothing".into()));
    }

    let (network, entries) = require_context(project)?;
    let stellar = Stellar::discover()?;

    if !args.submit {
        println!(
            "dry run: nothing will be submitted. Target: {} as {}, {}\n",
            network.contract,
            network.source,
            network.label()
        );
    }

    let status_output = crate::commands::status::call(&stellar, network, &entries.status_fn)?;
    let active = if is_null(&status_output) {
        false
    } else {
        MigrationStatusView::parse(&status_output)?.active
    };

    if !active {
        if args.skip_begin {
            return Err(CliError::Refused(
                "no migration is in flight and --skip-begin was passed, so there is nothing to \
                 advance. Drop --skip-begin to start one."
                    .into(),
            ));
        }
        begin(&stellar, network, entries, args.submit)?;
        if !args.submit {
            println!(
                "\nA simulated `begin` writes nothing, so the migration is not actually in flight \
                 and the first batch cannot be simulated after it. Either pass --submit to start \
                 it for real, or run `soroban-migrate dry-run` to replay the whole migration \
                 against a forked ledger, where nothing costs anything."
            );
            return Ok(());
        }
    }

    let mut batches = 0u32;
    loop {
        if batches >= args.max_batches {
            println!(
                "\nstopping after --max-batches {} without finishing. Re-run to continue: the \
                 cursor is on chain, so the next run resumes exactly here.",
                args.max_batches
            );
            return Ok(());
        }

        let view = batch(&stellar, network, entries, args.limit, args.submit)?;
        println!("  {}", view.summary());
        batches += 1;

        if view.is_stall() {
            return Err(CliError::Refused(format!(
                "the batch visited no keys and did not finish, so the cursor is pinned at {} of \
                 {}. Retrying cannot help: the index does not hold a key the cursor can reach. \
                 Check that the contract's key index is intact, and that `{}` is the entry point \
                 that advances this migration.",
                view.cursor, view.total, entries.batch_fn
            )));
        }
        if view.done {
            println!(
                "\nmigration complete: v{} -> v{}. The contract now reports v{}. Run \
                 `soroban-migrate status` to confirm, and confirm the migration on chain to \
                 discard its bookkeeping.",
                view.from, view.to, view.to
            );
            return Ok(());
        }
        if !args.submit {
            println!(
                "\none batch simulated. {} keys remain, which is about {} more transaction{} at {} \
                 per batch. State only moves when a batch is submitted, so a full dry run needs \
                 either --submit or a forked ledger: see `soroban-migrate dry-run`.",
                view.remaining(),
                transactions_needed(view.remaining(), args.limit),
                if transactions_needed(view.remaining(), args.limit) == 1 {
                    ""
                } else {
                    "s"
                },
                args.limit,
            );
            return Ok(());
        }
    }
}

fn begin(stellar: &Stellar, network: &Network, entries: &ContractCall, submit: bool) -> Result<()> {
    let output = stellar.invoke(&Invocation {
        contract: network.contract.clone(),
        source: network.source.clone(),
        send: submit,
        cost: false,
        network: network.target()?,
        function: entries.begin_fn.clone(),
        args: Vec::new(),
    })?;
    let state = MigrationStatusView::parse(&output)?;
    println!(
        "{} migration v{} -> v{}, {} of {} keys to visit",
        if submit { "began" } else { "would begin" },
        state.from,
        state.to,
        state.cursor,
        state.total,
    );
    Ok(())
}

fn batch(
    stellar: &Stellar,
    network: &Network,
    entries: &ContractCall,
    limit: u32,
    submit: bool,
) -> Result<BatchOutcomeView> {
    let output = stellar.invoke(&Invocation {
        contract: network.contract.clone(),
        source: network.source.clone(),
        send: submit,
        // `--cost` is the point of a dry run: the simulated footprint and fee are what
        // tell an operator whether the batch size is too ambitious.
        cost: !submit,
        network: network.target()?,
        function: entries.batch_fn.clone(),
        args: vec![(entries.batch_limit_arg.clone(), limit.to_string())],
    })?;
    BatchOutcomeView::parse(&output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        Args {
            limit: 100,
            submit: false,
            max_batches: 10,
            skip_begin: false,
        }
    }

    fn project_with_network() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::scaffold("vault", false);
        config.network = Some(crate::config::Network {
            name: Some("testnet".into()),
            rpc_url: None,
            contract: "CABC".into(),
            source: "alice".into(),
            passphrase: None,
            ledger: None,
        });
        config.save(dir.path()).unwrap();
        let project = Project::open(dir.path()).unwrap();
        (dir, project)
    }

    #[test]
    fn a_zero_batch_is_refused_because_it_spends_fees_forever() {
        let (_dir, project) = project_with_network();
        let error = run(&project, &Args { limit: 0, ..args() }).unwrap_err();
        assert!(matches!(error, CliError::Usage(_)));
    }

    #[test]
    fn a_batch_larger_than_the_frameworks_budget_is_refused_before_any_call() {
        let (_dir, project) = project_with_network();
        let error = run(
            &project,
            &Args {
                limit: soroban_migrate::MAX_KEYS_PER_BATCH + 1,
                ..args()
            },
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("dry-run"), "got: {message}");
    }

    #[test]
    fn zero_batches_is_refused() {
        let (_dir, project) = project_with_network();
        let error = run(
            &project,
            &Args {
                max_batches: 0,
                ..args()
            },
        )
        .unwrap_err();
        assert!(matches!(error, CliError::Usage(_)));
    }

    #[test]
    fn a_project_without_network_settings_says_which_section_to_add() {
        let dir = tempfile::tempdir().unwrap();
        crate::config::scaffold("vault", false)
            .save(dir.path())
            .unwrap();
        let project = Project::open(dir.path()).unwrap();
        let error = run(&project, &args()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("[network]"), "got: {message}");
        assert!(message.contains("source ="), "got: {message}");
    }
}
