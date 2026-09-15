//! What one batch did, as the CLI sees it.
//!
//! # Why the CLI parses the outcome rather than trusting "it worked"
//!
//! The distinction that matters to an operator driving a migration is not success
//! versus failure — a failed batch reverts, and the cursor does not move, so a retry
//! is always safe. It is whether the batch *did anything*. A batch that visits keys
//! and reports them already current is cheap and means the migration is nearly done.
//! A batch that visits nothing while not being done means the cursor is pinned, and a
//! driver that loops on that condition sends transactions forever without advancing.
//!
//! That second case is the reason this module exists: the loop has to be able to
//! recognise a stall it cannot fix, because the only alternative is an infinite loop
//! that spends the operator's XLM.

use crate::error::Result;
use crate::stellar::{json_bool, json_u32};

/// A batch's reported outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchOutcomeView {
    /// Version read from.
    pub from: u32,
    /// Version written to.
    pub to: u32,
    /// Cursor position after the batch.
    pub cursor: u32,
    /// Keys the batch believes it is working through.
    pub total: u32,
    /// Keys examined.
    pub visited: u32,
    /// Keys rewritten.
    pub migrated: u32,
    /// Keys already correct.
    pub already_current: u32,
    /// Keys this migration does not manage.
    pub skipped: u32,
    /// Whether the migration has finished.
    pub done: bool,
}

impl BatchOutcomeView {
    /// Reads the outcome out of the `stellar` CLI's JSON rendering of the
    /// contract's `BatchOutcome`.
    ///
    /// # Errors
    ///
    /// [`crate::error::CliError::Network`] when the payload is not the expected object,
    /// naming the field that was missing — which is how a wrong `batch_fn` in the
    /// configuration gets diagnosed.
    pub fn parse(output: &str) -> Result<Self> {
        Ok(Self {
            from: json_u32(output, "from")?,
            to: json_u32(output, "to")?,
            cursor: json_u32(output, "cursor")?,
            total: json_u32(output, "total")?,
            visited: json_u32(output, "visited")?,
            migrated: json_u32(output, "migrated")?,
            already_current: json_u32(output, "already_current")?,
            skipped: json_u32(output, "skipped")?,
            done: json_bool(output, "done")?,
        })
    }

    /// True when the batch made no progress and the migration is not finished.
    ///
    /// Reached when the index holds no keys from the cursor onward while `total` still
    /// claims there are some: an inconsistency the framework cannot recover from on
    /// its own. The driver stops rather than resending, because a retry cannot help
    /// and each attempt costs a fee.
    pub fn is_stall(&self) -> bool {
        self.visited == 0 && !self.done
    }

    /// Keys left, by the batch's own accounting.
    pub fn remaining(&self) -> u32 {
        self.total.saturating_sub(self.cursor)
    }

    /// How much of the migration is done, in basis points.
    pub fn progress_basis_points(&self) -> u32 {
        if self.total == 0 {
            return 10_000;
        }
        let done = u64::from(self.cursor.min(self.total));
        u32::try_from(done * 10_000 / u64::from(self.total)).unwrap_or(10_000)
    }

    /// A one-line summary for a progress line.
    pub fn summary(&self) -> String {
        format!(
            "v{} -> v{}  {}/{} ({:.1}%)  visited {}: {} migrated, {} already current, {} skipped",
            self.from,
            self.to,
            self.cursor,
            self.total,
            f64::from(self.progress_basis_points()) / 100.0,
            self.visited,
            self.migrated,
            self.already_current,
            self.skipped,
        )
    }
}

impl From<soroban_migrate::BatchOutcome> for BatchOutcomeView {
    /// Converts the contract's own type. Only `dry-run` can do this — it runs the
    /// migration in-process, so the value never had to survive a JSON rendering — and
    /// going through the real type rather than parsed text is what makes a fork-based
    /// dry run trustworthy: the numbers are the ones the contract computed.
    fn from(outcome: soroban_migrate::BatchOutcome) -> Self {
        Self {
            from: outcome.from,
            to: outcome.to,
            cursor: outcome.cursor,
            total: outcome.total,
            visited: outcome.visited,
            migrated: outcome.migrated,
            already_current: outcome.already_current,
            skipped: outcome.skipped,
            done: outcome.done,
        }
    }
}

/// The state of an in-flight migration, as `status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationStatusView {
    /// Version read from.
    pub from: u32,
    /// Version written to.
    pub to: u32,
    /// Cursor position.
    pub cursor: u32,
    /// Total keys observed.
    pub total: u32,
    /// Whether the migration is still running or rolling back.
    pub active: bool,
}

impl MigrationStatusView {
    /// Reads the state out of the `stellar` CLI's JSON rendering.
    ///
    /// # Errors
    ///
    /// [`crate::error::CliError::Network`] when the payload is not the expected object.
    pub fn parse(output: &str) -> Result<Self> {
        let status = crate::stellar::json_string(output, "status")?;
        Ok(Self {
            from: json_u32(output, "from")?,
            to: json_u32(output, "to")?,
            cursor: json_u32(output, "cursor")?,
            total: json_u32(output, "total")?,
            // `RollingBack` is active too: a rollback walks the same index and the
            // driver must not start a forward run on top of it.
            active: matches!(status.as_str(), "Running" | "RollingBack"),
        })
    }

    /// Keys left.
    pub fn remaining(&self) -> u32 {
        self.total.saturating_sub(self.cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DONE: &str = r#"{"from":1,"to":2,"cursor":1000,"total":1000,"visited":150,
        "migrated":150,"already_current":0,"skipped":0,"done":true}"#;
    const MIDWAY: &str = r#"{"from":1,"to":2,"cursor":150,"total":1000,"visited":150,
        "migrated":150,"already_current":0,"skipped":0,"done":false}"#;
    const STALLED: &str = r#"{"from":1,"to":2,"cursor":300,"total":1000,"visited":0,
        "migrated":0,"already_current":0,"skipped":0,"done":false}"#;

    #[test]
    fn an_outcome_is_read_field_by_field() {
        let outcome = BatchOutcomeView::parse(MIDWAY).unwrap();
        assert_eq!(outcome.cursor, 150);
        assert_eq!(outcome.total, 1000);
        assert_eq!(outcome.visited, 150);
        assert!(!outcome.done);
    }

    #[test]
    fn a_completed_batch_reports_done() {
        assert!(BatchOutcomeView::parse(DONE).unwrap().done);
    }

    #[test]
    fn a_batch_that_visits_nothing_without_finishing_is_a_stall() {
        let stalled = BatchOutcomeView::parse(STALLED).unwrap();
        assert!(stalled.is_stall());
        // The last batch of a migration can legitimately visit nothing before
        // reporting done, so `done` has to win.
        let empty_but_finished = r#"{"from":1,"to":2,"cursor":1000,"total":1000,"visited":0,
            "migrated":0,"already_current":0,"skipped":0,"done":true}"#;
        assert!(!BatchOutcomeView::parse(empty_but_finished)
            .unwrap()
            .is_stall());
    }

    #[test]
    fn progress_is_reported_in_basis_points_so_it_can_show_almost_done() {
        let midway = BatchOutcomeView::parse(MIDWAY).unwrap();
        assert_eq!(midway.progress_basis_points(), 1500);
        let nearly = BatchOutcomeView::parse(
            r#"{"from":1,"to":2,"cursor":9999,"total":10000,"visited":1,"migrated":1,
                "already_current":0,"skipped":0,"done":false}"#,
        )
        .unwrap();
        // 99.99% — which a rounded percentage would report as 100.
        assert_eq!(nearly.progress_basis_points(), 9999);
        assert_eq!(BatchOutcomeView::parse(DONE).unwrap().remaining(), 0);
    }

    #[test]
    fn a_missing_field_names_itself() {
        let error = BatchOutcomeView::parse(r#"{"from":1}"#).unwrap_err();
        assert!(error.to_string().contains("to"), "got: {error}");
    }

    #[test]
    fn a_wrong_entry_point_is_diagnosed_as_a_shape_mismatch() {
        // A number parses as JSON but is not the struct, so the field is what gets
        // named — which is how a `batch_fn` pointing at the wrong entry point reads.
        let error = BatchOutcomeView::parse("7").unwrap_err();
        assert!(error.to_string().contains("`from`"), "got: {error}");
        let error = BatchOutcomeView::parse("not json").unwrap_err();
        assert!(error.to_string().contains("JSON object"), "got: {error}");
    }

    #[test]
    fn a_rollback_counts_as_active_so_a_forward_run_does_not_start_on_top() {
        let rolling = r#"{"from":1,"to":2,"cursor":10,"total":100,"status":"RollingBack",
            "started_ledger":1}"#;
        assert!(MigrationStatusView::parse(rolling).unwrap().active);
        let finished = r#"{"from":1,"to":2,"cursor":100,"total":100,"status":"Completed",
            "started_ledger":1}"#;
        assert!(!MigrationStatusView::parse(finished).unwrap().active);
    }
}
