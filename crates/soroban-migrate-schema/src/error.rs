//! Errors from parsing, diffing, and generating migration code.

use thiserror::Error;

/// Everything that can go wrong before a migration reaches a network.
#[derive(Debug, Error)]
pub enum SchemaError {
    /// A schema declaration could not be parsed out of Rust source.
    #[error("could not parse a storage schema in {file}: {message}")]
    Parse {
        /// The file being parsed.
        file: String,
        /// What went wrong.
        message: String,
    },

    /// A schema file was not valid JSON, or did not match the model.
    #[error("could not read schema {file}: {message}")]
    Read {
        /// The file being read.
        file: String,
        /// What went wrong.
        message: String,
    },

    /// No `#[storage_schema]` declaration was found where one was required.
    #[error("no storage schema found in {file}")]
    NoSchema {
        /// The file or directory that was searched.
        file: String,
    },

    /// Two schemas claim the same version but disagree.
    #[error(
        "two different schemas both claim version {version}; a schema file must not change \
         without its version changing, because both sides of an upgrade would then believe \
         they are migrating to the same shape"
    )]
    VersionConflict {
        /// The contested version.
        version: u32,
    },

    /// A migration plan disagrees with the schemas it is meant to explain.
    #[error("migration plan {file} does not match the schemas: {message}")]
    PlanMismatch {
        /// The plan file.
        file: String,
        /// What disagrees.
        message: String,
    },

    /// A plan declares treatment for a field that no schema change introduced.
    #[error(
        "migration plan declares `{directive}` for field `{field}`, but no change to that field \
         exists between v{from} and v{to}; a stale declaration hides the change it was written \
         for when that change is reintroduced"
    )]
    StaleDirective {
        /// The directive (`init`, `dropped`, `converted`, or `renamed`).
        directive: String,
        /// The field it names.
        field: String,
        /// Schema version the plan migrates from.
        from: u32,
        /// Schema version the plan migrates to.
        to: u32,
    },

    /// Input was malformed in a way the caller could fix.
    #[error("{0}")]
    Invalid(String),

    /// Underlying IO error, with the path attached.
    #[error("{path}: {source}")]
    Io {
        /// The path involved.
        path: String,
        /// The IO error.
        #[source]
        source: std::io::Error,
    },

    /// The diff cannot be turned into a plan because it is unsatisfiable.
    #[error("{0}")]
    Undecidable(String),
}

impl SchemaError {
    /// The process exit code the CLI should use for this error.
    ///
    /// `2` for anything an operator must act on, `1` for a usage error. CI needs
    /// to distinguish "the check failed" from "you invoked the tool wrongly", so
    /// they are never collapsed into one code.
    pub fn exit_code(&self) -> i32 {
        match self {
            SchemaError::Invalid(_) | SchemaError::Read { .. } | SchemaError::Parse { .. } => 1,
            _ => 2,
        }
    }
}
