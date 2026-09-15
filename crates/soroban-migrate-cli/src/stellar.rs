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

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

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
                out.push('\'');
                out.push_str(&arg);
                out.push('\'');
            } else {
                out.push_str(&arg);
            }
        }
        out
    }
}

/// The wall-clock limit for one `stellar` invocation, in seconds, when the
/// configuration does not say otherwise.
///
/// Generous, because a batch against a busy endpoint legitimately takes a while and a
/// limit that fired on a healthy slow network would be worse than none. It is finite
/// because the alternative is a process that never returns: a rejection arrives in
/// seconds, and anything that has not answered in minutes is a stall.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 120;

/// How often the wait loop looks at whether the child has exited.
///
/// The cost of a shorter interval is one `waitpid` per tick on a process that is
/// already running; the cost of a longer one is that a deadline is overshot by up to the
/// interval, which for a migration is nothing.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The `stellar` CLI, discovered once and reused.
pub struct Stellar {
    program: String,
    timeout: Option<Duration>,
}

impl Stellar {
    /// Locates a `stellar` binary that will be given `timeout` per invocation.
    ///
    /// `None` waits indefinitely, which the configuration can ask for explicitly by
    /// setting `stellar_timeout_seconds = 0`.
    ///
    /// # Errors
    ///
    /// [`CliError::Missing`] when none is on `PATH`, naming the install source. This is
    /// checked up front rather than at the first invocation so that a `run` with many
    /// batches does not discover the problem after it has already started.
    ///
    /// The version probe is subject to the same deadline: a binary that hangs is exactly
    /// as unrecoverable as one that is absent, and far more confusing.
    pub fn discover(timeout: Option<Duration>) -> Result<Self> {
        let program = std::env::var("SOROBAN_MIGRATE_STELLAR").unwrap_or_else(|_| "stellar".into());
        match run_with_timeout(&program, &["--version".to_string()], timeout) {
            Ok((status, _, _)) if status.success() => Ok(Self { program, timeout }),
            Ok(_) => Err(CliError::Missing(format!(
                "`{program} --version` failed. Set SOROBAN_MIGRATE_STELLAR to a working binary."
            ))),
            Err(CliError::Network(_)) => Err(CliError::Missing(format!(
                "`{program} --version` did not finish in time. Set SOROBAN_MIGRATE_STELLAR to a \
                 working binary, or raise `stellar_timeout_seconds` in the `[network]` section."
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
    /// for a failed simulation or a rejected transaction appears — and the same error,
    /// with a different message, when the invocation exceeded the deadline.
    pub fn invoke(&self, invocation: &Invocation) -> Result<String> {
        let arguments = invocation.arguments();
        let (status, stdout, stderr) =
            match run_with_timeout(&self.program, &arguments, self.timeout) {
                Ok(result) => result,
                // The deadline error is about the process; the operator is looking at an
                // invocation. Name it, so the message is something they can re-run by hand.
                Err(CliError::Network(detail)) => {
                    return Err(CliError::Network(format!(
                        "{}: {detail}",
                        invocation.command_line()
                    )));
                }
                Err(other) => return Err(other),
            };
        if !status.success() {
            return Err(CliError::Network(format!(
                "`{}` failed:\n{}",
                invocation.command_line(),
                stderr.trim()
            )));
        }
        Ok(stdout)
    }
}

/// Runs `program args...` with a wall-clock deadline, draining both pipes.
///
/// # Why both pipes are drained on their own threads
///
/// A child that writes more than one pipe buffer's worth blocks until someone reads it.
/// Polling `try_wait` while leaving the pipes unread would therefore report a timeout on
/// a process that is merely waiting to be read — turning a slow-but-fine invocation into
/// a false alarm about an ambiguous submission, which is the worst possible thing to be
/// wrong about here.
///
/// # Errors
///
/// [`CliError::Io`] when the process cannot be started, and [`CliError::Network`] when it
/// exceeds `timeout`. A non-zero exit is returned rather than raised, because the callers
/// want different messages for it.
fn run_with_timeout(
    program: &str,
    args: &[String],
    timeout: Option<Duration>,
) -> Result<(ExitStatus, String, String)> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CliError::io(program, e))?;

    let stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stderr_pipe = child.stderr.take().expect("stderr was piped");
    let drain = |mut pipe: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    };
    let stdout_thread = drain(Box::new(stdout_pipe));
    let stderr_thread = drain(Box::new(stderr_pipe));

    let deadline = timeout.map(|t| Instant::now() + t);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| CliError::io(program, e))? {
            break status;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            // Kill before reporting, so a stalled child cannot outlive the command that
            // gave up on it, and reap it so no zombie is left behind.
            let _ = child.kill();
            let _ = child.wait();
            // The reader threads are deliberately *not* joined. A child that spawned
            // something which inherited the pipes — `sh -c 'sleep 30'`, where `sh` forks
            // rather than execs — leaves the write ends open after the child itself is
            // gone, so joining here would block for exactly as long as the deadline was
            // supposed to save us. They exit on their own once the last holder of the
            // pipe closes it, and a deadline is terminal for the run that hit it, so the
            // process they belong to is on its way out regardless.
            //
            // The consequence to be honest about: a grandchild that outlives its parent
            // is not killed. Killing the process *group* would cover that, at the cost of
            // a `unix`-only path where this one is portable.
            drop(stdout_thread);
            drop(stderr_thread);
            let seconds = timeout.map_or(0, |t| t.as_secs());
            return Err(CliError::Network(format!(
                "`{program}` did not finish within {seconds}s and was killed. A network that \
                 does not answer is not the same as one that refused: this invocation may \
                 still have been submitted. Check it with `soroban-migrate status` before \
                 retrying anything, and raise `stellar_timeout_seconds` under `[network]` in \
                 soroban-migrate.toml if the endpoint is merely slow."
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok((
        status,
        String::from_utf8_lossy(&stdout).to_string(),
        String::from_utf8_lossy(&stderr).to_string(),
    ))
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

    // --- The deadline ------------------------------------------------------------
    //
    // These run a real child process. `sh` rather than anything Soroban-related, because
    // the point is the runner itself: what it does with a process that finishes, one that
    // will never finish, and one that writes more than a pipe buffer before finishing.

    /// A `sh -c` argument vector.
    fn sh(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    #[test]
    fn a_command_that_finishes_returns_its_output() {
        let (status, stdout, _) =
            run_with_timeout("sh", &sh("printf hello"), Some(Duration::from_secs(10)))
                .expect("a fast command must not be an error");
        assert!(status.success());
        assert_eq!(stdout, "hello");
    }

    #[test]
    fn a_non_zero_exit_is_returned_rather_than_raised() {
        // The callers want different messages for this — `discover` calls it a missing
        // binary, `invoke` calls it a rejected call — so the runner does not decide.
        let (status, _, _) = run_with_timeout("sh", &sh("exit 3"), Some(Duration::from_secs(10)))
            .expect("a non-zero exit is not a runner error");
        assert!(!status.success());
    }

    #[test]
    fn a_command_that_exceeds_the_deadline_is_killed() {
        let started = Instant::now();
        let error = run_with_timeout("sh", &sh("sleep 30"), Some(Duration::from_millis(200)))
            .expect_err("a hung command must not return success");
        assert!(matches!(error, CliError::Network(_)));
        assert!(error.to_string().contains("did not finish"), "got: {error}");
        // Killed rather than abandoned: 30 seconds of `sleep` must not be waited out.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}, so the child was not killed at the deadline",
            started.elapsed()
        );
    }

    #[test]
    fn the_deadline_error_says_the_invocation_may_still_have_landed() {
        // The whole value of the timeout. An operator who reads "failed" and re-runs
        // submits the batch twice; one who reads this checks first.
        let error = run_with_timeout("sh", &sh("sleep 30"), Some(Duration::from_millis(200)))
            .expect_err("a hung command must not return success");
        let message = error.to_string();
        assert!(message.contains("may"), "got: {message}");
        assert!(
            message.contains("status"),
            "it must name the way to find out: {message}"
        );
        assert!(
            message.contains("stellar_timeout_seconds"),
            "and the way to raise the limit: {message}"
        );
    }

    #[test]
    fn no_deadline_means_no_limit() {
        let (status, stdout, _) =
            run_with_timeout("sh", &sh("printf ok"), None).expect("no deadline cannot fail");
        assert!(status.success());
        assert_eq!(stdout, "ok");
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_does_not_look_like_a_hang() {
        // The reason both pipes are drained on their own threads. 200 KB is well past the
        // 64 KB a pipe holds, so a runner that polled `try_wait` without reading would
        // report a timeout on a process that had already finished its work and was
        // merely waiting to be read — and the operator would be told, wrongly, that their
        // batch might have been submitted.
        let (status, stdout, _) = run_with_timeout(
            "sh",
            &sh("head -c 200000 /dev/zero | tr '\\0' x"),
            Some(Duration::from_secs(10)),
        )
        .expect("a command that writes a lot must not time out");
        assert!(status.success());
        assert_eq!(stdout.len(), 200_000);
    }

    #[test]
    fn a_program_that_cannot_be_run_is_an_io_error_not_a_deadline() {
        let error = run_with_timeout(
            "soroban-migrate-no-such-binary-xyz",
            &[],
            Some(Duration::from_secs(1)),
        )
        .expect_err("a missing binary must fail");
        assert!(matches!(error, CliError::Io { .. }), "got: {error}");
    }

    // --- The configured limit ----------------------------------------------------

    fn network(stellar_timeout_seconds: Option<u64>) -> crate::config::Network {
        crate::config::Network {
            name: Some("testnet".into()),
            rpc_url: None,
            contract: "CABC".into(),
            source: "alice".into(),
            passphrase: None,
            ledger: None,
            stellar_timeout_seconds,
        }
    }

    #[test]
    fn an_unstated_limit_gets_a_finite_default() {
        // Asserting it is `Some` is the point, not the number: a default of "no limit"
        // would make the timeout an opt-in, which is the state this issue was filed
        // about. Read through the configuration rather than the constant, so the
        // assertion is about behaviour and cannot be folded away at compile time.
        let default = network(None).stellar_timeout();
        assert!(default.is_some(), "the default must be a finite deadline");
        assert_eq!(default, Some(Duration::from_secs(DEFAULT_TIMEOUT_SECONDS)));
    }

    #[test]
    fn a_stated_limit_is_honoured() {
        assert_eq!(
            network(Some(7)).stellar_timeout(),
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn zero_states_that_there_is_no_limit() {
        assert_eq!(network(Some(0)).stellar_timeout(), None);
    }
}
