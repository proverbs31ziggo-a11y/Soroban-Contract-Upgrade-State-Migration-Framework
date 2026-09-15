//! Schema metadata, available to the contract and to anything that reads a build.
//!
//! # Why a contract carries its own schema
//!
//! Every other mechanism in this crate migrates state. This one does not: it lets a
//! contract *say* what its entries look like, which is what makes three things
//! possible that are otherwise guesswork.
//!
//! * An operator can ask a deployed contract what version it is at and what fields
//!   each entry has, without reading its source or its build artifacts. A migration
//!   that is failing in production is exactly when the source is least likely to
//!   match what is deployed.
//! * The CLI can compare what a contract *says* its shape is against what the
//!   schema snapshots in the repository claim, and refuse to migrate when they
//!   disagree. Deploying a build whose declared schema does not match its snapshot
//!   is how a migration gets run against the wrong shape.
//! * A build can be checked before it is uploaded, rather than after.
//!
//! # Why the metadata is `&'static` and not a parsed structure
//!
//! Contracts cannot afford `serde`, and the metadata has to be generated anyway.
//! So it is emitted as `&'static str` tables by the derive macro: the contract pays
//! for the bytes it uses, and nothing at runtime. The JSON form exists for
//! tooling — a build script, a CI check, an `soroban-migrate schema export` — and is
//! never parsed on chain.
//!
//! # Cost
//!
//! [`StorageSchema::JSON`] is the largest item here and is only worth linking in when
//! something reads it. A contract that wants to stay lean can have its `u32`-valued
//! schema entry point return [`StorageSchema::VERSION`] and leave the JSON const
//! unused; Wasm's dead-code elimination removes what is not reachable.

use soroban_sdk::{contracttype, Env, String};

/// One field of a schema, in a form a contract can return.
///
/// Mirrors `soroban_migrate_schema::model::Field`, deliberately: the off-chain type
/// derives `serde` and this one derives `#[contracttype]`, because a contract cannot
/// use `serde` and a CLI does not need an XDR encoding.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldSpec {
    /// The field's name, which is also the `Symbol` key in the serialized entry.
    pub name: String,
    /// The type as written in Rust, e.g. `Option<Vec<Address>>`.
    pub declared: String,
    /// Whether the declared type is `Option<..>`.
    ///
    /// The property that decides whether an entry that predates this field decodes
    /// or traps. See this module's crate-level documentation in
    /// `soroban_migrate_schema`.
    pub optional: bool,
}

/// What a contract's storage shape is, as of the code that is running.
///
/// Implemented by `#[derive(StorageSchema)]`. Hand-writing it is supported and is
/// what a generated implementation expands to.
pub trait StorageSchema {
    /// The schema version this shape is. Must match the version the contract records
    /// in [`crate::version`].
    const VERSION: u32;

    /// The declared type name, e.g. `Account`.
    const NAME: &'static str;

    /// The canonical JSON snapshot of this shape.
    ///
    /// Byte-for-byte identical to the file `soroban-migrate schema export` writes,
    /// so a build can be checked against the repository by comparing strings rather
    /// than by re-deriving anything.
    const JSON: &'static str;

    /// The fields, in declaration order.
    fn fields() -> &'static [&'static str];

    /// The same fields rendered as contract types, for an on-chain accessor.
    fn field_specs(env: &Env) -> soroban_sdk::Vec<FieldSpec> {
        let mut out = soroban_sdk::Vec::new(env);
        for entry in Self::fields() {
            // Each entry is `name:declared`, with the optional marker implied by the
            // declared text. Keeping the table as strings and parsing here is what
            // lets the compiler emit one `&'static str` slice instead of a
            // `Vec`-allocated structure per field.
            let (name, declared) = match entry.split_once(':') {
                Some((n, d)) => (n, d),
                None => (*entry, ""),
            };
            out.push_back(FieldSpec {
                name: String::from_str(env, name),
                declared: String::from_str(env, declared),
                optional: declared.starts_with("Option<"),
            });
        }
        out
    }
}

/// Returns a contract's declared schema version, or `None` if it has not recorded
/// one.
///
/// The entry point a contract should expose so an operator can ask a *deployed*
/// contract what it is, rather than trusting a build artifact.
pub fn reported_version<K: crate::key::Keyspace>(env: &Env) -> Option<u32> {
    crate::version::read::<K>(env)
}
