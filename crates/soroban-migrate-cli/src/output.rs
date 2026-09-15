//! Terminal output: headings, findings, and the arithmetic that turns raw resource
//! counts into something an operator can act on.
//!
//! # Why reporting lives in its own module
//!
//! Two properties of the reports are load-bearing, and neither is obvious at a call
//! site. Every report has to be *deterministic* — the same repository must produce
//! byte-identical output, or a CI diff of two runs is noise. And every number shown
//! next to a Mainnet cap has to be a percentage of that same cap, computed in one
//! place, because a report that compares against a stale constant is worse than no
//! report at all: it is a number an operator will trust.
//!
//! # Why no colour
//!
//! Colour codes in a CI log are either stripped or shown as escapes depending on the
//! runner, and `check` output is meant to be greppable and diffable. Severity is
//! carried by an uppercase tag and by indentation instead.

use std::fmt::Write as _;

/// The Mainnet per-invocation caps the framework measures itself against.
///
/// Duplicated from `soroban_migrate::executor`'s documentation rather than imported,
/// because this crate is deliberately not a dependency of the contract-side crate:
/// the CLI has to be usable against a contract built with any version of the
/// framework, and it reads these caps out of the *host*, which is the authority.
/// `INVOCATION_LIMITS` in `soroban-env-host` is where they come from.
pub const MAINNET_INSTRUCTIONS: u64 = 400_000_000;
/// See [`MAINNET_INSTRUCTIONS`].
pub const MAINNET_MEMORY_BYTES: u64 = 41_943_040;
/// See [`MAINNET_INSTRUCTIONS`].
pub const MAINNET_LEDGER_ENTRIES: u64 = 400;

/// Prints a section heading.
pub fn heading(text: &str) {
    println!("{text}");
    println!("{}", "=".repeat(text.chars().count()));
}

/// Prints a heading subordinate to [`heading`].
pub fn subheading(text: &str) {
    println!("\n{text}");
    println!("{}", "-".repeat(text.chars().count()));
}

/// Formats a value against a cap as a percentage, for a report that has to make the
/// headroom visible rather than leaving an operator to divide by hand.
///
/// The casts lose precision above 2^53, which no value here can reach: the caps are
/// hundreds of millions and a footprint is thousands. A `f64` is the right type for a
/// figure whose only job is to be read as a percentage; the exact counts are printed
/// alongside it.
#[allow(clippy::cast_precision_loss)]
pub fn percent_of(value: u64, cap: u64) -> String {
    if cap == 0 {
        return "n/a".into();
    }
    format!(
        "{:.1}% of {}",
        (value as f64) * 100.0 / (cap as f64),
        thousands(cap)
    )
}

/// Groups digits for readability: `41943040` becomes `41,943,040`.
///
/// Hand-rolled so that reports do not depend on the host's locale, which would make
/// the same run print differently on two machines and break the determinism the
/// reports are supposed to have.
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Renders the entries a migration still has to cover as a transaction count.
///
/// The ceiling division is the point: 1000 entries at 150 per batch is seven
/// transactions, not six, and an operator who budgets for six sends a batch that
/// fails. Always one batch is needed even for zero entries, because the batch that
/// finishes the migration is the one that advances the version.
pub fn transactions_needed(remaining: u32, batch_size: u32) -> u32 {
    if batch_size == 0 {
        return 0;
    }
    remaining.div_ceil(batch_size).max(1)
}

/// A multi-line indented block, for a finding's detail.
pub fn indented(text: &str, prefix: &str) -> String {
    let mut out = String::new();
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let _ = write!(out, "{prefix}{line}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(41_943_040), "41,943,040");
        assert_eq!(thousands(400_000_000), "400,000,000");
    }

    #[test]
    fn percentages_are_taken_against_the_named_cap() {
        assert_eq!(percent_of(0, 400), "0.0% of 400");
        assert_eq!(percent_of(200, 400), "50.0% of 400");
        assert_eq!(percent_of(41_943_040, 41_943_040), "100.0% of 41,943,040");
        assert_eq!(percent_of(1, 0), "n/a");
    }

    #[test]
    fn transaction_counts_round_up() {
        assert_eq!(transactions_needed(1_000, 150), 7);
        assert_eq!(transactions_needed(900, 150), 6);
        assert_eq!(transactions_needed(901, 150), 7);
        // Zero entries still needs the one batch that advances the version.
        assert_eq!(transactions_needed(0, 150), 1);
        assert_eq!(transactions_needed(5, 0), 0);
    }

    #[test]
    fn indenting_keeps_line_breaks() {
        assert_eq!(indented("a\nb", "  "), "  a\n  b");
    }
}
