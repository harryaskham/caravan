//! Queue-owned native Stack repair consumes trees, not merge-synced history.
//!
//! The generic sequencer deliberately preserves authored topology. Its clean
//! final-tree proof cannot prevent intermediate conflicts while replaying main
//! commits imported by a merge (bd-93b5ba). Here a unique three-way base proves
//! the final tree directly; a new merge retains both exact input heads. Explicit
//! owner rebase and non-force repair keep their separate policies.

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    PlannedBase, PlannedRangeBase, PreparedRebase, RebaseExecutionBudget, RebasePlan,
    RebaseTopologyCommit, TemporaryWorktree, decision, process_runner, require_success, rev_parse,
    validate_oid, validate_rewritten_metadata,
};
use crate::AppError;
use crate::command::CommandSpec;
use crate::model::{CommitOid, PullRequestSnapshot};

/// The exact three-way tree inputs, retained alongside head/base generations in
/// the plan and receipt. This is content evidence, not publication authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CumulativeTreeReplay {
    pub merge_base_oid: CommitOid,
    pub source_tree_oid: CommitOid,
    pub target_tree_oid: CommitOid,
    pub result_tree_oid: CommitOid,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn prepare(
    repository_path: &Path,
    candidate: &PullRequestSnapshot,
    range_source: PlannedRangeBase,
    new_base: PlannedBase,
    range_base: CommitOid,
    ci_trigger_workflows: Vec<String>,
    budget: &RebaseExecutionBudget,
) -> Result<PreparedRebase, AppError> {
    if budget.flatten_squashed_root
        || budget.reconcile_squash_equivalent
        || budget.replay_upstream.is_some()
    {
        return Err(AppError::validation(
            "rebase_cumulative_policy_conflict",
            "cumulative tree repair cannot also flatten or drop a source history range",
        ));
    }
    let target = new_base.branch();
    let runner = process_runner(
        repository_path,
        budget.command_timeout,
        budget.operation_deadline,
        budget.writer_fence.as_ref(),
    );
    let bases = require_success(
        &runner,
        CommandSpec::new("git").args(["merge-base", "--all", &target.oid.0, &candidate.head.oid.0]),
        "rebase_range_ambiguous",
        "cumulative replay requires one proven three-way base",
    )?;
    let bases = bases.stdout.lines().collect::<Vec<_>>();
    if bases.len() != 1 {
        return Err(decision(
            "rebase_range_ambiguous",
            "cumulative replay requires one proven three-way base",
            json!({"pr": candidate.number, "merge_bases": bases, "mutated": false}),
        ));
    }
    let merge_base_oid = CommitOid(bases[0].to_owned());
    validate_oid(&merge_base_oid)?;
    let already_satisfied = merge_base_oid == target.oid;
    let source_tree_oid = rev_parse(&runner, &format!("{}^{{tree}}", candidate.head.oid))?;
    let target_tree_oid = rev_parse(&runner, &format!("{}^{{tree}}", target.oid))?;
    let result_tree_oid = if already_satisfied {
        source_tree_oid.clone()
    } else {
        let (outcome, tree, conflicting_paths) =
            crate::compatibility::merge_tree_with_base_with_runner(
                &runner,
                &candidate.head.oid,
                &target.oid,
                Some(&merge_base_oid),
            )?;
        if outcome != crate::model::CompatibilityOutcome::Clean {
            return Err(decision(
                "rebase_merge_tree_conflict",
                "the member's cumulative tree conflicts with its exact new predecessor",
                json!({
                    "pr": candidate.number, "target": target,
                    "old_head": candidate.head.oid, "old_base": candidate.base,
                    "merge_base": merge_base_oid, "conflicting_paths": conflicting_paths,
                    "mutated": false, "resumable": true,
                }),
            ));
        }
        CommitOid(tree)
    };
    // Count for the existing receipt only. Never sequence or reconstruct this
    // range: its net tree already includes the author's merge resolutions.
    let count = require_success(
        &runner,
        CommandSpec::new("git").args([
            "rev-list",
            "--count",
            &candidate.head.oid.0,
            &format!("^{range_base}"),
            &format!("^{}", target.oid),
        ]),
        "rebase_range_invalid",
        "cannot count the retained source range",
    )?;
    let commit_count =
        count.stdout.trim().parse::<usize>().map_err(|_| {
            AppError::validation("rebase_range_invalid", "invalid source commit count")
        })?;
    let worktree = TemporaryWorktree::create(
        repository_path,
        &candidate.head.oid,
        budget.worktree_setup_timeout(),
        budget.operation_deadline,
        budget.writer_fence.clone(),
    )?;
    let runner = process_runner(
        &worktree.path,
        budget.command_timeout,
        budget.operation_deadline,
        budget.writer_fence.as_ref(),
    );
    if !already_satisfied {
        let message = format!(
            "Merge branch '{}' into {}\n\nCaravan cumulative tree repair for PR #{}.",
            target.name, candidate.head.name, candidate.number,
        );
        let commit = require_success(
            &runner,
            CommandSpec::new("git").args([
                "commit-tree",
                &result_tree_oid.0,
                "-p",
                &candidate.head.oid.0,
                "-p",
                &target.oid.0,
                "-m",
                &message,
            ]),
            "rebase_cumulative_commit_failed",
            "could not retain the proven tree with both exact input parents",
        )?;
        let head = CommitOid(commit.stdout.trim().to_owned());
        validate_oid(&head)?;
        require_success(
            &runner,
            CommandSpec::new("git").args(["reset", "--hard", &head.0]),
            "rebase_cumulative_commit_failed",
            "could not retain the proven cumulative head for publication",
        )?;
        let parents = require_success(
            &runner,
            CommandSpec::new("git").args(["show", "-s", "--format=%P", "HEAD"]),
            "rebase_result_invalid",
            "cannot verify cumulative head parents",
        )?;
        if parents.stdout.split_whitespace().collect::<Vec<_>>()
            != [candidate.head.oid.0.as_str(), target.oid.0.as_str()]
        {
            return Err(AppError::validation(
                "rebase_result_invalid",
                "cumulative head lost one of its exact input parents",
            ));
        }
        validate_rewritten_metadata(
            &runner,
            &[RebaseTopologyCommit {
                oid: head,
                parents: vec![candidate.head.oid.clone(), target.oid.clone()],
                tree_oid: result_tree_oid.clone(),
            }],
            target,
            candidate.number,
        )?;
    }
    let new_head_oid = rev_parse(&runner, "HEAD")?;
    let new_tree_oid = rev_parse(&runner, "HEAD^{tree}")?;
    if new_tree_oid != result_tree_oid {
        return Err(AppError::validation(
            "rebase_merge_tree_mismatch",
            "prepared cumulative head differs from its independently proven tree",
        ));
    }
    let lease = format!(
        "--force-with-lease=refs/heads/{}:{}",
        candidate.head.name, candidate.head.oid,
    );
    require_success(
        &runner,
        CommandSpec::new("git").args([
            "push",
            "--dry-run",
            &lease,
            "origin",
            &format!("HEAD:refs/heads/{}", candidate.head.name),
        ]),
        "rebase_push_preflight_failed",
        "push permission, branch ownership, or exact lease preflight failed",
    )?;
    Ok(PreparedRebase {
        plan: RebasePlan {
            pr: candidate.number,
            branch: candidate.head.name.clone(),
            old_head_oid: candidate.head.oid.clone(),
            old_base_oid: range_base,
            range_source,
            new_base,
            new_head_oid,
            new_tree_oid,
            commit_count,
            merge_topology: None,
            cumulative_tree: Some(CumulativeTreeReplay {
                merge_base_oid,
                source_tree_oid,
                target_tree_oid,
                result_tree_oid,
            }),
            squash_reconciliation: None,
            ci_trigger_workflows,
            lease,
            already_satisfied,
            rewrite_reason: budget.rewrite_reason.clone(),
        },
        worktree,
    })
}
