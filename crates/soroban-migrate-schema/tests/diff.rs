//! Compatibility classification: what each change kind means and when the diff
//! refuses.
//!
//! These tests are the specification for "is this upgrade safe?", so they assert on
//! the verdict and on which finding produced it — a verdict alone would pass even if
//! the diff reached it for the wrong reason.

use soroban_migrate_schema::diff::{diff, diff_with_plan, ChangeKind, Severity, Verdict};
use soroban_migrate_schema::model::{Field, Schema};
use soroban_migrate_schema::plan::MigrationPlan;

fn schema(version: u32, fields: &[(&str, &str)]) -> Schema {
    Schema::new(
        version,
        "Account",
        fields.iter().map(|(n, t)| Field::new(*n, *t)).collect(),
    )
}

fn kind_of(from: &Schema, to: &Schema, plan: &MigrationPlan, field: &str) -> ChangeKind {
    let d = diff_with_plan(from, to, plan).unwrap();
    d.findings
        .iter()
        .find(|f| f.field == field)
        .unwrap_or_else(|| panic!("no finding for `{field}` in\n{}", d.report()))
        .kind
        .clone()
}

fn severity_of(from: &Schema, to: &Schema, plan: &MigrationPlan, field: &str) -> Severity {
    let d = diff_with_plan(from, to, plan).unwrap();
    d.findings
        .iter()
        .find(|f| f.field == field)
        .unwrap()
        .severity
}

#[test]
fn identical_schemas_need_no_migration() {
    let a = schema(1, &[("owner", "Address"), ("amount", "i128")]);
    let b = schema(2, &[("owner", "Address"), ("amount", "i128")]);
    let d = diff(&a, &b).unwrap();
    assert_eq!(d.verdict, Verdict::Safe);
    assert!(d.findings.is_empty());
    assert!(d.report().contains("no changes"));
}

#[test]
fn adding_an_optional_field_is_safe_to_do_lazily() {
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(2, &[("amount", "i128"), ("frozen", "Option<bool>")]);
    let d = diff(&a, &b).unwrap();

    assert_eq!(d.verdict, Verdict::SafeWithLazyMigration);
    assert_eq!(
        kind_of(&a, &b, &MigrationPlan::new(1, 2), "frozen"),
        ChangeKind::AddedOptional
    );
    assert_eq!(
        severity_of(&a, &b, &MigrationPlan::new(1, 2), "frozen"),
        Severity::Warning
    );
    // The whole reason a batched migration can run against a live contract: the
    // new code can already read old entries.
    assert!(d.report().contains("decode"));
}

#[test]
fn adding_an_optional_field_with_an_initialiser_is_informational() {
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(2, &[("amount", "i128"), ("frozen", "Option<bool>")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.init.insert("frozen".into(), "false".into());

    // `Safe`, not `SafeWithLazyMigration`: with an initialiser declared, nothing has
    // to be sequenced around the upgrade for the contract to stay correct. The
    // migration is still worth running — it is what fills the field in — and the
    // finding says so.
    let d = diff_with_plan(&a, &b, &plan).unwrap();
    assert_eq!(d.verdict, Verdict::Safe);
    assert_eq!(severity_of(&a, &b, &plan, "frozen"), Severity::Info);
    let finding = &d.findings.iter().find(|f| f.field == "frozen").unwrap();
    assert!(
        finding.detail.contains("sets it from `false`"),
        "{}",
        finding.detail
    );
}

#[test]
fn adding_a_required_field_is_denied_without_an_initialiser() {
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(2, &[("amount", "i128"), ("frozen", "bool")]);
    let d = diff(&a, &b).unwrap();

    assert_eq!(d.verdict, Verdict::Denied);
    assert!(d.verdict.is_failure());
    // The detail must name both ways out, because "traps" alone leaves an author
    // guessing whether the fix is a default or a type change.
    let finding = &d.findings.iter().find(|f| f.field == "frozen").unwrap();
    assert!(finding.detail.contains("traps"));
    assert!(finding.detail.contains("Option<"));
    assert!(d.report().contains("init[\"frozen\"]"));
}

#[test]
fn adding_a_required_field_with_an_initialiser_requires_sequencing() {
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(2, &[("amount", "i128"), ("frozen", "bool")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.init.insert("frozen".into(), "false".into());

    let d = diff_with_plan(&a, &b, &plan).unwrap();
    assert_eq!(d.verdict, Verdict::SafeWithLazyMigration);
    // Not `Safe`: the migration must finish before code that reads the field is
    // promoted, and the diff's job is to say so rather than let it be discovered.
    let finding = &d.findings.iter().find(|f| f.field == "frozen").unwrap();
    assert!(finding.detail.contains("before"));
}

#[test]
fn removing_a_field_is_denied_because_the_decoder_discards_it_silently() {
    let a = schema(1, &[("amount", "i128"), ("legacy", "u32")]);
    let b = schema(2, &[("amount", "i128")]);
    let d = diff(&a, &b).unwrap();

    assert_eq!(d.verdict, Verdict::Denied);
    let finding = &d.findings.iter().find(|f| f.field == "legacy").unwrap();
    assert_eq!(finding.kind, ChangeKind::Removed);
    assert!(finding.detail.contains("discards"));
    assert!(d.report().contains("dropped"));
}

#[test]
fn a_declared_removal_is_allowed_and_its_loss_is_stated() {
    let a = schema(1, &[("amount", "i128"), ("legacy", "u32")]);
    let b = schema(2, &[("amount", "i128")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.dropped.push("legacy".into());

    let d = diff_with_plan(&a, &b, &plan).unwrap();
    assert_eq!(d.verdict, Verdict::Safe);
    let finding = &d.findings.iter().find(|f| f.field == "legacy").unwrap();
    assert_eq!(finding.severity, Severity::Info);
    assert!(finding.detail.contains("no rollback"));
}

#[test]
fn a_type_change_is_denied_without_a_conversion() {
    let a = schema(1, &[("amount", "u32")]);
    let b = schema(2, &[("amount", "i128")]);
    let d = diff(&a, &b).unwrap();

    assert_eq!(d.verdict, Verdict::Denied);
    let finding = &d.findings.iter().find(|f| f.field == "amount").unwrap();
    assert!(matches!(finding.kind, ChangeKind::Retyped { .. }));
    // The detail has to name the *silent* failure mode too, not just the trap:
    // reading a u32 payload as i128 can also produce a plausible wrong number.
    assert!(finding.detail.contains("wrong value"));
}

#[test]
fn a_type_change_with_a_conversion_is_reviewable() {
    let a = schema(1, &[("amount", "u32")]);
    let b = schema(2, &[("amount", "i128")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.converted.insert("amount".into(), "i128::from".into());

    let d = diff_with_plan(&a, &b, &plan).unwrap();
    assert_eq!(d.verdict, Verdict::SafeWithLazyMigration);
    assert_eq!(severity_of(&a, &b, &plan, "amount"), Severity::Warning);
}

#[test]
fn a_conversion_declared_without_an_expression_is_still_denied() {
    let a = schema(1, &[("amount", "u32")]);
    let b = schema(2, &[("amount", "i128")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.converted.insert("amount".into(), "   ".into());

    // An empty expression is a decision the author has not made. Treating it as a
    // declaration would let `generate` emit code that cannot compile, or worse,
    // code that silently passes the value through.
    assert_eq!(
        diff_with_plan(&a, &b, &plan).unwrap().verdict,
        Verdict::Denied
    );
}

#[test]
fn a_field_becoming_optional_is_the_safe_direction() {
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(2, &[("amount", "Option<i128>")]);
    let d = diff(&a, &b).unwrap();

    assert_eq!(d.verdict, Verdict::Safe);
    assert_eq!(
        kind_of(&a, &b, &MigrationPlan::new(1, 2), "amount"),
        ChangeKind::BecameOptional
    );
}

#[test]
fn a_field_becoming_required_needs_an_initialiser() {
    let a = schema(1, &[("amount", "Option<i128>")]);
    let b = schema(2, &[("amount", "i128")]);

    let denied = diff(&a, &b).unwrap();
    assert_eq!(denied.verdict, Verdict::Denied);
    assert_eq!(
        kind_of(&a, &b, &MigrationPlan::new(1, 2), "amount"),
        ChangeKind::BecameRequired
    );

    let mut plan = MigrationPlan::new(1, 2);
    plan.init.insert("amount".into(), "0".into());
    assert_eq!(
        diff_with_plan(&a, &b, &plan).unwrap().verdict,
        Verdict::SafeWithLazyMigration
    );
}

#[test]
fn a_declared_rename_is_one_change_not_a_removal_and_an_addition() {
    let a = schema(1, &[("body", "String")]);
    let b = schema(2, &[("note", "String")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.renamed.insert("body".into(), "note".into());

    let d = diff_with_plan(&a, &b, &plan).unwrap();
    assert_eq!(d.verdict, Verdict::Safe);
    assert_eq!(
        kind_of(&a, &b, &plan, "note"),
        ChangeKind::Renamed {
            from: "body".into(),
            to: "note".into()
        }
    );
    // The removal must not also be reported, or the author would be asked to
    // declare a loss that is not happening.
    assert!(d.findings.iter().all(|f| f.field != "body"));
}

#[test]
fn an_undeclared_rename_reads_as_two_separate_problems() {
    let a = schema(1, &[("body", "String")]);
    let b = schema(2, &[("note", "String")]);
    let d = diff(&a, &b).unwrap();

    assert_eq!(d.verdict, Verdict::Denied);
    assert_eq!(
        kind_of(&a, &b, &MigrationPlan::new(1, 2), "note"),
        ChangeKind::AddedRequired
    );
    assert_eq!(
        kind_of(&a, &b, &MigrationPlan::new(1, 2), "body"),
        ChangeKind::Removed
    );
}

#[test]
fn a_rename_that_also_changes_type_is_reviewable_when_both_are_declared() {
    let a = schema(1, &[("body", "String")]);
    let b = schema(2, &[("note_len", "u32")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.renamed.insert("body".into(), "note_len".into());
    // Declaring the conversion for a renamed-and-retyped field must not be
    // reported as stale: the rename implies a retype, and the diff has to account
    // for that when it validates the plan.
    plan.converted
        .insert("note_len".into(), "|s: String| s.len() as u32".into());

    let d = diff_with_plan(&a, &b, &plan).unwrap();
    assert_eq!(d.verdict, Verdict::SafeWithLazyMigration);
}

#[test]
fn reordering_fields_is_reported_but_harmless() {
    let a = schema(1, &[("owner", "Address"), ("amount", "i128")]);
    let b = schema(2, &[("amount", "i128"), ("owner", "Address")]);
    let d = diff(&a, &b).unwrap();

    assert_eq!(d.verdict, Verdict::Safe);
    assert_eq!(d.findings.len(), 1);
    assert_eq!(d.findings[0].kind, ChangeKind::Reordered);
    assert!(d.findings[0].detail.contains("symbol-keyed"));
}

#[test]
fn fully_qualified_type_spellings_are_not_changes() {
    let a = schema(1, &[("flag", "std::option::Option<bool>")]);
    let b = schema(2, &[("flag", "Option<bool>")]);
    let d = diff(&a, &b).unwrap();

    assert_eq!(d.verdict, Verdict::Safe);
    assert_eq!(
        kind_of(&a, &b, &MigrationPlan::new(1, 2), "flag"),
        ChangeKind::SpellingChanged
    );
}

#[test]
fn skipping_a_version_is_refused() {
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(3, &[("amount", "i128")]);
    let err = diff(&a, &b).unwrap_err();
    assert!(err.to_string().contains("skips versions"), "{err}");
}

#[test]
fn a_plan_for_the_wrong_version_pair_is_refused() {
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(2, &[("amount", "i128")]);
    let err = diff_with_plan(&a, &b, &MigrationPlan::new(4, 5)).unwrap_err();
    assert!(err.to_string().contains("plan covers"), "{err}");
}

#[test]
fn a_stale_declaration_is_refused_rather_than_ignored() {
    let a = schema(1, &[("amount", "i128")]);
    let b = schema(2, &[("amount", "i128")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.dropped.push("amount".into());

    // `amount` still exists, so a `dropped` declaration for it is stale. Letting it
    // through would mean that the next time `amount` *is* removed, the stale
    // declaration silently authorises the loss.
    let err = diff_with_plan(&a, &b, &plan).unwrap_err();
    assert!(err.to_string().contains("no change to that field"), "{err}");
}

#[test]
fn a_dropped_declaration_for_an_unknown_field_is_refused() {
    let a = schema(1, &[("amount", "i128"), ("legacy", "u32")]);
    let b = schema(2, &[("amount", "i128")]);
    let mut plan = MigrationPlan::new(1, 2);
    plan.dropped.push("never_existed".into());

    let err = diff_with_plan(&a, &b, &plan).unwrap_err();
    assert!(err.to_string().contains("never_existed"), "{err}");
}

#[test]
fn every_finding_at_or_above_a_severity_can_be_selected() {
    let a = schema(
        1,
        &[
            ("amount", "u32"),
            ("legacy", "u32"),
            ("flag", "Option<bool>"),
        ],
    );
    let b = schema(2, &[("amount", "i128"), ("flag", "bool")]);
    let d = diff(&a, &b).unwrap();

    assert_eq!(d.verdict, Verdict::Denied);
    assert_eq!(d.findings_at_least(Severity::Denied).len(), 3);
    assert!(d.findings_at_least(Severity::Info).len() >= 3);
    // The report must offer a way forward for everything a plan can fix.
    let report = d.report();
    for expected in [
        "converted[\"amount\"]",
        "dropped[\"legacy\"]",
        "init[\"flag\"]",
    ] {
        assert!(
            report.contains(expected),
            "report is missing {expected}:\n{report}"
        );
    }
}
