//! `soroban-migrate status`: where a deployed contract is.
//!
//! # Why this reads the contract rather than a local record
//!
//! A migration is driven by an operator loop, and the state that matters — the cursor
//! — lives on chain precisely so that it survives an interrupted run, a crashed
//! laptop, or a different operator picking the work up. A local record would be a
//! second source of truth to disagree with the first, and it would be the one that is
//! wrong: the batch that advanced the cursor is the transaction that committed.
//!
//! The two calls are made with `--send=no`, so asking costs nothing and needs no
//! signature. Both are read-only, which is what makes `status` safe to run against
//! mainnet while a migration is in flight.

use crate::batch::MigrationStatusView;
use crate::config::{ContractCall, Network};
use crate::error::{CliError, Result};
use crate::output::{percent_of, transactions_needed};
use crate::project::Project;
use crate::stellar::{parse_optional_u32, Invocation, Stellar};

/// Arguments for `status`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Batch size to project the remaining work against. Defaults to the framework's
    /// recommendation.
    #[arg(long, default_value_t = soroban_migrate::DEFAULT_KEYS_PER_BATCH)]
    pub limit: u32,
}

/// Runs the command.
///
/// # Errors
///
/// [`CliError::Missing`] when the project has no `[network]` section or no `stellar`
/// binary is available, and [`CliError::Network`] when a call fails.
pub fn run(project: &Project, args: &Args) -> Result<()> {
    let network = require_network(project)?;
    let entries = &project.config.contract;
    let stellar = Stellar::discover()?;

    let version = parse_optional_u32(&call(&stellar, network, &entries.version_fn)?)?;
    let status_output = call(&stellar, network, &entries.status_fn)?;

    crate::output::heading(&format!("{} @ {}", network.contract, network.label()));
    match version {
        Some(version) => println!("  schema version:   v{version}"),
        None => println!(
            "  schema version:   none recorded. The contract does not use the framework, or its \
             constructor has not run `soroban_migrate::version::initialize`."
        ),
    }

    if crate::stellar::is_null(&status_output) {
        println!("  migration:        none in flight or recorded");
        return Ok(());
    }

    let status = MigrationStatusView::parse(&status_output)?;
    println!(
        "  migration:        v{} -> v{}, {}",
        status.from,
        status.to,
        if status.active {
            "in flight"
        } else {
            "finished"
        }
    );
    let done = status.cursor.min(status.total);
    println!(
        "  progress:         {done}/{} ({:.2}%)",
        status.total,
        f64::from(status.cursor.min(status.total)) * 100.0 / f64::from(status.total.max(1))
    );
    let remaining = status.remaining();
    println!(
        "  remaining:        {remaining} keys, about {} transaction{} at {} per batch",
        transactions_needed(remaining, args.limit),
        if transactions_needed(remaining, args.limit) == 1 {
            ""
        } else {
            "s"
        },
        args.limit
    );
    if remaining > 0 && version == Some(status.from) {
        println!(
            "\n  The contract still reports v{}, so it is safe to keep serving: un-migrated \
             entries decode through the new shape while the batches catch up.",
            status.from
        );
    }
    println!(
        "\n  A batch of {} is {}. Run `soroban-migrate run` to see what the next one would cost, \
         or `soroban-migrate run --submit` to apply it.",
        args.limit,
        percent_of(
            u64::from(args.limit),
            u64::from(soroban_migrate::MAX_KEYS_PER_BATCH)
        )
    );
    Ok(())
}

/// The project's network settings and contract entry points, or an error saying what
/// to add.
///
/// Bundled because every chain-facing command needs both, and reading one without the
/// other is not a coherent state: a network with no entry point to call, or an entry
/// point with no network to call it on.
pub(crate) fn require_context(project: &Project) -> Result<(&Network, &ContractCall)> {
    Ok((require_network(project)?, &project.config.contract))
}

/// The project's network settings, or an error saying what to add.
pub(crate) fn require_network(project: &Project) -> Result<&Network> {
    project.config.network.as_ref().ok_or_else(|| {
        CliError::Missing(format!(
            "no [network] section in {}. The commands that talk to a contract need one:\n\n\
             [network]\nrpc_url = \"https://soroban-testnet.stellar.org\"\ncontract = \"C...\"\n\
             source = \"alice\"\npassphrase = \"Test SDF Network ; September 2015\"\n",
            crate::config::FILE_NAME
        ))
    })
}

/// A read-only call to a contract entry point with no arguments.
pub(crate) fn call(stellar: &Stellar, network: &Network, function: &str) -> Result<String> {
    stellar.invoke(&Invocation {
        contract: network.contract.clone(),
        source: network.source.clone(),
        // Never send: a status query must not be able to change anything, even if the
        // configured function name turns out to be a write.
        send: false,
        cost: false,
        network: network.target()?,
        function: function.to_string(),
        args: Vec::new(),
    })
}
