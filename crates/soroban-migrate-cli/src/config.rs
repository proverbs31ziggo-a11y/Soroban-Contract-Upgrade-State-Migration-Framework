//! `soroban-migrate.toml`: what a project tells the tool about itself.
//!
//! # Why a file rather than flags
//!
//! Every value here is a property of the *project*, not of an invocation: where the
//! schemas live, what the keyspace type is called, which contract is being migrated.
//! Threading them through flags would mean every CI job and every developer having to
//! agree on them, which is exactly the kind of drift that makes a tool stop being
//! used. They are read once, committed, and reviewed like any other project setting.
//!
//! # Why every field has a default
//!
//! `soroban-migrate init` writes the file, but a project that has one only by habit
//! should still work if a field is deleted. More importantly, the defaults are the
//! framework's *conventional* names — `schema_version`, `migrate_batch` — so a
//! contract written against the documented example needs no configuration at all.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CliError, Result};

/// The configuration file's name.
pub const FILE_NAME: &str = "soroban-migrate.toml";

/// The migration directory's default name.
///
/// The same word Rails and Diesel use, and for the same reason: the point of a
/// migrations directory is that a new contributor can find it without being told.
pub const DEFAULT_MIGRATIONS_DIR: &str = "migrations";

/// Where schemas are read from, what the generated code refers to, and — for the
/// commands that need a network — which contract to talk to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The shape name to act on when a command is given no `--schema`.
    ///
    /// Set it for the common case of a contract with one storage shape. A contract
    /// with several sets it to whichever is usually the subject, and names the others
    /// per command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,

    /// Directories scanned for `#[storage_schema]` declarations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source: Vec<String>,

    /// The crate-relative path of the type implementing `soroban_migrate::Keyspace`,
    /// written into generated migrations, e.g. `crate::keys::DataKey`.
    #[serde(
        default = "default_keyspace",
        skip_serializing_if = "is_default_keyspace"
    )]
    pub keyspace: String,

    /// The module the generated migration refers to the version structs through,
    /// e.g. `crate::schema`.
    #[serde(
        default = "default_types_module",
        skip_serializing_if = "is_default_types_module"
    )]
    pub types_module: String,

    /// Directory holding schema snapshots, plans, and generated migrations.
    #[serde(
        default = "default_migrations_dir",
        skip_serializing_if = "is_default_migrations_dir"
    )]
    pub migrations_dir: String,

    /// Entry point names on the contract. Defaults are the names the framework's
    /// documentation uses, so a contract written against the guide needs no section.
    #[serde(default, skip_serializing_if = "is_default_contract")]
    pub contract: ContractCall,

    /// How to reach a deployed contract. Absent for the off-chain commands, which is
    /// what lets a repository be checked in CI without network access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Network>,
}

/// The contract entry points the chain-facing commands call.
///
/// Configurable rather than fixed because these are the *application's* functions:
/// the framework supplies the machinery, and the contract decides what to expose and
/// what to call it. Guessing a name and failing with "no entry point" would be a
/// worse experience than reading it from the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractCall {
    /// Returns the recorded schema version.
    pub version_fn: String,
    /// Returns the in-flight migration's state, or nothing if there is none.
    pub status_fn: String,
    /// Starts or resumes a migration.
    pub begin_fn: String,
    /// Applies one batch.
    pub batch_fn: String,
    /// The name of the batch's `limit` argument.
    pub batch_limit_arg: String,
}

impl Default for Config {
    /// Hand-written rather than derived so that it matches the `serde` defaults
    /// exactly. A derived `Default` would leave these empty strings, and a `Config`
    /// built in code would then differ from a `Config` parsed from an empty file —
    /// the kind of divergence that shows up as a settings file which suddenly grows
    /// fields it never needed.
    fn default() -> Self {
        Self {
            schema: None,
            source: Vec::new(),
            keyspace: default_keyspace(),
            types_module: default_types_module(),
            migrations_dir: default_migrations_dir(),
            contract: ContractCall::default(),
            network: None,
        }
    }
}

impl Default for ContractCall {
    fn default() -> Self {
        Self {
            version_fn: "schema_version".into(),
            status_fn: "migration_status".into(),
            begin_fn: "begin_migration".into(),
            batch_fn: "migrate_batch".into(),
            batch_limit_arg: "limit".into(),
        }
    }
}

/// How to reach a network and which contract to act on.
///
/// # Why there are two ways to name a network
///
/// An operator working interactively already has a network configured in the
/// `stellar` CLI, with their own RPC provider and passphrase, and re-declaring it
/// here would create a second copy to drift. A CI job has no such configuration and
/// needs the endpoint stated explicitly so the run is reproducible from a clean
/// checkout. So either a `name` the CLI resolves, or an explicit `rpc_url` and
/// `passphrase`, and exactly one of the two — stating both is a contradiction, and
/// silently preferring one would make a CI run use whatever endpoint the developer
/// happened to have configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// A network the `stellar` CLI knows by name, e.g. `testnet`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Soroban RPC endpoint. Required when `name` is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,

    /// The contract to migrate: an id, or an alias the `stellar` CLI knows.
    pub contract: String,

    /// The `stellar` identity used to sign and pay, e.g. `alice`.
    pub source: String,

    /// Network passphrase. Required when `rpc_url` is set: RPC alone does not
    /// identify a network, and a transaction signed for the wrong passphrase is
    /// rejected by the network and by nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,

    /// Ledger sequence to fork, or `None` for the latest. Pinning is what makes a
    /// dry run reproducible, which is what makes its cost figure comparable to the
    /// next one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger: Option<u32>,

    /// Wall-clock limit for a single `stellar` invocation, in seconds.
    ///
    /// Defaults to [`crate::stellar::DEFAULT_TIMEOUT_SECONDS`]. Set `0` to wait
    /// indefinitely, which is occasionally right for an endpoint that is merely slow and
    /// never right for an unattended run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stellar_timeout_seconds: Option<u64>,
}

impl Network {
    /// How long one `stellar` invocation may take.
    ///
    /// # Why there is a limit at all
    ///
    /// A rejected transaction comes back in seconds. A *stalled* endpoint does not come
    /// back at all, and the distinction matters because the two call for opposite
    /// responses: a rejection is safe to retry, and a stall means it is unknown whether
    /// the batch landed. `std::process::Command` has no deadline of its own, so without
    /// this a stalled RPC leaves `run` blocked at a prompt that looks like a slow batch,
    /// with the operator unable to tell which case they are in.
    pub fn stellar_timeout(&self) -> Option<std::time::Duration> {
        match self.stellar_timeout_seconds {
            // An explicit zero is the documented way to say "no limit".
            Some(0) => None,
            Some(seconds) => Some(std::time::Duration::from_secs(seconds)),
            None => Some(std::time::Duration::from_secs(
                crate::stellar::DEFAULT_TIMEOUT_SECONDS,
            )),
        }
    }

    /// How to name this network to the `stellar` CLI.
    ///
    /// # Errors
    ///
    /// [`CliError::Usage`] when the section states neither or both of `name` and
    /// `rpc_url`, naming what to add or remove.
    pub fn target(&self) -> Result<crate::stellar::NetworkTarget> {
        match (&self.name, &self.rpc_url) {
            (Some(name), None) => Ok(crate::stellar::NetworkTarget::Named(name.clone())),
            (None, Some(rpc_url)) => {
                let passphrase = self.passphrase.clone().ok_or_else(|| {
                    CliError::Usage(format!(
                        "[network] sets `rpc_url` but no `passphrase`. An RPC endpoint does not \
                         identify a network, and a transaction signed for the wrong one is \
                         rejected. Add the passphrase, or replace `rpc_url` with `name` so the \
                         Stellar CLI supplies both. See {FILE_NAME}."
                    ))
                })?;
                Ok(crate::stellar::NetworkTarget::Custom {
                    rpc_url: rpc_url.clone(),
                    passphrase,
                })
            }
            (Some(_), Some(_)) => Err(CliError::Usage(format!(
                "[network] sets both `name` and `rpc_url`. They contradict each other: one asks \
                 the Stellar CLI to resolve a configured network, the other states the endpoint. \
                 Keep one, in {FILE_NAME}."
            ))),
            (None, None) => Err(CliError::Usage(format!(
                "[network] sets neither `name` nor `rpc_url`, so there is nothing to connect to. \
                 Add `name = \"testnet\"`, or both `rpc_url` and `passphrase`. See {FILE_NAME}."
            ))),
        }
    }

    /// A short label for reports.
    pub fn label(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.rpc_url.clone())
            .unwrap_or_else(|| "unknown".into())
    }
}

fn default_keyspace() -> String {
    "crate::keys::DataKey".into()
}

fn is_default_keyspace(value: &String) -> bool {
    *value == default_keyspace()
}

fn default_types_module() -> String {
    "crate::schema".into()
}

fn is_default_types_module(value: &String) -> bool {
    *value == default_types_module()
}

fn default_migrations_dir() -> String {
    DEFAULT_MIGRATIONS_DIR.into()
}

fn is_default_migrations_dir(value: &String) -> bool {
    *value == default_migrations_dir()
}

fn is_default_contract(value: &ContractCall) -> bool {
    *value == ContractCall::default()
}

impl Config {
    /// Reads the configuration file next to `root`.
    ///
    /// # Errors
    ///
    /// [`CliError::Missing`] when the file does not exist, naming `init` as the fix,
    /// because that is always the right next step and the default `File not found`
    /// does not say so.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(FILE_NAME);
        if !path.exists() {
            return Err(CliError::Missing(format!(
                "no {FILE_NAME} in {}. Run `soroban-migrate init` to create one.",
                root.display()
            )));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| CliError::io(path.display(), e))?;
        let config: Self = toml::from_str(&text)
            .map_err(|e| CliError::Usage(format!("{}: {e}", path.display())))?;
        Ok(config)
    }

    /// Writes the configuration file next to `root`.
    ///
    /// # Errors
    ///
    /// Propagates IO failures with the path attached.
    pub fn save(&self, root: &Path) -> Result<()> {
        let path = root.join(FILE_NAME);
        let text = toml::to_string_pretty(self)
            .map_err(|e| CliError::Usage(format!("could not serialize configuration: {e}")))?;
        std::fs::write(&path, text).map_err(|e| CliError::io(path.display(), e))
    }

    /// The directories to scan for schema declarations.
    ///
    /// Defaults to `src`, which is where a contract's source lives in every Cargo
    /// project, so a configuration that omits `source` still does the right thing.
    pub fn source_paths(&self, root: &Path) -> Vec<PathBuf> {
        if self.source.is_empty() {
            vec![root.join("src")]
        } else {
            self.source.iter().map(|p| root.join(p)).collect()
        }
    }

    /// The migrations directory.
    pub fn migrations_path(&self, root: &Path) -> PathBuf {
        root.join(&self.migrations_dir)
    }
}

/// The configuration `soroban-migrate init` writes for a project it knows nothing
/// about.
///
/// Derived from what is actually on disk — the package name and whether `src`
/// exists — rather than being a fixed template, so the file it writes is already
/// correct and the operator has nothing to edit before running the next command.
pub fn scaffold(package_name: &str, has_src: bool) -> Config {
    Config {
        schema: Some(package_name.to_string()),
        source: if has_src {
            vec!["src".into()]
        } else {
            Vec::new()
        },
        keyspace: default_keyspace(),
        types_module: default_types_module(),
        migrations_dir: default_migrations_dir(),
        contract: ContractCall::default(),
        network: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_yields_the_same_config_as_default() {
        // These have to agree: a config built in code and a config read from a file
        // must not diverge, or `init` would write a file that changes behaviour.
        let parsed: Config = toml::from_str("").unwrap();
        assert_eq!(parsed, Config::default());
        assert_eq!(parsed.keyspace, "crate::keys::DataKey");
        assert_eq!(parsed.contract.batch_fn, "migrate_batch");
        assert!(parsed.network.is_none());
    }

    #[test]
    fn defaults_are_not_written_back_out() {
        // A configuration file should contain decisions, not restate the defaults.
        // Otherwise every project's file grows every time the tool gains a setting.
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!text.contains("keyspace"), "got: {text}");
        assert!(!text.contains("migrations_dir"), "got: {text}");
        assert!(!text.contains("contract"), "got: {text}");
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        // A typo in a configuration file that the tool silently ignores is a setting
        // that appears to be applied and is not.
        let error = toml::from_str::<Config>("keypsace = \"x\"").unwrap_err();
        assert!(error.to_string().contains("keypsace"), "got: {error}");
    }

    #[test]
    fn an_unknown_contract_key_is_refused_too() {
        let error = toml::from_str::<Config>("[contract]\nbatch_fnn = \"x\"").unwrap_err();
        assert!(error.to_string().contains("batch_fnn"), "got: {error}");
    }

    fn network_text(body: &str) -> String {
        format!(
            "[network]\ncontract = \"CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM\"\n\
             source = \"alice\"\n{body}"
        )
    }

    #[test]
    fn a_network_section_round_trips() {
        let text = network_text(
            "rpc_url = \"https://soroban-testnet.stellar.org\"\n\
             passphrase = \"Test SDF Network ; September 2015\"\nledger = 1234\n",
        );
        let parsed: Config = toml::from_str(&text).unwrap();
        let network = parsed.network.clone().unwrap();
        assert_eq!(network.ledger, Some(1234));
        assert_eq!(network.source, "alice");
        let again: Config = toml::from_str(&toml::to_string_pretty(&parsed).unwrap()).unwrap();
        assert_eq!(again, parsed);
    }

    #[test]
    fn an_explicit_endpoint_becomes_a_custom_target() {
        let config: Config = toml::from_str(&network_text(
            "rpc_url = \"https://rpc.example\"\npassphrase = \"p\"\n",
        ))
        .unwrap();
        let target = config.network.unwrap().target().unwrap();
        assert!(matches!(
            target,
            crate::stellar::NetworkTarget::Custom { .. }
        ));
    }

    #[test]
    fn a_named_network_becomes_a_named_target() {
        let config: Config = toml::from_str(&network_text("name = \"testnet\"\n")).unwrap();
        assert!(matches!(
            config.network.unwrap().target().unwrap(),
            crate::stellar::NetworkTarget::Named(name) if name == "testnet"
        ));
    }

    #[test]
    fn stating_both_a_name_and_an_endpoint_is_refused() {
        let config: Config = toml::from_str(&network_text(
            "name = \"testnet\"\nrpc_url = \"https://rpc.example\"\npassphrase = \"p\"\n",
        ))
        .unwrap();
        let error = config.network.unwrap().target().unwrap_err();
        assert!(error.to_string().contains("contradict"), "got: {error}");
    }

    #[test]
    fn an_endpoint_without_a_passphrase_is_refused() {
        let config: Config =
            toml::from_str(&network_text("rpc_url = \"https://rpc.example\"\n")).unwrap();
        let error = config.network.unwrap().target().unwrap_err();
        assert!(error.to_string().contains("passphrase"), "got: {error}");
    }

    #[test]
    fn neither_a_name_nor_an_endpoint_is_refused() {
        let config: Config = toml::from_str(&network_text("")).unwrap();
        let error = config.network.unwrap().target().unwrap_err();
        assert!(
            error.to_string().contains("nothing to connect to"),
            "got: {error}"
        );
    }

    #[test]
    fn source_defaults_to_src_only_at_use_time() {
        let config = Config::default();
        assert!(config.source.is_empty());
        let paths = config.source_paths(Path::new("/tmp/project"));
        assert_eq!(paths, vec![PathBuf::from("/tmp/project/src")]);
    }

    #[test]
    fn scaffold_sets_the_package_name_as_the_schema() {
        let config = scaffold("vault", true);
        assert_eq!(config.schema.as_deref(), Some("vault"));
        assert_eq!(config.source, vec!["src"]);
        assert_eq!(
            config,
            Config {
                schema: Some("vault".into()),
                source: vec!["src".into()],
                ..Config::default()
            }
        );
    }
}
