//! Errors, and the exit codes they map to.
//!
//! # Why exit codes are part of the interface
//!
//! `soroban-migrate check` is meant to run in CI, and CI has to distinguish three
//! outcomes that a single non-zero exit code would collapse: the check ran and found
//! a problem an operator must act on; the check could not run because the project is
//! misconfigured; and the tool was invoked wrongly. The first means "block the
//! merge", the second means "fix the repository", and they have different owners. So
//! they are different codes, and every command here maps its failures onto them
//! deliberately rather than letting a generic error handler pick one.

use std::fmt::Display;
use std::process::ExitCode;

use soroban_migrate_schema::SchemaError;
use thiserror::Error;

/// Everything ran, and the answer was "fine".
pub const EXIT_OK: u8 = 0;

/// The invocation or the project could not be understood: a missing file, a bad
/// flag, malformed source. An operator fixes the repository, not the contract.
pub const EXIT_USAGE: u8 = 1;

/// The command ran to completion and the answer is "do not do this". A denied
/// compatibility check, an unreachable network, a batch that would exceed a cap.
pub const EXIT_REFUSED: u8 = 2;

/// Anything a CLI command can fail with.
#[derive(Debug, Error)]
pub enum CliError {
    /// The project is missing something the command needs.
    #[error("{0}")]
    Missing(String),

    /// A flag or argument combination is invalid, or a value could not be parsed.
    #[error("{0}")]
    Usage(String),

    /// A file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The path involved.
        path: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// The schema layer refused: a parse error, a version conflict, a stale plan
    /// directive. Carries its own exit code, so it is mapped rather than guessed at.
    #[error("{0}")]
    Schema(#[from] SchemaError),

    /// A check found a change that must not ship.
    #[error("{0}")]
    Refused(String),

    /// The network, or the tool used to reach it, failed.
    #[error("{0}")]
    Network(String),
}

impl CliError {
    /// Wraps an IO failure with the path that produced it.
    pub fn io(path: impl Display, source: std::io::Error) -> Self {
        CliError::Io {
            path: path.to_string(),
            source,
        }
    }

    /// The process exit code for this failure.
    pub fn exit_code(&self) -> u8 {
        match self {
            CliError::Missing(_) | CliError::Usage(_) | CliError::Io { .. } => EXIT_USAGE,
            CliError::Schema(e) => match e.exit_code() {
                1 => EXIT_USAGE,
                _ => EXIT_REFUSED,
            },
            CliError::Refused(_) | CliError::Network(_) => EXIT_REFUSED,
        }
    }
}

/// A command's result.
pub type Result<T> = std::result::Result<T, CliError>;

/// Runs a command, printing any failure and returning the matching exit code.
///
/// The failure is printed as a single sentence on stderr with no backtrace: an
/// operator running `check` in CI wants the reason, and a backtrace buries it.
pub fn run(command: impl FnOnce() -> Result<()>) -> ExitCode {
    match command() {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
