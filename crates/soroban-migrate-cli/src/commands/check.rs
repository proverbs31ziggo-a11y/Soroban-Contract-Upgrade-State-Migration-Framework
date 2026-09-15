//! `soroban-migrate check`: the compatibility gate.
//!
//! # What this refuses, and why it is the point of the tool
//!
//! Every change it blocks is one the Rust compiler accepts. Two kinds of silent
//! damage are possible when a storage shape changes, and neither is a build error:
//!
//! * A **new required field** makes every entry written before it *trap* on decode.
//!   CAP-86's relaxed unpacking fills a missing map key with nothing, and only an
//!   `Option` field turns that into `None`. The contract keeps compiling, deploys,
//!   and then panics on every call that touches an entry that predates the field.
//! * A **removed field** is discarded when an entry is read and written back, because
//!   `#[contracttype]` ignores map keys the struct has no field for. The contract
//!   keeps compiling, deploys, and quietly loses that column the first time anything
//!   writes the entry back.
//!
//! `check` walks every consecutive version pair the repository has committed, applies
//! that pair's plan if one exists, and fails when a change is neither declared nor
//! safe. It also verifies that the committed snapshots still describe the source,
//! because a stale snapshot is how a check passes while the code no longer matches:
//! the diff then compares two snapshots that are *both* out of date and reports that
//! nothing changed.
//!
//! # What "denied" costs
//!
//! Nothing but the merge. A denied change is refused at a point where all that exists
//! is a snapshot file and a plan file, which is the only point at which a schema
//! change is cheap to reconsider.

use soroban_migrate_schema::diff::{ChangeKind, Diff, Finding, Severity, Verdict};
use soroban_migrate_schema::model::SchemaSet;

use crate::error::{CliError, Result};
use crate::project::Project;

/// The severity at which `check` fails the build.
///
/// Ordered, so `--fail-on=warn` is a comparison against each finding rather than a
/// second code path — and so a team can tighten the gate without changing what the
/// tool computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FailOn {
    /// Fail only on a change that loses data or makes entries unreadable.
    Deny,
    /// Also fail on a change that requires the migration to have run.
    Warn,
    /// Also fail on anything worth knowing.
    Info,
}

impl FailOn {
    fn threshold(self) -> Severity {
        match self {
            FailOn::Deny => Severity::Denied,
            FailOn::Warn => Severity::Warning,
            FailOn::Info => Severity::Info,
        }
    }

    fn label(self) -> &'static str {
        match self {
            FailOn::Deny => "deny",
            FailOn::Warn => "warn",
            FailOn::Info => "info",
        }
    }
}

/// Arguments for `check`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The shape to check. Defaults to the configured schema, or every committed one.
    #[arg(long)]
    pub schema: Option<String>,

    /// Check only the migration starting at this version. Requires `--to`.
    #[arg(long, requires = "to")]
    pub from: Option<u32>,

    /// Check only the migration ending at this version. Requires `--from`.
    #[arg(long, requires = "from")]
    pub to: Option<u32>,

    /// Fail on findings at or above this severity.
    #[arg(long, value_enum, default_value_t = FailOn::Deny)]
    pub fail_on: FailOn,

    /// Emit machine-readable findings instead of a report.
    #[arg(long)]
    pub json: bool,

    /// Check only the committed snapshots, without comparing them to the source.
    #[arg(long)]
    pub no_source: bool,
}

/// Runs the command.
///
/// # Errors
///
/// [`CliError::Refused`] when a finding reaches the configured severity, and
/// [`CliError::Missing`] when the named shape or version pair does not exist.
pub fn run(project: &Project, args: &Args) -> Result<()> {
    let committed = project.schema_set()?;
    if committed.names().next().is_none() {
        return Err(CliError::Missing(format!(
            "no committed schemas in {}. Run `soroban-migrate schema export` first.",
            project.migrations_dir().display()
        )));
    }

    let mut report = Report::default();
    if !args.no_source {
        collect_drift(project, &committed, &mut report);
    }
    collect_version_suffix_families(&committed, &mut report);

    let names = select_names(project, args, &committed)?;
    for name in &names {
        report
            .migrations
            .extend(check_shape(project, &committed, name, args)?);
    }

    if args.json {
        report.print_json();
    } else {
        report.print();
    }

    let threshold = args.fail_on.threshold();
    let worst = report.worst_severity();
    if worst >= threshold {
        let count = report.count_at_least(threshold);
        return Err(CliError::Refused(format!(
            "{count} finding{} at or above `{}`. Nothing has been changed: fix the findings \
             above, or run `soroban-migrate plan` and `soroban-migrate generate` to declare what \
             the migration does.",
            if count == 1 { "" } else { "s" },
            args.fail_on.label()
        )));
    }
    Ok(())
}

/// The shapes to examine.
///
/// With `--from`/`--to` the subject has to be a single shape, because a version pair
/// is meaningless across shapes: v2 of `Balance` and v2 of `Stats` are unrelated
/// numbers, and diffing one against the other would compare a shape to itself.
fn select_names(project: &Project, args: &Args, committed: &SchemaSet) -> Result<Vec<String>> {
    if let Some(name) = &args.schema {
        return Ok(vec![name.clone()]);
    }
    if args.from.is_some() {
        return Ok(vec![project.resolve_schema_name(None, committed)?]);
    }
    Ok(committed.names().map(str::to_string).collect())
}

/// Records a finding for every way the committed snapshots and the source disagree.
///
/// Both directions matter, and the diff cannot see either: it compares two snapshots,
/// so it has no way to know that a snapshot is not what the code says.
fn collect_drift(project: &Project, committed: &SchemaSet, report: &mut Report) {
    let declared = match project.schemas_from_source() {
        Ok(declared) => declared,
        Err(error) => {
            // A source that will not parse is `schema export`'s diagnostic to give,
            // but `check` must not pass silently on it.
            report.drift.push(Drift {
                shape: "(source)".into(),
                version: 0,
                detail: format!(
                    "the source could not be parsed, so the committed snapshots could not be \
                     compared against it: {error}"
                ),
            });
            return;
        }
    };

    for name in declared.names() {
        for version in declared.versions_of(name) {
            let schema = declared.get(name, version).expect("from the set");
            match committed.get(name, version) {
                Some(existing) if existing == schema => {}
                Some(_) => report.drift.push(Drift {
                    shape: name.to_string(),
                    version,
                    detail: "changed in the source without its version changing. Bump \
                             `#[storage_schema(version = N)]` and the version the contract \
                             records with it, or this snapshot stops describing the code."
                        .into(),
                }),
                None => report.drift.push(Drift {
                    shape: name.to_string(),
                    version,
                    detail: "is declared in the source but has no committed snapshot. Run \
                             `soroban-migrate schema export`."
                        .into(),
                }),
            }
        }
    }
}

/// Denies shapes that are versions of each other but were never unified.
///
/// # The mistake this catches, and why nothing else can
///
/// A shape's name defaults to the *struct's* name, and versioned structs cannot all
/// be called `Balance` — so the natural thing is to call them `BalanceV1` and
/// `BalanceV2`. That registers two unrelated shapes, each with exactly one version,
/// and the consequence is silent: there is no version pair, so there is nothing to
/// diff, so `check` reports success and a migration that covers the change does not
/// exist. The version numbers are right there in the names, which is what makes it so
/// easy to miss that the *shape* identity was never declared.
///
/// The pattern is not guessed. Two shapes whose names differ only by a trailing
/// `V<digits>` are versions of one shape by construction: a storage struct is named
/// that way for no other reason. Refusing is cheap — the fix is one attribute — and
/// the alternative is a tool that reports "safe" about a migration that will not run.
fn collect_version_suffix_families(committed: &SchemaSet, report: &mut Report) {
    let mut families: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for name in committed.names() {
        if let Some(stem) = version_suffix_stem(name) {
            families.entry(stem).or_default().push(name.to_string());
        }
    }

    for (stem, members) in families {
        if members.len() < 2 {
            continue;
        }
        report.drift.push(Drift {
            shape: members.join(", "),
            version: 0,
            detail: format!(
                "these are versions of one shape named `{stem}`, but each registers as a shape \
                 of its own, so there is no version pair and no migration will ever be checked. \
                 Declare the shared identity: `#[storage_schema(version = N, name = \"{stem}\")]` \
                 on each of them, then re-run `soroban-migrate schema export`."
            ),
        });
    }
}

/// `BalanceV2` becomes `Balance`; a name without a trailing version is left alone.
fn version_suffix_stem(name: &str) -> Option<String> {
    let split = name.rfind('V')?;
    let (stem, suffix) = name.split_at(split);
    let digits = &suffix[1..];
    if stem.is_empty() || digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(stem.to_string())
}

/// A shape-and-version pair whose snapshot no longer matches the source.
#[derive(Debug, Clone, serde::Serialize)]
struct Drift {
    shape: String,
    version: u32,
    detail: String,
}

/// The checked status of one migration.
#[derive(Debug, Clone, serde::Serialize)]
struct MigrationCheck {
    shape: String,
    from: u32,
    to: u32,
    verdict: Verdict,
    plan: Option<String>,
    generated: Option<String>,
    diff: Diff,
    /// Findings about the *repository* rather than the diff — a missing generated
    /// migration, say. Kept separate so `diff` stays comparable to what the schema
    /// crate produced on its own.
    notes: Vec<Note>,
}

/// A repository-level observation attached to a migration.
#[derive(Debug, Clone, serde::Serialize)]
struct Note {
    severity: Severity,
    detail: String,
}

/// Everything `check` found.
#[derive(Debug, Default)]
struct Report {
    drift: Vec<Drift>,
    migrations: Vec<MigrationCheck>,
}

impl Report {
    fn worst_severity(&self) -> Severity {
        let mut worst = if self.drift.is_empty() {
            Severity::Info
        } else {
            Severity::Denied
        };
        for migration in &self.migrations {
            for finding in &migration.diff.findings {
                worst = worst.max(finding.severity);
            }
            for note in &migration.notes {
                worst = worst.max(note.severity);
            }
        }
        worst
    }

    fn count_at_least(&self, threshold: Severity) -> usize {
        let mut count = if self.drift.is_empty() {
            0
        } else {
            self.drift.len()
        };
        for migration in &self.migrations {
            count += migration.diff.findings_at_least(threshold).len();
            count += migration
                .notes
                .iter()
                .filter(|n| n.severity >= threshold)
                .count();
        }
        count
    }

    fn print(&self) {
        crate::output::heading("storage schema compatibility");

        if !self.drift.is_empty() {
            crate::output::subheading("the committed snapshots do not describe the source");
            for drift in &self.drift {
                println!(
                    "  DENY  {} v{}: {}",
                    drift.shape, drift.version, drift.detail
                );
            }
            println!();
            println!(
                "  Nothing below is trustworthy until this is fixed: the diffs compare snapshots, \
                 and these snapshots are not the code."
            );
        }

        if self.migrations.is_empty() {
            println!("\nno version pairs to check");
        }

        for migration in &self.migrations {
            crate::output::subheading(&format!(
                "{} v{} -> v{}",
                migration.shape, migration.from, migration.to
            ));
            println!("  verdict:  {}", verdict_label(migration.verdict));
            match &migration.plan {
                Some(path) => println!("  plan:     {path}"),
                None => println!("  plan:     none (every destructive change is denied)"),
            }
            match &migration.generated {
                Some(path) => println!("  generated: {path}"),
                None => println!("  generated: none"),
            }
            if migration.diff.findings.is_empty() {
                println!("  findings: none");
            }
            for finding in &migration.diff.findings {
                let tag = finding.severity.tag();
                if finding.field.is_empty() {
                    println!("  {tag:<5} {}", finding.kind.describe());
                } else {
                    println!(
                        "  {tag:<5} {:<20} {}",
                        finding.field,
                        finding.kind.describe()
                    );
                }
                println!("{}", crate::output::indented(&finding.detail, "          "));
            }
            for note in &migration.notes {
                println!("  {:<5} {}", note.severity.tag(), note.detail);
            }
        }

        println!();
        crate::output::subheading("summary");
        let denied = self
            .migrations
            .iter()
            .filter(|m| m.verdict.is_failure())
            .count();
        println!(
            "  {} version pair{}, {} denied, {} broken snapshot{}",
            self.migrations.len(),
            if self.migrations.len() == 1 { "" } else { "s" },
            denied,
            self.drift.len(),
            if self.drift.len() == 1 { "" } else { "s" },
        );
        println!("  verdict: {}", severity_label(self.worst_severity()));
    }

    fn print_json(&self) {
        let payload = serde_json::json!({
            "drift": self.drift,
            "migrations": self.migrations,
            "verdict": self.worst_severity(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).expect("the report always serializes")
        );
    }
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Safe => "safe",
        Verdict::SafeWithLazyMigration => "safe, with a lazy migration",
        Verdict::Denied => "DENIED",
    }
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "safe",
        Severity::Warning => "safe, with warnings",
        Severity::Denied => "DENIED",
    }
}

/// Diffs every consecutive pair of one shape, plus a denial for any version jump.
fn check_shape(
    project: &Project,
    committed: &SchemaSet,
    name: &str,
    args: &Args,
) -> Result<Vec<MigrationCheck>> {
    if committed.versions_of(name).is_empty() {
        return Err(CliError::Missing(format!(
            "no committed schema named `{name}` in {}",
            project.migrations_dir().display()
        )));
    }

    let mut checked = Vec::new();
    for (from, to) in committed.consecutive_pairs(name) {
        if args.from.is_some_and(|f| f != from) || args.to.is_some_and(|t| t != to) {
            continue;
        }
        // A jump has no intermediate shape to diff against, so it is not a pair. It is
        // reported by the gaps loop below, which can say what is wrong instead of
        // failing inside the diff with a message about the version numbers.
        if to != from + 1 {
            continue;
        }
        checked.push(check_pair(project, committed, name, from, to)?);
    }
    if args.from.is_some() && checked.is_empty() {
        return Err(CliError::Missing(format!(
            "no migration from v{} for `{name}`. Committed versions: {:?}.",
            args.from.unwrap_or(0),
            committed.versions_of(name)
        )));
    }

    // A jump is a denial, not a pair: the intermediate shape was never committed, so
    // there is no from-shape to diff against. The executor advances one version per
    // migration and `rollback` walks the same index in reverse, so a jump leaves the
    // rollback with no unambiguous target.
    for (from, to) in committed.gaps_of(name) {
        checked.push(MigrationCheck {
            shape: name.to_string(),
            from,
            to,
            verdict: Verdict::Denied,
            plan: None,
            generated: None,
            diff: Diff {
                from_version: from,
                to_version: to,
                findings: vec![Finding {
                    field: String::new(),
                    kind: ChangeKind::Removed,
                    severity: Severity::Denied,
                    detail: format!(
                        "v{from} jumps to v{to}. Commit the intermediate shapes: a version jump \
                         cannot be migrated or rolled back, because there is no shape to \
                         intermediate at."
                    ),
                }],
                verdict: Verdict::Denied,
                plan_applied: false,
            },
            notes: Vec::new(),
        });
    }
    Ok(checked)
}

/// A path as the operator would type it, relative to the project root.
///
/// `check`'s output is read in a terminal and in a CI log, and an absolute path is both
/// longer than the message around it and different on every machine, which makes two
/// runs of the same check diff.
fn relative(path: &std::path::Path, project: &Project) -> String {
    path.strip_prefix(&project.root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn check_pair(
    project: &Project,
    committed: &SchemaSet,
    name: &str,
    from: u32,
    to: u32,
) -> Result<MigrationCheck> {
    let (from_schema, to_schema) = (
        committed.get(name, from).expect("pair came from the set"),
        committed.get(name, to).expect("pair came from the set"),
    );
    let plan_path = project.plan_path(name, from, to);
    let plan = project.load_plan(name, from, to)?;
    let diff = match &plan {
        Some(plan) => soroban_migrate_schema::diff::diff_with_plan(from_schema, to_schema, plan)?,
        None => soroban_migrate_schema::diff::diff(from_schema, to_schema)?,
    };

    let mut notes = Vec::new();
    if plan.is_none() && diff.verdict.is_failure() {
        notes.push(Note {
            severity: Severity::Info,
            detail: format!(
                "run `soroban-migrate plan {from} {to}` to write {} with every required \
                 declaration pre-filled",
                relative(&plan_path, project)
            ),
        });
    }
    let migration_path = project.migration_path(name, from, to);
    let generated = migration_path.is_file();
    if diff.verdict.is_failure() {
        // Nothing to say about a generated file: it cannot exist without a plan, and
        // `generate` refuses to write one for a denied diff.
    } else if generated {
        notes.push(Note {
            severity: Severity::Info,
            detail: format!(
                "generated migration at {}",
                relative(&migration_path, project)
            ),
        });
    } else {
        notes.push(Note {
            severity: Severity::Warning,
            detail: format!(
                "no generated migration at {}. Either generate one with \
                 `soroban-migrate generate {from} {to}`, or implement \
                 `soroban_migrate::migration::Migration` by hand and reference it from the \
                 contract. Without one, nothing moves existing entries to the new shape.",
                relative(&migration_path, project)
            ),
        });
    }

    Ok(MigrationCheck {
        shape: name.to_string(),
        from,
        to,
        verdict: diff.verdict,
        plan: plan.is_some().then(|| relative(&plan_path, project)),
        generated: generated.then(|| relative(&migration_path, project)),
        diff,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_source(dir: &std::path::Path, v1_fields: &str, v2_fields: &str) {
        std::fs::write(
            dir.join("src/schema.rs"),
            format!(
                "#[storage_schema(version = 1, name = \"Balance\")]\n\
                 #[contracttype]\n\
                 pub struct BalanceV1 {{\n{v1_fields}\n}}\n\n\
                 #[storage_schema(version = 2, name = \"Balance\")]\n\
                 #[contracttype]\n\
                 pub struct BalanceV2 {{\n{v2_fields}\n}}\n"
            ),
        )
        .unwrap();
    }

    fn project_with(v1_fields: &str, v2_fields: &str) -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut config = crate::config::scaffold("balance", true);
        config.schema = Some("Balance".into());
        config.save(dir.path()).unwrap();
        write_source(dir.path(), v1_fields, v2_fields);
        let project = Project::open(dir.path()).unwrap();
        crate::commands::schema::export_summary(&project, false).unwrap();
        (dir, project)
    }

    fn args() -> Args {
        Args {
            schema: None,
            from: None,
            to: None,
            fail_on: FailOn::Deny,
            json: false,
            no_source: false,
        }
    }

    #[test]
    fn an_optional_addition_passes_the_gate() {
        let (_dir, project) = project_with(
            "    pub amount: i128,",
            "    pub amount: i128,\n    pub frozen: Option<bool>,",
        );
        assert!(run(&project, &args()).is_ok());
    }

    #[test]
    fn a_required_addition_is_denied() {
        let (_dir, project) = project_with(
            "    pub amount: i128,",
            "    pub amount: i128,\n    pub frozen: bool,",
        );
        let error = run(&project, &args()).unwrap_err();
        assert!(matches!(error, CliError::Refused(_)));
        assert!(error.to_string().contains("deny"), "got: {error}");
    }

    #[test]
    fn a_removed_field_is_denied_until_the_plan_declares_it() {
        let (_dir, project) = project_with(
            "    pub amount: i128,\n    pub legacy: u32,",
            "    pub amount: i128,",
        );
        let error = run(&project, &args()).unwrap_err();
        assert!(error.to_string().contains("deny"), "got: {error}");

        std::fs::write(
            project.plan_path("Balance", 1, 2),
            r#"{ "from": 1, "to": 2, "dropped": ["legacy"] }"#,
        )
        .unwrap();
        assert!(
            run(&project, &args()).is_ok(),
            "declaring the drop must clear the denial"
        );
    }

    #[test]
    fn a_retype_is_denied_until_a_conversion_is_declared() {
        let (_dir, project) = project_with("    pub amount: i64,", "    pub amount: i128,");
        assert!(run(&project, &args()).is_err());
        std::fs::write(
            project.plan_path("Balance", 1, 2),
            r#"{ "from": 1, "to": 2, "converted": { "amount": "i128::from(amount)" } }"#,
        )
        .unwrap();
        assert!(run(&project, &args()).is_ok());
    }

    #[test]
    fn snapshot_drift_is_a_denial() {
        let (dir, project) = project_with("    pub amount: i128,", "    pub amount: i128,");
        // Change v1 without bumping its version.
        std::fs::write(
            dir.path().join("src/schema.rs"),
            "#[storage_schema(version = 1, name = \"Balance\")]\n\
             #[contracttype]\n\
             pub struct BalanceV1 { pub amount: i64 }\n",
        )
        .unwrap();
        let error = run(&project, &args()).unwrap_err();
        assert!(matches!(error, CliError::Refused(_)));
    }

    #[test]
    fn no_source_skips_the_drift_check() {
        let (dir, project) = project_with("    pub amount: i128,", "    pub amount: i128,");
        std::fs::write(
            dir.path().join("src/schema.rs"),
            "#[storage_schema(version = 1, name = \"Balance\")]\n\
             #[contracttype]\n\
             pub struct BalanceV1 { pub amount: i64 }\n",
        )
        .unwrap();
        let mut args = args();
        args.no_source = true;
        assert!(run(&project, &args).is_ok());
    }

    #[test]
    fn a_version_jump_is_denied() {
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
             #[storage_schema(version = 3, name = \"Balance\")]\n\
             #[contracttype]\n\
             pub struct BalanceV3 { pub amount: i128, pub extra: Option<bool> }\n",
        )
        .unwrap();
        let project = Project::open(dir.path()).unwrap();
        crate::commands::schema::export_summary(&project, false).unwrap();
        let error = run(&project, &args()).unwrap_err();
        assert!(error.to_string().contains("deny"), "got: {error}");
    }

    #[test]
    fn fail_on_warn_tightens_the_gate() {
        // An optional addition with no `init`, and no generated migration: two
        // warnings that the default gate lets through.
        let (_dir, project) = project_with(
            "    pub amount: i128,",
            "    pub amount: i128,\n    pub frozen: Option<bool>,",
        );
        let mut args = args();
        args.fail_on = FailOn::Warn;
        let error = run(&project, &args).unwrap_err();
        assert!(matches!(error, CliError::Refused(_)));
    }

    #[test]
    fn a_missing_generated_migration_is_a_warning_not_a_denial() {
        // The derive can cover an additive change, so generation is not mandatory —
        // but "nothing will move your entries" is worth saying loudly.
        let (_dir, project) = project_with(
            "    pub amount: i128,",
            "    pub amount: i128,\n    pub frozen: Option<bool>,",
        );
        let committed = project.schema_set().unwrap();
        let checks = check_shape(&project, &committed, "Balance", &args()).unwrap();
        assert_eq!(checks.len(), 1);
        assert!(checks[0].notes.iter().any(
            |n| n.severity == Severity::Warning && n.detail.contains("no generated migration")
        ));
    }

    #[test]
    fn version_suffixes_are_recognised() {
        assert_eq!(version_suffix_stem("BalanceV2").as_deref(), Some("Balance"));
        assert_eq!(version_suffix_stem("StatsV10").as_deref(), Some("Stats"));
        assert_eq!(version_suffix_stem("Balance"), None);
        assert_eq!(version_suffix_stem("VaultV"), None);
        assert_eq!(version_suffix_stem("V2"), None);
        assert_eq!(version_suffix_stem("AccountView"), None);
    }

    #[test]
    fn structs_named_by_version_without_a_shared_identity_are_denied() {
        // The real mistake, reproduced: this is what the example contract did.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut config = crate::config::scaffold("balance", true);
        config.schema = None;
        config.save(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("src/schema.rs"),
            "#[storage_schema(version = 1)]\n\
             #[contracttype]\n\
             pub struct BalanceV1 { pub amount: i128 }\n\n\
             #[storage_schema(version = 2)]\n\
             #[contracttype]\n\
             pub struct BalanceV2 { pub amount: i128, pub frozen: Option<bool> }\n",
        )
        .unwrap();
        let project = Project::open(dir.path()).unwrap();
        crate::commands::schema::export_summary(&project, false).unwrap();

        let error = run(&project, &args()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("deny"), "got: {message}");
    }

    #[test]
    fn a_declared_shared_identity_clears_the_denial() {
        let (_dir, project) = project_with("    pub amount: i128,", "    pub amount: i128,");
        // `project_with` declares `name = "Balance"` on both, so the family check has
        // nothing to object to even though the structs are called BalanceV1/V2.
        assert!(run(&project, &args()).is_ok());
    }

    #[test]
    fn a_named_pair_that_does_not_exist_is_an_error_that_lists_the_versions() {
        let (_dir, project) = project_with("    pub amount: i128,", "    pub amount: i128,");
        let mut args = args();
        args.from = Some(9);
        args.to = Some(10);
        let error = run(&project, &args).unwrap_err();
        assert!(matches!(error, CliError::Missing(_)));
        assert!(error.to_string().contains("[1, 2]"), "got: {error}");
    }
}
