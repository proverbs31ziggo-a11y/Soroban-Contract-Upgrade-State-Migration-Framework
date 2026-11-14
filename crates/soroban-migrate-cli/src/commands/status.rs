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

use std::fmt::Write as _;

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
    let stellar = Stellar::discover(network.stellar_timeout())?;

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
    print!("{}", format_status(&status, version, args.limit));
    Ok(())
}

/// Renders the migration status lines for an active or finished migration.
fn format_status(status: &MigrationStatusView, version: Option<u32>, limit: u32) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
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
    let _ = writeln!(
        out,
        "  progress:         {done}/{} ({:.2}%)",
        status.total,
        f64::from(done) * 100.0 / f64::from(status.total.max(1))
    );

    if !status.active {
        let _ = writeln!(
            out,
            "\n  The migration is finished. Call `confirm_migration` on the contract to discard \
             its bookkeeping."
        );
        return out;
    }

    let remaining = status.remaining();
    let txs = transactions_needed(remaining, limit);
    let _ = writeln!(
        out,
        "  remaining:        {remaining} keys, about {txs} transaction{} at {limit} per batch",
        if txs == 1 { "" } else { "s" },
    );
    if remaining > 0 && version == Some(status.from) {
        let _ = writeln!(
            out,
            "\n  The contract still reports v{}, so it is safe to keep serving: un-migrated \
             entries decode through the new shape while the batches catch up.",
            status.from
        );
    }
    let _ = writeln!(
        out,
        "\n  A batch of {limit} is {}. Run `soroban-migrate run` to see what the next one would \
         cost, or `soroban-migrate run --submit` to apply it.",
        percent_of(
            u64::from(limit),
            u64::from(soroban_migrate::MAX_KEYS_PER_BATCH)
        )
    );
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_migration_does_not_suggest_another_batch() {
        let status = MigrationStatusView {
            from: 1,
            to: 2,
            cursor: 6,
            total: 6,
            active: false,
        };
        let rendered = format_status(&status, Some(2), 100);
        assert!(rendered.contains("v1 -> v2, finished"), "got:\n{rendered}");
        assert!(rendered.contains("6/6 (100.00%)"), "got:\n{rendered}");
        assert!(
            rendered.contains("confirm_migration"),
            "finished status should mention confirm_migration:\n{rendered}"
        );
        assert!(
            !rendered.contains("remaining:"),
            "finished status must not report remaining keys:\n{rendered}"
        );
        assert!(
            !rendered.contains("run --submit"),
            "finished status must not suggest running another batch:\n{rendered}"
        );
    }

    #[test]
    fn active_migration_reports_remaining_keys_and_batch_hint() {
        let status = MigrationStatusView {
            from: 1,
            to: 2,
            cursor: 2,
            total: 6,
            active: true,
        };
        let rendered = format_status(&status, Some(1), 100);
        assert!(rendered.contains("v1 -> v2, in flight"), "got:\n{rendered}");
        assert!(
            rendered.contains("remaining:        4 keys, about 1 transaction at 100 per batch"),
            "got:\n{rendered}"
        );
        assert!(rendered.contains("run --submit"), "got:\n{rendered}");
    }
}
