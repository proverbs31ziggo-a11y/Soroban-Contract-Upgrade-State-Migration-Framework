//! The multi-shape contract: several independently-versioned storage shapes in one
//! contract, which is the normal case and the one a version-keyed set cannot
//! represent.

use soroban_migrate_schema::model::{Field, Schema, SchemaSet};
use soroban_migrate_schema::SchemaError;

fn balance(version: u32, extra: bool) -> Schema {
    let mut fields = vec![Field::new("amount", "i128")];
    if extra {
        fields.push(Field::new("frozen", "Option<bool>"));
    }
    Schema::new(version, "Balance", fields)
}

fn stats(version: u32) -> Schema {
    Schema::new(version, "Stats", vec![Field::new("total", "i128")])
}

fn set() -> SchemaSet {
    let mut set = SchemaSet::new();
    // Two shapes sharing version 2. Keyed by version alone this is a conflict; keyed
    // by name it is simply two shapes.
    set.insert(balance(1, false)).unwrap();
    set.insert(balance(2, true)).unwrap();
    set.insert(stats(2)).unwrap();
    set
}

#[test]
fn two_shapes_may_share_a_version() {
    let set = set();
    assert_eq!(set.versions_of("Balance"), vec![1, 2]);
    assert_eq!(set.versions_of("Stats"), vec![2]);
    assert!(set.get("Balance", 2).unwrap().field("frozen").is_some());
    assert!(set.get("Stats", 2).unwrap().field("frozen").is_none());
}

#[test]
fn names_are_deterministic() {
    let set = set();
    assert_eq!(set.names().collect::<Vec<_>>(), vec!["Balance", "Stats"]);
}

#[test]
fn a_shape_that_changed_without_a_version_bump_is_refused() {
    let mut set = set();
    // Same name, same version, different shape: the failure mode is two developers
    // migrating to incompatible shapes while both believe they are at v2.
    let error = set.insert(balance(2, false)).unwrap_err();
    assert!(matches!(error, SchemaError::VersionConflict { version: 2 }));
}

#[test]
fn reinserting_an_identical_schema_is_not_a_conflict() {
    let mut set = set();
    assert!(set.insert(balance(2, true)).is_ok());
    assert_eq!(set.versions_of("Balance"), vec![1, 2]);
}

#[test]
fn consecutive_pairs_are_the_migrations_that_must_exist() {
    let mut set = set();
    set.insert(balance(3, true)).unwrap();
    assert_eq!(set.consecutive_pairs("Balance"), vec![(1, 2), (2, 3)]);
    // A shape that only ever existed at one version needs no migration at all.
    assert!(set.consecutive_pairs("Stats").is_empty());
}

#[test]
fn a_version_jump_is_reported_rather_than_turned_into_a_pair() {
    let mut set = set();
    // v1 to v4 with nothing in between. `consecutive_pairs` must not invent a
    // migration for a pair that has no schema for an endpoint.
    set.insert(balance(4, true)).unwrap();
    assert_eq!(set.consecutive_pairs("Balance"), vec![(1, 2), (2, 4)]);
    assert_eq!(set.gaps_of("Balance"), vec![(2, 4)]);
}

#[test]
fn the_next_version_continues_the_shape_from_its_own_history() {
    let set = set();
    // Stats is at 2 even though Balance is too, and has never been at 1. Its next
    // version is 3, not 2 — which is what a version-keyed set would have got wrong,
    // since 2 is already taken by a *different* shape.
    assert_eq!(set.next_version("Stats"), 3);
    assert_eq!(set.next_version("Balance"), 3);
    assert_eq!(set.next_version("NeverSeen"), 1);
}

#[test]
fn latest_of_is_per_shape() {
    let set = set();
    assert_eq!(set.latest_of("Balance").unwrap().version, 2);
    assert_eq!(set.latest_of("Stats").unwrap().version, 2);
    assert!(set.latest_of("NeverSeen").is_none());
}

#[test]
fn an_empty_set_has_no_pairs_and_no_gaps() {
    let set = SchemaSet::new();
    assert_eq!(set.names().count(), 0);
    assert!(set.consecutive_pairs("Balance").is_empty());
    assert!(set.gaps_of("Balance").is_empty());
    assert!(set.latest_of("Balance").is_none());
}

#[test]
fn a_set_round_trips_through_json() {
    // The CLI persists nothing in this shape today, but a `SchemaSet` that cannot be
    // serialized would be a trap for the next thing that wants to.
    let set = set();
    let json = serde_json::to_string(&set).unwrap();
    let back: SchemaSet = serde_json::from_str(&json).unwrap();
    assert_eq!(back, set);
}
