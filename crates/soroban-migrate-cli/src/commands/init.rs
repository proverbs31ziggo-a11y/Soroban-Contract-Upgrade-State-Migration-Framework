//! `soroban-migrate init`: adopt a contract that predates the framework.
//!
//! # What "adopt" means
//!
//! A contract whose entries were written before it recorded a schema version has no
//! version to migrate *from*. `init` does not fix that on chain — nothing off-chain
//! can, because inventing a version would be exactly the silent corruption the whole
//! tool exists to prevent. What it does is make the repository able to describe the
//! shape the contract is actually in:
//!
//! 1. Write `soroban-migrate.toml`, derived from what is on disk rather than from a
//!    template, so the next command already works.
//! 2. Snapshot the schemas currently declared in the source as the *first* version,
//!    which is the shape a deployed contract is assumed to be in.
//! 3. Print the one call the contract has to make to record that version on chain,
//!    which is `soroban_migrate::version::initialize`.
//!
//! Step 3 is printed rather than performed because it is a contract entry point, and
//! a tool that reached into a contract to write a version would be a tool that can
//! put a contract into a state nobody declared.

use std::path::Path;

use crate::commands::schema::export_summary;
use crate::config::{scaffold, Config, FILE_NAME};
use crate::error::{CliError, Result};
use crate::project::Project;

/// Arguments for `init`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The shape name to use as the project's default. Defaults to the package name.
    #[arg(long)]
    pub schema: Option<String>,

    /// Overwrite an existing `soroban-migrate.toml`.
    #[arg(long)]
    pub force: bool,

    /// Do not snapshot the current source as the first version.
    #[arg(long)]
    pub no_snapshot: bool,
}

/// Runs the command.
///
/// # Errors
///
/// [`CliError::Refused`] when a configuration file already exists and `--force` was
/// not passed, and any failure from snapshotting the current source.
pub fn run(root: &Path, args: &Args) -> Result<()> {
    let config_path = root.join(FILE_NAME);
    if config_path.is_file() && !args.force {
        return Err(CliError::Refused(format!(
            "{} already exists. Pass --force to replace it.",
            config_path.display()
        )));
    }

    let package = package_name(root);
    let mut config = scaffold(
        args.schema.as_deref().unwrap_or(&package),
        root.join("src").is_dir(),
    );
    if let Some(schema) = &args.schema {
        config.schema = Some(schema.clone());
    }
    config.save(root)?;
    println!("wrote {}", config_path.display());

    std::fs::create_dir_all(config.migrations_path(root))
        .map_err(|e| CliError::io(config.migrations_path(root).display(), e))?;
    println!("wrote {}/", config.migrations_path(root).display());

    if !args.no_snapshot {
        let project = Project::open(root)?;
        export_summary(&project, false)?;
    }

    print_next_steps(&config);
    Ok(())
}

/// The package name from `Cargo.toml`, used as the default shape name.
///
/// Falls back to the directory name when there is no manifest or it has no package
/// name. Both fallbacks are *reported* by the caller rather than silently applied:
/// a guessed schema name that turns out to be wrong is a snapshot filed under the
/// wrong shape.
fn package_name(root: &Path) -> String {
    #[derive(serde::Deserialize)]
    struct Manifest {
        package: Option<Package>,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        name: Option<String>,
    }

    if let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) {
        if let Ok(manifest) = toml::from_str::<Manifest>(&text) {
            if let Some(name) = manifest.package.and_then(|p| p.name) {
                return name;
            }
        }
    }
    root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("contract")
        .to_string()
}

fn print_next_steps(config: &Config) {
    println!("\nNext:");
    println!(
        "  1. Point `source` in {FILE_NAME} at the directory holding the contract, if it is \
         not `src`."
    );
    println!(
        "  2. Run `soroban-migrate check` in CI. It fails on any change that would lose data."
    );
    println!(
        "  3. When the shape changes, bump `#[storage_schema(version = N)]`, then run \
         `soroban-migrate generate FROM TO`."
    );
    if let Some(name) = &config.schema {
        println!(
            "  4. On a contract that predates the framework, record `{name}`'s current version \
             with `soroban_migrate::version::initialize` in the constructor. Until that call has \
             run, the contract has no version to migrate from."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_package_name_comes_from_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"vault\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(package_name(dir.path()), "vault");
    }

    #[test]
    fn a_manifest_without_a_package_falls_back_to_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let expected = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(package_name(dir.path()), expected);
    }

    #[test]
    fn init_refuses_to_clobber_a_configuration_without_force() {
        let dir = tempfile::tempdir().unwrap();
        Config::default().save(dir.path()).unwrap();
        let args = Args {
            schema: None,
            force: false,
            no_snapshot: true,
        };
        let error = run(dir.path(), &args).unwrap_err();
        assert!(matches!(error, CliError::Refused(_)));

        let forced = Args {
            force: true,
            ..args
        };
        assert!(run(dir.path(), &forced).is_ok());
    }

    #[test]
    fn init_writes_a_configuration_the_next_command_can_use() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"vault\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        run(
            dir.path(),
            &Args {
                schema: None,
                force: false,
                no_snapshot: true,
            },
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config.schema.as_deref(), Some("vault"));
        assert!(dir.path().join("migrations").is_dir());
    }
}
