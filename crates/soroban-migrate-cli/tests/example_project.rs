//! The CLI run against the repository's own worked example.
//!
//! # Why the tests point at a real project
//!
//! Every other test in this crate builds a throwaway project with hand-written schema
//! declarations. Those tests can all pass while the CLI is unusable on the project it
//! ships with — a different `source` layout, a shape whose Rust type names carry their
//! version suffixes, two shapes sharing a version number, generated code whose `use`
//! paths do not match the crate it lands in. The example has all of those awkward
//! properties on purpose, so running the CLI against it is what makes "the tool works
//! on a real contract" a claim with evidence behind it.
//!
//! # Why the generated file is compared byte for byte
//!
//! A generated artefact that is checked in is only useful if regenerating it produces
//! the same file. The moment it does not, every reviewer sees a diff they did not
//! cause, and the file stops being read. So `generate` is deterministic and this test
//! holds it to that.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The repository root: this crate is `crates/soroban-migrate-cli`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root is reachable from the crate directory")
}

/// The worked example's project directory.
fn example_root() -> PathBuf {
    repo_root().join("examples/vault")
}

/// Runs the built binary with `--root` pointed at `root`.
fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_soroban-migrate"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("the binary built for this test is runnable")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn the_examples_committed_snapshots_match_its_source() {
    let output = cli(&example_root(), &["schema", "export", "--check"]);
    assert!(
        output.status.success(),
        "`schema export --check` must pass on the example; stderr: {}",
        stderr(&output)
    );
}

#[test]
fn the_examples_migration_passes_the_gate() {
    let output = cli(&example_root(), &["check"]);
    assert!(
        output.status.success(),
        "the example's v1 -> v2 change is safe and must be accepted; stderr: {}",
        stderr(&output)
    );
    let report = stdout(&output);
    assert!(
        report.contains("Balance v1 -> v2"),
        "the report must name the pair it examined, or a passing check proves nothing \
         was looked at. Got:\n{report}"
    );
    assert!(report.contains("safe"), "got:\n{report}");
}

#[test]
fn the_committed_generated_migration_is_what_generate_produces() {
    let output = cli(&example_root(), &["generate", "1", "2", "--stdout"]);
    assert!(
        output.status.success(),
        "generation must succeed for the example; stderr: {}",
        stderr(&output)
    );
    let generated = stdout(&output);
    let committed_path = example_root().join("migrations/balance_1_to_2.rs");
    let committed = std::fs::read_to_string(&committed_path)
        .unwrap_or_else(|e| panic!("{} must be committed: {e}", committed_path.display()));

    assert_eq!(
        generated, committed,
        "the checked-in migration has drifted from what `generate` produces. Either \
         regenerate it with `soroban-migrate generate 1 2 --force`, or fix the generator \
         — but do not hand-edit the generated file."
    );
}

#[test]
fn the_generated_migration_is_syntactically_valid_rust() {
    // The generated file is not part of any crate's build, so nothing else would catch
    // a generator that emits something which does not parse.
    let output = cli(&example_root(), &["generate", "1", "2", "--stdout"]);
    assert!(output.status.success());
    let source = stdout(&output);
    syn::parse_file(&source)
        .unwrap_or_else(|e| panic!("generated code does not parse: {e}\n\n{source}"));
}

#[test]
fn a_migration_with_no_plan_is_refused_with_a_code_ci_can_act_on() {
    // A required field is the case the whole tool exists for: it compiles, deploys, and
    // then traps on every entry that predates it.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("soroban-migrate.toml"),
        "schema = \"Balance\"\nsource = [\"src\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/schema.rs"),
        "#[storage_schema(version = 1, name = \"Balance\")]\n\
         #[contracttype]\n\
         pub struct BalanceV1 { pub amount: i128 }\n\n\
         #[storage_schema(version = 2, name = \"Balance\")]\n\
         #[contracttype]\n\
         pub struct BalanceV2 { pub amount: i128, pub frozen: bool }\n",
    )
    .unwrap();

    let export = cli(dir.path(), &["schema", "export"]);
    assert!(export.status.success(), "stderr: {}", stderr(&export));

    let output = cli(dir.path(), &["check"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a denied change exits 2 so CI can tell it apart from a broken repository \
         (exit 1). stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let all = format!("{}{}", stdout(&output), stderr(&output));
    assert!(all.contains("frozen"), "the field must be named: {all}");
    assert!(
        !all.contains("panicked at"),
        "the refusal must be a report rather than a crash: {all}"
    );
}

#[test]
fn a_snapshot_that_disagrees_with_the_source_is_refused() {
    // A shape edited without its version bumped is how a check passes while the code no
    // longer matches: the diff then compares two snapshots that are both out of date.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("soroban-migrate.toml"),
        "schema = \"Balance\"\nsource = [\"src\"]\n",
    )
    .unwrap();
    let source = dir.path().join("src/schema.rs");
    std::fs::write(
        &source,
        "#[storage_schema(version = 1, name = \"Balance\")]\n\
         #[contracttype]\n\
         pub struct BalanceV1 { pub amount: i128 }\n",
    )
    .unwrap();
    assert!(cli(dir.path(), &["schema", "export"]).status.success());

    std::fs::write(
        &source,
        "#[storage_schema(version = 1, name = \"Balance\")]\n\
         #[contracttype]\n\
         pub struct BalanceV1 { pub amount: i64 }\n",
    )
    .unwrap();
    let output = cli(dir.path(), &["schema", "export", "--check"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("version changing"),
        "the error must say what to bump: {}",
        stderr(&output)
    );
}

#[test]
fn structs_named_by_version_without_a_shared_identity_are_refused() {
    // The mistake the example itself made: without `name`, `BalanceV1` and `BalanceV2`
    // are two unrelated shapes, so there is no version pair and `check` would report
    // success about a change it never looked at.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("soroban-migrate.toml"),
        "schema = \"Balance\"\nsource = [\"src\"]\n",
    )
    .unwrap();
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
    assert!(cli(dir.path(), &["schema", "export"]).status.success());

    let output = cli(dir.path(), &["check"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("name = \"Balance\""),
        "the error must show the attribute to add: {}",
        stdout(&output)
    );
}

#[test]
fn running_outside_a_project_says_how_to_create_one() {
    let dir = tempfile::tempdir().unwrap();
    let output = cli(dir.path(), &["check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("init"), "got: {}", stderr(&output));
}
