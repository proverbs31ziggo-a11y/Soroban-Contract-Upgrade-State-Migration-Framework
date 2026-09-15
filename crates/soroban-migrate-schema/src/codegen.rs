//! Migration code generation.
//!
//! Turns a schema diff plus a plan into a Rust `Migration` implementation. The hard
//! requirement on the output is not that it compiles — it is that it is
//! **idempotent**, because the framework's executor may visit the same key more
//! than once (see `soroban_migrate::migration::Migration`). A generator that emits
//! plausible-looking but non-idempotent code is worse than no generator: it works
//! on every test and corrupts one entry per interrupted batch in production.
//!
//! # How idempotency is achieved per change kind
//!
//! * **Adding an optional field.** Guarded by `is_none()`: the field is filled only
//!   when it is empty. A second visit finds it populated and does nothing. Note
//!   that `None` is a legitimate value, so an entry whose field is correctly `None`
//!   is reported as already current rather than rewritten — which is the right
//!   outcome, not a missed migration.
//! * **Adding a required field.** There is no old key to find, so the value is
//!   computed from the entry and assigned unconditionally. This is idempotent
//!   exactly when the plan's expression is *stable* — a constant, or a function of
//!   fields the same expression does not modify. An expression like
//!   `old.count + 1` is not stable, and the generated file says so at the point
//!   where it matters.
//! * **Renames and retypes.** Both are "consume an old key, produce a new one", so
//!   both need to know whether the old key has already been consumed. Reading the
//!   old struct directly would trap once the key is gone, because a key absent from
//!   a serialized map only decodes as `None` for an `Option` field.
//!
//!   The generator therefore emits a **tolerant shadow struct** declaring only the
//!   old keys the migration consumes, each as `Option<..>`. That decodes any entry,
//!   before or after the migration, with absent keys arriving as `None` — which is
//!   CAP-86's relaxed unpacking doing the work the migration needs. "The old key is
//!   present" is then a plain `Option` check, and the conversion runs only when the
//!   old value is still there. A second visit sees `None` and stops.
//! * **Dropping a field.** Nothing to generate: the new struct simply has no field
//!   for the key, and the decoder discards it. The data is gone, which is what
//!   `dropped` declares.

use crate::diff::{ChangeKind, Diff, Finding, Severity};
use crate::error::SchemaError;
use crate::model::Schema;
use crate::plan::MigrationPlan;

/// What the generated file should call things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodegenOptions {
    /// Name for the generated migration struct, e.g. `VaultEntryV1ToV2`.
    pub struct_name: String,
    /// Path of the keyspace type implementing `soroban_migrate::Keyspace`, e.g.
    /// `crate::DataKey`.
    pub keyspace: String,
    /// Module path under which the version structs are in scope, e.g. `crate`.
    pub types_module: String,
    /// The Rust type name of the old shape, e.g. `VaultEntryV1`.
    pub from_type: String,
    /// The Rust type name of the new shape.
    pub to_type: String,
}

impl CodegenOptions {
    /// Derives names from the schemas: `VaultEntry` at v1 and v2 becomes
    /// `VaultEntryV1ToV2`, with the version structs named `VaultEntryV1` and
    /// `VaultEntryV2`.
    ///
    /// The convention is fixed rather than configurable because the generated file
    /// has to be re-generatable: two developers running `generate` should produce
    /// byte-identical output, and a configurable naming scheme guarantees they will
    /// not.
    pub fn conventional(from: &Schema, to: &Schema, keyspace: impl Into<String>) -> Self {
        Self {
            struct_name: format!("{}V{}ToV{}", from.name, from.version, to.version),
            keyspace: keyspace.into(),
            types_module: "crate".into(),
            from_type: format!("{}V{}", from.name, from.version),
            to_type: format!("{}V{}", to.name, to.version),
        }
    }
}

/// One old key the migration consumes, and how.
struct Consumer {
    /// Field name in the old shape.
    old: String,
    /// Field name in the new shape.
    new: String,
    /// The new field's declared type.
    new_declared: String,
    /// Whether the new field is `Option<..>`.
    new_optional: bool,
    /// How to turn the old value into the new one. `None` means the types are
    /// identical and the value moves unchanged.
    conversion: Option<String>,
}

/// Generates the migration implementation.
///
/// # Errors
///
/// [`SchemaError::Undecidable`] when the diff demands a decision the plan has not
/// made — a conversion for a retyped field, say. Generating code that compiles but
/// does the wrong thing is the failure mode this refuses.
pub fn generate(
    from: &Schema,
    to: &Schema,
    diff: &Diff,
    plan: &MigrationPlan,
    options: &CodegenOptions,
) -> Result<String, SchemaError> {
    if diff.verdict.is_failure() {
        return Err(SchemaError::Undecidable(format!(
            "refusing to generate a migration for a denied change. Run `soroban-migrate check` \
             to see which declarations are missing:\n{}",
            diff.report()
        )));
    }

    let mut consumers = Vec::new();
    let mut assign_optional = Vec::new();
    let mut assign_required = Vec::new();
    let mut carried = Vec::new();

    for finding in &diff.findings {
        match &finding.kind {
            ChangeKind::AddedOptional => {
                if let Some(expr) = plan.init.get(&finding.field) {
                    assign_optional.push((finding.field.clone(), expr.clone()));
                }
            }
            ChangeKind::AddedRequired => {
                let expr = plan.init.get(&finding.field).ok_or_else(|| {
                    SchemaError::Undecidable(format!(
                        "`{}` is a new required field with no `init` expression",
                        finding.field
                    ))
                })?;
                assign_required.push((finding.field.clone(), expr.clone()));
            }
            ChangeKind::BecameRequired => {
                let expr = plan.init.get(&finding.field).ok_or_else(|| {
                    SchemaError::Undecidable(format!(
                        "`{}` became required with no `init` expression for the entries that have \
                         no value for it",
                        finding.field
                    ))
                })?;
                assign_required.push((finding.field.clone(), expr.clone()));
            }
            ChangeKind::Renamed { from: old, to: new } => {
                consumers.push(consumer_for_rename(from, to, old, new, plan)?);
            }
            ChangeKind::Retyped { .. } => {
                consumers.push(consumer_for_retype(from, to, finding, plan)?);
            }
            ChangeKind::Removed => {
                // Carried forward only to be written back. Nothing to do at runtime:
                // the new struct has no field for the key, so the decoder discards it.
                carried.push(finding.field.clone());
            }
            ChangeKind::BecameOptional | ChangeKind::SpellingChanged | ChangeKind::Reordered => {}
        }
    }

    // A renamed field whose new name is also carried: the consumer handles the
    // value, so do not also try to remove the old key.
    carried.retain(|f| !plan.renamed.contains_key(f));

    Ok(render(
        from,
        to,
        diff,
        plan,
        options,
        &consumers,
        &assign_optional,
        &assign_required,
        &carried,
    ))
}

fn consumer_for_rename(
    from: &Schema,
    to: &Schema,
    old: &str,
    new: &str,
    plan: &MigrationPlan,
) -> Result<Consumer, SchemaError> {
    let old_field = from.field(old).ok_or_else(|| {
        SchemaError::Undecidable(format!("rename source `{old}` is not in v{}", from.version))
    })?;
    let new_field = to.field(new).ok_or_else(|| {
        SchemaError::Undecidable(format!("rename target `{new}` is not in v{}", to.version))
    })?;
    let conversion = if old_field.base == new_field.base {
        None
    } else {
        match plan.converted.get(new) {
            Some(e) if !e.trim().is_empty() => Some(e.clone()),
            _ => {
                return Err(SchemaError::Undecidable(format!(
                    "`{old}` -> `{new}` also changes type ({} -> {}); declare \
                     `converted[\"{new}\"]` so the generated code knows how",
                    old_field.declared, new_field.declared
                )))
            }
        }
    };
    Ok(Consumer {
        old: old.to_string(),
        new: new.to_string(),
        new_declared: new_field.declared.clone(),
        new_optional: new_field.optional,
        conversion,
    })
}

fn consumer_for_retype(
    from: &Schema,
    to: &Schema,
    finding: &Finding,
    plan: &MigrationPlan,
) -> Result<Consumer, SchemaError> {
    let old_field = from.field(&finding.field).ok_or_else(|| {
        SchemaError::Undecidable(format!("`{}` is not in v{}", finding.field, from.version))
    })?;
    let new_field = to.field(&finding.field).ok_or_else(|| {
        SchemaError::Undecidable(format!("`{}` is not in v{}", finding.field, to.version))
    })?;
    let conversion = match plan.converted.get(&finding.field) {
        Some(e) if !e.trim().is_empty() => Some(e.clone()),
        _ => {
            return Err(SchemaError::Undecidable(format!(
                "`{}` changes type from `{}` to `{}`; declare `converted[\"{}\"]`",
                finding.field, old_field.declared, new_field.declared, finding.field
            )))
        }
    };
    Ok(Consumer {
        old: finding.field.clone(),
        new: finding.field.clone(),
        new_declared: new_field.declared.clone(),
        new_optional: new_field.optional,
        conversion,
    })
}

/// Emits the file.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render(
    from: &Schema,
    to: &Schema,
    diff: &Diff,
    plan: &MigrationPlan,
    options: &CodegenOptions,
    consumers: &[Consumer],
    assign_optional: &[(String, String)],
    assign_required: &[(String, String)],
    carried: &[String],
) -> String {
    let name = &options.struct_name;
    let types = &options.types_module;
    let keyspace = &options.keyspace;
    let from_type = &options.from_type;
    let to_type = &options.to_type;

    let mut out = String::new();

    out.push_str(&format!(
        "//! Storage schema migration from v{} to v{} for `{}`.\n\
         //!\n\
         //! Generated by `soroban-migrate generate`. Do not edit: the next run\n\
         //! overwrites it. Change the schemas or the plan and regenerate with\n\
         //!\n\
         //! ```text\n\
         //! soroban-migrate generate {} {}\n\
         //! ```\n\
         //!\n",
        from.version, to.version, to.name, from.version, to.version,
    ));

    if let Some(note) = &plan.note {
        out.push_str(&format!("//! {note}\n//!\n"));
    }

    out.push_str("//! # Idempotency\n//!\n");
    if assign_required.is_empty() && consumers.is_empty() {
        out.push_str(
            "//! Every assignment below is guarded so that visiting an entry twice is a no-op.\n",
        );
    } else {
        out.push_str("//! Guards, and why each one is sufficient:\n//!\n");
        if !assign_optional.is_empty() {
            out.push_str(
                "//! * New optional fields are filled only when empty, so a second visit finds \
                 them populated.\n",
            );
        }
        if !assign_required.is_empty() {
            out.push_str(
                "//! * Newly required fields are assigned unconditionally, because there is no \
                 old key to detect. This is idempotent only while the assignment is *stable* — \
                 a constant, or a function of fields it does not itself change. Verify the \
                 expressions below against that rule.\n",
            );
        }
        if !consumers.is_empty() {
            out.push_str(&format!(
                "//! * Renames and type changes consume an old key, so they are guarded by a \
                 tolerant `{}` read in which every key the migration consumes is optional. A \
                 consumed key decodes as `None` on a second visit, so the conversion does not \
                 run again.\n",
                shadow_name(name)
            ));
        }
    }

    // The glob imports are deliberate. A schema field's declared type is written
    // the way the author writes it — `Address`, `Vec<u32>`, `BytesN<32>` — and the
    // tolerant shadow struct has to name those types. Importing the contract's own
    // module and the SDK wholesale is what lets generated code compile without the
    // generator having to resolve paths, and it is why the file is generated into a
    // module of the contract crate rather than a crate of its own.
    out.push_str(
        "\n#[allow(unused_imports)]\n\
         use soroban_migrate::migration::{EntryOutcome, Migration};\n\
         #[allow(unused_imports)]\n\
         use soroban_migrate::MigrationError;\n\
         #[allow(unused_imports)]\n\
         use soroban_sdk::*;\n",
    );
    out.push_str(&format!("#[allow(unused_imports)]\nuse {types}::*;\n"));

    if !carried.is_empty() {
        out.push_str(&format!(
            "\n// `{}` {} present in v{} and intentionally absent in v{}. Nothing to generate:\n\
             // `#[contracttype]` drops map keys the struct has no field for, which is what the\n\
             // plan's `dropped` declaration acknowledges.\n",
            carried.join("`, `"),
            if carried.len() == 1 { "is" } else { "are" },
            from.version,
            to.version,
        ));
    }

    // The tolerant shadow of the old shape, only when a rename or retype needs it.
    if !consumers.is_empty() {
        out.push_str(&format!(
            "\n/// A tolerant view of a v{}-shaped entry, holding only the keys this migration\n\
             /// consumes, each as `Option<..>`.\n\
             ///\n\
             /// Reading `{}` directly would trap once this migration has run, because a key that\n\
             /// is absent from a serialized map only decodes as `None` for an `Option` field.\n\
             /// Declaring the consumed keys as optional is therefore what makes the migration\n\
             /// re-runnable.\n\
             #[allow(dead_code)]\n\
             #[contracttype]\n\
             struct {}\n{{\n",
            from.version,
            from_type,
            shadow_name(name),
        ));
        for c in consumers {
            let old_field = from
                .field(&c.old)
                .expect("validated when the consumer was built");
            out.push_str(&format!(
                "    {}: Option<{}>,\n",
                rust_ident(&c.old),
                old_field.base
            ));
        }
        out.push_str("}\n");
    }

    out.push_str(&format!(
        "\n/// Moves `{}` from the v{} shape to the v{} shape.\n\
         #[allow(dead_code)]\n\
         pub struct {name};\n\
         \n\
         impl Migration for {name} {{\n\
         \x20   type Keys = {keyspace};\n\
         \x20   const FROM: u32 = {};\n\
         \x20   const TO: u32 = {};\n\
         \n\
         \x20   fn up(env: &Env, key: &Val) -> Result<EntryOutcome, MigrationError> {{\n\
         \x20       let storage = env.storage().persistent();\n\
         \x20       if !storage.has::<Val>(key) {{\n\
         \x20           return Ok(EntryOutcome::Skipped);\n\
         \x20       }}\n",
        to.name, from.version, to.version, from.version, to.version,
    ));

    // Nothing to assign and nothing to consume: the change is entirely absorbed by
    // tolerant decoding, so there is no work for `up` to do. Emitting a `changed` flag
    // that can never be set, and a `storage.set` that can never be reached, would be
    // both dead code and a suggestion that something happens here when nothing does.
    let has_work =
        !assign_optional.is_empty() || !assign_required.is_empty() || !consumers.is_empty();
    if has_work {
        if consumers.is_empty() {
            out.push_str(&format!(
            "        // Decoding with the new type is what makes this work: keys the new shape does\n\
             \x20       // not declare are discarded, and keys it declares but the entry lacks arrive as\n\
             \x20       // `None` for an `Option` field.\n\
             \x20       let mut entry: {to_type} = match storage.get::<Val, {to_type}>(key) {{\n\
             \x20           Some(e) => e,\n\
             \x20           None => return Ok(EntryOutcome::Skipped),\n\
             \x20       }};\n\
             \x20       let mut changed = false;\n"
        ));
        } else {
            out.push_str(&format!(
                "        let mut entry: {to_type} = match storage.get::<Val, {to_type}>(key) {{\n\
             \x20           Some(e) => e,\n\
             \x20           None => return Ok(EntryOutcome::Skipped),\n\
             \x20       }};\n\
             \x20       let mut changed = false;\n\
             \x20       let old: {} = match storage.get::<Val, {}>(key) {{\n\
             \x20           Some(o) => o,\n\
             \x20           None => return Ok(EntryOutcome::Skipped),\n\
             \x20       }};\n",
                shadow_name(name),
                shadow_name(name),
            ));
        }

        for (field, expr) in assign_optional {
            out.push_str(&format!(
            "        // Optional addition: fill only when empty, so a second visit is a no-op.\n\
             \x20       if entry.{field}.is_none() {{\n\
             \x20           entry.{} = Some({expr});\n\
             \x20           changed = true;\n\
             \x20       }}\n",
            rust_ident(field)
        ));
        }

        for (field, expr) in assign_required {
            out.push_str(&format!(
            "        // Required addition. Idempotent only while `{expr}` is stable — a constant, or\n\
             \x20       // computed from fields this assignment does not modify.\n\
             \x20       entry.{} = {expr};\n\
             \x20       changed = true;\n",
            rust_ident(field)
        ));
        }

        for c in consumers {
            let conversion = match &c.conversion {
                Some(expr) => format!("({expr})(value)"),
                None => "value".to_string(),
            };
            out.push_str(&format!(
            "        // `{}` -> `{}`. Guarded on the *old* key still being present, which is what\n\
             \x20       // makes a re-run safe.\n\
             \x20       if let Some(value) = old.{} {{\n",
            c.old,
            c.new,
            rust_ident(&c.old)
        ));
            if c.new_optional {
                out.push_str(&format!(
                    "            if entry.{}.is_none() {{\n\
                 \x20               entry.{} = Some({conversion});\n\
                 \x20               changed = true;\n\
                 \x20           }}\n",
                    rust_ident(&c.new),
                    rust_ident(&c.new)
                ));
            } else {
                out.push_str(&format!(
                    "            entry.{} = {conversion}; // type: {}\n\
                 \x20           changed = true;\n",
                    rust_ident(&c.new),
                    c.new_declared
                ));
            }
            out.push_str("        }\n");
        }

        out.push_str(&format!(
            "        if !changed {{\n\
         \x20           return Ok(EntryOutcome::AlreadyCurrent);\n\
         \x20       }}\n\
         \x20       storage.set::<Val, {to_type}>(key, &entry);\n\
         \x20       Ok(EntryOutcome::Migrated)\n\
         \x20   }}\n\
         }}\n"
        ));
    } else {
        out.push_str(
            "        // This migration rewrites no field. It still has to run: `executor::batch`\n\
             \x20       // records every visited key under the new version's index and advances the\n\
             \x20       // schema version once the cursor reaches the end, so running it is what makes\n\
             \x20       // the new version the contract's declared shape. The change itself is absorbed\n\
             \x20       // by tolerant decoding: an entry written before it arrives with the new field\n\
             \x20       // as `None`, which is what lets the upgrade ship before the migration finishes.\n\
             \x20       Ok(EntryOutcome::AlreadyCurrent)\n\
             \x20   }\n\
             }\n",
        );
    }

    if diff.findings_at_least(Severity::Warning).is_empty() {
        out.push_str(
            "\n// This migration has no compensating transformation: the change is additive, and\n\
             // nothing it does is recoverable-by-derivation from the new shape. If a rollback is\n\
             // required, publish the previous Wasm under a fleet tag *before* promoting the new\n\
             // one, so the rollback is a single `fleet::promote` rather than a state operation.\n",
        );
    }

    out
}

/// The name of the tolerant shadow struct for a migration.
fn shadow_name(migration: &str) -> String {
    format!("{migration}Consumed")
}

/// Escapes a field name into a Rust identifier.
///
/// The only case that needs escaping is a field named after a Rust keyword; Soroban
/// symbols are otherwise valid identifiers.
fn rust_ident(name: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
        "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final",
        "macro", "override", "priv", "try", "typeof", "unsized", "virtual", "yield",
    ];
    if KEYWORDS.contains(&name) {
        format!("r#{name}")
    } else {
        name.to_string()
    }
}

/// The list of Rust fields a schema declares, for use by generators and tests.
pub fn field_idents(schema: &Schema) -> Vec<String> {
    schema.fields.iter().map(|f| rust_ident(&f.name)).collect()
}
