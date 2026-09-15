//! Reading a contract's key space, and deciding whether a change to it is safe.
//!
//! # The property these tests exist to pin
//!
//! `#[contracttype]` encodes an enum as a vector whose first element is the **name** of
//! the variant, as a `Symbol`. The generated reader resolves that symbol against a list
//! of case names and dispatches on the position it finds in it, and both the list and the
//! dispatch arms are generated from declaration order.
//!
//! So the two changes that look most alike are opposites: renaming a variant makes every
//! stored key undecodable, and reordering variants does nothing at all. A diff that got
//! this backwards would be worse than no diff, because it would refuse safe changes while
//! waving through the fatal one. Most of what follows is here to keep it the right way
//! round.

use soroban_migrate_schema::diff::{key_space_findings, Severity};
use soroban_migrate_schema::model::{KeySpace, KeyVariant};
use soroban_migrate_schema::parse::parse_key_space;

/// The example contract's key space, as Rust source.
const DATA_KEY: &str = r"
use soroban_migrate::key::MigrationKey;
use soroban_migrate_macros::Keyspace;
use soroban_sdk::{contracttype, Address};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq, Keyspace)]
pub enum DataKey {
    #[migration]
    Migration(MigrationKey),
    Balance(Address),
    Stats,
    Admin,
}
";

fn parsed(source: &str) -> KeySpace {
    parse_key_space("keys.rs", source)
        .expect("the source parses")
        .expect("the source declares a key space")
}

fn variant(name: &str, payload: Option<&str>) -> KeyVariant {
    KeyVariant {
        name: name.into(),
        payload: payload.map(str::to_string),
        migration: false,
    }
}

fn space(variants: Vec<KeyVariant>) -> KeySpace {
    KeySpace::new("DataKey", variants)
}

/// The names of the variants a set of findings is worried about, with severities.
fn severities(findings: &[soroban_migrate_schema::diff::KeySpaceFinding]) -> Vec<(&str, Severity)> {
    findings
        .iter()
        .map(|f| (f.variant.as_str(), f.severity))
        .collect()
}

// --- Parsing -----------------------------------------------------------------

#[test]
fn a_key_space_is_found_by_its_migration_variant() {
    let space = parsed(DATA_KEY);
    assert_eq!(space.name, "DataKey");
    let names: Vec<&str> = space.variants.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, vec!["Migration", "Balance", "Stats", "Admin"]);
}

#[test]
fn the_migration_variant_is_recorded_as_such() {
    let space = parsed(DATA_KEY);
    assert!(space.variant("Migration").unwrap().migration);
    assert!(!space.variant("Balance").unwrap().migration);
}

#[test]
fn a_key_space_is_found_by_the_derive_alone() {
    // No `#[migration]` marker at all: the derive is the only signal. This matters for a
    // contract that renamed the framework's variant, where the marker-based search would
    // otherwise miss the key space entirely and report nothing.
    let space = parsed(
        r"
#[contracttype]
#[derive(Clone, Keyspace)]
pub enum Storage {
    Balances(Address),
}
",
    );
    assert_eq!(space.name, "Storage");
    assert!(!space.variant("Balances").unwrap().migration);
}

#[test]
fn a_unit_variant_has_no_payload_and_a_tuple_variant_keeps_its_order() {
    let space = parsed(
        r"
#[contracttype]
#[derive(Keyspace)]
pub enum Key2 {
    Nothing,
    One(Address),
    Two(Address, u32),
}
",
    );
    assert_eq!(space.variant("Nothing").unwrap().payload, None);
    assert_eq!(
        space.variant("One").unwrap().payload.as_deref(),
        Some("Address")
    );
    assert_eq!(
        space.variant("Two").unwrap().payload.as_deref(),
        Some("Address, u32"),
        "the order of a variant's fields is part of its encoding"
    );
}

#[test]
fn a_struct_variant_keeps_its_field_names() {
    let space = parsed(
        r"
#[contracttype]
#[derive(Keyspace)]
pub enum Key2 {
    Tagged { owner: Address, seq: u32 },
}
",
    );
    assert_eq!(
        space.variant("Tagged").unwrap().payload.as_deref(),
        Some("owner: Address, seq: u32")
    );
}

#[test]
fn an_ordinary_enum_is_not_mistaken_for_a_key_space() {
    // The overwhelmingly common case: most enums in a contract are values, not keys. If
    // this returned one, every contract with a status enum would have a phantom key space
    // and the check would compare against the wrong enum.
    let found = parse_key_space(
        "status.rs",
        r"
#[contracttype]
#[derive(Clone)]
pub enum Status {
    Active,
    Closed,
}
",
    )
    .unwrap();
    assert!(found.is_none());
}

#[test]
fn a_file_with_no_enum_at_all_yields_nothing() {
    let found = parse_key_space("lib.rs", "pub fn f() {}\n").unwrap();
    assert!(found.is_none());
}

#[test]
fn malformed_source_is_an_error_that_names_the_file() {
    let error = parse_key_space("broken.rs", "pub enum {").unwrap_err();
    assert!(error.to_string().contains("broken.rs"), "got: {error}");
}

// --- The dangerous changes ---------------------------------------------------

#[test]
fn renaming_a_variant_is_denied_because_every_stored_key_stops_decoding() {
    // A rename is indistinguishable from a removal plus an addition at the encoding
    // level, so that is how it is reported.
    let before = space(vec![
        variant("Balance", Some("Address")),
        variant("Stats", None),
    ]);
    let after = space(vec![
        variant("Account", Some("Address")),
        variant("Stats", None),
    ]);

    let findings = key_space_findings(&before, &after);
    assert_eq!(
        severities(&findings),
        vec![("Account", Severity::Info), ("Balance", Severity::Denied)],
        "the removal is the fatal half and must be present"
    );
    let removal = findings
        .iter()
        .find(|f| f.variant == "Balance")
        .expect("the removal is reported");
    assert!(
        removal.detail.contains("rename"),
        "the message has to say that a rename is what this usually is: {}",
        removal.detail
    );
}

#[test]
fn changing_a_variants_payload_is_denied() {
    let before = space(vec![variant("Balance", Some("Address"))]);
    let after = space(vec![variant("Balance", Some("Address, u32"))]);

    let findings = key_space_findings(&before, &after);
    assert_eq!(severities(&findings), vec![("Balance", Severity::Denied)]);
}

#[test]
fn adding_a_payload_to_a_unit_variant_is_denied() {
    let before = space(vec![variant("Stats", None)]);
    let after = space(vec![variant("Stats", Some("u32"))]);
    let findings = key_space_findings(&before, &after);
    assert_eq!(severities(&findings), vec![("Stats", Severity::Denied)]);
}

#[test]
fn removing_a_variant_is_denied_because_its_entries_become_unreachable() {
    let before = space(vec![
        variant("Balance", Some("Address")),
        variant("Admin", None),
    ]);
    let after = space(vec![variant("Balance", Some("Address"))]);

    let findings = key_space_findings(&before, &after);
    assert_eq!(severities(&findings), vec![("Admin", Severity::Denied)]);
}

#[test]
fn moving_the_migration_marker_is_denied() {
    // The marker is how the framework finds the version marker, the cursor, and the index
    // pages. Moving it orphans all three.
    let mut from = variant("Migration", Some("MigrationKey"));
    from.migration = true;
    let before = space(vec![from, variant("Balance", Some("Address"))]);

    let before_marks = KeyVariant {
        name: "Migration".into(),
        payload: Some("MigrationKey".into()),
        migration: false,
    };
    let after = space(vec![
        before_marks,
        KeyVariant {
            name: "Balance".into(),
            payload: Some("Address".into()),
            migration: true,
        },
    ]);

    let findings = key_space_findings(&before, &after);
    assert_eq!(
        severities(&findings),
        vec![
            ("Migration", Severity::Denied),
            ("Balance", Severity::Denied)
        ]
    );
    assert!(findings.iter().all(|f| f.severity == Severity::Denied));
}

// --- The safe changes --------------------------------------------------------

#[test]
fn reordering_variants_is_safe_and_is_reported_as_information() {
    // The property that is easy to get backwards. The discriminant is the variant's name,
    // and the case list and dispatch arms are both generated from declaration order, so
    // they move together and no stored key changes meaning.
    let before = space(vec![
        variant("Migration", Some("MigrationKey")),
        variant("Balance", Some("Address")),
        variant("Stats", None),
    ]);
    let after = space(vec![
        variant("Stats", None),
        variant("Balance", Some("Address")),
        variant("Migration", Some("MigrationKey")),
    ]);

    let findings = key_space_findings(&before, &after);
    assert_eq!(severities(&findings), vec![("", Severity::Info)]);
    assert_eq!(findings[0].kind, "reordered");
    assert!(
        findings[0].detail.contains("safe"),
        "the report has to say outright that this is safe: {}",
        findings[0].detail
    );
}

#[test]
fn adding_a_variant_is_information() {
    let before = space(vec![variant("Balance", Some("Address"))]);
    let after = space(vec![
        variant("Balance", Some("Address")),
        variant("Delegate", Some("Address")),
    ]);

    let findings = key_space_findings(&before, &after);
    assert_eq!(severities(&findings), vec![("Delegate", Severity::Info)]);
}

#[test]
fn renaming_the_enum_type_is_not_a_change_at_all() {
    // Only variant names are encoded, so a rename of the type that holds them changes
    // nothing on the wire. Reporting it would train people to ignore the report.
    let before = space(vec![variant("Balance", Some("Address"))]);
    let mut after = space(vec![variant("Balance", Some("Address"))]);
    after.name = "StorageKey".into();

    assert!(key_space_findings(&before, &after).is_empty());
}

#[test]
fn an_unchanged_key_space_produces_nothing() {
    let before = parsed(DATA_KEY);
    let after = parsed(DATA_KEY);
    assert!(key_space_findings(&before, &after).is_empty());
}

#[test]
fn a_spelling_change_in_a_payload_is_not_a_change() {
    // The same normalization the field-level diff uses, so a qualified path does not read
    // as a retype. Without this, `soroban_sdk::Address` would look like a change and the
    // check would cry wolf.
    let before = space(vec![variant("Balance", Some("Address"))]);
    let after = space(vec![variant("Balance", Some("soroban_sdk::Address"))]);

    assert!(key_space_findings(&before, &after).is_empty());
}

#[test]
fn a_reordering_alongside_a_real_change_does_not_add_a_finding() {
    // Noise on top of something that already requires action is noise that gets the
    // report skimmed.
    let before = space(vec![
        variant("Balance", Some("Address")),
        variant("Stats", None),
    ]);
    let after = space(vec![
        variant("Stats", None),
        variant("Balance", Some("Address, u32")),
    ]);

    let findings = key_space_findings(&before, &after);
    assert_eq!(severities(&findings), vec![("Balance", Severity::Denied)]);
}

// --- The snapshot format ----------------------------------------------------

#[test]
fn a_key_space_round_trips_through_its_committed_form() {
    let space = parsed(DATA_KEY);
    let restored = KeySpace::from_json(&space.to_json()).expect("what it wrote, it can read");
    assert_eq!(restored, space);
    assert!(
        space.to_json().ends_with('\n'),
        "and it is a well-formed text file"
    );
}

#[test]
fn the_summary_names_every_variant_and_marks_the_migration_one() {
    let summary = parsed(DATA_KEY).summary();
    for name in ["Migration", "Balance", "Stats", "Admin"] {
        assert!(summary.contains(name), "missing {name} in: {summary}");
    }
    assert!(summary.contains("#[migration]"));
}
