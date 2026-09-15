//! The compatibility diff: what changed between two schema versions, and whether
//! the change is safe.
//!
//! # What "safe" means here
//!
//! Not "does it compile" — the compiler answers that. Two failures are possible
//! when a contract's storage shape changes and neither is a compile error:
//!
//! * **Trapping.** The new code reads an un-migrated entry and the decoder fails,
//!   so every call touching that entry panics. This happens when a new field is
//!   *required*: CAP-86's relaxed unpacking fills a missing key with `Void`, which
//!   decodes as `None` for an `Option` field and errors for anything else.
//! * **Silent loss.** The new code reads an old entry, and writing it back discards
//!   data the new shape has no home for. `soroban-sdk` 28 documents this exactly:
//!   a key in the map that is not a field of the struct "is ignored and discarded,
//!   and is lost if the value is packed and written back". So removing a field is
//!   only safe if the author says so.
//!
//! The diff's job is to find both, name the field, and refuse when the author has
//! not accounted for them.

use serde::{Deserialize, Serialize};

use crate::error::SchemaError;
use crate::model::{normalize_type, Schema};
use crate::plan::MigrationPlan;

/// How bad a finding is.
///
/// Ordinal so the overall verdict is the maximum over all findings, and so
/// `--fail-on=warning` in CI is a comparison rather than a second code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    /// Worth knowing. Cannot break anything.
    Info,
    /// The upgrade can proceed, but the migration does not cover this change and
    /// something must be true at deploy time for it to be safe. The detail says what.
    Warning,
    /// The upgrade must not proceed. Proceeding loses data or makes the contract
    /// unreadable, and the diff does not have a declaration that says otherwise.
    Denied,
}

impl Severity {
    /// A short uppercase tag for reports.
    pub fn tag(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warn",
            Severity::Denied => "DENY",
        }
    }
}

/// What happened to one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    /// A field exists only in the new version and is `Option<..>`, so un-migrated
    /// entries decode it as `None`.
    AddedOptional,
    /// A field exists only in the new version and is *not* `Option<..>`, so
    /// un-migrated entries trap on decode unless they are rewritten first.
    AddedRequired,
    /// A field exists only in the old version. Its value is lost when an entry is
    /// rewritten, unless the migration copies it somewhere.
    Removed,
    /// A field was renamed. Recognised only when the plan declares the pair;
    /// otherwise it appears as a `Removed` and an `Added`.
    Renamed {
        /// The name it had.
        from: String,
        /// The name it has now.
        to: String,
    },
    /// The field's base type changed, e.g. `u32` to `i128`.
    Retyped {
        /// The old declared type.
        from: String,
        /// The new declared type.
        to: String,
    },
    /// The field became optional. Safe: a key that is absent now decodes as `None`
    /// instead of trapping.
    BecameOptional,
    /// The field stopped being optional. An entry whose key is absent — including
    /// every entry written before the field was added — traps on decode.
    BecameRequired,
    /// Only the spelling of the type changed, e.g. `std::option::Option<bool>` to
    /// `Option<bool>`. No effect on the encoding.
    SpellingChanged,
    /// Declaration order changed. No effect: `#[contracttype]` structs encode as
    /// symbol-keyed maps, so the order fields are declared in is not part of the
    /// shape.
    Reordered,
}

impl ChangeKind {
    /// A short noun phrase for reports.
    pub fn describe(&self) -> String {
        match self {
            ChangeKind::AddedOptional => "added, optional".into(),
            ChangeKind::AddedRequired => "added, required".into(),
            ChangeKind::Removed => "removed".into(),
            ChangeKind::Renamed { from, to } => format!("renamed {from} -> {to}"),
            ChangeKind::Retyped { from, to } => format!("retyped {from} -> {to}"),
            ChangeKind::BecameOptional => "became optional".into(),
            ChangeKind::BecameRequired => "became required".into(),
            ChangeKind::SpellingChanged => "type spelling changed".into(),
            ChangeKind::Reordered => "order changed".into(),
        }
    }

    /// The directive a plan must use to declare this change, if any.
    pub fn directive(&self) -> Option<&'static str> {
        match self {
            ChangeKind::AddedOptional | ChangeKind::AddedRequired | ChangeKind::BecameRequired => {
                Some("init")
            }
            ChangeKind::Removed => Some("dropped"),
            ChangeKind::Retyped { .. } => Some("converted"),
            ChangeKind::Renamed { .. } => Some("renamed"),
            ChangeKind::SpellingChanged | ChangeKind::Reordered | ChangeKind::BecameOptional => {
                None
            }
        }
    }
}

/// One classified change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The field involved. Empty for findings that are about the version pair
    /// rather than a field.
    pub field: String,
    /// What changed.
    pub kind: ChangeKind,
    /// How bad it is.
    pub severity: Severity,
    /// What the operator must know or do, in one paragraph.
    pub detail: String,
}

/// The overall verdict, which is the maximum severity present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// Nothing that can break anything changed.
    Safe,
    /// The upgrade can proceed, and the contract keeps working on un-migrated
    /// entries because decoding is tolerant. Run the migration to close the gap;
    /// nothing must be sequenced around it.
    SafeWithLazyMigration,
    /// The upgrade must not proceed. At least one change would lose data or make
    /// entries unreadable, and no declaration accounts for it.
    Denied,
}

impl Verdict {
    /// Whether a `soroban-migrate check` should fail on this verdict.
    pub fn is_failure(self) -> bool {
        matches!(self, Verdict::Denied)
    }
}

/// The full result of comparing two schema versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    /// Version compared from.
    pub from_version: u32,
    /// Version compared to.
    pub to_version: u32,
    /// Every finding, in a deterministic order.
    pub findings: Vec<Finding>,
    /// The verdict implied by the findings.
    pub verdict: Verdict,
    /// True when a plan was supplied and its declarations were applied.
    pub plan_applied: bool,
}

impl Diff {
    /// Findings at or above `min`.
    pub fn findings_at_least(&self, min: Severity) -> Vec<&Finding> {
        self.findings.iter().filter(|f| f.severity >= min).collect()
    }

    /// Findings that a plan could clear, i.e. those with a directive.
    pub fn undeclared(&self) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Denied && f.kind.directive().is_some())
            .collect()
    }

    /// A human-readable report.
    pub fn report(&self) -> String {
        let mut out = format!(
            "storage schema v{} -> v{}\n\n",
            self.from_version, self.to_version
        );
        if self.findings.is_empty() {
            out.push_str("no changes; the two versions describe the same shape\n");
            return out;
        }
        for f in &self.findings {
            let subject = if f.field.is_empty() {
                "(version pair)".to_string()
            } else {
                f.field.clone()
            };
            out.push_str(&format!(
                "[{}] {subject}: {}\n      {}\n",
                f.severity.tag(),
                f.kind.describe(),
                f.detail
            ));
        }
        out.push('\n');
        match self.verdict {
            // "Safe" means nothing *requires* a migration, not that there is nothing
            // to migrate: an added optional field with an `init` needs no migration to
            // keep the contract correct, but running one is what fills the field in.
            // The wording is deliberate, because an operator who reads "no migration is
            // required" and then skips a migration they wanted is a worse outcome than
            // a slightly longer sentence.
            Verdict::Safe => out.push_str(
                "verdict: safe — the upgrade can proceed without migrating state; any findings\n\
                 above marked `info` describe work a migration may optionally do\n",
            ),
            Verdict::SafeWithLazyMigration => {
                out.push_str(
                    "verdict: safe with lazy migration — un-migrated entries stay readable, so the\n\
                     code can be promoted before, during, or after the migration\n",
                );
            }
            Verdict::Denied => {
                out.push_str("verdict: DENIED — this upgrade would lose data or trap on read\n");
                let undeclared = self.undeclared();
                if !undeclared.is_empty() {
                    out.push_str("\ndeclare the following in a migration plan to proceed:\n");
                    for f in undeclared {
                        let directive = f.kind.directive().unwrap_or("?");
                        out.push_str(&format!(
                            "  {directive}[\"{}\"]  # {} -> {}\n",
                            f.field, self.from_version, self.to_version
                        ));
                    }
                    out.push_str("\n`soroban-migrate generate` writes this plan for you.\n");
                }
            }
        }
        out
    }
}

/// Compares two schema versions with no plan, so every destructive change is
/// denied.
///
/// # Errors
///
/// [`SchemaError::VersionConflict`] if `to` does not come after `from`, or if the
/// versions are not one step apart. A multi-version jump cannot be rolled back
/// unambiguously — see `soroban_migrate::error::MigrationError::NotSequential` for
/// the runtime half of the same rule.
pub fn diff(from: &Schema, to: &Schema) -> Result<Diff, SchemaError> {
    diff_with_plan(from, to, &MigrationPlan::new(from.version, to.version))
}

/// Compares two schema versions, applying the plan's declarations.
///
/// # Errors
///
/// * [`SchemaError::VersionConflict`] if the versions are not one step apart, or
///   the plan's `from`/`to` disagree with the schemas.
/// * [`SchemaError::StaleDirective`] if the plan declares treatment for a field
///   that no change concerns. See [`crate::plan`] for why that is refused rather
///   than ignored.
pub fn diff_with_plan(
    from: &Schema,
    to: &Schema,
    plan: &MigrationPlan,
) -> Result<Diff, SchemaError> {
    if plan.from != from.version || plan.to != to.version {
        return Err(SchemaError::PlanMismatch {
            file: "plan".into(),
            message: format!(
                "plan covers v{} -> v{}, schemas are v{} -> v{}",
                plan.from, plan.to, from.version, to.version
            ),
        });
    }
    if to.version <= from.version {
        return Err(SchemaError::VersionConflict {
            version: from.version,
        });
    }
    if to.version != from.version + 1 {
        return Err(SchemaError::Invalid(format!(
            "v{} -> v{} skips versions. Migrations advance one version at a time so that a \
             rollback has an unambiguous target; declare the intermediate change instead",
            from.version, to.version
        )));
    }

    let mut findings = Vec::new();
    let mut declared = DeclaredFields::default();

    // Additions, retypes, and optionality changes, walked over the new shape so
    // that the report reads in the order the author wrote the new struct.
    for new in &to.fields {
        match from.field(&new.name) {
            None => {
                // A declared rename turns an add plus a remove into one change.
                let old_name = plan.renamed_from(&new.name);
                if let Some(old_name) = old_name {
                    if let Some(old) = from.field(old_name) {
                        declared.added(&new.name);
                        declared.removed(old_name);
                        let mut severity = Severity::Info;
                        if old.base != new.base {
                            // The rename is *also* a retype, so a `converted`
                            // declaration is legitimate for the new name. Recording
                            // it here is what stops `validate_plan` from rejecting
                            // that declaration as stale.
                            declared.changed(&new.name);
                        }
                        let mut detail = format!(
                            "the value moves from `{old_name}` to `{}`. Un-migrated entries keep \
                             the old key until the migration rewrites them.",
                            new.name
                        );
                        if old.optional != new.optional || old.base != new.base {
                            severity = Severity::Warning;
                            detail.push_str(&format!(
                                " The declared types differ ({} -> {}), so the generated migration \
                                 converts the value.",
                                old.declared, new.declared
                            ));
                        }
                        findings.push(Finding {
                            field: new.name.clone(),
                            kind: ChangeKind::Renamed {
                                from: old_name.to_string(),
                                to: new.name.clone(),
                            },
                            severity,
                            detail,
                        });
                        continue;
                    }
                }

                if new.optional {
                    declared.added(&new.name);
                    let (severity, detail) = if plan.init.contains_key(&new.name) {
                        (
                            Severity::Info,
                            format!(
                                "un-migrated entries decode `{}` as `None`; the migration sets it \
                                 from `{}`.",
                                new.name, plan.init[&new.name]
                            ),
                        )
                    } else {
                        (
                            Severity::Warning,
                            format!(
                                "un-migrated entries decode `{}` as `None`, which is safe because \
                                 the field is optional. If `None` is not a meaningful value for it, \
                                 declare `init[\"{}\"]` so the migration fills it in.",
                                new.name, new.name
                            ),
                        )
                    };
                    findings.push(Finding {
                        field: new.name.clone(),
                        kind: ChangeKind::AddedOptional,
                        severity,
                        detail,
                    });
                } else {
                    declared.added(&new.name);
                    let (severity, detail) = if plan.init.contains_key(&new.name) {
                        (
                            Severity::Warning,
                            format!(
                                "`{}` is required, so an entry that predates it fails to decode \
                                 rather than defaulting. The migration sets it from `{}`, which \
                                 means the migration must run to completion before any code that \
                                 reads this field is promoted.",
                                new.name, plan.init[&new.name]
                            ),
                        )
                    } else {
                        (
                            Severity::Denied,
                            format!(
                                "`{}` is required, and a key absent from a serialized entry traps \
                                 the decoder instead of defaulting. Every entry written before this \
                                 field existed will panic on read. Declare `init[\"{}\"]`, or make \
                                 the field `Option<{}>`.",
                                new.name, new.name, new.base
                            ),
                        )
                    };
                    findings.push(Finding {
                        field: new.name.clone(),
                        kind: ChangeKind::AddedRequired,
                        severity,
                        detail,
                    });
                }
            }
            Some(old) => {
                if old.base != new.base {
                    declared.changed(&new.name);
                    let expression = plan.converted.get(&new.name);
                    let (severity, detail) = match expression {
                        Some(e) if !e.trim().is_empty() => (
                            Severity::Warning,
                            format!(
                                "`{}` changes type from `{}` to `{}`. The migration converts each \
                                 value with `{e}`; entries not yet converted decode as the new type \
                                 and will trap if the old encoding is not readable as it — run the \
                                 migration before promoting code that reads this field.",
                                new.name, old.declared, new.declared
                            ),
                        ),
                        _ => (
                            Severity::Denied,
                            format!(
                                "`{}` changes type from `{}` to `{}` with no conversion declared. \
                                 Old values will be read as the new type, which either traps or, \
                                 worse, decodes to a plausible wrong value. Declare \
                                 `converted[\"{}\"]` with an expression converting the old value.",
                                new.name, old.declared, new.declared, new.name
                            ),
                        ),
                    };
                    findings.push(Finding {
                        field: new.name.clone(),
                        kind: ChangeKind::Retyped {
                            from: old.declared.clone(),
                            to: new.declared.clone(),
                        },
                        severity,
                        detail,
                    });
                    continue;
                }

                if old.optional && !new.optional {
                    declared.changed(&new.name);
                    let (severity, detail) = if plan.init.contains_key(&new.name) {
                        (
                            Severity::Warning,
                            format!(
                                "`{}` became required. Entries written while it was optional may \
                                 have no value for it; the migration sets those to `{}`.",
                                new.name, plan.init[&new.name]
                            ),
                        )
                    } else {
                        (
                            Severity::Denied,
                            format!(
                                "`{}` became required. Any entry that has no value for it — every \
                                 entry written before the field existed, and any written while it \
                                 was optional and left unset — now traps on decode. Declare \
                                 `init[\"{}\"]`.",
                                new.name, new.name
                            ),
                        )
                    };
                    findings.push(Finding {
                        field: new.name.clone(),
                        kind: ChangeKind::BecameRequired,
                        severity,
                        detail,
                    });
                } else if !old.optional && new.optional {
                    findings.push(Finding {
                        field: new.name.clone(),
                        kind: ChangeKind::BecameOptional,
                        severity: Severity::Info,
                        detail: format!(
                            "`{}` became optional. This is the safe direction: an entry with no \
                             value for the field now decodes as `None` instead of failing.",
                            new.name
                        ),
                    });
                } else if old.declared != new.declared
                    && normalize_type(&old.declared) == normalize_type(&new.declared)
                {
                    findings.push(Finding {
                        field: new.name.clone(),
                        kind: ChangeKind::SpellingChanged,
                        severity: Severity::Info,
                        detail: format!(
                            "`{}` is written as `{}` instead of `{}`. Only the spelling changed, \
                             and the encoding is identical.",
                            new.name, new.declared, old.declared
                        ),
                    });
                }
            }
        }
    }

    // Removals last, so the report reads as "here is the new shape, here is what it
    // no longer has".
    for old in &from.fields {
        if to.field(&old.name).is_some() || declared.is_removed(&old.name) {
            continue;
        }
        if let Some(new_name) = plan.renamed_to(&old.name) {
            if to.field(new_name).is_some() {
                declared.removed(&old.name);
                continue;
            }
        }
        declared.removed(&old.name);
        let (severity, detail) = if plan.dropped.contains(&old.name) {
            (
                Severity::Info,
                format!(
                    "`{}` is intentionally discarded, declared in the plan's `dropped` list. Its \
                     values are gone once entries are rewritten, and no rollback can recover them.",
                    old.name
                ),
            )
        } else {
            (
                Severity::Denied,
                format!(
                    "`{}` exists in v{} but not v{}. Reading an old entry and writing it back \
                     discards it, silently: the decoder ignores keys the struct has no field for. \
                     Declare `dropped = [\"{}\"]` if the loss is intended, or rename it if the \
                     value moves to another field.",
                    old.name, from.version, to.version, old.name
                ),
            )
        };
        findings.push(Finding {
            field: old.name.clone(),
            kind: ChangeKind::Removed,
            severity,
            detail,
        });
    }

    // Reordering, reported only when the sets of names are identical: a reorder
    // alongside real changes is noise, and noise is how a report stops being read.
    if from.field_names() != to.field_names() && same_field_set(from, to) {
        findings.push(Finding {
            field: String::new(),
            kind: ChangeKind::Reordered,
            severity: Severity::Info,
            detail:
                "fields are declared in a different order. This has no effect: `#[contracttype]` \
                     structs encode as symbol-keyed maps, not as a positional sequence."
                    .into(),
        });
    }

    validate_plan(from, to, plan, &declared)?;

    let verdict = findings
        .iter()
        .map(|f| f.severity)
        .max()
        .map_or(Verdict::Safe, verdict_for);

    Ok(Diff {
        from_version: from.version,
        to_version: to.version,
        findings,
        verdict,
        plan_applied: !plan.is_empty(),
    })
}

fn verdict_for(severity: Severity) -> Verdict {
    match severity {
        Severity::Info => Verdict::Safe,
        Severity::Warning => Verdict::SafeWithLazyMigration,
        Severity::Denied => Verdict::Denied,
    }
}

fn same_field_set(from: &Schema, to: &Schema) -> bool {
    let mut a = from.field_names();
    let mut b = to.field_names();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

/// Which fields the walk has already accounted for, so the removals pass does not
/// report a rename twice.
#[derive(Default)]
struct DeclaredFields {
    added: Vec<String>,
    removed: Vec<String>,
    changed: Vec<String>,
}

impl DeclaredFields {
    fn added(&mut self, name: &str) {
        self.added.push(name.to_string());
    }

    fn removed(&mut self, name: &str) {
        self.removed.push(name.to_string());
    }

    fn changed(&mut self, name: &str) {
        self.changed.push(name.to_string());
    }

    fn is_removed(&self, name: &str) -> bool {
        self.removed.iter().any(|n| n == name)
    }
}

/// Rejects declarations that no change justifies.
///
/// See [`crate::plan`] for why a stale directive is an error rather than a
/// harmless leftover.
fn validate_plan(
    from: &Schema,
    to: &Schema,
    plan: &MigrationPlan,
    declared: &DeclaredFields,
) -> Result<(), SchemaError> {
    let stale = |directive: &str, field: &str| SchemaError::StaleDirective {
        directive: directive.to_string(),
        field: field.to_string(),
        from: from.version,
        to: to.version,
    };

    for field in plan.init.keys() {
        if !declared.added.contains(field) && !declared.changed.contains(field) {
            return Err(stale("init", field));
        }
    }
    for field in &plan.dropped {
        if !declared.removed.contains(field) {
            return Err(stale("dropped", field));
        }
    }
    for field in plan.converted.keys() {
        if !declared.changed.contains(field) {
            return Err(stale("converted", field));
        }
    }
    for (old, new) in &plan.renamed {
        if from.field(old).is_none() || to.field(new).is_none() {
            return Err(stale("renamed", old));
        }
    }
    Ok(())
}
