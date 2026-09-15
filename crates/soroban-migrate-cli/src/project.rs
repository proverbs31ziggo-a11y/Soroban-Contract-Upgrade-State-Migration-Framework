//! Where a project's schemas live on disk, and how they are read.
//!
//! # The directory layout
//!
//! ```text
//! soroban-migrate.toml
//! migrations/
//!   balance_1.json              # the committed shape of `Balance` at v1
//!   balance_2.json
//!   stats_2.json                # a second shape, versioned independently
//!   balance_1_to_2.plan.json    # the author's declarations for one version pair
//!   balance_1_to_2.rs           # the generated migration
//! ```
//!
//! # Why snapshots are committed rather than derived on demand
//!
//! The diff that justifies a migration needs *both* sides of it. The old side stops
//! existing in the source as soon as someone deletes the old struct — which is the
//! natural thing to do once the migration is written and tested. A repository that
//! derived its schemas from the working tree would therefore lose the ability to
//! re-check, re-generate, or audit an upgrade that already shipped. Snapshotting is
//! the same trade Rails makes with `db/schema.rb` and Diesel with its migration
//! snapshots, and it is the reason `check` can run on a checkout with no network.

use std::path::{Path, PathBuf};

use soroban_migrate_schema::model::{Schema, SchemaSet};
use soroban_migrate_schema::plan::MigrationPlan;
use soroban_migrate_schema::{plan_file_name, schema_file_name};
use walkdir::WalkDir;

use crate::config::Config;
use crate::error::{CliError, Result};

/// A directory holding a project's configuration, schemas, and migrations.
#[derive(Debug, Clone)]
pub struct Project {
    /// The directory containing `soroban-migrate.toml`.
    pub root: PathBuf,
    /// The loaded configuration.
    pub config: Config,
}

impl Project {
    /// Opens the project rooted at `root`.
    ///
    /// # Errors
    ///
    /// [`CliError::Missing`] when there is no configuration file.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let config = Config::load(&root)?;
        Ok(Self { root, config })
    }

    /// Opens the project containing `start`, searching that directory and then its
    /// ancestors.
    ///
    /// The search is what lets every command work from a subdirectory, which is how
    /// people actually use a CLI: `cargo test` from a crate, `soroban-migrate check`
    /// from wherever the editor left the terminal.
    ///
    /// # Errors
    ///
    /// [`CliError::Missing`] when no ancestor has a configuration file.
    pub fn discover(start: &Path) -> Result<Self> {
        let start = if start.as_os_str().is_empty() {
            Path::new(".")
        } else {
            start
        };
        let mut dir = std::fs::canonicalize(start).map_err(|e| CliError::io(start.display(), e))?;
        loop {
            if dir.join(crate::config::FILE_NAME).is_file() {
                return Self::open(dir);
            }
            if !dir.pop() {
                return Err(CliError::Missing(format!(
                    "no {} in {} or any parent directory. Run `soroban-migrate init` \
                     in the crate root.",
                    crate::config::FILE_NAME,
                    start.display()
                )));
            }
        }
    }

    /// The migrations directory.
    pub fn migrations_dir(&self) -> PathBuf {
        self.config.migrations_path(&self.root)
    }

    /// The path of `name` v`version`'s snapshot.
    pub fn schema_path(&self, name: &str, version: u32) -> PathBuf {
        self.migrations_dir().join(schema_file_name(name, version))
    }

    /// The path of the plan covering `from -> to` of `name`.
    pub fn plan_path(&self, name: &str, from: u32, to: u32) -> PathBuf {
        self.migrations_dir().join(plan_file_name(name, from, to))
    }

    /// The path of the generated migration for `from -> to` of `name`.
    pub fn migration_path(&self, name: &str, from: u32, to: u32) -> PathBuf {
        self.migrations_dir()
            .join(format!("{}_{}_to_{}.rs", name.to_lowercase(), from, to))
    }

    /// Every schema snapshot in the migrations directory.
    ///
    /// # Errors
    ///
    /// [`CliError::Usage`] when a snapshot is unreadable, and
    /// [`soroban_migrate_schema::SchemaError::VersionConflict`] when two files claim the
    /// same version of the same shape with different contents.
    pub fn schema_set(&self) -> Result<SchemaSet> {
        let dir = self.migrations_dir();
        let mut set = SchemaSet::new();
        if !dir.is_dir() {
            return Ok(set);
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| CliError::io(dir.display(), e))?
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| is_schema_snapshot(p))
            .collect();
        // `read_dir` order is the filesystem's, which is stable enough in practice and
        // not guaranteed in principle. Sorting makes a conflict report the same pair
        // every run.
        entries.sort();

        for path in entries {
            let text =
                std::fs::read_to_string(&path).map_err(|e| CliError::io(path.display(), e))?;
            let schema = Schema::from_json(&text)
                .map_err(|e| CliError::Usage(format!("{}: {e}", path.display())))?;
            set.insert(schema)?;
        }
        Ok(set)
    }

    /// Every schema declared in the project's source.
    ///
    /// # Errors
    ///
    /// [`CliError::Usage`] for a file that is not valid Rust, and for a
    /// `#[storage_schema]` attribute that does not parse. Both name the file.
    pub fn schemas_from_source(&self) -> Result<SchemaSet> {
        let mut set = SchemaSet::new();
        let mut files = 0usize;
        for root in self.config.source_paths(&self.root) {
            if !root.exists() {
                return Err(CliError::Missing(format!(
                    "source path {} does not exist. Fix `source` in {}.",
                    root.display(),
                    crate::config::FILE_NAME
                )));
            }
            for entry in WalkDir::new(&root)
                .sort_by_file_name()
                .into_iter()
                .filter_entry(|e| !is_ignored(e.path()))
            {
                let entry = entry.map_err(|e| {
                    CliError::Usage(format!("could not walk {}: {e}", root.display()))
                })?;
                if !entry.file_type().is_file()
                    || entry.path().extension().is_none_or(|ext| ext != "rs")
                {
                    continue;
                }
                files += 1;
                let path = entry.path();
                let text =
                    std::fs::read_to_string(path).map_err(|e| CliError::io(path.display(), e))?;
                for schema in
                    soroban_migrate_schema::parse::parse_source(&path.display().to_string(), &text)?
                {
                    set.insert(schema)?;
                }
            }
        }
        if files == 0 {
            return Err(CliError::Missing(format!(
                "no Rust source found under the configured `source` paths. Set `source` in {} \
                 to the directory holding the contract.",
                crate::config::FILE_NAME
            )));
        }
        Ok(set)
    }

    /// Loads the plan for `from -> to`, if one has been committed.
    ///
    /// # Errors
    ///
    /// [`CliError::Usage`] when the file exists but does not parse, and
    /// [`soroban_migrate_schema::SchemaError::PlanMismatch`] when it parses but covers a
    /// different pair — the failure mode of a plan copied from another migration and
    /// half-edited.
    pub fn load_plan(&self, name: &str, from: u32, to: u32) -> Result<Option<MigrationPlan>> {
        let path = self.plan_path(name, from, to);
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).map_err(|e| CliError::io(path.display(), e))?;
        let plan: MigrationPlan = MigrationPlan::from_json(&text)
            .map_err(|e| CliError::Usage(format!("{}: {e}", path.display())))?;
        if plan.from != from || plan.to != to {
            return Err(CliError::Schema(
                soroban_migrate_schema::SchemaError::PlanMismatch {
                    file: path.display().to_string(),
                    message: format!(
                        "the file covers v{} -> v{}, but it is named for v{from} -> v{to}",
                        plan.from, plan.to
                    ),
                },
            ));
        }
        Ok(Some(plan))
    }

    /// Writes a schema snapshot, refusing to overwrite a differing one.
    ///
    /// # Errors
    ///
    /// [`CliError::Refused`] when the path exists with different contents. A snapshot
    /// that changes without its version changing is exactly the state the whole tool
    /// exists to prevent, so it is never overwritten silently.
    pub fn write_schema(&self, schema: &Schema, force: bool) -> Result<WriteOutcome> {
        let path = self.schema_path(&schema.name, schema.version);
        let text = schema.to_json();
        if path.is_file() && !force {
            let existing =
                std::fs::read_to_string(&path).map_err(|e| CliError::io(path.display(), e))?;
            if existing == text {
                return Ok(WriteOutcome::Unchanged(path));
            }
            return Err(CliError::Refused(format!(
                "{} is already at v{} with a different shape. A shape must not change without \
                 its version changing: bump `#[storage_schema(version = N)]` on the struct and \
                 run this again, or pass --force if the snapshot is genuinely wrong.",
                path.display(),
                schema.version
            )));
        }
        std::fs::create_dir_all(self.migrations_dir())
            .map_err(|e| CliError::io(self.migrations_dir().display(), e))?;
        std::fs::write(&path, text).map_err(|e| CliError::io(path.display(), e))?;
        Ok(WriteOutcome::Written(path))
    }

    /// Resolves the shape a command should act on.
    ///
    /// # Errors
    ///
    /// [`CliError::Usage`] when nothing names the shape and the configuration does
    /// not default one, listing what is available so the fix is a copy-paste.
    pub fn resolve_schema_name(
        &self,
        explicit: Option<&str>,
        available: &SchemaSet,
    ) -> Result<String> {
        if let Some(name) = explicit {
            return Ok(name.to_string());
        }
        if let Some(name) = &self.config.schema {
            return Ok(name.clone());
        }
        let known: Vec<&str> = available.names().collect();
        if known.len() == 1 {
            return Ok(known[0].to_string());
        }
        Err(CliError::Usage(format!(
            "no shape named. Pass --schema, or set `schema` in {} so the commands that act on \
             one shape do not need it. Known shapes: {}.",
            crate::config::FILE_NAME,
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        )))
    }
}

/// What writing a snapshot did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The file was created or overwritten.
    Written(PathBuf),
    /// The file already had exactly these contents.
    Unchanged(PathBuf),
}

impl WriteOutcome {
    /// The path involved, whichever case.
    pub fn path(&self) -> &Path {
        match self {
            WriteOutcome::Written(p) | WriteOutcome::Unchanged(p) => p,
        }
    }

    /// Whether anything was written.
    ///
    /// The distinction is what `schema export --check` is built on: a snapshot that
    /// is already correct must not be an error, and one that would change must be.
    pub fn changed(&self) -> bool {
        matches!(self, WriteOutcome::Written(_))
    }
}

/// Whether a path is a schema snapshot rather than a plan or an unrelated JSON file.
fn is_schema_snapshot(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "json") && !path.to_string_lossy().ends_with(".plan.json")
}

/// Whether a directory should be skipped while scanning for schemas.
///
/// `target` because it holds generated code that would otherwise be parsed, and
/// dot-directories because they hold editor and VCS state. Both are conventions
/// rather than guarantees, but parsing `target/` would make a scan quadratic in the
/// size of the build directory, which is a real cost for a real project.
fn is_ignored(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name == "target" || (name.starts_with('.') && name != ".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let config = crate::config::scaffold("vault", true);
        config.save(dir.path()).unwrap();
        let project = Project::open(dir.path()).unwrap();
        (dir, project)
    }

    #[test]
    fn a_missing_config_is_named_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let error = Project::open(dir.path()).unwrap_err();
        assert!(matches!(error, CliError::Missing(_)));
        assert!(error.to_string().contains("init"));
    }

    #[test]
    fn discovery_walks_up_to_the_root() {
        let (dir, _) = temp_project();
        let nested = dir.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        let found = Project::discover(&nested).unwrap();
        assert_eq!(found.root, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn a_plan_file_is_not_mistaken_for_a_snapshot() {
        assert!(is_schema_snapshot(Path::new("migrations/balance_1.json")));
        assert!(!is_schema_snapshot(Path::new(
            "migrations/balance_1_to_2.plan.json"
        )));
        assert!(!is_schema_snapshot(Path::new(
            "migrations/balance_1_to_2.rs"
        )));
    }

    #[test]
    fn a_snapshot_is_written_once_and_then_reported_unchanged() {
        let (_dir, project) = temp_project();
        let schema = Schema::new(
            1,
            "Balance",
            vec![soroban_migrate_schema::model::Field::new("amount", "i128")],
        );
        let first = project.write_schema(&schema, false).unwrap();
        assert!(first.changed());
        let second = project.write_schema(&schema, false).unwrap();
        assert!(!second.changed());
    }

    #[test]
    fn changing_a_shape_without_bumping_its_version_is_refused() {
        let (_dir, project) = temp_project();
        let v1 = Schema::new(
            1,
            "Balance",
            vec![soroban_migrate_schema::model::Field::new("amount", "i128")],
        );
        project.write_schema(&v1, false).unwrap();

        let mutated = Schema::new(
            1,
            "Balance",
            vec![soroban_migrate_schema::model::Field::new("amount", "i64")],
        );
        let error = project.write_schema(&mutated, false).unwrap_err();
        assert!(matches!(error, CliError::Refused(_)));
        assert!(error.to_string().contains("version"));
        // ...and `--force` is the documented way out when the snapshot is wrong.
        assert!(project.write_schema(&mutated, true).unwrap().changed());
    }

    #[test]
    fn source_scanning_finds_declarations_and_ignores_target() {
        let (dir, project) = temp_project();
        std::fs::write(
            dir.path().join("src/schema.rs"),
            "#[storage_schema(version = 3)]\n#[contracttype]\npub struct Balance {\n    pub amount: i128,\n}\n",
        )
        .unwrap();
        let target = dir.path().join("target/debug/leftover.rs");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(
            &target,
            "#[storage_schema(version = 99)]\npub struct Wrong { pub x: u32 }\n",
        )
        .unwrap();

        let set = project.schemas_from_source().unwrap();
        assert_eq!(set.versions_of("Balance"), vec![3]);
        assert!(set.get("Wrong", 99).is_none());
    }

    #[test]
    fn a_source_tree_with_no_rust_is_an_error_that_says_what_to_do() {
        let (dir, project) = temp_project();
        std::fs::remove_dir(dir.path().join("src")).unwrap();
        let error = project.schemas_from_source().unwrap_err();
        assert!(error.to_string().contains("source"), "got: {error}");
    }

    #[test]
    fn a_shape_is_named_by_configuration_but_never_guessed_among_several() {
        let (_dir, project) = temp_project();
        let mut set = SchemaSet::new();
        assert_eq!(project.resolve_schema_name(None, &set).unwrap(), "vault");
        set.insert(Schema::new(
            1,
            "Balance",
            vec![soroban_migrate_schema::model::Field::new("a", "u32")],
        ))
        .unwrap();
        // The configured name still wins, even with schemas present.
        assert_eq!(project.resolve_schema_name(None, &set).unwrap(), "vault");
        assert_eq!(
            project.resolve_schema_name(Some("Balance"), &set).unwrap(),
            "Balance"
        );
    }

    #[test]
    fn with_no_configured_name_and_several_shapes_the_error_lists_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::scaffold("vault", false);
        config.schema = None;
        config.save(dir.path()).unwrap();
        let project = Project::open(dir.path()).unwrap();

        let mut set = SchemaSet::new();
        for name in ["Balance", "Stats"] {
            set.insert(Schema::new(
                1,
                name,
                vec![soroban_migrate_schema::model::Field::new("a", "u32")],
            ))
            .unwrap();
        }
        let error = project.resolve_schema_name(None, &set).unwrap_err();
        assert!(error.to_string().contains("Balance, Stats"), "got: {error}");
    }

    #[test]
    fn a_single_shape_needs_no_configuration_to_be_named() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::scaffold("vault", false);
        config.schema = None;
        config.save(dir.path()).unwrap();
        let project = Project::open(dir.path()).unwrap();

        let mut set = SchemaSet::new();
        set.insert(Schema::new(
            1,
            "Balance",
            vec![soroban_migrate_schema::model::Field::new("a", "u32")],
        ))
        .unwrap();
        assert_eq!(project.resolve_schema_name(None, &set).unwrap(), "Balance");
    }
}
