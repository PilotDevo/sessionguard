// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! What SessionGuard decided, and why — including when it decided to do
//! nothing.
//!
//! # The bug this exists to prevent
//!
//! SessionGuard's core promise was false for Claude Code, Codex and OpenCode
//! for months, and nothing in the product said so. This was the shape of it:
//!
//! ```ignore
//! ReconcileStrategy::Notify => {
//!     info!(tool = %tool.name, "notify-only strategy, no paths rewritten");
//!     ReconcileResult { actions_taken: vec![], success: true, error: None }
//! }
//! ```
//!
//! `success: true` with zero actions. Every time the daemon declined to do
//! anything it recorded a success, so **"worked" and "did nothing" were the
//! same value in the data model** and no amount of log-reading could tell them
//! apart. The `info!` line was even there; it told nobody anything, because a
//! healthy daemon and a completely inert one produced indistinguishable
//! output.
//!
//! A `bool` cannot express "succeeded and did nothing", so it gets used for
//! "didn't fail" — which is not the same thing. [`Outcome`] replaces it with a
//! type that can, and [`Outcome::acted`] makes the failing shape
//! unconstructible: ask for `Acted` with zero actions and you get a `NoOp`
//! with a reason instead.

use serde::{Deserialize, Serialize};

/// Why an operation deliberately did nothing.
///
/// An enum rather than a free-form string because these are a closed set worth
/// matching on, and because prose is exactly how the original bug hid: "no
/// paths rewritten" reads like a status line, not like a capability gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoOpReason {
    /// The tool declares `on_move = "notify"` — it has no in-project path to
    /// rewrite. **This is the v0.9.0 bug's reason code.** For a tool with a
    /// `session_store` this is expected and fine (re-keying handles it); for a
    /// tool with neither, it means the tool is not actually supported on move.
    ToolDeclaresNotify,
    /// Nothing matched the tool's declared session patterns at this path.
    NoArtifactsFound,
    /// The tool's home-dir store holds no sessions for this project.
    NoStoreForProject,
    /// The declared fields were found but already held the target value.
    AlreadyCurrent,
}

impl NoOpReason {
    /// Operator-facing explanation. Says what it means, not just what happened.
    pub fn explain(self) -> &'static str {
        match self {
            NoOpReason::ToolDeclaresNotify => {
                "tool declares on_move = \"notify\" — no in-project path to rewrite"
            }
            NoOpReason::NoArtifactsFound => "no session artifacts matched at this path",
            NoOpReason::NoStoreForProject => "the tool's store holds no sessions for this project",
            NoOpReason::AlreadyCurrent => "paths already pointed at the current location",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            NoOpReason::ToolDeclaresNotify => "tool_declares_notify",
            NoOpReason::NoArtifactsFound => "no_artifacts_found",
            NoOpReason::NoStoreForProject => "no_store_for_project",
            NoOpReason::AlreadyCurrent => "already_current",
        }
    }
}

/// What an operation actually did. Replaces `success: bool`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Did work. Carries the count, so "acted on nothing" is unrepresentable
    /// — see [`Outcome::acted`].
    Acted { actions: usize },
    /// Deliberately did nothing. **Not** a failure, and **not** a success:
    /// counted separately from both, everywhere.
    NoOp { reason: NoOpReason },
    /// Declined on purpose to protect data (destination store exists, database
    /// locked). Distinct from `Failed`: nothing broke, and the operator has
    /// something to do about it.
    Refused { reason: String },
    /// Tried and broke.
    Failed { error: String },
}

impl Outcome {
    /// The only way to build an `Acted`.
    ///
    /// Zero actions is not an action — it is a no-op, and the caller must say
    /// which kind. This is what makes the v0.9.0 shape (`success: true` with
    /// an empty action list) impossible to write.
    pub fn acted(actions: usize, if_none: NoOpReason) -> Outcome {
        if actions == 0 {
            Outcome::NoOp { reason: if_none }
        } else {
            Outcome::Acted { actions }
        }
    }

    /// True only when something actually changed. Deliberately NOT named
    /// `success`: a no-op and a refusal are both "not a failure", and
    /// conflating those with "worked" is the bug this module exists for.
    pub fn changed_something(&self) -> bool {
        matches!(self, Outcome::Acted { .. })
    }

    /// True when the operation broke, as opposed to declining.
    pub fn is_failure(&self) -> bool {
        matches!(self, Outcome::Failed { .. })
    }

    /// True when the operator has something to fix.
    pub fn needs_attention(&self) -> bool {
        matches!(self, Outcome::Failed { .. } | Outcome::Refused { .. })
    }

    /// Stored discriminant.
    pub fn kind(&self) -> &'static str {
        match self {
            Outcome::Acted { .. } => "acted",
            Outcome::NoOp { .. } => "noop",
            Outcome::Refused { .. } => "refused",
            Outcome::Failed { .. } => "failed",
        }
    }

    /// Stored reason, if any.
    pub fn reason(&self) -> Option<String> {
        match self {
            Outcome::Acted { .. } => None,
            Outcome::NoOp { reason } => Some(reason.as_str().to_string()),
            Outcome::Refused { reason } => Some(reason.clone()),
            Outcome::Failed { error } => Some(error.clone()),
        }
    }

    /// How many things changed (0 unless `Acted`).
    pub fn actions(&self) -> usize {
        match self {
            Outcome::Acted { actions } => *actions,
            _ => 0,
        }
    }

    /// One operator-facing line.
    pub fn describe(&self) -> String {
        match self {
            Outcome::Acted { actions } => format!("acted on {actions} item(s)"),
            Outcome::NoOp { reason } => format!("did nothing — {}", reason.explain()),
            Outcome::Refused { reason } => format!("refused — {reason}"),
            Outcome::Failed { error } => format!("failed — {error}"),
        }
    }
}

/// Which subsystem made the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Reconcile,
    Rekey,
    Daemon,
}

impl ActivityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActivityKind::Reconcile => "reconcile",
            ActivityKind::Rekey => "rekey",
            ActivityKind::Daemon => "daemon",
        }
    }
}

/// One recorded decision.
#[derive(Debug, Clone)]
pub struct ActivityRecord {
    pub kind: ActivityKind,
    pub tool_name: Option<String>,
    pub project_path: Option<String>,
    pub outcome: Outcome,
}

impl ActivityRecord {
    pub fn new(kind: ActivityKind, outcome: Outcome) -> Self {
        Self {
            kind,
            tool_name: None,
            project_path: None,
            outcome,
        }
    }

    pub fn tool(mut self, tool: impl Into<String>) -> Self {
        self.tool_name = Some(tool.into());
        self
    }

    pub fn project(mut self, path: impl Into<String>) -> Self {
        self.project_path = Some(path.into());
        self
    }

    /// Record to the event log AND emit a matching tracing event, so the two
    /// channels can never disagree about what happened. Best-effort: failing
    /// to record observability must never fail the operation being observed.
    pub fn emit(self, log: Option<&crate::event_log::EventLog>) {
        match &self.outcome {
            Outcome::Acted { actions } => tracing::info!(
                kind = self.kind.as_str(),
                tool = self.tool_name.as_deref().unwrap_or("-"),
                project = self.project_path.as_deref().unwrap_or("-"),
                actions,
                "acted"
            ),
            Outcome::NoOp { reason } => tracing::info!(
                kind = self.kind.as_str(),
                tool = self.tool_name.as_deref().unwrap_or("-"),
                project = self.project_path.as_deref().unwrap_or("-"),
                reason = reason.as_str(),
                "no-op"
            ),
            Outcome::Refused { reason } => tracing::warn!(
                kind = self.kind.as_str(),
                tool = self.tool_name.as_deref().unwrap_or("-"),
                project = self.project_path.as_deref().unwrap_or("-"),
                reason = %reason,
                "refused"
            ),
            Outcome::Failed { error } => tracing::error!(
                kind = self.kind.as_str(),
                tool = self.tool_name.as_deref().unwrap_or("-"),
                project = self.project_path.as_deref().unwrap_or("-"),
                error = %error,
                "failed"
            ),
        }
        if let Some(log) = log {
            if let Err(e) = log.record_activity(&self) {
                tracing::debug!(error = %e, "could not record activity");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acted_with_zero_actions_is_a_noop_not_a_success() {
        // The v0.9.0 shape, made unconstructible. `Notify` did no work and
        // reported success; through this constructor it cannot.
        let o = Outcome::acted(0, NoOpReason::ToolDeclaresNotify);
        assert_eq!(
            o,
            Outcome::NoOp {
                reason: NoOpReason::ToolDeclaresNotify
            }
        );
        assert!(!o.changed_something(), "a no-op did not change anything");
        assert!(!o.is_failure(), "...but it is not a failure either");
        assert_eq!(o.kind(), "noop");

        let real = Outcome::acted(3, NoOpReason::NoArtifactsFound);
        assert_eq!(real, Outcome::Acted { actions: 3 });
        assert!(real.changed_something());
        assert_eq!(real.actions(), 3);
    }

    #[test]
    fn refusal_is_neither_success_nor_failure_but_needs_attention() {
        // A refusal protected data on purpose. Reporting it as a failure cries
        // wolf; reporting it as a success hides that the operator must act.
        let r = Outcome::Refused {
            reason: "destination store exists".into(),
        };
        assert!(!r.changed_something());
        assert!(!r.is_failure());
        assert!(r.needs_attention());

        let noop = Outcome::NoOp {
            reason: NoOpReason::NoStoreForProject,
        };
        assert!(
            !noop.needs_attention(),
            "a routine no-op must not nag the operator"
        );
    }

    #[test]
    fn every_outcome_explains_itself() {
        // Whatever happened, the operator gets a sentence — the original bug
        // logged prose that explained nothing.
        for o in [
            Outcome::acted(2, NoOpReason::NoArtifactsFound),
            Outcome::NoOp {
                reason: NoOpReason::ToolDeclaresNotify,
            },
            Outcome::Refused {
                reason: "db locked".into(),
            },
            Outcome::Failed { error: "io".into() },
        ] {
            let d = o.describe();
            assert!(!d.is_empty() && d.len() > 8, "weak description: {d}");
            assert!(o.reason().is_some() || o.changed_something());
        }
        assert!(NoOpReason::ToolDeclaresNotify.explain().contains("notify"));
    }
}
