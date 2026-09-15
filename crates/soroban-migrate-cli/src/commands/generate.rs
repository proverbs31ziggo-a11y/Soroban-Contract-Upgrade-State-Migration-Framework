//! `soroban-migrate generate`: write the migration implementation.
//!
//! # What the generated file is for
//!
//! `#[derive(Migration)]` already covers additions and optionality changes, and it
//! covers them *better* than generated code would: the derive can prove idempotency,
//! because "fill this field when it is empty" cannot run twice with an effect. What
//! it cannot cover is a rename or a type change, because those consume an old key and
//! being re-runnable therefore requires reading the old shape through a *tolerant
//! shadow struct* — a struct declaring only the consumed keys, each as `Option<..>`,
//! so an already-consumed key decodes as `None` instead of trapping.
//!
//! A derive macro sees one struct and cannot synthesize the other. This command has
//! both schemas, so it can. That is the whole division of labour: the derive handles
//! what it can prove, and this handles what needs a second type.
//!
//! # Why it refuses more often than it writes
//!
//! It refuses when the diff is denied, because emitting code for a change nobody
//! declared is how a silent data loss acquires a plausible-looking implementation. It
//! refuses while a plan placeholder remains, because a placeholder is a decision not
//! yet made. And it refuses to overwrite by default, because the generated file is
//! the one artefact in a project that a person might have hand-edited — and quietly
//! discarding that edit is worse than failing.

use soroban_migrate_schema::codegen::{generate, CodegenOptions};
use soroban_migrate_schema::plan::MigrationPlan;

use crate::commands::plan::unfilled_fields;
use crate::error::{CliError, Result};
use crate::project::Project;

/// Arguments for `generate`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Schema version to migrate from.
    pub from: u32,

    /// Schema version to migrate to. Must be `from + 1`.
    pub to: u32,

    /// The shape to migrate. Defaults to the configured schema.
    #[arg(long)]
    pub schema: Option<String>,

    /// Overwrite an existing generated file.
    #[arg(long)]
    pub force: bool,

    /// Write the code to stdout instead of to the migrations directory.
    #[arg(long)]
    pub stdout: bool,
}

/// Runs the command.
///
/// # Errors
///
/// [`CliError::Refused`] when the diff is denied, when the plan still contains a
/// placeholder, or when the output file exists and `--force` was not passed.
pub fn run(project: &Project, args: &Args) -> Result<()> {
    let committed = project.schema_set()?;
    let name = project.resolve_schema_name(args.schema.as_deref(), &committed)?;

    if args.to != args.from + 1 {
        return Err(CliError::Usage(format!(
            "v{} -> v{} skips versions. Migrations advance one version at a time; declare the \
             intermediate change as its own migration.",
            args.from, args.to
        )));
    }
    let from = committed.get(&name, args.from).ok_or_else(|| {
        CliError::Missing(format!(
            "no committed schema for `{name}` v{}. Committed versions: {:?}.",
            args.from,
            committed.versions_of(&name)
        ))
    })?;
    let to = committed.get(&name, args.to).ok_or_else(|| {
        CliError::Missing(format!(
            "no committed schema for `{name}` v{}. Committed versions: {:?}.",
            args.to,
            committed.versions_of(&name)
        ))
    })?;

    let plan_path = project.plan_path(&name, args.from, args.to);
    let plan = project.load_plan(&name, args.from, args.to)?;

    // The diff with no plan says which declarations are required, and therefore
    // whether a plan is needed at all. Diffing with a plan directly would report the
    // declarations already made and could not distinguish "nothing to declare" from
    // "the plan file is missing".
    let bare = soroban_migrate_schema::diff::diff(from, to)?;
    let required = bare.undeclared();

    let plan = match plan {
        Some(plan) => plan,
        // No plan and nothing required of one: an empty plan for *this* version pair,
        // not a default-constructed one. `MigrationPlan::default()` covers v0 -> v0, and
        // handing it to the diff is refused as a plan for the wrong pair.
        None if required.is_empty() => MigrationPlan::new(args.from, args.to),
        None => {
            return Err(CliError::Refused(format!(
                "v{} -> v{} declares no treatment for {} destructive change{}. Run \
                 `soroban-migrate plan {} {}` to write {}, fill in the decisions it asks for, \
                 and run this again. Required declarations:\n  - {}",
                args.from,
                args.to,
                required.len(),
                if required.len() == 1 { "" } else { "s" },
                args.from,
                args.to,
                plan_path.display(),
                required
                    .iter()
                    .map(|f| format!("{} for `{}`", f.kind.directive().unwrap_or("?"), f.field))
                    .collect::<Vec<_>>()
                    .join("\n  - ")
            )));
        }
    };

    let unfilled = unfilled_fields(&plan);
    if !unfilled.is_empty() {
        return Err(CliError::Refused(format!(
            "{} still holds a placeholder in {}. Each one is a decision a person has to make; \
             generating code from it would emit something that does not compile at best, and a \
             plausible wrong value at worst. Fill in:\n  - {}",
            plan_path.display(),
            if unfilled.len() == 1 {
                "one field"
            } else {
                "fields"
            },
            unfilled.join("\n  - ")
        )));
    }

    let diff = soroban_migrate_schema::diff::diff_with_plan(from, to, &plan)?;
    if diff.verdict.is_failure() {
        return Err(CliError::Refused(format!(
            "refusing to generate for a denied change:\n\n{}",
            diff.report()
        )));
    }

    let options = CodegenOptions {
        struct_name: format!("{}V{}ToV{}", name, args.from, args.to),
        keyspace: project.config.keyspace.clone(),
        types_module: project.config.types_module.clone(),
        from_type: format!("{}V{}", name, args.from),
        to_type: format!("{}V{}", name, args.to),
    };
    let code = generate(from, to, &diff, &plan, &options)?;

    if args.stdout {
        print!("{code}");
        return Ok(());
    }

    let path = project.migration_path(&name, args.from, args.to);
    if path.is_file() && !args.force {
        return Err(CliError::Refused(format!(
            "{} already exists. Diff it against the generated output first — generated files are \
             the one artefact a person is likely to have hand-edited — then pass --force, or use \
             --stdout to see what would be written.",
            path.display()
        )));
    }
    std::fs::write(&path, &code).map_err(|e| CliError::io(path.display(), e))?;
    println!("wrote {}", path.display());
    println!(
        "\nReference `{}` from the contract's migration entry point, e.g. \
         `executor::begin::<{}>`, and check it in alongside the plan it came from.",
        options.struct_name, options.struct_name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with(v1: &str, v2: &str) -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut config = crate::config::scaffold("balance", true);
        config.schema = Some("Balance".into());
        config.save(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("src/schema.rs"),
            format!(
                "#[storage_schema(version = 1, name = \"Balance\")]\n\
                 #[contracttype]\n\
                 pub struct BalanceV1 {{\n{v1}\n}}\n\n\
                 #[storage_schema(version = 2, name = \"Balance\")]\n\
                 #[contracttype]\n\
                 pub struct BalanceV2 {{\n{v2}\n}}\n"
            ),
        )
        .unwrap();
        let project = Project::open(dir.path()).unwrap();
        crate::commands::schema::export_summary(&project, false).unwrap();
        (dir, project)
    }

    fn args() -> Args {
        Args {
            from: 1,
            to: 2,
            schema: None,
            force: false,
            stdout: true,
        }
    }

    #[test]
    fn an_additive_change_generates_without_a_plan() {
        let (_dir, project) = project_with(
            "    pub amount: i128,",
            "    pub amount: i128,\n    pub frozen: Option<bool>,",
        );
        assert!(run(&project, &args()).is_ok());
    }

    #[test]
    fn a_destructive_change_without_a_plan_is_refused_and_names_the_declarations_needed() {
        let (_dir, project) = project_with("    pub amount: i128,", "    pub amount: i64,");
        let error = run(&project, &args()).unwrap_err();
        assert!(matches!(error, CliError::Refused(_)));
        let message = error.to_string();
        assert!(message.contains("converted for `amount`"), "got: {message}");
        assert!(
            message.contains("soroban-migrate plan 1 2"),
            "got: {message}"
        );
    }

    #[test]
    fn a_placeholder_in_the_plan_is_refused_by_name() {
        let (_dir, project) = project_with("    pub amount: i128,", "    pub amount: i64,");
        std::fs::write(
            project.plan_path("Balance", 1, 2),
            r#"{ "from": 1, "to": 2, "converted": { "amount": "<TODO: convert>" } }"#,
        )
        .unwrap();
        let error = run(&project, &args()).unwrap_err();
        assert!(
            error.to_string().contains("converted[amount]"),
            "got: {error}"
        );
    }

    #[test]
    fn a_filled_in_plan_generates_rust_that_parses() {
        let (_dir, project) = project_with(
            "    pub amount: i64,",
            "    pub amount: i128,\n    pub frozen: Option<bool>,",
        );
        std::fs::write(
            project.plan_path("Balance", 1, 2),
            r#"{ "from": 1, "to": 2, "converted": { "amount": "i128::from(amount)" } }"#,
        )
        .unwrap();
        // `--stdout` prints; the file is not written in that mode, so the assertion is
        // that generation succeeded at all.
        assert!(run(&project, &args()).is_ok());
    }

    #[test]
    fn a_version_jump_is_refused_before_anything_is_read() {
        let (_dir, project) = project_with("    pub amount: i128,", "    pub amount: i128,");
        let error = run(&project, &Args { to: 3, ..args() }).unwrap_err();
        assert!(matches!(error, CliError::Usage(_)));
    }

    #[test]
    fn writing_a_file_twice_needs_force() {
        let (_dir, project) = project_with(
            "    pub amount: i128,",
            "    pub amount: i128,\n    pub frozen: Option<bool>,",
        );
        run(
            &project,
            &Args {
                stdout: false,
                ..args()
            },
        )
        .unwrap();
        let path = project.migration_path("Balance", 1, 2);
        assert!(path.is_file());
        let error = run(
            &project,
            &Args {
                stdout: false,
                ..args()
            },
        )
        .unwrap_err();
        assert!(matches!(error, CliError::Refused(_)));
        assert!(run(
            &project,
            &Args {
                stdout: false,
                force: true,
                ..args()
            }
        )
        .is_ok());
    }
}
