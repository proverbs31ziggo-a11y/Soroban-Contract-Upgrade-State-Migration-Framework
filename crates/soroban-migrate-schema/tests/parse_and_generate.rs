//! Source parsing and code generation.
//!
//! The codegen tests check that the output is *syntactically valid Rust* by parsing
//! it with `syn`, and that the guards which make it idempotent are present. They do
//! not compile it: compiling generated code needs the contract's own types, and the
//! worked example in `examples/vault` is where a generated-style migration is
//! checked against a real contract.

use soroban_migrate_schema::codegen::{generate, CodegenOptions};
use soroban_migrate_schema::diff::diff_with_plan;
use soroban_migrate_schema::model::{Field, Schema};
use soroban_migrate_schema::parse::{parse_source, to_rust_module};
use soroban_migrate_schema::plan::MigrationPlan;

fn schema(version: u32, fields: &[(&str, &str)]) -> Schema {
    Schema::new(
        version,
        "Account",
        fields.iter().map(|(n, t)| Field::new(*n, *t)).collect(),
    )
}

#[test]
fn parses_a_declaration_with_its_version_fields_and_doc_comment() {
    let src = r#"
/// Balances held for a single account.
#[storage_schema(version = 2)]
#[contracttype]
pub struct Account {
    pub owner: Address,
    pub amount: i128,
    pub frozen: Option<bool>,
    pub history: Vec<u32>,
}
"#;
    let schemas = parse_source("src/schema.rs", src).unwrap();
    assert_eq!(schemas.len(), 1);
    let s = &schemas[0];
    assert_eq!(s.version, 2);
    assert_eq!(s.name, "Account");
    assert_eq!(
        s.note.as_deref(),
        Some("Balances held for a single account.")
    );
    assert_eq!(
        s.field_names(),
        vec!["owner", "amount", "frozen", "history"]
    );

    let frozen = s.field("frozen").unwrap();
    assert!(frozen.optional);
    assert_eq!(frozen.base, "bool");
    // The declared text is canonical, so two runs of `schema export` produce
    // byte-identical files and CI can assert the snapshot is current.
    assert_eq!(frozen.declared, "Option<bool>");
    assert_eq!(s.field("history").unwrap().declared, "Vec<u32>");
    assert!(!s.field("owner").unwrap().optional);
}

/// Generates the migration for a pair and parses it, so a test can assert on the shape
/// of the output without restating the whole file.
fn generate_for(from: &Schema, to: &Schema, plan: &MigrationPlan) -> String {
    let diff = diff_with_plan(from, to, plan).unwrap();
    let options = CodegenOptions::conventional(from, to, "crate::DataKey");
    let code = generate(from, to, &diff, plan, &options).unwrap();
    syn::parse_file(&code).expect("generated code must parse");
    code
}

#[test]
fn a_change_with_nothing_to_rewrite_generates_an_up_that_says_so() {
    // An optional field added with no `init` produces a migration whose `up` has no
    // work to do. That is not a bug: running it is what moves the key index forward and
    // advances the version, and the new field arrives as `None` through tolerant
    // decoding. What would be a bug is emitting a `changed` flag that can never be set
    // and a `storage.set` that can never be reached — dead code that reads like the
    // migration might do something.
    let from = schema(1, &[("owner", "Address")]);
    let to = schema(2, &[("owner", "Address"), ("frozen", "Option<bool>")]);
    let code = generate_for(&from, &to, &MigrationPlan::new(1, 2));

    assert!(
        code.contains("Ok(EntryOutcome::AlreadyCurrent)"),
        "got:\n{code}"
    );
    assert!(!code.contains("let mut changed"), "got:\n{code}");
    assert!(!code.contains("storage.set"), "got:\n{code}");
    assert!(
        !code.contains("fn down"),
        "no `down` unless it was asked for"
    );
}

#[test]
fn an_optional_field_with_an_init_does_rewrite() {
    // The other side of the same decision: with `init` declared the field is filled,
    // and the guard that makes a second visit a no-op has to be there.
    let from = schema(1, &[("owner", "Address")]);
    let to = schema(2, &[("owner", "Address"), ("frozen", "Option<bool>")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.init.insert("frozen".into(), "false".into());
    let code = generate_for(&from, &to, &plan);

    assert!(code.contains("if entry.frozen.is_none()"), "got:\n{code}");
    assert!(code.contains("entry.frozen = Some(false);"), "got:\n{code}");
    assert!(
        code.contains("storage.set::<Val, AccountV2>"),
        "got:\n{code}"
    );
}

#[test]
fn a_dropped_field_generates_no_write_either() {
    // `dropped` is a declaration about intent, not an instruction: the new struct has
    // no field for the key, so `#[contracttype]` discards it and there is nothing to
    // generate. The generated file records that so a reader does not go looking.
    let from = schema(1, &[("owner", "Address"), ("legacy", "u32")]);
    let to = schema(2, &[("owner", "Address")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.dropped.push("legacy".into());
    let code = generate_for(&from, &to, &plan);

    assert!(code.contains("`legacy` is present"), "got:\n{code}");
    assert!(!code.contains("let mut changed"), "got:\n{code}");
}

#[test]
fn a_file_without_declarations_is_not_an_error() {
    // A directory walk hands the parser plenty of files that have nothing to do
    // with migrations.
    assert!(parse_source("src/lib.rs", "pub fn f() {}")
        .unwrap()
        .is_empty());
}

#[test]
fn multiple_declarations_in_one_file_are_all_returned() {
    let src = r#"
#[storage_schema(version = 1)]
#[contracttype]
pub struct AccountV1 { pub amount: u32 }

#[storage_schema(version = 2, name = "Account")]
#[contracttype]
pub struct AccountV2 { pub amount: i128, pub frozen: Option<bool> }
"#;
    let schemas = parse_source("src/schema.rs", src).unwrap();
    assert_eq!(schemas.len(), 2);
    assert_eq!(schemas[0].name, "AccountV1");
    // `name` overrides the struct name, which is how a schema survives having its
    // struct renamed between versions.
    assert_eq!(schemas[1].name, "Account");
}

#[test]
fn a_missing_version_is_refused_with_the_reason() {
    let src = "#[storage_schema]\n#[contracttype]\npub struct Account { pub amount: u32 }";
    let err = parse_source("src/schema.rs", src).unwrap_err();
    assert!(err.to_string().contains("missing `version = N`"), "{err}");
}

#[test]
fn a_schema_on_an_enum_is_refused() {
    let src = "#[storage_schema(version = 1)]\npub enum Account { A, B }";
    let err = parse_source("src/schema.rs", src).unwrap_err();
    assert!(
        err.to_string().contains("must be applied to a struct"),
        "{err}"
    );
}

#[test]
fn a_schema_on_a_tuple_struct_is_refused() {
    let src = "#[storage_schema(version = 1)]\npub struct Account(u32);";
    let err = parse_source("src/schema.rs", src).unwrap_err();
    assert!(err.to_string().contains("named fields"), "{err}");
}

#[test]
fn an_unknown_argument_is_refused_at_parse_time() {
    let src = "#[storage_schema(version = 1, verison = 2)]\npub struct A { pub x: u32 }";
    let err = parse_source("src/schema.rs", src).unwrap_err();
    assert!(err.to_string().contains("unrecognised"), "{err}");
}

#[test]
fn version_zero_is_refused() {
    let src = "#[storage_schema(version = 0)]\npub struct A { pub x: u32 }";
    let err = parse_source("src/schema.rs", src).unwrap_err();
    assert!(err.to_string().contains("start at 1"), "{err}");
}

#[test]
fn malformed_rust_is_reported_with_the_file() {
    let err = parse_source("src/broken.rs", "struct {").unwrap_err();
    assert!(err.to_string().contains("src/broken.rs"), "{err}");
}

#[test]
fn export_round_trips_through_the_parser() {
    let src = r#"
#[storage_schema(version = 3)]
#[contracttype]
pub struct Account { pub owner: Address, pub frozen: Option<bool> }
"#;
    let schemas = parse_source("src/schema.rs", src).unwrap();
    let rendered = to_rust_module(&schemas);
    let reparsed = parse_source("generated.rs", &rendered).unwrap();
    assert_eq!(schemas, reparsed);
    // The rendered module names each version's struct, which is what makes the
    // exported file compilable and reviewable rather than a wall of JSON.
    assert!(rendered.contains("pub struct AccountV3"));
}

// --- code generation -------------------------------------------------------

fn options(from: &Schema, to: &Schema) -> CodegenOptions {
    CodegenOptions::conventional(from, to, "crate::DataKey")
}

fn generated(from: &Schema, to: &Schema, plan: &MigrationPlan) -> String {
    let d = diff_with_plan(from, to, plan).unwrap();
    let out = generate(from, to, &d, plan, &options(from, to)).unwrap();
    // The first check is always the same: valid Rust.
    syn::parse_file(&out).unwrap_or_else(|e| panic!("generated code does not parse: {e}\n{out}"));
    out
}

#[test]
fn an_optional_addition_is_guarded_so_a_second_visit_is_a_no_op() {
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(2, &[("amount", "i128"), ("frozen", "Option<bool>")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.init.insert("frozen".into(), "false".into());

    let out = generated(&a, &b, &plan);
    assert!(out.contains("if entry.frozen.is_none()"), "{out}");
    assert!(out.contains("entry.frozen = Some(false);"), "{out}");
    assert!(out.contains("const FROM: u32 = 1;"));
    assert!(out.contains("const TO: u32 = 2;"));
    assert!(out.contains("impl Migration for AccountV1ToV2"));
    // The idempotency argument has to travel with the code, because the next person
    // to edit it is the one who can break it.
    assert!(out.contains("Idempotency"), "{out}");
}

#[test]
fn an_optional_addition_without_an_initialiser_generates_no_write() {
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(2, &[("amount", "i128"), ("frozen", "Option<bool>")]);
    let plan = MigrationPlan::new(1, 2);

    let out = generated(&a, &b, &plan);
    // Nothing to populate, so the migration reports every entry as already current.
    // That is the correct outcome for a field whose `None` is meaningful.
    assert!(!out.contains("entry.frozen"), "{out}");
    assert!(out.contains("already current") || out.contains("AlreadyCurrent"));
}

#[test]
fn a_rename_gets_a_tolerant_shadow_struct() {
    let a = schema(1, &[("body", "String")]);
    let b = schema(2, &[("note", "Option<String>")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.renamed.insert("body".into(), "note".into());

    let out = generated(&a, &b, &plan);
    // The shadow struct is what makes a re-run safe: after the first pass the old
    // key is gone, and reading the real v1 struct would trap on it.
    assert!(out.contains("struct AccountV1ToV2Consumed"), "{out}");
    assert!(out.contains("body: Option<String>"), "{out}");
    assert!(out.contains("if let Some(value) = old.body"), "{out}");
    assert!(out.contains("if entry.note.is_none()"), "{out}");
}

#[test]
fn a_retype_consumes_the_old_key_and_guards_on_its_presence() {
    let a = schema(1, &[("amount", "u32")]);
    let b = schema(2, &[("amount", "i128")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.converted.insert("amount".into(), "i128::from".into());

    let out = generated(&a, &b, &plan);
    assert!(out.contains("amount: Option<u32>"), "{out}");
    assert!(out.contains("(i128::from)(value)"), "{out}");
}

#[test]
fn a_denied_diff_is_never_generated() {
    let a = schema(1, &[("amount", "i128"), ("legacy", "u32")]);
    let b = schema(2, &[("amount", "i128")]);
    let plan = MigrationPlan::new(1, 2);

    let d = diff_with_plan(&a, &b, &plan).unwrap();
    // Generating code for a change the diff refused would turn the pre-flight check
    // into a formality that the very next command ignores.
    let err = generate(&a, &b, &d, &plan, &options(&a, &b)).unwrap_err();
    assert!(err.to_string().contains("denied"), "{err}");
}

#[test]
fn a_retype_without_a_conversion_cannot_be_generated() {
    let a = schema(1, &[("amount", "u32")]);
    let b = schema(2, &[("amount", "i128")]);
    let mut plan = MigrationPlan::new(1, 2);
    // Declare the type change without saying how to convert, which the diff allows
    // only as a denied finding; `generate` must refuse rather than guess.
    plan.converted.insert("amount".into(), String::new());
    let d = diff_with_plan(&a, &b, &plan).unwrap();
    assert!(generate(&a, &b, &d, &plan, &options(&a, &b)).is_err());
}

#[test]
fn generation_is_deterministic() {
    // Two developers running `generate` must produce byte-identical output, or every
    // regeneration shows up as a diff and the file stops being reviewable.
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(
        2,
        &[
            ("amount", "i128"),
            ("frozen", "Option<bool>"),
            ("note", "Option<String>"),
        ],
    );
    let mut plan = MigrationPlan::new(1, 2);
    plan.init.insert("frozen".into(), "false".into());
    plan.note = Some("Adds a freeze flag and a note.".into());

    let first = generated(&a, &b, &plan);
    let second = generated(&a, &b, &plan);
    assert_eq!(first, second);
    assert!(first.contains("Adds a freeze flag and a note."));
}

#[test]
fn a_dropped_field_is_documented_in_the_generated_file() {
    let a = schema(1, &[("amount", "i128"), ("legacy", "u32")]);
    let b = schema(2, &[("amount", "i128")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.dropped.push("legacy".into());

    let out = generated(&a, &b, &plan);
    // Nothing to generate for a removal — the decoder discards the key — so the file's
    // only job is to record that the loss was deliberate.
    assert!(out.contains("legacy"), "{out}");
    assert!(out.contains("drops map keys"), "{out}");
}

#[test]
fn generated_types_come_from_the_configured_module_path() {
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(2, &[("amount", "i128"), ("note", "Option<String>")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.init.insert("note".into(), "None".into());
    let d = diff_with_plan(&a, &b, &plan).unwrap();
    let mut opts = options(&a, &b);
    opts.keyspace = "my_crate::keys::DataKey".into();
    opts.types_module = "my_crate::schema".into();

    let out = generate(&a, &b, &d, &plan, &opts).unwrap();
    syn::parse_file(&out).unwrap();
    assert!(
        out.contains("type Keys = my_crate::keys::DataKey;"),
        "{out}"
    );
    assert!(out.contains("use my_crate::schema::*;"), "{out}");
}
