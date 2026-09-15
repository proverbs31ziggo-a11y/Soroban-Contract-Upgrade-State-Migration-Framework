//! The migration plan: the migration author's declarations about changes a schema
//! diff cannot interpret on its own.
//!
//! A diff can see that a field disappeared. It cannot know whether that loss is
//! intentional, or whether it is the half of a rename that the author has not
//! written down yet. Every framework that has tried to guess here has guessed
//! wrong in the direction of silent data loss.
//!
//! So the plan is explicit, small, checked into the repository next to the schema
//! it applies to, and *required*: a destructive change with no matching declaration
//! is [`crate::diff::Verdict::Denied`], and `soroban-migrate check` refuses the
//! upgrade. `soroban-migrate generate` writes the plan for you, pre-filled with
//! every declaration the diff demands, so the common path is still one command.
//!
//! # Why declarations are validated against the diff
//!
//! A plan that declares treatment for a change that does not exist is an error, not
//! a harmless leftover. It means the plan and the schemas have drifted: someone
//! reverted the field removal but kept the `dropped` entry, and the next time that
//! field is removed the stale declaration will silently authorise a lossy change.
//! [`crate::diff::diff_with_plan`] rejects those rather than letting them accumulate.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Declarations that turn a schema change from denied into reviewable.
///
/// Every map is keyed by field name and ordered deterministically, so a plan file
/// does not churn in version control between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationPlan {
    /// Version being migrated from. Checked against the schemas so a plan cannot be
    /// applied to the wrong pair.
    pub from: u32,
    /// Version being migrated to.
    pub to: u32,
    /// How each newly added field is populated, as a Rust expression for the *old*
    /// entry's value. `amount` means "read `amount` from the old shape", any other
    /// text is copied verbatim into the generated code.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub init: BTreeMap<String, String>,
    /// Fields the new shape intentionally discards, and whose values are therefore
    /// lost when an entry is rewritten.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<String>,
    /// Fields whose type changed, and the expression converting the old value.
    /// Empty string means "the generated code must decide", and the diff reports it
    /// as a decision still outstanding.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub converted: BTreeMap<String, String>,
    /// Fields renamed between versions, as `old name -> new name`.
    ///
    /// A rename is a removal plus an addition, which the diff would otherwise report
    /// as two separate problems. Declaring it lets the diff recognise the pair and
    /// tell the generated migration to carry the value across.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub renamed: BTreeMap<String, String>,
    /// Free-form note for the operator, carried into the generated file's header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl MigrationPlan {
    /// An empty plan for the given version pair, with nothing declared.
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            from,
            to,
            ..Self::default()
        }
    }

    /// Whether anything at all is declared.
    pub fn is_empty(&self) -> bool {
        self.init.is_empty()
            && self.dropped.is_empty()
            && self.converted.is_empty()
            && self.renamed.is_empty()
    }

    /// Parses a plan from JSON.
    ///
    /// # Errors
    ///
    /// `serde_json` errors are wrapped with the path by the caller.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Renders the plan as pretty JSON with a trailing newline.
    ///
    /// # Panics
    ///
    /// Never in practice: every field is a plain map or list, so serializing cannot
    /// fail. `expect` rather than `unwrap` so that a future field which makes it
    /// fallible says so at the call site.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("plan serialization cannot fail");
        s.push('\n');
        s
    }

    /// The new name for `old`, if a rename was declared.
    pub fn renamed_to(&self, old: &str) -> Option<&str> {
        self.renamed.get(old).map(String::as_str)
    }

    /// The old name for `new`, if a rename was declared.
    pub fn renamed_from(&self, new: &str) -> Option<&str> {
        self.renamed
            .iter()
            .find(|(_, v)| v.as_str() == new)
            .map(|(k, _)| k.as_str())
    }
}
