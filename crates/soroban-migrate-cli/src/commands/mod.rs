//! One module per subcommand.
//!
//! The split is by operator intent rather than by layer. `check` answers "may this
//! ship", `generate` answers "write the code", `run` answers "do it", and `status`
//! answers "where did it get to". Each module owns its whole command, including its
//! reporting, because the report *is* part of the command: an operator reads
//! `check`'s output to decide whether to merge, so the output is a deliverable
//! rather than a debugging aid.

pub mod check;
pub mod dry_run;
pub mod generate;
pub mod init;
pub mod plan;
pub mod run;
pub mod schema;
pub mod status;
