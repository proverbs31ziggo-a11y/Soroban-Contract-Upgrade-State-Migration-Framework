//! `soroban-migrate schema`: snapshot, list, and inspect storage shapes.
//!
//! # Why `export --check` is a separate mode rather than a separate command
//!
//! Snapshotting and verifying that the snapshots match the source are the same
//! *computation* with opposite side effects, and keeping them one code path is what
//! makes the CI check trustworthy: a `--check` implemented separately can pass while
//! `export` would have written something different. The failure it catches is worth
//! naming: a developer edits a struct's shape, forgets to bump its version, and the
//! committed snapshot silently stops describing the code. Then `check` diffs two
//! snapshots that are both stale and reports "no changes".

use std::path::Path;

use soroban_migrate_schema::model::Schema;

use crate::error::{CliError, Result};
use crate::project::{Project, WriteOutcome};

/// Arguments for `schema`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The shape to act on. Defaults to the configured schema, or the only one.
    #[arg(long, global = true)]
    pub schema: Option<String>,

    /// Print snapshots as JSON instead of a table.
    #[arg(long, global = true)]
    pub json: bool,

    /// What to do.
    #[command(subcommand)]
    pub command: Sub,
}

/// The `schema` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum Sub {
    /// Write a snapshot for every `#[storage_schema]` declaration in the source.
    Export(ExportArgs),
    /// List the shapes and versions the repository has committed.
    List,
    /// Print one committed shape.
    Show(ShowArgs),
}

/// Arguments for `schema export`.
#[derive(Debug, clap::Args)]
pub struct ExportArgs {
    /// Write nothing; fail if any snapshot is missing or out of date.
    #[arg(long)]
    pub check: bool,
}

/// Arguments for `schema show`.
#[derive(Debug, clap::Args)]
pub struct ShowArgs {
    /// The version to print. Defaults to the shape's latest.
    #[arg(long)]
    pub version: Option<u32>,
}

/// Runs the command.
///
/// # Errors
///
/// Propagates parse failures, version conflicts, and — under `--check` — the
/// [`CliError::Refused`] that reports drift.
pub fn run(project: &Project, args: &Args) -> Result<()> {
    match &args.command {
        Sub::Export(export) => export_schemas(project, export.check),
        Sub::List => list(project, args.schema.as_deref(), args.json),
        Sub::Show(show_args) => show(
            project,
            args.schema.as_deref(),
            show_args.version,
            args.json,
        ),
    }
}

/// Snapshots every declared shape that is not already committed, and reports what it
/// found.
///
/// Returns nothing on success; the caller is `init`, which wants the same report.
///
/// # Errors
///
/// [`CliError::Refused`] when a shape's version is already committed with a different
/// shape — the version must be bumped — and, under `check`, when anything at all
/// would have been written.
pub fn export_summary(project: &Project, check: bool) -> Result<()> {
    export_schemas(project, check)
}

fn export_schemas(project: &Project, check: bool) -> Result<()> {
    let declared = project.schemas_from_source()?;
    let committed = project.schema_set()?;

    let names: Vec<String> = declared.names().map(str::to_string).collect();
    if names.is_empty() {
        return Err(CliError::Missing(format!(
            "no `#[storage_schema]` declaration found in the source paths configured in {}. A \
             shape is declared with `#[derive(StorageSchema)]` and \
             `#[storage_schema(version = N)]` on a struct.",
            crate::config::FILE_NAME
        )));
    }

    let mut written = 0usize;
    let mut unchanged = 0usize;
    let mut drift: Vec<String> = Vec::new();

    for name in &names {
        for version in declared.versions_of(name) {
            let schema = declared
                .get(name, version)
                .expect("version came from the set");
            let outcome = if check {
                match committed.get(name, version) {
                    Some(existing) if existing == schema => {
                        WriteOutcome::Unchanged(project.schema_path(name, version))
                    }
                    Some(_) => {
                        drift.push(format!(
                            "{} v{version} changed in the source without its version changing; \
                             bump `#[storage_schema(version = N)]`",
                            schema.name
                        ));
                        WriteOutcome::Written(project.schema_path(name, version))
                    }
                    None => {
                        drift.push(format!(
                            "{} v{version} is declared in the source but not committed",
                            schema.name
                        ));
                        WriteOutcome::Written(project.schema_path(name, version))
                    }
                }
            } else {
                project.write_schema(schema, false)?
            };
            let path = relative(outcome.path(), project);
            if !outcome.changed() {
                unchanged += 1;
                if !check {
                    println!("  unchanged {path}");
                }
            } else if check {
                println!("  would write {path}");
            } else {
                written += 1;
                println!("  wrote {path}");
            }
        }
    }

    // A committed snapshot with no declaration in the source is normal and not an
    // error: deleting the old struct once its migration has shipped is the intended
    // end state, and the snapshot is kept precisely so the migration can still be
    // audited. It is reported because it is also what a *typo* in the version number
    // looks like.
    for name in committed.names() {
        for version in committed.versions_of(name) {
            if declared.get(name, version).is_none() {
                println!(
                    "  {name} v{version} is committed but no longer declared in the source; the \
                     snapshot is kept so the migration into it stays checkable"
                );
            }
        }
    }

    if check {
        if drift.is_empty() {
            println!(
                "{} snapshots up to date across {} shape{}",
                unchanged,
                names.len(),
                if names.len() == 1 { "" } else { "s" }
            );
            return Ok(());
        }
        return Err(CliError::Refused(format!(
            "{} snapshot{} out of date (run `soroban-migrate schema export`):\n  - {}",
            drift.len(),
            if drift.len() == 1 { "" } else { "s" },
            drift.join("\n  - ")
        )));
    }

    println!(
        "{written} written, {unchanged} unchanged, {} shape{}",
        names.len(),
        if names.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

fn list(project: &Project, explicit: Option<&str>, json: bool) -> Result<()> {
    let set = project.schema_set()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&set).expect("a schema set always serializes")
        );
        return Ok(());
    }

    let names: Vec<String> = match explicit {
        Some(name) => vec![name.to_string()],
        None => set.names().map(str::to_string).collect(),
    };
    if names.is_empty() {
        println!(
            "no committed schemas in {}",
            project.migrations_dir().display()
        );
        return Ok(());
    }

    for name in &names {
        let versions = set.versions_of(name);
        if versions.is_empty() {
            return Err(CliError::Missing(format!(
                "no committed schema named `{name}` in {}",
                project.migrations_dir().display()
            )));
        }
        println!("{name}: {}", join_versions(&versions));
        for pair in set.consecutive_pairs(name) {
            let (from, to) = pair;
            let plan = if project.plan_path(name, from, to).is_file() {
                "plan"
            } else {
                "no plan"
            };
            let migration = if project.migration_path(name, from, to).is_file() {
                "migration"
            } else {
                "no migration"
            };
            println!("  v{from} -> v{to}: {plan}, {migration}");
        }
        for (from, to) in set.gaps_of(name) {
            println!(
                "  v{from} -> v{to}: SKIPS VERSIONS. Add the intermediate shapes: the executor \
                 advances one version at a time and a rollback needs an unambiguous target."
            );
        }
    }
    Ok(())
}

fn show(project: &Project, explicit: Option<&str>, version: Option<u32>, json: bool) -> Result<()> {
    let set = project.schema_set()?;
    let name = project.resolve_schema_name(explicit, &set)?;
    let version = match version {
        Some(v) => v,
        None => *set.versions_of(&name).last().ok_or_else(|| {
            CliError::Missing(format!(
                "no committed schema named `{name}` in {}",
                project.migrations_dir().display()
            ))
        })?,
    };
    let schema: &Schema = set.get(&name, version).ok_or_else(|| {
        CliError::Missing(format!(
            "no committed schema for `{name}` v{version}. Committed versions: {}.",
            join_versions(&set.versions_of(&name))
        ))
    })?;

    if json {
        print!("{}", schema.to_json());
    } else {
        print!("{}", schema.summary());
        if let Some(note) = &schema.note {
            println!("note: {note}");
        }
    }
    Ok(())
}

/// Renders a version list the way a person would say it: `1, 2, 3` and `1-3`.
fn join_versions(versions: &[u32]) -> String {
    if versions.is_empty() {
        return "none".into();
    }
    if versions.len() > 2 {
        let mut contiguous = true;
        for pair in versions.windows(2) {
            if pair[1] != pair[0] + 1 {
                contiguous = false;
                break;
            }
        }
        if contiguous {
            return format!("{}-{}", versions[0], versions[versions.len() - 1]);
        }
    }
    versions
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A path as the operator would type it, relative to the project root.
fn relative(path: &Path, project: &Project) -> String {
    path.strip_prefix(&project.root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_lists_collapse_only_when_contiguous() {
        assert_eq!(join_versions(&[]), "none");
        assert_eq!(join_versions(&[1]), "1");
        assert_eq!(join_versions(&[1, 2]), "1, 2");
        assert_eq!(join_versions(&[1, 2, 3]), "1-3");
        assert_eq!(join_versions(&[1, 2, 4]), "1, 2, 4");
    }

    fn project_with_schema() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        crate::config::scaffold("balance", true)
            .save(dir.path())
            .unwrap();
        std::fs::write(
            dir.path().join("src/schema.rs"),
            r"
#[storage_schema(version = 1)]
#[contracttype]
pub struct Balance {
    pub owner: Address,
    pub amount: i128,
}
",
        )
        .unwrap();
        let project = Project::open(dir.path()).unwrap();
        (dir, project)
    }

    #[test]
    fn export_writes_a_snapshot_then_reports_it_unchanged() {
        let (_dir, project) = project_with_schema();
        export_schemas(&project, false).unwrap();
        assert!(project.schema_path("Balance", 1).is_file());

        // A second export must not fail or rewrite: it is the state every commit and
        // every CI run is in.
        export_schemas(&project, true).unwrap();
    }

    #[test]
    fn a_check_fails_when_a_declared_shape_was_never_snapshotted() {
        let (_dir, project) = project_with_schema();
        let error = export_schemas(&project, true).unwrap_err();
        assert!(matches!(error, CliError::Refused(_)));
        assert!(error.to_string().contains("not committed"), "got: {error}");
    }

    #[test]
    fn a_snapshot_that_no_longer_matches_the_source_is_reported_as_drift() {
        let (dir, project) = project_with_schema();
        export_schemas(&project, false).unwrap();
        // The same version, a different shape. `check` must not silently accept it.
        std::fs::write(
            dir.path().join("src/schema.rs"),
            r"
#[storage_schema(version = 1)]
#[contracttype]
pub struct Balance {
    pub owner: Address,
    pub amount: i64,
}
",
        )
        .unwrap();
        let error = export_schemas(&project, true).unwrap_err();
        assert!(error.to_string().contains("without its version changing"));
    }

    #[test]
    fn a_source_tree_with_no_declarations_says_what_one_looks_like() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        crate::config::scaffold("vault", true)
            .save(dir.path())
            .unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn nothing() {}\n").unwrap();
        let project = Project::open(dir.path()).unwrap();
        let error = export_schemas(&project, false).unwrap_err();
        assert!(error.to_string().contains("storage_schema"), "got: {error}");
    }
}
