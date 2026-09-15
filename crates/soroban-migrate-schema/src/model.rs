//! The schema model: what a version of a contract's storage looks like, in a form
//! that can be committed to a repository and diffed.
//!
//! A schema is deliberately *structural*. It records field names, declared types,
//! and whether a field is optional — and nothing else. It does not record what a
//! field means, because a diff cannot act on meaning, and it does not record
//! defaults or conversions, because those are the migration author's decisions and
//! live in the [`crate::plan::MigrationPlan`] that accompanies the schema.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One version of a contract's storage shape.
///
/// Serialized as JSON and committed alongside the contract's source, one file per
/// version, in the same way Rails keeps `db/schema.rb` and Diesel keeps migration
/// snapshots. The reason to snapshot rather than re-derive is that old schema
/// versions cease to exist in the source as soon as a struct is edited — and the
/// diff that justifies a migration needs *both* sides long after the old struct has
/// been deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    /// The schema version this describes. Must match the `version` in the
    /// contract's declaration.
    pub version: u32,
    /// The declared type name, e.g. `VaultEntry`. Used in reports and in generated
    /// code, not in the diff: two schemas can be diffed even if the type was
    /// renamed, because the diff works on fields.
    pub name: String,
    /// Free-form note, for the operator. Never interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Fields, in declaration order.
    ///
    /// Order is preserved in the file so that diffs of the schema files themselves
    /// are readable, but it carries no meaning: Soroban's `#[contracttype]` structs
    /// are serialized as symbol-keyed maps, so reordering fields is not a change.
    pub fields: Vec<Field>,
}

/// One field of a schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    /// The field's name, which is the `Symbol` key in the serialized map. Renaming
    /// a field is therefore a *removal plus an addition*, never a no-op.
    pub name: String,
    /// The type exactly as written in Rust, e.g. `Option<Vec<Address>>`. Kept
    /// verbatim because the diff has to show an operator what actually changed.
    pub declared: String,
    /// `declared` with any `Option<..>` wrapper removed. Comparing base types is
    /// how the diff distinguishes "the field became optional" from "the field
    /// changed type", which need very different responses.
    pub base: String,
    /// Whether the declared type is `Option<..>`.
    ///
    /// This is the single most load-bearing property in the whole model. A missing
    /// key in a serialized map decodes as `None` for an `Option` field and *traps*
    /// for any other field — see CAP-86's relaxed unpacking. So a new field being
    /// optional is the difference between an upgrade that keeps serving reads and
    /// one that panics on every un-migrated entry.
    pub optional: bool,
}

impl Field {
    /// Builds a field, deriving `base` and `optional` from `declared`.
    pub fn new(name: impl Into<String>, declared: impl Into<String>) -> Self {
        let declared = declared.into();
        let (base, optional) = split_option(&declared);
        Self {
            name: name.into(),
            declared,
            base,
            optional,
        }
    }

    /// The field's identity for diff purposes: its base type, ignoring
    /// optionality.
    pub fn identity(&self) -> &str {
        &self.base
    }
}

impl Schema {
    /// Builds a schema from a version, a name, and field declarations.
    pub fn new(version: u32, name: impl Into<String>, fields: Vec<Field>) -> Self {
        Self {
            version,
            name: name.into(),
            note: None,
            fields,
        }
    }

    /// A lookup by field name.
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Field names in declaration order.
    pub fn field_names(&self) -> Vec<&str> {
        self.fields.iter().map(|f| f.name.as_str()).collect()
    }

    /// Fields that are absent from `other`, keyed by name.
    pub fn only_in<'a>(&'a self, other: &'a Schema) -> Vec<&'a Field> {
        self.fields
            .iter()
            .filter(|f| other.field(&f.name).is_none())
            .collect()
    }

    /// Parses a schema from JSON.
    ///
    /// # Errors
    ///
    /// Returns the underlying `serde_json` error, with the file's own text not
    /// included: callers are expected to add the path, which they know and this
    /// function does not.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Renders the schema as pretty JSON, with a trailing newline.
    ///
    /// # Panics
    ///
    /// Never in practice: the model is plain data with no fallible `serde` attribute, so
    /// serializing it cannot fail. `expect` rather than `unwrap` so that a future field
    /// which makes it fallible says so at the call site.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("schema serialization cannot fail");
        s.push('\n');
        s
    }

    /// Renders a stable, human-readable summary for `--format=text` output.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "{} v{} ({} field{})\n",
            self.name,
            self.version,
            self.fields.len(),
            if self.fields.len() == 1 { "" } else { "s" }
        );
        for f in &self.fields {
            out.push_str(&format!(
                "  {:<24} {:<32} {}\n",
                f.name,
                f.declared,
                if f.optional { "optional" } else { "required" }
            ));
        }
        out
    }
}

/// Splits `Option<T>` into `(T, true)`, and anything else into `(itself, false)`.
///
/// Handles nested paths like `core::option::Option<T>` and trims whitespace, because
/// the input is whatever an author typed. A nested `Option<Option<T>>` collapses to
/// `(Option<T>, true)`, which is correct for this purpose: the *outer* optionality
/// is what decides whether a missing key traps.
pub fn split_option(declared: &str) -> (String, bool) {
    let t = declared.trim();
    let stripped = t
        .strip_prefix("core::option::Option<")
        .or_else(|| t.strip_prefix("std::option::Option<"))
        .or_else(|| t.strip_prefix("Option<"));
    match stripped.and_then(|inner| inner.strip_suffix('>')) {
        Some(inner) => (inner.trim().to_string(), true),
        None => (t.to_string(), false),
    }
}

/// Normalizes a type for comparison: removes whitespace, and resolves the
/// fully-qualified `Option` paths so that `std::option::Option<bool>` and
/// `Option<bool>` compare equal.
///
/// Only used for *reporting* which fields changed type. Two types that are
/// structurally different but semantically identical (`Vec<u8>` and `Bytes`) are
/// deliberately reported as a retype, because the diff cannot know they are
/// interchangeable and guessing would hide a real conversion behind a no-op.
pub fn normalize_type(declared: &str) -> String {
    declared
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("")
        .replace("core::option::Option<", "Option<")
        .replace("std::option::Option<", "Option<")
        .replace("soroban_sdk::", "")
        .replace("std::string::String", "String")
        .replace("alloc::string::String", "String")
        .replace("alloc::vec::Vec<", "Vec<")
        .replace("std::vec::Vec<", "Vec<")
        .replace("core::primitive::", "")
}

/// One variant of a contract's key-space enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyVariant {
    /// The variant's name.
    ///
    /// This is the discriminant on the wire. `#[contracttype]` encodes an enum as a
    /// vector whose first element is the *name* of the variant as a `Symbol`, not its
    /// position — which is why renaming a variant breaks stored data and reordering
    /// variants does not. See [`crate::diff::key_space_findings`].
    pub name: String,
    /// The payload as declared, in declaration order: `Address`, or `Address, u32`, or
    /// for a struct variant `memo: Symbol, amount: i128`. `None` for a unit variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    /// Whether the variant carries `#[migration]`, reserving it for the framework's own
    /// bookkeeping keys.
    #[serde(default)]
    pub migration: bool,
}

impl KeyVariant {
    /// The payload normalized for comparison, so that `soroban_sdk::Address` and
    /// `Address` are the same payload rather than a reported change.
    ///
    /// Reuses [`normalize_type`], which is the same normalization the field-level diff
    /// uses, so the two cannot drift apart in what they consider a spelling.
    pub fn normalized_payload(&self) -> Option<String> {
        self.payload.as_deref().map(normalize_type)
    }
}

/// A contract's key space: the enum every storage key is encoded from.
///
/// # Why this is committed separately from the schemas
///
/// A schema describes what a *value* looks like. This describes which entry a key
/// refers to, and the two fail differently. A schema change that is unsafe makes an
/// entry unreadable; an unsafe key-space change makes an entry unreachable — the
/// contract keeps serving reads and answers them out of the wrong place, or fails to
/// find anything at all.
///
/// It is deliberately unversioned. The version numbers a contract records gate the
/// *shape* of an entry, and a key space is not versioned in that sense: every read goes
/// through it, so it has to be correct for entries of every version at once. What
/// replaces a version number here is the committed snapshot itself — a change to the
/// source that the snapshot does not describe is reported, and re-exporting it is the
/// deliberate acknowledgement that the change was reviewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeySpace {
    /// The enum's type name. *Not* part of the encoding — only variant names are — so a
    /// rename of the type itself is not a change and is not reported.
    pub name: String,
    /// Variants in declaration order.
    pub variants: Vec<KeyVariant>,
}

impl KeySpace {
    /// Builds a key space from its parts.
    pub fn new(name: impl Into<String>, variants: Vec<KeyVariant>) -> Self {
        Self {
            name: name.into(),
            variants,
        }
    }

    /// One variant by name.
    pub fn variant(&self, name: &str) -> Option<&KeyVariant> {
        self.variants.iter().find(|v| v.name == name)
    }

    /// Parses a committed key-space snapshot.
    ///
    /// # Errors
    ///
    /// The `serde` error for a file that is not a key space.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Renders the key space as pretty JSON, with a trailing newline.
    ///
    /// # Panics
    ///
    /// Never in practice: the model is plain data, so serializing it cannot fail.
    pub fn to_json(&self) -> String {
        let mut s =
            serde_json::to_string_pretty(self).expect("key space serialization cannot fail");
        s.push('\n');
        s
    }

    /// Renders a stable, human-readable summary.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "{} ({} variant{})\n",
            self.name,
            self.variants.len(),
            if self.variants.len() == 1 { "" } else { "s" }
        );
        for v in &self.variants {
            let payload = match &v.payload {
                Some(p) => format!("({p})"),
                None => String::new(),
            };
            let marker = if v.migration { "  #[migration]" } else { "" };
            out.push_str(&format!("    {}{payload}{marker}\n", v.name));
        }
        out
    }
}

/// The set of schemas a project has committed.
///
/// Keyed by *shape name*, then by version, and never by version alone. A contract
/// routinely holds several independently-versioned shapes at once — a balance entry
/// and a stats entry, say — and those version numbers are unrelated: there is no
/// reason a new stats shape should force a balance migration, or vice versa. Keying
/// by version would make the two collide on whatever number they happened to share.
///
/// Grouping by name is also what makes a migration well-defined: a migration is a
/// `from -> to` pair *of one shape*, because the executor walks a single index and a
/// single cursor, and "which shape is this entry" has to be answerable before the
/// entry is decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaSet {
    /// Schemas by name, then by version. Both levels are `BTreeMap`s so iteration is
    /// deterministic and reports do not reshuffle between runs.
    pub by_name: BTreeMap<String, BTreeMap<u32, Schema>>,
}

impl SchemaSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a schema, returning the one it displaced, if any.
    ///
    /// Refuses to replace a *differing* schema for the same name and version: a schema
    /// file that changes without its version changing is how two developers end up
    /// migrating to incompatible shapes while both believing they are at v3.
    ///
    /// # Errors
    ///
    /// [`crate::SchemaError::VersionConflict`] when the name and version are already
    /// present with different contents.
    pub fn insert(&mut self, schema: Schema) -> Result<Option<Schema>, crate::error::SchemaError> {
        let versions = self.by_name.entry(schema.name.clone()).or_default();
        if let Some(existing) = versions.get(&schema.version) {
            if existing != &schema {
                return Err(crate::error::SchemaError::VersionConflict {
                    version: schema.version,
                });
            }
        }
        Ok(versions.insert(schema.version, schema))
    }

    /// Every shape name, in a deterministic order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// Every version of `name`, ascending.
    pub fn versions_of(&self, name: &str) -> Vec<u32> {
        self.by_name
            .get(name)
            .map(|v| v.keys().copied().collect())
            .unwrap_or_default()
    }

    /// One schema.
    pub fn get(&self, name: &str, version: u32) -> Option<&Schema> {
        self.by_name.get(name)?.get(&version)
    }

    /// Consecutive version pairs of `name` — that is, exactly the migrations that must
    /// exist for the shape to be upgradeable from its first version to its last.
    ///
    /// Only consecutive pairs: the runtime refuses a migration that skips a version,
    /// because a rollback walks the same index backwards and a jump makes its target
    /// ambiguous. Reporting gaps here is what lets `check` say so before deploy time.
    pub fn consecutive_pairs(&self, name: &str) -> Vec<(u32, u32)> {
        self.versions_of(name)
            .windows(2)
            .map(|w| (w[0], w[1]))
            .collect()
    }

    /// Versions of `name` that are missing their predecessor, i.e. the shape jumped
    /// from one version to a number more than one higher.
    pub fn gaps_of(&self, name: &str) -> Vec<(u32, u32)> {
        self.versions_of(name)
            .windows(2)
            .filter(|w| w[1] != w[0] + 1)
            .map(|w| (w[0], w[1]))
            .collect()
    }

    /// The next version to use for a new shape of `name`.
    pub fn next_version(&self, name: &str) -> u32 {
        self.versions_of(name)
            .last()
            .map_or(version::FIRST, |v| v + 1)
    }

    /// The highest recorded version of `name`.
    pub fn latest_of(&self, name: &str) -> Option<&Schema> {
        let versions = self.by_name.get(name)?;
        versions.values().next_back()
    }
}

/// The first version any shape starts at.
///
/// Duplicated from the runtime crate's `INITIAL_VERSION` on purpose: this crate is
/// the off-chain half and must not depend on the contract-side crate, but the two
/// numbers are a protocol between them and a test in `diff` asserts they agree.
pub mod version {
    /// Schema versions start at 1. `0` means "no version recorded", which is a
    /// different state from being at the first version.
    pub const FIRST: u32 = 1;
}
