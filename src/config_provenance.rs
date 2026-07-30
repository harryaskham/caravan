//! Where the effective configuration came from, and whether that is the
//! repository's policy or one branch's proposal.
//!
//! `.caravan/config.yaml` is read from the working tree, so the answer changes
//! with whatever branch happens to be checked out. A stale copy on an old
//! branch can fail validation and refuse every subsequent command, which is
//! exactly how an operator lost a session: `cara loop --manual` left them in an
//! agent checkout whose config predated a validation rule, and from then on
//! `cara` would not start.
//!
//! This module only *reports*. It deliberately changes no behaviour, so the
//! fleet can be measured before anything depends on the answer: in particular
//! how often `origin/HEAD` is absent, and how often a branch-local config
//! actually differs from the default branch's.
//!
//! Every probe is local, bounded, and failure-tolerant. Configuration loading
//! runs before any provider discovery, so the authoritative default branch is
//! not yet known and cannot be consulted without a circular dependency.

use std::path::Path;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::command::{CommandRunner, CommandSpec, ProcessRunner};

/// How the effective configuration relates to the repository's own policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigRelation {
    /// Read from an explicit `--config`, which always wins.
    Explicit,
    /// The checkout is on the default branch, so the working tree is policy.
    DefaultBranch,
    /// On another branch, but byte-identical to the default branch's copy.
    MatchesDefaultBranch,
    /// On another branch, and the content differs from the default branch.
    ///
    /// This is the case worth knowing about: the effective policy is one
    /// branch's proposal, not the repository's.
    DiffersFromDefaultBranch,
    /// The default branch could not be resolved locally, so nothing can be
    /// compared. Never treated as agreement.
    Unknown,
}

/// Bounded, local-only evidence about the effective configuration's origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigProvenance {
    pub schema_version: u32,
    pub relation: ConfigRelation,
    /// Branch the working tree is on, when it is on one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_branch: Option<String>,
    /// Locally recorded default branch, e.g. `origin/trunk`. Never assumed to
    /// be `main`, and stale whenever the remote default was renamed after the
    /// clone recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch_ref: Option<String>,
    /// Exact explanation, safe to show an operator.
    pub reason: String,
    /// Commits this checkout is behind the recorded default branch.
    ///
    /// A sync worktree was found parked on a dead agent's branch 95 commits
    /// behind main, so every policy value came from a three-day-old commit and
    /// nothing noticed. The distance is locally available and costs one
    /// `rev-list --count`, so it is always reported (bd-6f234e).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind_default_branch: Option<usize>,
}

impl ConfigProvenance {
    /// Whether the effective policy is provably an older generation's.
    ///
    /// Differing content alone is a deliberate branch proposal, which is
    /// legitimate. Differing content *from a checkout that is behind* is the
    /// stale-worktree shape: the operator's current policy was never read.
    #[must_use]
    pub fn is_stale_policy(&self) -> bool {
        self.relation == ConfigRelation::DiffersFromDefaultBranch
            && self.behind_default_branch.is_some_and(|behind| behind > 0)
    }
}

impl ConfigProvenance {
    /// Whether the effective policy is one branch's proposal rather than the
    /// repository's own.
    #[must_use]
    pub const fn is_branch_local_proposal(&self) -> bool {
        matches!(self.relation, ConfigRelation::DiffersFromDefaultBranch)
    }
}

/// Probe where the effective configuration came from. Never fails.
#[must_use]
pub fn resolve(repository_path: &Path, config_relative: &Path, explicit: bool) -> ConfigProvenance {
    if explicit {
        return ConfigProvenance {
            schema_version: 1,
            relation: ConfigRelation::Explicit,
            current_branch: None,
            default_branch_ref: None,
            reason: "explicit --config always wins over repository policy".to_owned(),
            behind_default_branch: None,
        };
    }
    let runner = ProcessRunner::in_directory(repository_path).with_timeout(Duration::from_secs(5));
    let current_branch = probe(&runner, &["branch", "--show-current"]);
    let default_branch_ref = probe(
        &runner,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    );

    let Some(default_ref) = default_branch_ref.clone() else {
        return ConfigProvenance {
            schema_version: 1,
            relation: ConfigRelation::Unknown,
            current_branch,
            default_branch_ref,
            reason: "no local refs/remotes/origin/HEAD, so the default branch is unknown; the working tree config is being used unchecked".to_owned(),
            behind_default_branch: None,
        };
    };
    // `origin/trunk` -> `trunk`. The default branch is never assumed to be
    // `main`; whatever the clone recorded is what gets compared.
    let default_branch = default_ref
        .split_once('/')
        .map_or(default_ref.as_str(), |(_, branch)| branch);

    // One cheap local count. Nothing else in Cara noticed that a worktree can
    // be arbitrarily far behind while still answering every question.
    let behind_default_branch = probe(
        &runner,
        &["rev-list", "--count", &format!("HEAD..{default_ref}")],
    )
    .and_then(|value| value.trim().parse::<usize>().ok());

    if current_branch.as_deref() == Some(default_branch) {
        return ConfigProvenance {
            schema_version: 1,
            relation: ConfigRelation::DefaultBranch,
            current_branch,
            default_branch_ref,
            reason: format!(
                "checkout is on the default branch `{default_branch}`, so the working tree is the repository policy"
            ),
            behind_default_branch,
        };
    }

    let relative = config_relative.to_string_lossy().replace('\\', "/");
    let committed = probe(&runner, &["show", &format!("{default_ref}:{relative}")]);
    let working = std::fs::read_to_string(repository_path.join(config_relative)).ok();
    let relation = match (committed, working) {
        (Some(committed), Some(working)) if committed.trim() == working.trim() => {
            ConfigRelation::MatchesDefaultBranch
        }
        (Some(_), Some(_)) => ConfigRelation::DiffersFromDefaultBranch,
        // An absent copy on either side is not agreement, and not a defect
        // either: `cara init` runs before any config is committed.
        _ => ConfigRelation::Unknown,
    };
    let reason = match relation {
        ConfigRelation::MatchesDefaultBranch => format!(
            "on branch `{}`, but the config is byte-identical to `{default_ref}`",
            current_branch
                .clone()
                .unwrap_or_else(|| "<detached>".to_owned())
        ),
        ConfigRelation::DiffersFromDefaultBranch => format!(
            "on branch `{}`, and its config DIFFERS from `{default_ref}`; the effective policy is this branch's proposal, not the repository's",
            current_branch
                .clone()
                .unwrap_or_else(|| "<detached>".to_owned())
        ),
        _ => format!("could not compare the working tree config against `{default_ref}`"),
    };
    let reason = match behind_default_branch {
        Some(behind) if behind > 0 && relation == ConfigRelation::DiffersFromDefaultBranch => {
            format!(
                "{reason}; the checkout is also {behind} commit(s) behind, so this policy is an older generation, not a current proposal"
            )
        }
        Some(behind) if behind > 0 => format!("{reason}; checkout is {behind} commit(s) behind"),
        _ => reason,
    };
    ConfigProvenance {
        schema_version: 1,
        relation,
        current_branch,
        default_branch_ref,
        reason,
        behind_default_branch,
    }
}

fn probe(runner: &ProcessRunner, arguments: &[&str]) -> Option<String> {
    let output = runner
        .run(&CommandSpec::new("git").args(arguments.iter().copied()))
        .ok()?;
    if !output.is_success() {
        return None;
    }
    let value = output.stdout.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default branch is whatever the repository says it is. Assuming
    /// `main` would silently mis-report every repository using `master`,
    /// `trunk`, or a bespoke name.
    #[test]
    fn an_explicit_config_needs_no_branch_comparison() {
        let provenance = resolve(Path::new("."), Path::new(".caravan/config.yaml"), true);

        assert_eq!(provenance.relation, ConfigRelation::Explicit);
        assert!(!provenance.is_branch_local_proposal());
    }

    /// Unknown must never read as agreement: an unresolvable default branch is
    /// exactly when an operator is most likely to be running stale policy.
    #[test]
    fn unknown_is_not_treated_as_agreement() {
        assert!(
            !ConfigProvenance {
                schema_version: 1,
                relation: ConfigRelation::Unknown,
                current_branch: None,
                default_branch_ref: None,
                reason: String::new(),
                behind_default_branch: None,
            }
            .is_branch_local_proposal()
        );
    }

    /// bd-6f234e: a sync worktree parked on a dead agent's branch 95 commits
    /// behind main read every policy value from a three-day-old commit, and
    /// nothing noticed. Differing config alone is a legitimate branch proposal;
    /// differing config from a checkout that is *behind* is stale policy.
    #[test]
    fn a_behind_checkout_with_differing_config_is_stale_policy_not_a_proposal() {
        let stale = ConfigProvenance {
            schema_version: 1,
            relation: ConfigRelation::DiffersFromDefaultBranch,
            current_branch: Some("agent/ms-dev-3/cacophony/abandoned".to_owned()),
            default_branch_ref: Some("origin/main".to_owned()),
            reason: String::new(),
            behind_default_branch: Some(95),
        };
        assert!(stale.is_stale_policy());
        assert!(stale.is_branch_local_proposal());

        let current_proposal = ConfigProvenance {
            behind_default_branch: Some(0),
            ..stale.clone()
        };
        assert!(
            !current_proposal.is_stale_policy(),
            "an up-to-date branch proposing new policy is legitimate"
        );

        let identical_but_behind = ConfigProvenance {
            relation: ConfigRelation::MatchesDefaultBranch,
            ..stale
        };
        assert!(
            !identical_but_behind.is_stale_policy(),
            "byte-identical policy is unaffected by unrelated commits"
        );
    }
}
