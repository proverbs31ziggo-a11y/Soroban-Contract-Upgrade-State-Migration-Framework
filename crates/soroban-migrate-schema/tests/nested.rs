//! Shapes embedded inside a field's type: a change to them is a change to every entry
//! that carries them, even when the embedding schema is untouched.

use soroban_migrate_schema::diff::{diff, diff_with_plan, ChangeKind, Severity, Verdict};
use soroban_migrate_schema::model::{Field, Schema};
use soroban_migrate_schema::parse::{parse_source, parse_sources};
use soroban_migrate_schema::plan::MigrationPlan;
use soroban_migrate_schema::SchemaError;

const BALANCE: &str = r#"
#[storage_schema(version = 1, name = "Balance")]
#[contracttype]
pub struct BalanceV1 {
    pub owner: Address,
    pub amount: i128,
    pub meta: Metadata,
}

#[storage_schema(version = 2, name = "Balance")]
#[contracttype]
pub struct BalanceV2 {
    pub owner: Address,
    pub amount: i128,
    pub meta: Metadata,
    pub frozen: Option<bool>,
}
"#;

fn metadata(fields: &str) -> String {
    format!("#[contracttype]\n#[derive(Clone)]\npub struct Metadata {{\n{fields}\n}}\n")
}

fn parse(files: &[(&str, &str)]) -> Vec<Schema> {
    parse_sources(files).unwrap()
}

/// v1 snapshotted with `Metadata` as `before`, v2 with `Metadata` as `after` — the
/// struct was edited in place between the two exports.
fn pair(before: &str, after: &str) -> (Schema, Schema) {
    let old = metadata(before);
    let new = metadata(after);
    let v1 = parse(&[("schema.rs", BALANCE), ("meta.rs", &old)]).remove(0);
    let v2 = parse(&[("schema.rs", BALANCE), ("meta.rs", &new)]).remove(1);
    (v1, v2)
}

fn nested_finding(
    d: &soroban_migrate_schema::diff::Diff,
) -> &soroban_migrate_schema::diff::Finding {
    d.findings
        .iter()
        .find(|f| matches!(f.kind, ChangeKind::NestedChanged { .. }))
        .unwrap_or_else(|| panic!("no nested finding in {:#?}", d.findings))
}

#[test]
fn embedded_shapes_are_recorded_across_files() {
    let meta = metadata("    pub memo: Symbol,");
    let schemas = parse(&[("schema.rs", BALANCE), ("meta.rs", &meta)]);
    assert_eq!(schemas.len(), 2);
    for s in &schemas {
        assert_eq!(
            s.nested.get("Metadata"),
            Some(&vec![Field::new("memo", "Symbol")]),
            "{} v{}",
            s.name,
            s.version
        );
    }
}

#[test]
fn embedded_shapes_are_followed_through_containers_and_other_shapes() {
    let src = r#"
#[contracttype]
pub struct Inner { pub n: u32 }

#[contracttype]
pub struct Metadata { pub tags: Vec<Inner> }

#[soroban_sdk::contracttype]
pub struct Other { pub x: u32 }

pub struct NotAShape { pub y: u32 }

#[storage_schema(version = 1)]
#[contracttype]
pub struct Entry {
    pub meta: Option<Metadata>,
    pub by_key: Map<Symbol, Other>,
    pub plain: NotAShape,
}
"#;
    let schema = parse_source("lib.rs", src).unwrap().remove(0);
    let names: Vec<&str> = schema.nested.keys().map(String::as_str).collect();
    assert_eq!(names, ["Inner", "Metadata", "Other"]);
    assert_eq!(
        schema.reached_from("Option<Metadata>"),
        ["Inner", "Metadata"]
    );
}

#[test]
fn a_schema_without_embedded_shapes_serializes_as_before() {
    let src = "#[storage_schema(version = 1)]\n#[contracttype]\npub struct A { pub x: u32 }\n";
    let schema = parse_source("lib.rs", src).unwrap().remove(0);
    assert!(schema.nested.is_empty());
    assert!(!schema.to_json().contains("nested"));
}

#[test]
fn a_shape_that_contains_itself_is_refused() {
    let direct = r#"
#[storage_schema(version = 1)]
#[contracttype]
pub struct Node { pub children: Vec<Node> }
"#;
    let err = parse_source("lib.rs", direct).unwrap_err();
    assert!(matches!(err, SchemaError::Parse { .. }));
    assert!(err.to_string().contains("Node -> Node"), "{err}");

    let mutual = r#"
#[contracttype]
pub struct A { pub b: Option<B> }
#[contracttype]
pub struct B { pub a: Vec<A> }
#[storage_schema(version = 1)]
#[contracttype]
pub struct Entry { pub a: A }
"#;
    let err = parse_source("lib.rs", mutual).unwrap_err();
    assert!(err.to_string().contains("A -> B -> A"), "{err}");
}

#[test]
fn a_shape_reached_twice_without_a_cycle_is_not_a_cycle() {
    let src = r#"
#[contracttype]
pub struct Leaf { pub n: u32 }
#[storage_schema(version = 1)]
#[contracttype]
pub struct Entry { pub a: Leaf, pub b: Vec<Leaf> }
"#;
    let schema = parse_source("lib.rs", src).unwrap().remove(0);
    assert_eq!(schema.nested.len(), 1);
}

#[test]
fn an_ambiguous_embedded_name_is_refused_only_when_reached() {
    let one = "#[contracttype]\npub struct Metadata { pub memo: Symbol }\n";
    let two = "#[contracttype]\npub struct Metadata { pub memo: u32 }\n";
    let err = parse_sources(&[("schema.rs", BALANCE), ("a.rs", one), ("b.rs", two)]).unwrap_err();
    assert!(err.to_string().contains("more than one struct"), "{err}");

    let unrelated =
        "#[storage_schema(version = 1)]\n#[contracttype]\npub struct S { pub x: u32 }\n";
    assert!(parse_sources(&[("s.rs", unrelated), ("a.rs", one), ("b.rs", two)]).is_ok());
}

#[test]
fn an_unchanged_embedded_shape_is_not_reported() {
    let (v1, v2) = pair("    pub memo: Symbol,", "    pub memo: Symbol,");
    let d = diff(&v1, &v2).unwrap();
    assert!(
        !d.findings
            .iter()
            .any(|f| matches!(f.kind, ChangeKind::NestedChanged { .. })),
        "{:#?}",
        d.findings
    );
}

/// The case from issue #7: `Metadata` gains a required field, `Balance` itself does not
/// change, and every stored entry would trap on decode.
#[test]
fn a_required_field_added_to_an_embedded_shape_is_denied() {
    let (v1, v2) = pair(
        "    pub memo: Symbol,",
        "    pub memo: Symbol,\n    pub flag: bool,",
    );
    let d = diff(&v1, &v2).unwrap();
    assert_eq!(d.verdict, Verdict::Denied);
    let f = nested_finding(&d);
    assert_eq!(f.field, "meta");
    assert_eq!(f.severity, Severity::Denied);
    assert_eq!(
        f.kind,
        ChangeKind::NestedChanged {
            changes: vec!["Metadata.flag: added, required".into()]
        }
    );
    assert!(f.detail.contains("own name"), "{}", f.detail);
}

#[test]
fn removing_or_retyping_or_requiring_an_embedded_field_is_denied() {
    for (before, after) in [
        (
            "    pub memo: Symbol,\n    pub n: u32,",
            "    pub memo: Symbol,",
        ),
        ("    pub memo: Symbol,", "    pub memo: u32,"),
        ("    pub memo: Option<Symbol>,", "    pub memo: Symbol,"),
    ] {
        let (v1, v2) = pair(before, after);
        let d = diff(&v1, &v2).unwrap();
        assert_eq!(
            nested_finding(&d).severity,
            Severity::Denied,
            "{before} -> {after}"
        );
    }
}

#[test]
fn an_optional_field_added_to_an_embedded_shape_is_a_warning() {
    let (v1, v2) = pair(
        "    pub memo: Symbol,",
        "    pub memo: Symbol,\n    pub note: Option<Symbol>,",
    );
    let d = diff(&v1, &v2).unwrap();
    assert_eq!(nested_finding(&d).severity, Severity::Warning);
    assert_eq!(d.verdict, Verdict::SafeWithLazyMigration);
}

#[test]
fn a_change_two_levels_down_is_reported_on_the_top_level_field() {
    let inner = |fields: &str| {
        format!(
            "#[contracttype]\npub struct Inner {{ {fields} }}\n\
             #[contracttype]\npub struct Metadata {{ pub inner: Vec<Inner> }}\n"
        )
    };
    let v1 = parse(&[("schema.rs", BALANCE), ("m.rs", &inner("pub n: u32"))]).remove(0);
    let v2 = parse(&[
        ("schema.rs", BALANCE),
        ("m.rs", &inner("pub n: u32, pub m: u32")),
    ])
    .remove(1);
    let d = diff(&v1, &v2).unwrap();
    let f = nested_finding(&d);
    assert_eq!(f.field, "meta");
    assert_eq!(f.severity, Severity::Denied);
    assert_eq!(
        f.kind,
        ChangeKind::NestedChanged {
            changes: vec!["Inner.m: added, required".into()]
        }
    );
}

#[test]
fn an_embedded_shape_that_is_itself_a_schema_is_followed() {
    let src = |extra: &str| {
        format!(
            "#[storage_schema(version = 1, name = \"Stats\")]\n#[contracttype]\n\
             pub struct StatsV1 {{ pub total: i128, {extra} }}\n\
             #[storage_schema(version = 1, name = \"Holder\")]\n#[contracttype]\n\
             pub struct HolderV1 {{ pub stats: StatsV1 }}\n\
             #[storage_schema(version = 2, name = \"Holder\")]\n#[contracttype]\n\
             pub struct HolderV2 {{ pub stats: StatsV1, pub tag: Option<u32> }}\n"
        )
    };
    let find = |schemas: Vec<Schema>, version| {
        schemas
            .into_iter()
            .find(|s| s.name == "Holder" && s.version == version)
            .unwrap()
    };
    let v1 = find(parse(&[("lib.rs", &src(""))]), 1);
    let v2 = find(parse(&[("lib.rs", &src("pub count: u32"))]), 2);
    let d = diff(&v1, &v2).unwrap();
    assert_eq!(nested_finding(&d).severity, Severity::Denied);
}

#[test]
fn a_retyped_field_is_not_also_reported_as_a_nested_change() {
    let src = "#[contracttype]\npub struct Metadata { pub memo: Symbol }\n\
               #[contracttype]\npub struct MetadataV2 { pub memo: Symbol, pub flag: bool }\n";
    let mut v1 = parse(&[("schema.rs", BALANCE), ("m.rs", src)]).remove(0);
    let mut v2 = parse(&[("schema.rs", BALANCE), ("m.rs", src)]).remove(1);
    v1.fields[2] = Field::new("meta", "Metadata");
    v2.fields[2] = Field::new("meta", "MetadataV2");
    v2.nested = [(
        "MetadataV2".to_string(),
        vec![Field::new("memo", "Symbol"), Field::new("flag", "bool")],
    )]
    .into_iter()
    .collect();
    let mut plan = MigrationPlan::new(1, 2);
    plan.converted.insert("meta".into(), "convert(meta)".into());
    let d = diff_with_plan(&v1, &v2, &plan).unwrap();
    assert!(d
        .findings
        .iter()
        .all(|f| !matches!(f.kind, ChangeKind::NestedChanged { .. })));
    assert!(!d.verdict.is_failure());
}

#[test]
fn a_snapshot_that_predates_nested_tracking_is_a_warning_not_a_pass() {
    let (v1, v2) = pair("    pub memo: Symbol,", "    pub memo: Symbol,");
    // A file written by an earlier release: the same shape, with no `nested` key.
    let mut stripped = v1.clone();
    stripped.nested.clear();
    let json = stripped.to_json();
    assert!(!json.contains("nested"));
    let legacy = Schema::from_json(&json).unwrap();

    let d = diff(&legacy, &v2).unwrap();
    let f = nested_finding(&d);
    assert_eq!(f.severity, Severity::Warning);
    assert!(f.detail.contains("cannot be checked"), "{}", f.detail);

    assert!(legacy.predates_nested_tracking(&v1));
    assert!(!v1.predates_nested_tracking(&v1));
}
