//! `soroban-migrate`: versioned storage-schema migrations for Soroban contracts.
//!
//! # The two halves
//!
//! The off-chain half — `init`, `schema`, `check`, `plan`, `generate` — works on
//! source, schema snapshots, and plan files. It needs no network, which is what lets
//! it run in CI on every pull request, and it is where every decision about an
//! upgrade is made and recorded.
//!
//! The chain-facing half — `status`, `run`, `dry-run` — executes those decisions
//! against a deployed contract. It never holds a key: signing and submission are
//! delegated to the Stellar CLI, and `dry-run` replays against an in-memory fork of a
//! ledger snapshot so that nothing at all is at stake.
//!
//! # Exit codes
//!
//! `0` succeeded, `1` the repository or the invocation is wrong, `2` the tool ran and
//! refused. CI needs the last two apart: one is fixed by a person editing the
//! repository, the other by a person deciding not to ship the change.

#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

mod batch;
mod commands;
mod config;
mod error;
mod output;
mod project;
mod stellar;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use error::Result;
use project::Project;

/// The tool's top-level command line.
#[derive(Debug, Parser)]
#[command(
    name = "soroban-migrate",
    version,
    about = "Versioned storage-schema migrations for Soroban contracts",
    long_about = "Versioned storage-schema migrations for Soroban contracts.\n\n\
                  A contract's Wasm can be replaced; the state underneath does not migrate \
                  itself. This tool records what shape each version's entries are in, refuses \
                  upgrades that would leave live entries unreadable or silently lose data, \
                  generates the code that moves them, and drives that code in batches that fit \
                  a transaction.",
    // Without this, clap derives the binary's version from the crate, which for a
    // workspace member is the workspace version. Stated explicitly so `--version` and
    // the docs cannot disagree.
    disable_help_subcommand = true
)]
struct Cli {
    /// The directory containing `soroban-migrate.toml`. Defaults to the current
    /// directory, then its ancestors.
    #[arg(long, global = true, value_name = "DIR", default_value = ".")]
    root: PathBuf,

    /// What to do.
    #[command(subcommand)]
    command: Command,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Adopt a contract: write a configuration file and snapshot the current shapes.
    Init(commands::init::Args),

    /// Snapshot, list, and inspect storage shapes.
    Schema(commands::schema::Args),

    /// Check that every version pair is safe to upgrade, and that the snapshots are
    /// up to date. Exits 2 when a change would lose data or make entries unreadable.
    Check(commands::check::Args),

    /// Draft the declarations a version pair's diff demands.
    Plan(commands::plan::Args),

    /// Write the migration implementation for a version pair.
    Generate(commands::generate::Args),

    /// Report a deployed contract's schema version and migration progress.
    Status(commands::status::Args),

    /// Drive a migration's batches. Simulates unless `--submit` is passed.
    Run(commands::run::Args),

    /// Replay a whole migration against a forked ledger snapshot and report what each
    /// batch would cost.
    DryRun(commands::dry_run::Args),
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    error::run(|| dispatch(&cli))
}

fn dispatch(cli: &Cli) -> Result<()> {
    match &cli.command {
        // `init` is the only command that does not require a configuration to exist,
        // for the obvious reason.
        Command::Init(args) => commands::init::run(&cli.root, args),
        Command::Schema(args) => commands::schema::run(&project(&cli.root)?, args),
        Command::Check(args) => commands::check::run(&project(&cli.root)?, args),
        Command::Plan(args) => commands::plan::run(&project(&cli.root)?, args),
        Command::Generate(args) => commands::generate::run(&project(&cli.root)?, args),
        Command::Status(args) => commands::status::run(&project(&cli.root)?, args),
        Command::Run(args) => commands::run::run(&project(&cli.root)?, args),
        Command::DryRun(args) => commands::dry_run::run(&project(&cli.root)?, args),
    }
}

fn project(root: &std::path::Path) -> Result<Project> {
    Project::discover(root)
}
