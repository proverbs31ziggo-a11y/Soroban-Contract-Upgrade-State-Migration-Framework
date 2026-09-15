//! `soroban-migrate plan`: draft the declarations a diff demands.
//!
//! # Why the draft exists, and why it is not a blank file
//!
//! A schema diff can see that a field disappeared. It cannot know whether that loss
//! was intended, or whether it is one half of a rename the author has not written
//! down yet. Every framework that has tried to guess here has guessed in the
//! direction of silent data loss, so this one refuses to guess at all — which would
//! leave every destructive change with a hand-written JSON file between it and a
//! migration.
//!
//! The draft removes that friction without removing the decision. It contains exactly
//! the declarations the diff asked for, each carrying a [`TODO`] placeholder where a
//! human has to decide something:
//!
//! * a **dropped** field, because only the author knows the loss is intended;
//! * an **init** expression for a field that became required, because there is no
//!   safe default and inventing one is the failure being prevented;
//! * a **conversion** for a retyped field, for the same reason.
//!
//! [`generate`](crate::commands::generate) refuses to emit code while any placeholder
//! remains, so the placeholder cannot reach a contract. The plan file itself is
//! written immediately, because a plan is a review artefact: a reviewer sees which
//! decisions a change requires before anyone makes them.

use soroban_migrate_schema::diff::{ChangeKind, Diff, Severity};
use soroban_migrate_schema::model::Schema;
use soroban_migrate_schema::plan::MigrationPlan;

use crate::error::{CliError, Result};
use crate::project::Project;

/// The marker a draft plan uses where a human must decide something.
///
/// Chosen so that leaving it in produces a *compile* error if it ever reached
/// generated code: it is not a Rust expression, and not a plausible one either. A
/// placeholder like `0` or `None` would compile, deploy, and write that value into
/// every entry — which is precisely the class of bug this whole tool exists to
/// prevent.
pub const TODO: &str = "<TODO";

/// Arguments for `plan`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Schema version to migrate from.
    pub from: u32,

    /// Schema version to migrate to. Must be `from + 1`.
    pub to: u32,

    /// The shape to migrate. Defaults to the configured schema.
    #[arg(long)]
    pub schema: Option<String>,

    /// Overwrite an existing plan file.
    #[arg(long)]
    pub force: bool,
}

/// Whether a plan value is still an unfilled placeholder.
pub fn is_unfilled(value: &str) -> bool {
    value.trim_start().starts_with(TODO)
}

/// Every declaration in `plan` that is still a placeholder, as `field` names.
pub fn unfilled_fields(plan: &MigrationPlan) -> Vec<String> {
    let mut out = Vec::new();
    for (field, value) in &plan.init {
        if is_unfilled(value) {
            out.push(format!("init[{field}]"));
        }
    }
    for (field, value) in &plan.converted {
        if is_unfilled(value) {
            out.push(format!("converted[{field}]"));
        }
    }
    out
}

/// Runs the command.
///
/// # Errors
///
/// [`CliError::Refused`] when a plan already exists and `--force` was not passed,
/// and [`CliError::Missing`] when the version pair has no committed schemas.
pub fn run(project: &Project, args: &Args) -> Result<()> {
    let committed = project.schema_set()?;
    let name = project.resolve_schema_name(args.schema.as_deref(), &committed)?;
    let (from_schema, to_schema) = pair(&committed, &name, args)?;

    let path = project.plan_path(&name, args.from, args.to);
    if path.is_file() && !args.force {
        return Err(CliError::Refused(format!(
            "{} already exists. Pass --force to replace it, or edit it in place.",
            path.display()
        )));
    }

    // Diffed with no plan at all, so every change that could be destructive shows up
    // as denied and gets a placeholder. Diffing with an existing plan would report
    // the declarations already made, and the draft would be missing the ones that
    // were never made.
    let bare = soroban_migrate_schema::diff::diff(from_schema, to_schema)?;
    let plan = draft(args.from, args.to, &bare);

    // Nothing to declare and nothing to review: writing an empty plan would create a
    // file whose only effect is to make the next person open it and wonder what it is
    // for. A migration is still possible — the derive covers an additive change — so
    // this is an answer, not a failure.
    if plan.is_empty() {
        println!(
            "v{} -> v{} needs no declarations: the diff has no change that could lose data or \
             make an entry unreadable. Run `soroban-migrate generate {} {}` to write the \
             migration.",
            args.from, args.to, args.from, args.to
        );
        return Ok(());
    }

    std::fs::create_dir_all(project.migrations_dir())
        .map_err(|e| CliError::io(project.migrations_dir().display(), e))?;
    std::fs::write(&path, plan.to_json()).map_err(|e| CliError::io(path.display(), e))?;

    println!("wrote {}", path.display());
    print_guidance(&plan);
    Ok(())
}

/// Builds a plan containing exactly what the diff asked for.
fn draft(from: u32, to: u32, diff: &Diff) -> MigrationPlan {
    let mut plan = MigrationPlan::new(from, to);
    for finding in &diff.findings {
        if finding.severity != Severity::Denied {
            continue;
        }
        match &finding.kind {
            ChangeKind::AddedRequired | ChangeKind::BecameRequired => {
                plan.init.insert(
                    finding.field.clone(),
                    format!(
                        "{TODO}: value for `{}`, for entries that predate it>",
                        finding.field
                    ),
                );
            }
            ChangeKind::Removed => {
                if !plan.dropped.contains(&finding.field) {
                    plan.dropped.push(finding.field.clone());
                }
            }
            ChangeKind::Retyped { from: old, to: new } => {
                plan.converted.insert(
                    finding.field.clone(),
                    format!("{TODO}: expression converting `{old}` to `{new}`>"),
                );
            }
            ChangeKind::AddedOptional
            | ChangeKind::Renamed { .. }
            | ChangeKind::BecameOptional
            | ChangeKind::SpellingChanged
            | ChangeKind::Reordered => {}
        }
    }
    plan
}

/// The schema pair a command names, with the one-step rule applied.
fn pair<'a>(
    committed: &'a soroban_migrate_schema::model::SchemaSet,
    name: &str,
    args: &Args,
) -> Result<(&'a Schema, &'a Schema)> {
    if args.to != args.from + 1 {
        return Err(CliError::Usage(format!(
            "v{} -> v{} skips versions. Migrations advance one version at a time, so a rollback \
             has an unambiguous target; declare the intermediate change as its own migration.",
            args.from, args.to
        )));
    }
    let from = committed.get(name, args.from).ok_or_else(|| {
        CliError::Missing(format!(
            "no committed schema for `{name}` v{}. Committed versions: {:?}. Run \
             `soroban-migrate schema export` if the shape was just declared.",
            args.from,
            committed.versions_of(name)
        ))
    })?;
    let to = committed.get(name, args.to).ok_or_else(|| {
        CliError::Missing(format!(
            "no committed schema for `{name}` v{}. Committed versions: {:?}.",
            args.to,
            committed.versions_of(name)
        ))
    })?;
    Ok((from, to))
}

fn print_guidance(plan: &MigrationPlan) {
    let unfilled = unfilled_fields(plan);
    println!("\nThis plan is a draft. Before it can generate code:");
    for entry in &unfilled {
        println!("  - replace the {TODO}...> placeholder in `{entry}`");
    }
    if !plan.dropped.is_empty() {
        println!(
            "  - confirm that dropping [{}] is intended. Those values are lost when entries are \
             rewritten, and no rollback can recover them.",
            plan.dropped.join(", ")
        );
    }
    if !plan.renamed.is_empty() {
        println!(
            "  - check the declared renames: {}",
            plan.renamed
                .iter()
                .map(|(old, new)| format!("{old} -> {new}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!(
        "\nA `dropped` field that actually moved to a new name is a rename, not a loss. If that \
         is what happened, delete it from `dropped` and add it to `renamed` instead — the diff \
         cannot tell the two apart, which is why it refuses to guess."
    );
    if !unfilled.is_empty() || plan.dropped.is_empty() {
        println!(
            "\nAdding an expression for an optional field's `init` is optional: an un-migrated \
             entry decodes it as `None`, which is safe. Declare it when `None` is not a \
             meaningful value for that field."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_migrate_schema::model::Field;

    fn schema(version: u32, fields: Vec<Field>) -> Schema {
        Schema::new(version, "Balance", fields)
    }

    fn field(name: &str, declared: &str) -> Field {
        Field::new(name, declared)
    }

    #[test]
    fn a_draft_carries_a_placeholder_for_every_required_decision() {
        let from = schema(1, vec![field("amount", "i64"), field("legacy", "u32")]);
        let to = schema(2, vec![field("amount", "i128"), field("frozen", "bool")]);
        let diff = soroban_migrate_schema::diff::diff(&from, &to).unwrap();
        let plan = draft(1, 2, &diff);

        assert!(is_unfilled(&plan.init["frozen"]));
        assert!(is_unfilled(&plan.converted["amount"]));
        assert_eq!(plan.dropped, vec!["legacy".to_string()]);
        assert_eq!(unfilled_fields(&plan).len(), 2);
    }

    #[test]
    fn a_draft_of_a_safe_change_asks_for_nothing() {
        let from = schema(1, vec![field("amount", "i128")]);
        let to = schema(
            2,
            vec![field("amount", "i128"), field("frozen", "Option<bool>")],
        );
        let diff = soroban_migrate_schema::diff::diff(&from, &to).unwrap();
        let plan = draft(1, 2, &diff);
        assert!(plan.is_empty());
        assert!(unfilled_fields(&plan).is_empty());
    }

    #[test]
    fn a_placeholder_is_not_a_plausible_rust_expression() {
        // The property that makes the placeholder safe: if one ever reached generated
        // code it would fail to compile, rather than writing a wrong value into every
        // entry.
        let plan = draft(
            1,
            2,
            &soroban_migrate_schema::diff::diff(
                &schema(1, vec![field("a", "i64")]),
                &schema(2, vec![field("a", "i128")]),
            )
            .unwrap(),
        );
        let value = &plan.converted["a"];
        assert!(value.contains(TODO));
        assert!(!value.starts_with("Some"));
        assert!(!value.chars().all(|c| c.is_alphanumeric() || c == '_'));
    }

    #[test]
    fn a_draft_with_nothing_to_declare_is_empty_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut config = crate::config::scaffold("balance", true);
        config.schema = Some("Balance".into());
        config.save(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("src/schema.rs"),
            "#[storage_schema(version = 1, name = \"Balance\")]\n\
             #[contracttype]\n\
             pub struct BalanceV1 { pub amount: i128 }\n\n\
             #[storage_schema(version = 2, name = \"Balance\")]\n\
             #[contracttype]\n\
             pub struct BalanceV2 { pub amount: i128, pub frozen: Option<bool> }\n",
        )
        .unwrap();
        let project = Project::open(dir.path()).unwrap();
        crate::commands::schema::export_summary(&project, false).unwrap();
        run(
            &project,
            &Args {
                from: 1,
                to: 2,
                schema: None,
                force: false,
            },
        )
        .unwrap();
        assert!(!project.plan_path("Balance", 1, 2).exists());
    }

    #[test]
    fn unfilled_finds_placeholders_but_not_real_expressions() {
        assert!(is_unfilled("<TODO: whatever>"));
        assert!(is_unfilled("  <TODO>"));
        assert!(!is_unfilled("false"));
        assert!(!is_unfilled("i128::from(amount)"));
        assert!(!is_unfilled(""));
    }
}
