//! The one place this tool talks to a network.
//!
//! # Why it delegates to the `stellar` CLI
//!
//! Sending a Soroban transaction means building an operation, simulating it to
//! discover the footprint and the resource fee, assembling auth entries, paying the
//! inclusion fee, and signing with a key that the operator has stored somewhere
//! sensible. Every one of those steps already exists, is maintained by the people who
//! maintain the protocol, and is exercised daily by the whole ecosystem. Reimplementing
//! them here would add a second implementation to disagree with the first — and the
//! one thing this tool must never do is disagree about resource accounting with the
//! network it is migrating.
//!
//! Delegation also keeps the security boundary where it belongs. This tool never sees
//! a secret key: it names an identity and the `stellar` CLI resolves it, from wherever
//! the operator chose to keep it. A migration framework has no business holding keys.
//!
//! # Why command construction is a separate function
//!
//! So it can be tested. The `stellar` binary is not required to run this crate's
//! tests, and the thing most likely to break is the argument list — a flag renamed in
//! a CLI release, an argument passed before the `--` separator instead of after it.

use std::process::Command;

use crate::error::{CliError, Result};

/// What to invoke and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The contract id, or an alias the `stellar` CLI resolves.
    pub contract: String,
    /// The identity that signs and pays.
    pub source: String,
    /// Whether to submit. `false` simulates and returns the simulated result, which
    /// is what makes a dry run cost nothing.
    pub send: bool,
    /// Whether to print the resource cost to stderr. Only meaningful when
    /// simulating, where it is the measurement the dry run exists to produce.
    pub cost: bool,
    /// How to reach the network.
    pub network: NetworkTarget,
    /// The contract function to call.
    pub function: String,
    /// Its arguments, by name.
    pub args: Vec<(String, String)>,
}

/// How to name a network to the `stellar` CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkTarget {
    /// A name from the CLI's own configuration, e.g. `testnet`.
    Named(String),
    /// An explicit endpoint and passphrase, for a network the CLI does not know.
    Custom {
        /// RPC endpoint.
        rpc_url: String,
        /// Network passphrase.
        passphrase: String,
    },
}

impl Invocation {
    /// The argument vector, without the program name.
    ///
    /// The `--` separator is not cosmetic: everything after it is parsed as arguments
    /// to the *contract's own* generated subcommand, so passing a function's argument
    /// before it would be read as a flag for `stellar contract invoke` and rejected.
    pub fn arguments(&self) -> Vec<String> {
        let mut args = vec![
            "contract".to_string(),
            "invoke".to_string(),
            "--id".to_string(),
            self.contract.clone(),
            "--source".to_string(),
            self.source.clone(),
        ];
        // Spelled as `--send=yes` rather than `--send yes` because the value is an
        // enum, and the two forms are not interchangeable for clap's value-enum
        // parsing in every version.
        args.push(format!("--send={}", if self.send { "yes" } else { "no" }));
        if self.cost {
            args.push("--cost".to_string());
        }
        match &self.network {
            NetworkTarget::Named(name) => {
                args.push("--network".to_string());
                args.push(name.clone());
            }
            NetworkTarget::Custom {
                rpc_url,
                passphrase,
            } => {
                args.push("--rpc-url".to_string());
                args.push(rpc_url.clone());
                args.push("--network-passphrase".to_string());
                args.push(passphrase.clone());
            }
        }
        args.push("--".to_string());
        args.push(self.function.clone());
        for (name, value) in &self.args {
            args.push(format!("--{name}"));
            args.push(value.clone());
        }
        args
    }

    /// The whole command line, for a report or an error message.
    pub fn command_line(&self) -> String {
        let mut out = String::from("stellar");
        for arg in self.arguments() {
            out.push(' ');
            if arg.contains(' ') {
                out.push_str(&format!("'{arg}'"));
            } else {
                out.push_str(&arg);
            }
        }
        out
    }
}

/// The `stellar` CLI, discovered once and reused.
pub struct Stellar {
    program: String,
}

impl Stellar {
    /// Locates a `stellar` binary.
    ///
    /// # Errors
    ///
    /// [`CliError::Missing`] when none is on `PATH`, naming the install source. This
    /// is checked up front rather than at the first invocation so that a `run` with
    /// many batches does not discover the problem after it has already started.
    pub fn discover() -> Result<Self> {
        let program = std::env::var("SOROBAN_MIGRATE_STELLAR").unwrap_or_else(|_| "stellar".into());
        let output = Command::new(&program).arg("--version").output();
        match output {
            Ok(output) if output.status.success() => Ok(Self { program }),
            Ok(_) => Err(CliError::Missing(format!(
                "`{program} --version` failed. Set SOROBAN_MIGRATE_STELLAR to a working binary."
            ))),
            Err(e) => Err(CliError::Missing(format!(
                "could not run `{program}`: {e}. The network commands delegate signing and \
                 submission to the Stellar CLI; install it with `cargo install --locked \
                 stellar-cli`, or set SOROBAN_MIGRATE_STELLAR to its path."
            ))),
        }
    }

    /// Runs an invocation and returns its standard output.
    ///
    /// # Errors
    ///
    /// [`CliError::Network`] carrying the CLI's own stderr, which is where the reason
    /// for a failed simulation or a rejected transaction appears.
    pub fn invoke(&self, invocation: &Invocation) -> Result<String> {
        let output = Command::new(&self.program)
            .args(invocation.arguments())
            .output()
            .map_err(|e| CliError::io(&self.program, e))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CliError::Network(format!(
                "`{}` failed:\n{}",
                invocation.command_line(),
                stderr.trim()
            )));
        }
        Ok(stdout)
    }
}

/// Parses a contract call's return value as an unsigned integer.
///
/// The CLI renders a `u32` as a bare number, but a wrapped or aliased value can
/// arrive quoted, so both are accepted. An absent value is `None`: "the contract has
/// never recorded a version" is a state this tool has to report rather than fail on.
///
/// # Errors
///
/// [`CliError::Network`] when the output is neither a number nor `null`.
pub fn parse_optional_u32(output: &str) -> Result<Option<u32>> {
    let text = output.trim();
    if text.is_empty() || text == "null" {
        return Ok(None);
    }
    let unquoted = text.trim_matches('"');
    unquoted.parse::<u32>().map(Some).map_err(|_| {
        CliError::Network(format!(
            "expected a schema version, got `{text}`. The entry point should return \
             `Option<u32>`; check that the configured function is the right one."
        ))
    })
}

/// Extracts one numeric field from a contract struct rendered as JSON.
///
/// # Errors
///
/// [`CliError::Network`] when the output is not an object or the field is missing,
/// naming the field so the mismatch is obvious.
pub fn json_u32(output: &str, field: &str) -> Result<u32> {
    let value: serde_json::Value = serde_json::from_str(output.trim()).map_err(|e| {
        CliError::Network(format!(
            "expected a JSON object from the contract, got `{output}`: {e}"
        ))
    })?;
    let found = value.get(field).and_then(serde_json::Value::as_u64);
    found.and_then(|v| u32::try_from(v).ok()).ok_or_else(|| {
        CliError::Network(format!(
            "the contract's response has no numeric `{field}`. Got: {value}"
        ))
    })
}

/// Extracts a string field from a contract struct rendered as JSON.
///
/// # Errors
///
/// [`CliError::Network`] when the output is not an object or the field is missing.
pub fn json_string(output: &str, field: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(output.trim()).map_err(|e| {
        CliError::Network(format!(
            "expected a JSON object from the contract, got `{output}`: {e}"
        ))
    })?;
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CliError::Network(format!("the contract's response has no `{field}`")))
}

/// Extracts a boolean field from a contract struct rendered as JSON.
///
/// # Errors
///
/// [`CliError::Network`] when the output is not an object or the field is missing.
pub fn json_bool(output: &str, field: &str) -> Result<bool> {
    let value: serde_json::Value = serde_json::from_str(output.trim()).map_err(|e| {
        CliError::Network(format!(
            "expected a JSON object from the contract, got `{output}`: {e}"
        ))
    })?;
    value
        .get(field)
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| {
            CliError::Network(format!("the contract's response has no boolean `{field}`"))
        })
}

/// Whether a response is JSON's `null`, i.e. the contract returned `None`.
pub fn is_null(output: &str) -> bool {
    let text = output.trim();
    text.is_empty() || text == "null"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation() -> Invocation {
        Invocation {
            contract: "CABC".into(),
            source: "alice".into(),
            send: false,
            cost: true,
            network: NetworkTarget::Named("testnet".into()),
            function: "migrate_batch".into(),
            args: vec![("limit".into(), "150".into())],
        }
    }

    #[test]
    fn the_argument_order_puts_the_function_after_the_separator() {
        assert_eq!(
            invocation().arguments(),
            vec![
                "contract",
                "invoke",
                "--id",
                "CABC",
                "--source",
                "alice",
                "--send=no",
                "--cost",
                "--network",
                "testnet",
                "--",
                "migrate_batch",
                "--limit",
                "150",
            ]
        );
    }

    #[test]
    fn a_sent_transaction_asks_for_yes() {
        let sent = Invocation {
            send: true,
            cost: false,
            ..invocation()
        };
        let args = sent.arguments();
        assert!(args.contains(&"--send=yes".to_string()));
        assert!(!args.contains(&"--cost".to_string()));
    }

    #[test]
    fn an_explicit_endpoint_replaces_the_named_network() {
        let custom = Invocation {
            network: NetworkTarget::Custom {
                rpc_url: "https://rpc.example".into(),
                passphrase: "Test SDF Network ; September 2015".into(),
            },
            ..invocation()
        };
        let args = custom.arguments();
        assert!(!args.contains(&"--network".to_string()));
        assert!(args.contains(&"--rpc-url".to_string()));
        assert!(args.contains(&"--network-passphrase".to_string()));
    }

    #[test]
    fn a_command_line_quotes_a_passphrase_that_contains_spaces() {
        let custom = Invocation {
            network: NetworkTarget::Custom {
                rpc_url: "https://rpc.example".into(),
                passphrase: "Test SDF Network ; September 2015".into(),
            },
            ..invocation()
        };
        assert!(
            custom
                .command_line()
                .contains("'Test SDF Network ; September 2015'"),
            "got: {}",
            custom.command_line()
        );
    }

    #[test]
    fn a_version_parses_from_a_bare_number_or_a_quoted_one() {
        assert_eq!(parse_optional_u32("2").unwrap(), Some(2));
        assert_eq!(parse_optional_u32(" 2\n").unwrap(), Some(2));
        assert_eq!(parse_optional_u32("\"2\"").unwrap(), Some(2));
    }

    #[test]
    fn an_absent_version_is_none_rather_than_an_error() {
        // "This contract has never recorded a version" is a state the operator needs
        // told about, not a crash.
        assert_eq!(parse_optional_u32("").unwrap(), None);
        assert_eq!(parse_optional_u32("null").unwrap(), None);
    }

    #[test]
    fn a_non_numeric_version_names_the_likely_mistake() {
        let error = parse_optional_u32("Symbol(foo)").unwrap_err();
        assert!(error.to_string().contains("Option<u32>"), "got: {error}");
    }

    #[test]
    fn struct_fields_are_read_by_name() {
        let json =
            r#"{"from":1,"to":2,"cursor":150,"total":1000,"status":"Running","started_ledger":42}"#;
        assert_eq!(json_u32(json, "total").unwrap(), 1000);
        assert_eq!(json_string(json, "status").unwrap(), "Running");
        assert!(json_u32(json, "nope").is_err());
    }

    #[test]
    fn a_boolean_field_is_read_by_name() {
        assert!(json_bool(r#"{"done":true}"#, "done").unwrap());
        assert!(!json_bool(r#"{"done":false}"#, "done").unwrap());
        assert!(json_bool(r#"{"done":"yes"}"#, "done").is_err());
    }

    #[test]
    fn null_is_recognised() {
        assert!(is_null("null"));
        assert!(is_null("  "));
        assert!(!is_null("0"));
    }
}
