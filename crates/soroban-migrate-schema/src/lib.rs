//! Storage schema model, source parser, compatibility diff, and migration codegen.
//!
//! This crate is the off-chain half of `soroban-migrate`. It never runs inside a
//! contract: everything here works on *source* and on *schema files*, which is what
//! makes it possible to answer "is this upgrade safe?" before anything is deployed.
//!
//! # The four pieces
//!
//! * [`model`] — a schema as data: field names, declared types, and whether each
//!   field is optional. Committed one file per version, like a migration directory.
//! * [`parse`] — reads `#[storage_schema(version = N)]` declarations out of Rust
//!   source, so schemas can be snapshotted from a contract that has never been built
//!   for Wasm.
//! * [`diff`] — classifies every change between two versions and decides whether the
//!   upgrade is safe. This is the pre-flight compatibility check.
//! * [`codegen`] — emits an idempotent `Migration` implementation for a diff.
//!
//! # The one property everything is organised around
//!
//! Soroban's `#[contracttype]` structs serialize as symbol-keyed maps. As of
//! `soroban-sdk` 28 — the CAP-86 change — decoding a map whose keys do not exactly
//! match the struct is *tolerant*: an absent key decodes as `None` for an `Option`
//! field and errors for any other field, and a key the struct has no field for is
//! discarded.
//!
//! Tolerance gives, and it takes away:
//!
//! * It makes it possible to upgrade a contract's code before its state, because the
//!   new code can read entries the old code wrote. That is what makes a batched
//!   migration viable against a live contract at all.
//! * It makes it *quietly destructive* to remove a field, because the discarded key
//!   looks like nothing happened. So [`diff`] denies removals unless a plan declares
//!   them, and [`codegen`] will not generate code for a denied diff.
//!
//! # Example
//!
//! ```
//! use soroban_migrate_schema::diff::{Verdict, diff_with_plan};
//! use soroban_migrate_schema::model::{Field, Schema};
//! use soroban_migrate_schema::plan::MigrationPlan;
//!
//! let v1 = Schema::new(1, "Account", vec![Field::new("owner", "Address")]);
//! let v2 = Schema::new(
//!     2,
//!     "Account",
//!     vec![Field::new("owner", "Address"), Field::new("frozen", "Option<bool>")],
//! );
//!
//! let result = diff_with_plan(&v1, &v2, &MigrationPlan::new(1, 2)).unwrap();
//! // Adding an optional field is safe: entries that predate it decode it as `None`.
//! assert_eq!(result.verdict, Verdict::SafeWithLazyMigration);
//! assert!(result.report().contains("AddedOptional".replace("AddedOptional", "added, optional").as_str()));
//! ```

#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    // The generator builds Rust source by appending `format!`ed fragments to a `String`.
    // `write!` would remove one allocation per fragment in a code path that runs once per
    // migration and allocates kilobytes either way, at the cost of making every long
    // multi-line literal harder to read — which matters, because the generated output is
    // reviewed by the people adopting the framework.
    clippy::format_push_string,
    // `diff_with_plan` is deliberately one function. Its value is that every change kind
    // is classified in one place, in the order the report reads, against one running
    // record of what has already been accounted for. Splitting it would put the "is this
    // field already declared" bookkeeping in a different scope from the walk that fills
    // it, which is how a diff starts reporting a change twice.
    clippy::too_many_lines,
    // `Some('<' | '(' | '[' | '&' | '\'')` is not accepted in the pattern position used by
    // the type renderer, so the alternatives are spelled out.
    clippy::unnested_or_patterns
)]

pub mod codegen;
pub mod diff;
pub mod error;
pub mod model;
pub mod parse;
pub mod plan;

pub use error::SchemaError;

/// The directory a project keeps its schema snapshots and plans in.
///
/// A fixed convention rather than a flag, for the same reason Rails and Diesel fix
/// theirs: the point of a migrations directory is that a new contributor can find it
/// without being told, and CI can assert that it is up to date.
pub const MIGRATIONS_DIR: &str = "migrations";

/// The file name for the schema snapshot of `version` of `name`.
pub fn schema_file_name(name: &str, version: u32) -> String {
    format!("{}_{}.json", name.to_lowercase(), version)
}

/// The file name for the plan covering `from` to `to` of `name`.
pub fn plan_file_name(name: &str, from: u32, to: u32) -> String {
    format!("{}_{}_to_{}.plan.json", name.to_lowercase(), from, to)
}

/// The file name of the committed key-space snapshot.
///
/// A fixed name rather than one derived from the enum's type, because the file
/// describes the contract's key space and a contract has exactly one. Unlike a schema
/// snapshot it carries no version, for the reason [`model::KeySpace`] sets out.
pub const KEY_SPACE_FILE: &str = "keyspace.json";
