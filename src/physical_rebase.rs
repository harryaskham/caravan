//! Explicit, lease-protected physical branch chaining.
//!
//! This module is deliberately separate from compatibility checks: enabling
//! `rebase_on_join` authorizes history rewriting.  Every rewrite is first
//! completed in a detached temporary worktree, and the only remote mutation is
//! an exact force-with-lease push.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppError;
use crate::command::{CommandOutput, CommandRunner, CommandSpec, ProcessRunner};
use crate::model::{BranchSnapshot, CommitOid, PrNumber, PullRequestSnapshot, RepositoryId};
use crate::squash_equivalence::{self, SquashEquivalenceReport};

const MAX_MERGE_PRESERVING_COMMITS: usize = 256;

/// Shared child and whole-operation limits for one prepared generation.
/// Smallest timeout allowed for creating an isolated worktree.
///
/// Chosen from measurement rather than taste: the slowest observed cold run was
/// 23.6s on macOS for a 3801-file tree, so 60s leaves headroom for a larger
/// repository without ever being the thing that bounds a healthy tick. The
/// operation deadline remains the real bound.
const WORKTREE_SETUP_FLOOR: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct RebaseExecutionBudget {
    pub command_timeout: Duration,
    pub operation_deadline: Option<Instant>,
    /// Explicitly authorize reconciling squash-equivalent stacked history.
    ///
    /// Disabled by default: replaying already-landed history stays a typed
    /// conflict until a reviewed operation opts in, so no live provider branch
    /// is ever rewritten by mere detection.
    pub reconcile_squash_equivalent: bool,
    /// Explicitly authorize flattening a merge-preserving root that is about to
    /// be squash-merged.
    ///
    /// bd-85b71d: when Cara owns the merge, the root lands as one squash commit
    /// and its history is discarded. Replaying that history commit-by-commit can
    /// only fail — a root that merged the default branch into itself re-hits
    /// conflicts its author already resolved by hand, with no stored rerere
    /// resolution — while proving nothing about what actually lands. Disabled by
    /// default so a child, whose ancestry must physically follow the chain, is
    /// never flattened.
    pub flatten_squashed_root: bool,
    /// Replay only commits after this exact boundary (bd-cef612).
    ///
    /// The sequencer normally uses the target as its upstream, so it replays
    /// everything in `target..head`. When a member is evicted, its descendants
    /// must instead replay only their own commits, which means starting from
    /// the evicted head. Set explicitly by eviction unwind; unset elsewhere so
    /// no ordinary rewrite silently changes which commits it carries.
    pub replay_upstream: Option<CommitOid>,
}

impl RebaseExecutionBudget {
    #[must_use]
    pub fn new(command_timeout: Duration) -> Self {
        Self {
            command_timeout,
            operation_deadline: None,
            reconcile_squash_equivalent: false,
            flatten_squashed_root: false,
            replay_upstream: None,
        }
    }

    /// Timeout for creating the isolated worktree.
    ///
    /// Worktree creation is a fixed setup cost proportional to TREE SIZE, not to
    /// the work being replayed, so pricing it at the same per-command share as
    /// every other step mis-prices it. Measured on an identical repository
    /// (3801 tracked files): 0.7-3.2s on every Linux node, but 12-23s on macOS,
    /// against an observed 10176ms sub-deadline. That made macOS structurally
    /// unable to complete a physical rebase at all, and physical rebase is how
    /// members stack (bd-1a4e28).
    ///
    /// The operation deadline still bounds this: `effective_timeout` clamps every
    /// command to the remaining tick budget, so a larger floor cannot overrun
    /// the tick, it only stops a slow filesystem being mistaken for a hang.
    #[must_use]
    pub fn worktree_setup_timeout(&self) -> Duration {
        self.command_timeout.max(WORKTREE_SETUP_FLOOR)
    }

    /// Replay only commits after `boundary` (bd-cef612).
    #[must_use]
    pub fn replaying_after(mut self, boundary: CommitOid) -> Self {
        self.replay_upstream = Some(boundary);
        self
    }

    /// Authorize flattening a squash-merged root (bd-85b71d).
    #[must_use]
    pub fn flattening_squashed_root(mut self, flatten: bool) -> Self {
        self.flatten_squashed_root = flatten;
        self
    }

    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.operation_deadline = Some(deadline);
        self
    }

    /// Authorize dropping stacked commits the target already holds byte-for-byte.
    #[must_use]
    pub const fn with_squash_reconciliation(mut self, authorized: bool) -> Self {
        self.reconcile_squash_equivalent = authorized;
        self
    }
}

/// Whether a planned base already exists remotely or is a retained simulated parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "branch", rename_all = "snake_case")]
pub enum PlannedBase {
    Remote(BranchSnapshot),
    Simulated(BranchSnapshot),
}

impl PlannedBase {
    fn branch(&self) -> &BranchSnapshot {
        match self {
            Self::Remote(branch) | Self::Simulated(branch) => branch,
        }
    }
}

/// Exact source used to retain and verify the candidate-only range boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannedRangeBase {
    RemoteBranch {
        branch: BranchSnapshot,
    },
    /// Provider-retained old base generation after the same default branch
    /// advanced. The old commit is verified as an ancestor of `current` rather
    /// than incorrectly requiring the moving branch ref to equal the old OID.
    HistoricalTargetBranch {
        branch: BranchSnapshot,
        current: BranchSnapshot,
    },
    /// Provider-retained old child base while the named parent branch has
    /// already advanced and will itself be rewritten to `new_base` in the same
    /// globally verified physical-sync batch.
    HistoricalParentBranch {
        branch: BranchSnapshot,
        current: BranchSnapshot,
    },
    /// Retained source boundary from an older configured-default generation.
    /// `current` proves the named default ref advanced while the source-only
    /// patch remains anchored at `branch`.
    HistoricalSourceBranch {
        branch: BranchSnapshot,
        current: BranchSnapshot,
    },
    PullRequestHead {
        pr: PrNumber,
        branch: BranchSnapshot,
    },
}

impl PlannedRangeBase {
    fn branch(&self) -> &BranchSnapshot {
        match self {
            Self::RemoteBranch { branch }
            | Self::HistoricalTargetBranch { branch, .. }
            | Self::HistoricalParentBranch { branch, .. }
            | Self::HistoricalSourceBranch { branch, .. }
            | Self::PullRequestHead { branch, .. } => branch,
        }
    }
}

/// Select an exact candidate range boundary for a live remote target.
///
/// GitHub's PR base OID is the candidate's retained lower range boundary, but
/// the named default branch can legitimately advance before the next sync. In
/// that case, bind both generations explicitly instead of treating the old OID
/// as the current remote branch lease.
#[must_use]
pub fn range_base_for_remote_target(
    candidate: &PullRequestSnapshot,
    target: &BranchSnapshot,
) -> PlannedRangeBase {
    if candidate.base.name == target.name && candidate.base.oid != target.oid {
        PlannedRangeBase::HistoricalTargetBranch {
            branch: candidate.base.clone(),
            current: target.clone(),
        }
    } else {
        PlannedRangeBase::RemoteBranch {
            branch: candidate.base.clone(),
        }
    }
}

/// Select a retained child range when GitHub's `BaseRefOid` lags the exact
/// current parent head which this same physical batch will rewrite.
#[must_use]
pub fn range_base_for_rewritten_parent(
    candidate: &PullRequestSnapshot,
    current_parent: &BranchSnapshot,
) -> PlannedRangeBase {
    if candidate.base.name == current_parent.name && candidate.base.oid != current_parent.oid {
        PlannedRangeBase::HistoricalParentBranch {
            branch: candidate.base.clone(),
            current: current_parent.clone(),
        }
    } else {
        PlannedRangeBase::RemoteBranch {
            branch: candidate.base.clone(),
        }
    }
}

/// One exact commit and parent set in a retained nonlinear candidate range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RebaseTopologyCommit {
    pub oid: CommitOid,
    #[serde(default)]
    pub parents: Vec<CommitOid>,
    pub tree_oid: CommitOid,
}

/// Deterministic old→new topology mapping for one replayed commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RebaseTopologyMapping {
    pub old_oid: CommitOid,
    pub new_oid: CommitOid,
    #[serde(default)]
    pub old_parents: Vec<CommitOid>,
    #[serde(default)]
    pub new_parents: Vec<CommitOid>,
}

/// Complete merge-preserving proof retained in plan and apply receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MergePreservingTopology {
    pub strategy: String,
    pub expected_merge_tree_oid: CommitOid,
    #[serde(default)]
    pub old_commits: Vec<RebaseTopologyCommit>,
    #[serde(default)]
    pub new_commits: Vec<RebaseTopologyCommit>,
    #[serde(default)]
    pub mapping: Vec<RebaseTopologyMapping>,
}

/// Serializable immutable plan produced once and pushed without recomputation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RebasePlan {
    pub pr: PrNumber,
    pub branch: String,
    pub old_head_oid: CommitOid,
    pub old_base_oid: CommitOid,
    pub range_source: PlannedRangeBase,
    pub new_base: PlannedBase,
    pub new_head_oid: CommitOid,
    pub new_tree_oid: CommitOid,
    /// Commits in the exact source range, including any this plan reconciled
    /// away because the target already holds them; `squash_reconciliation`
    /// lists exactly which commits were dropped and which were replayed.
    pub commit_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_topology: Option<MergePreservingTopology>,
    /// Exact proof for any stacked history this plan reconciled away because
    /// the target already holds its cumulative content byte-for-byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squash_reconciliation: Option<SquashEquivalenceReport>,
    pub ci_trigger_workflows: Vec<String>,
    pub lease: String,
    pub already_satisfied: bool,
}

/// A plan plus the exact temporary object/worktree generation that created it.
pub struct PreparedRebase {
    pub plan: RebasePlan,
    worktree: TemporaryWorktree,
}

/// Auditable receipt for one physical branch rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RebaseReceipt {
    pub pr: PrNumber,
    pub branch: String,
    pub old_head_oid: CommitOid,
    pub new_head_oid: CommitOid,
    pub old_base_oid: CommitOid,
    pub new_base_branch: String,
    pub new_base_oid: CommitOid,
    pub new_tree_oid: CommitOid,
    pub commit_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_topology: Option<MergePreservingTopology>,
    /// Exact proof for any stacked history this rewrite reconciled away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squash_reconciliation: Option<SquashEquivalenceReport>,
    /// Exact-default workflow files proven able to run for this PR base on
    /// both force-push (`synchronize`) and base edit (`edited`).
    pub ci_trigger_workflows: Vec<String>,
    /// The exact optimistic lease passed to Git.
    pub lease: String,
    /// True when no push was needed because the exact ancestry already held.
    pub already_satisfied: bool,
}

/// Rebase `candidate`'s candidate-only linear series onto `new_base`.
///
/// Both snapshots must belong to the base repository. The candidate's exact
/// provider base is the lower range boundary; refusing merge commits and a
/// non-ancestor boundary prevents accidentally forcing an ambiguous patch.
pub fn rewrite_candidate(
    repository_path: &Path,
    repository: &RepositoryId,
    candidate: &PullRequestSnapshot,
    new_base: &BranchSnapshot,
    workflow_source: &BranchSnapshot,
    timeout: Duration,
) -> Result<RebaseReceipt, AppError> {
    let prepared = prepare_candidate(
        repository_path,
        repository,
        candidate,
        range_base_for_remote_target(candidate, new_base),
        PlannedBase::Remote(new_base.clone()),
        workflow_source,
        RebaseExecutionBudget::new(timeout),
    )?;
    apply_prepared(&prepared)
}

/// Materialize one exact rebase generation and retain its worktree through apply.
#[allow(clippy::too_many_lines)]
#[allow(clippy::needless_pass_by_value)]
pub fn prepare_candidate(
    repository_path: &Path,
    repository: &RepositoryId,
    candidate: &PullRequestSnapshot,
    range_source: PlannedRangeBase,
    new_base: PlannedBase,
    workflow_source: &BranchSnapshot,
    budget: RebaseExecutionBudget,
) -> Result<PreparedRebase, AppError> {
    let target = new_base.branch();
    let range_branch = range_source.branch();
    if candidate.cross_repository
        || candidate.head.repository != *repository
        || candidate.base.repository != *repository
        || range_branch.repository != *repository
        || range_branch.oid != candidate.base.oid
        || target.repository != *repository
        || workflow_source.repository != *repository
    {
        return Err(decision(
            "rebase_repository_not_owned",
            "cumulative rebase requires candidate, old base, and new base branches in the owned base repository",
            json!({"pr": candidate.number, "resumable": true}),
        ));
    }
    if let PlannedRangeBase::HistoricalTargetBranch { current, .. } = &range_source
        && (current != target || current.repository != *repository)
    {
        return Err(decision(
            "rebase_historical_target_mismatch",
            "historical target range must bind the exact current remote target generation",
            json!({"pr": candidate.number, "historical_target": current, "target": target, "resumable": true}),
        ));
    }
    if let PlannedRangeBase::HistoricalParentBranch { branch, current } = &range_source
        && (branch.name != current.name
            || current.name != target.name
            || current.repository != *repository
            || !matches!(&new_base, PlannedBase::Simulated(_)))
    {
        return Err(decision(
            "rebase_historical_parent_mismatch",
            "historical child range must bind the exact current parent branch selected for same-batch rewrite",
            json!({"pr": candidate.number, "historical_parent": current, "target": target, "resumable": true}),
        ));
    }
    if let PlannedRangeBase::HistoricalSourceBranch { current, .. } = &range_source
        && (current != workflow_source || current.repository != *repository)
    {
        return Err(decision(
            "rebase_historical_source_mismatch",
            "historical source range must bind the exact current configured-default generation",
            json!({"pr": candidate.number, "historical_source": current, "workflow_source": workflow_source, "resumable": true}),
        ));
    }
    validate_branch(&candidate.head.name)?;
    for oid in [
        &candidate.head.oid,
        &candidate.base.oid,
        &target.oid,
        &workflow_source.oid,
    ] {
        validate_oid(oid)?;
    }

    let runner = process_runner(
        repository_path,
        budget.command_timeout,
        budget.operation_deadline,
    );
    fetch_exact(&runner, "origin", &candidate.head)?;
    match &new_base {
        PlannedBase::Remote(branch)
            if branch.name != candidate.base.name || branch.oid != candidate.base.oid =>
        {
            fetch_exact(&runner, "origin", branch)?;
        }
        PlannedBase::Simulated(branch) => {
            require_success(
                &runner,
                CommandSpec::new("git").args([
                    "cat-file",
                    "-e",
                    &format!("{}^{{commit}}", branch.oid),
                ]),
                "rebase_simulated_parent_missing",
                "planned parent object is not retained locally",
            )?;
        }
        PlannedBase::Remote(_) => {}
    }
    match &range_source {
        PlannedRangeBase::RemoteBranch { branch } => fetch_exact(&runner, "origin", branch)?,
        PlannedRangeBase::HistoricalTargetBranch { branch, current }
        | PlannedRangeBase::HistoricalSourceBranch { branch, current } => {
            retain_historical_target_base(&runner, branch, current)?;
        }
        PlannedRangeBase::HistoricalParentBranch { branch, current } => {
            fetch_exact(&runner, "origin", current)?;
            retain_historical_target_base(&runner, branch, current)?;
        }
        PlannedRangeBase::PullRequestHead { pr, branch } => {
            fetch_exact_pull_request_head(&runner, "origin", *pr, branch)?;
        }
    }
    if workflow_source.name != candidate.base.name || workflow_source.oid != candidate.base.oid {
        fetch_exact(&runner, "origin", workflow_source)?;
    }
    let ci_trigger_workflows = preflight_ci_triggers(&runner, workflow_source, &target.name)?;

    let merge_bases = require_success(
        &runner,
        CommandSpec::new("git").args([
            "merge-base",
            "--all",
            candidate.base.oid.0.as_str(),
            candidate.head.oid.0.as_str(),
        ]),
        "rebase_range_ambiguous",
        "the exact candidate/base merge base could not be determined",
    )?;
    let boundaries = merge_bases.stdout.lines().collect::<Vec<_>>();
    if boundaries.len() != 1 {
        return Err(decision(
            "rebase_range_ambiguous",
            "candidate and exact provider base do not have one unambiguous merge base",
            json!({"pr": candidate.number, "merge_bases": boundaries, "resumable": true}),
        ));
    }
    let range_base = CommitOid(boundaries[0].to_owned());
    validate_oid(&range_base)?;
    let old_topology = collect_range_topology(
        &runner,
        &[&range_base, &target.oid],
        &candidate.head.oid,
        "rebase_range_invalid",
    )?;
    if old_topology.is_empty() {
        return Err(decision(
            "rebase_empty_patch_range",
            "candidate has no commits beyond its exact old base",
            json!({"pr": candidate.number, "resumable": true}),
        ));
    }
    let commit_count = old_topology.len();
    let has_merges = old_topology.iter().any(|commit| commit.parents.len() == 2);
    let expected_merge_tree = if has_merges {
        validate_merge_preserving_topology(&runner, &old_topology, target, candidate.number)?;
        Some(expected_merge_tree(
            &runner,
            target,
            &candidate.head.oid,
            candidate.number,
        )?)
    } else {
        None
    };

    let worktree = TemporaryWorktree::create(
        repository_path,
        &candidate.head.oid,
        budget.worktree_setup_timeout(),
        budget.operation_deadline,
    )?;
    let worktree_runner = process_runner(
        &worktree.path,
        budget.command_timeout,
        budget.operation_deadline,
    );
    // Reconciliation is opt-in and additionally requires exact proof, so a
    // routine rewrite never silently drops history. A merge-preserving range
    // is excluded: only an ancestor-closed linear range can be proven here.
    let reconciliation = if budget.reconcile_squash_equivalent && !has_merges {
        authorized_reconciliation(&runner, candidate, target, &range_base)?
    } else {
        None
    };
    // bd-85b71d: a merge-preserving root that Cara will squash-merge does not
    // need its history replayed. `expected_merge_tree` has already proven the
    // exact content that will land, so build that commit directly instead of
    // re-resolving conflicts the author already resolved by hand.
    let flattened = if has_merges && budget.flatten_squashed_root {
        Some(flatten_to_target(
            &worktree_runner,
            candidate,
            target,
            expected_merge_tree
                .as_ref()
                .expect("a merge-preserving range always proves its merge tree"),
        )?)
    } else {
        None
    };
    let rebase = if flattened.is_some() {
        CommandOutput::success(String::new())
    } else {
        run_rebase(
            &worktree_runner,
            &range_base,
            &target.oid,
            &candidate.head.oid,
            has_merges,
            budget.replay_upstream.as_ref().or_else(|| {
                reconciliation
                    .as_ref()
                    .and_then(SquashEquivalenceReport::authorized_range_base)
            }),
        )?
    };
    if !rebase.is_success() {
        let conflicts = run(
            &worktree_runner,
            CommandSpec::new("git").args(["diff", "--name-only", "--diff-filter=U"]),
        )
        .map(|output| output.stdout.lines().map(str::to_owned).collect::<Vec<_>>())
        .unwrap_or_default();
        return Err(decision(
            if has_merges {
                "rebase_merge_replay_conflict"
            } else {
                "rebase_conflict"
            },
            if has_merges {
                "the independently clean final merge tree cannot be reproduced by the audited merge-preserving sequencer without an intermediate conflict; no remote or provider mutation was attempted"
            } else {
                "candidate-only commits do not rebase cleanly; no remote or provider mutation was attempted"
            },
            json!({
                "pr": candidate.number,
                "conflicting_paths": conflicts,
                "merge_commits": old_topology.iter().filter(|commit| commit.parents.len() == 2).map(|commit| &commit.oid).collect::<Vec<_>>(),
                "expected_merge_tree": &expected_merge_tree,
                "stderr": rebase.stderr,
                "resumable": true,
                "next": if has_merges {
                    "use a reviewed first-party repair/reshape of the reported merge sequence, then rerun the same command"
                } else {
                    "repair the branch and rerun the same command"
                }
            }),
        ));
    }
    let new_head = rev_parse(&worktree_runner, "HEAD")?;
    let new_tree = rev_parse(&worktree_runner, "HEAD^{tree}")?;
    if let Some(report) = &reconciliation {
        verify_reconciled_replay(
            &worktree_runner,
            report,
            &target.oid,
            &new_head,
            &new_tree,
            candidate.number,
        )?;
    }
    let merge_topology = expected_merge_tree
        .filter(|_| flattened.is_none())
        .map(|expected| {
            build_merge_topology_proof(
                &worktree_runner,
                &old_topology,
                &target.oid,
                &new_head,
                &new_tree,
                expected,
                candidate.number,
            )
        })
        .transpose()?;
    let lease = format!(
        "--force-with-lease=refs/heads/{}:{}",
        candidate.head.name, candidate.head.oid.0
    );
    let destination = format!("HEAD:refs/heads/{}", candidate.head.name);

    // This checks authentication, branch rules, and the exact lease without
    // updating the branch. It occurs only after the complete conflict simulation.
    require_success(
        &worktree_runner,
        CommandSpec::new("git").args([
            "push",
            "--dry-run",
            lease.as_str(),
            "origin",
            destination.as_str(),
        ]),
        "rebase_push_preflight_failed",
        "push permission, branch ownership, or exact lease preflight failed",
    )?;

    let already_satisfied = new_head == candidate.head.oid;
    let plan = RebasePlan {
        pr: candidate.number,
        branch: candidate.head.name.clone(),
        old_head_oid: candidate.head.oid.clone(),
        old_base_oid: range_base,
        range_source,
        new_base,
        new_head_oid: new_head,
        new_tree_oid: new_tree,
        commit_count,
        merge_topology,
        squash_reconciliation: reconciliation,
        ci_trigger_workflows,
        lease,
        already_satisfied,
    };
    Ok(PreparedRebase { plan, worktree })
}

fn collect_range_topology(
    runner: &impl CommandRunner,
    boundaries: &[&CommitOid],
    head: &CommitOid,
    code: &'static str,
) -> Result<Vec<RebaseTopologyCommit>, AppError> {
    let mut arguments = vec![
        "rev-list".to_owned(),
        "--reverse".to_owned(),
        "--topo-order".to_owned(),
        "--parents".to_owned(),
        head.0.clone(),
    ];
    arguments.extend(boundaries.iter().map(|base| format!("^{}", base.0)));
    let output = require_success(
        runner,
        CommandSpec::new("git").args(arguments),
        code,
        "could not enumerate the exact candidate topology",
    )?;
    let lines = output.stdout.lines().collect::<Vec<_>>();
    if lines.len() > MAX_MERGE_PRESERVING_COMMITS {
        return Err(decision(
            "rebase_topology_limit",
            "candidate topology exceeds the audited merge-preserving bound",
            json!({"commits": lines.len(), "limit": MAX_MERGE_PRESERVING_COMMITS, "resumable": true}),
        ));
    }
    lines
        .into_iter()
        .map(|line| {
            let mut fields = line.split_whitespace();
            let oid = CommitOid(fields.next().unwrap_or_default().to_owned());
            validate_oid(&oid)?;
            let parents = fields
                .map(|parent| {
                    let oid = CommitOid(parent.to_owned());
                    validate_oid(&oid)?;
                    Ok(oid)
                })
                .collect::<Result<Vec<_>, AppError>>()?;
            if parents.len() > 2 {
                return Err(decision(
                    "rebase_unsupported_octopus",
                    "merge-preserving rewrite supports only bounded two-parent topology",
                    json!({"commit": oid, "parents": parents, "resumable": true}),
                ));
            }
            let tree_oid = rev_parse(runner, &format!("{}^{{tree}}", oid.0))?;
            Ok(RebaseTopologyCommit {
                oid,
                parents,
                tree_oid,
            })
        })
        .collect()
}

fn is_ancestor(
    runner: &impl CommandRunner,
    ancestor: &CommitOid,
    descendant: &CommitOid,
) -> Result<bool, AppError> {
    Ok(run(
        runner,
        CommandSpec::new("git").args([
            "merge-base",
            "--is-ancestor",
            ancestor.0.as_str(),
            descendant.0.as_str(),
        ]),
    )?
    .is_success())
}

fn validate_merge_preserving_topology(
    runner: &impl CommandRunner,
    topology: &[RebaseTopologyCommit],
    target: &BranchSnapshot,
    pr: PrNumber,
) -> Result<(), AppError> {
    let range = topology
        .iter()
        .map(|commit| commit.oid.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for commit in topology {
        if commit.parents.is_empty() {
            return Err(decision(
                "rebase_cousin_history",
                "candidate topology contains a root that is not anchored in exact target ancestry",
                json!({"pr": pr, "commit": commit, "target": target, "resumable": true}),
            ));
        }
        let in_range_parents = commit
            .parents
            .iter()
            .filter(|parent| range.contains(*parent))
            .count();
        if commit.parents.len() == 2 && in_range_parents == 0 {
            return Err(decision(
                "rebase_external_merge_parents",
                "merge commit has no parent in the candidate-only range",
                json!({"pr": pr, "commit": commit, "target": target, "resumable": true}),
            ));
        }
        for parent in commit
            .parents
            .iter()
            .filter(|parent| !range.contains(*parent))
        {
            if !is_ancestor(runner, parent, &target.oid)? {
                return Err(decision(
                    "rebase_cousin_history",
                    "candidate merge references history outside both the candidate range and target ancestry",
                    json!({"pr": pr, "commit": commit, "external_parent": parent, "target": target, "resumable": true}),
                ));
            }
        }
    }
    Ok(())
}

/// Build one commit carrying the proven merge tree directly on the target.
///
/// bd-85b71d: used only for a root Cara is about to squash-merge, where history
/// is discarded at landing. The resulting head has the exact tree that
/// `expected_merge_tree` already proved clean against the target, so the content
/// that lands is unchanged while nothing has to be replayed.
fn flatten_to_target(
    runner: &impl CommandRunner,
    candidate: &PullRequestSnapshot,
    target: &BranchSnapshot,
    expected_tree: &CommitOid,
) -> Result<CommitOid, AppError> {
    let message = format!(
        "{}\n\nFlattened by Caravan for squash landing (PR #{}).",
        candidate.title, candidate.number
    );
    let commit = require_success(
        runner,
        CommandSpec::new("git").args([
            "commit-tree",
            expected_tree.0.as_str(),
            "-p",
            target.oid.0.as_str(),
            "-m",
            message.as_str(),
        ]),
        "rebase_flatten_failed",
        "the proven merge tree could not be committed onto the exact target",
    )?;
    let head = CommitOid(commit.stdout.trim().to_owned());
    validate_oid(&head)?;
    require_success(
        runner,
        CommandSpec::new("git").args(["reset", "--hard", head.0.as_str()]),
        "rebase_flatten_failed",
        "the flattened commit could not be checked out for verification",
    )?;
    Ok(head)
}

fn expected_merge_tree(
    runner: &impl CommandRunner,
    target: &BranchSnapshot,
    old_head: &CommitOid,
    pr: PrNumber,
) -> Result<CommitOid, AppError> {
    let output = run(
        runner,
        CommandSpec::new("git").args([
            "merge-tree",
            "--write-tree",
            target.oid.0.as_str(),
            old_head.0.as_str(),
        ]),
    )?;
    if !output.is_success() {
        return Err(decision(
            "rebase_merge_tree_conflict",
            "exact target and nonlinear candidate head do not have one clean merge result tree",
            json!({"pr": pr, "target": target, "old_head": old_head, "stdout": output.stdout, "stderr": output.stderr, "resumable": true}),
        ));
    }
    let tree = CommitOid(
        output
            .stdout
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned(),
    );
    validate_oid(&tree)?;
    Ok(tree)
}

fn run_rebase(
    runner: &impl CommandRunner,
    _range_base: &CommitOid,
    target: &CommitOid,
    old_head: &CommitOid,
    preserve_merges: bool,
    reconciled_upstream: Option<&CommitOid>,
) -> Result<CommandOutput, AppError> {
    let mut arguments = vec![
        "-c".to_owned(),
        "commit.gpgSign=false".to_owned(),
        "-c".to_owned(),
        "user.name=Caravan".to_owned(),
        "-c".to_owned(),
        "user.email=caravan@localhost.invalid".to_owned(),
        "rebase".to_owned(),
    ];
    if preserve_merges {
        arguments.extend([
            "--rebase-merges=no-rebase-cousins".to_owned(),
            "--reapply-cherry-picks".to_owned(),
            "--empty=keep".to_owned(),
        ]);
    }
    // Use the exact target as the upstream for both sequencers. Git can then
    // omit patch-equivalent commits already represented on the target while
    // replaying only genuinely unique source commits. The separately retained
    // range_base still binds and validates source provenance.
    //
    // A reconciled upstream replaces it only when squash-equivalence analysis
    // proved, path by path, that the target already holds that boundary's
    // cumulative content; the replay then starts from proven-landed history
    // instead of re-applying it against itself.
    let upstream = reconciled_upstream.unwrap_or(target);
    arguments.extend([
        "--committer-date-is-author-date".to_owned(),
        "--onto".to_owned(),
        target.0.clone(),
        upstream.0.clone(),
        old_head.0.clone(),
    ]);
    run(
        runner,
        CommandSpec::new("git")
            .args(arguments)
            .env("GIT_TERMINAL_PROMPT", "0"),
    )
}

/// Prove whether an opt-in reconciliation may drop stacked history here.
///
/// Returns a report only when squash-equivalence analysis authorized a
/// boundary *and* that boundary is a genuine interior commit of this exact
/// replay range. Anything else fails closed to the ordinary replay, which
/// still reports a typed conflict instead of dropping unproven history.
fn authorized_reconciliation(
    runner: &impl CommandRunner,
    candidate: &PullRequestSnapshot,
    target: &BranchSnapshot,
    range_base: &CommitOid,
) -> Result<Option<SquashEquivalenceReport>, AppError> {
    let report = squash_equivalence::analyze_with_runner(
        runner,
        &candidate.head,
        target,
        &candidate.head.oid,
        &target.oid,
    )?;
    let Some(boundary) = report.authorized_range_base() else {
        return Ok(None);
    };
    // The proven boundary must sit strictly inside this replay range: after the
    // retained source boundary, before the candidate head, and not already an
    // ancestor-equal of either endpoint.
    if boundary == &candidate.head.oid
        || boundary == range_base
        || !is_ancestor(runner, range_base, boundary)?
        || !is_ancestor(runner, boundary, &candidate.head.oid)?
    {
        return Ok(None);
    }
    Ok(Some(report))
}

/// Independently verify one reconciled replay against its own proof.
///
/// The rebase is not trusted to have produced the proven result: the rebuilt
/// tree must equal the cumulative tree the reconciliation computed, and the
/// rebuilt commit count must equal exactly the retained commits. Any deviation
/// fails closed before the push preflight, so nothing is pushed.
fn verify_reconciled_replay(
    runner: &impl CommandRunner,
    report: &SquashEquivalenceReport,
    target: &CommitOid,
    new_head: &CommitOid,
    new_tree: &CommitOid,
    pr: PrNumber,
) -> Result<(), AppError> {
    let expected_tree = report
        .after
        .as_ref()
        .map(|evidence| evidence.result_tree.clone())
        .ok_or_else(|| {
            decision(
                "rebase_reconciled_proof_missing",
                "reconciled replay lost its cumulative tree proof",
                json!({"pr": pr, "resumable": true}),
            )
        })?;
    if new_tree != &expected_tree {
        return Err(decision(
            "rebase_reconciled_tree_mismatch",
            "reconciled replay produced a different cumulative tree than the independently proven reconciliation; nothing was pushed",
            json!({
                "pr": pr,
                "expected_tree": expected_tree,
                "actual_tree": new_tree,
                "reconciliation": report.details(),
                "mutated": false,
                "resumable": true,
                "next": "rediscover exact revisions and rerun without squash-equivalence reconciliation",
            }),
        ));
    }
    let rebuilt = collect_range_topology(runner, &[target], new_head, "rebase_result_invalid")?;
    let retained = report.retained_commits().len();
    if rebuilt.len() != retained {
        return Err(decision(
            "rebase_reconciled_topology_mismatch",
            "reconciled replay rebuilt a different number of commits than the proven retained set; nothing was pushed",
            json!({
                "pr": pr,
                "retained_commit_count": retained,
                "rebuilt_commit_count": rebuilt.len(),
                "dropped_commits": report.dropped_commits(),
                "rebuilt_commits": rebuilt.iter().map(|commit| &commit.oid).collect::<Vec<_>>(),
                "mutated": false,
                "resumable": true,
                "next": "rediscover exact revisions and rerun without squash-equivalence reconciliation",
            }),
        ));
    }
    Ok(())
}

fn topology_commit_count_changed(
    pr: PrNumber,
    old_topology: &[RebaseTopologyCommit],
    new_topology: &[RebaseTopologyCommit],
) -> AppError {
    let dropped = old_topology.len().saturating_sub(new_topology.len());
    let added = new_topology.len().saturating_sub(old_topology.len());
    decision(
        "rebase_topology_changed",
        "join refused because Git rebuilt a different number of commits than the source range; this usually means source patches already exist on the selected tail/current default or the merge structure changed, so Cara cannot prove a one-to-one safe replay",
        json!({
            "pr": pr,
            "source_commit_count": old_topology.len(),
            "rebuilt_commit_count": new_topology.len(),
            "dropped_commit_count": dropped,
            "added_commit_count": added,
            "source_commits": old_topology.iter().map(|commit| &commit.oid).collect::<Vec<_>>(),
            "rebuilt_commits": new_topology.iter().map(|commit| &commit.oid).collect::<Vec<_>>(),
            "likely_causes": [
                "one or more source patches are already present on the tail or current default",
                "Git pruned an empty/duplicate commit",
                "the merge topology cannot be represented one-for-one after rebase",
            ],
            "mutated": false,
            "resumable": true,
            "safe_next_action": "inspect source commits against current main/tail; rebase the source to remove already-landed patches, or use reviewed Cara repair for an intentional topology change, then rerun join",
        }),
    )
}

fn build_merge_topology_proof(
    runner: &impl CommandRunner,
    old_topology: &[RebaseTopologyCommit],
    target: &CommitOid,
    new_head: &CommitOid,
    new_tree: &CommitOid,
    expected_tree: CommitOid,
    pr: PrNumber,
) -> Result<MergePreservingTopology, AppError> {
    if new_tree != &expected_tree {
        return Err(decision(
            "rebase_merge_tree_mismatch",
            "merge-preserving replay tree differs from the independently computed clean merge tree",
            json!({"pr": pr, "expected_tree": expected_tree, "actual_tree": new_tree, "resumable": true}),
        ));
    }
    let new_topology =
        collect_range_topology(runner, &[target], new_head, "rebase_result_invalid")?;
    if new_topology.len() != old_topology.len() {
        return Err(topology_commit_count_changed(
            pr,
            old_topology,
            &new_topology,
        ));
    }
    let oid_mapping = old_topology
        .iter()
        .zip(&new_topology)
        .map(|(old, new)| (old.oid.clone(), new.oid.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut mapping = Vec::with_capacity(old_topology.len());
    for (old, new) in old_topology.iter().zip(&new_topology) {
        if old.parents.len() != new.parents.len() {
            return Err(decision(
                "rebase_topology_changed",
                "merge-preserving replay changed a commit's parent cardinality",
                json!({"pr": pr, "old": old, "new": new, "resumable": true}),
            ));
        }
        for old_parent in &old.parents {
            if let Some(expected_parent) = oid_mapping.get(old_parent)
                && !new.parents.contains(expected_parent)
            {
                return Err(decision(
                    "rebase_topology_changed",
                    "merge-preserving replay changed an internal candidate parent edge",
                    json!({"pr": pr, "old": old, "new": new, "expected_parent": expected_parent, "resumable": true}),
                ));
            }
        }
        for new_parent in &new.parents {
            if !oid_mapping.values().any(|mapped| mapped == new_parent)
                && !is_ancestor(runner, new_parent, target)?
            {
                return Err(decision(
                    "rebase_topology_changed",
                    "rebuilt merge contains a parent outside the mapped candidate range and exact target ancestry",
                    json!({"pr": pr, "new": new, "external_parent": new_parent, "target": target, "resumable": true}),
                ));
            }
        }
        mapping.push(RebaseTopologyMapping {
            old_oid: old.oid.clone(),
            new_oid: new.oid.clone(),
            old_parents: old.parents.clone(),
            new_parents: new.parents.clone(),
        });
    }
    Ok(MergePreservingTopology {
        strategy: "git_rebase_merges_no_rebase_cousins_v1".to_owned(),
        expected_merge_tree_oid: expected_tree,
        old_commits: old_topology.to_vec(),
        new_commits: new_topology,
        mapping,
    })
}

/// Recheck the exact planned object, remote old head, permission, and lease without writing.
pub fn verify_prepared(prepared: &PreparedRebase) -> Result<(), AppError> {
    verify_prepared_before(prepared, prepared.worktree.operation_deadline)
}

/// Run final no-write verification under an earlier phase boundary so it
/// cannot consume wall-clock time reserved for commit/apply.
pub(crate) fn verify_prepared_before(
    prepared: &PreparedRebase,
    phase_deadline: Option<Instant>,
) -> Result<(), AppError> {
    let operation_deadline = match (prepared.worktree.operation_deadline, phase_deadline) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    };
    let runner = process_runner(
        &prepared.worktree.path,
        prepared.worktree.timeout,
        operation_deadline,
    );
    let retained_head = rev_parse(&runner, "HEAD")?;
    if retained_head != prepared.plan.new_head_oid {
        return Err(decision(
            "rebase_prepared_object_changed",
            "retained prepared rebase no longer resolves to its planned head",
            json!({"plan": prepared.plan, "retained_head": retained_head, "resumable": true}),
        ));
    }
    verify_remote_head(
        &runner,
        "origin",
        &prepared.plan.branch,
        &prepared.plan.old_head_oid,
    )?;
    match &prepared.plan.range_source {
        PlannedRangeBase::RemoteBranch { branch }
            if matches!(&prepared.plan.new_base, PlannedBase::Remote(_)) =>
        {
            verify_remote_head(&runner, "origin", &branch.name, &branch.oid)?;
        }
        PlannedRangeBase::RemoteBranch { .. } => {}
        PlannedRangeBase::HistoricalTargetBranch { branch, current }
        | PlannedRangeBase::HistoricalSourceBranch { branch, current } => {
            retain_historical_target_base(&runner, branch, current)?;
        }
        PlannedRangeBase::HistoricalParentBranch { branch, current } => {
            fetch_exact(&runner, "origin", current)?;
            retain_historical_target_base(&runner, branch, current)?;
        }
        PlannedRangeBase::PullRequestHead { pr, branch } => {
            fetch_exact_pull_request_head(&runner, "origin", *pr, branch)?;
        }
    }
    let destination = format!("HEAD:refs/heads/{}", prepared.plan.branch);
    require_success(
        &runner,
        CommandSpec::new("git").args([
            "push",
            "--dry-run",
            prepared.plan.lease.as_str(),
            "origin",
            destination.as_str(),
        ]),
        "rebase_push_preflight_failed",
        "push permission, branch ownership, or exact lease preflight failed",
    )?;
    Ok(())
}

/// Push the exact retained planned object under its original old-head lease.
pub fn apply_prepared(prepared: &PreparedRebase) -> Result<RebaseReceipt, AppError> {
    verify_prepared(prepared)?;
    let runner = process_runner(
        &prepared.worktree.path,
        prepared.worktree.timeout,
        prepared.worktree.operation_deadline,
    );
    match &prepared.plan.new_base {
        PlannedBase::Remote(target) | PlannedBase::Simulated(target) => {
            verify_remote_head(&runner, "origin", &target.name, &target.oid)?;
        }
    }
    push_prepared(prepared)
}

/// Apply after sync's global write barrier. Revalidate source/range/target
/// generations, but do not repeat the expensive permission dry-run after
/// control mutation: the exact force-with-lease push is the writer-race gate.
pub(crate) fn apply_prepared_after_write_barrier(
    prepared: &PreparedRebase,
) -> Result<RebaseReceipt, AppError> {
    let runner = process_runner(
        &prepared.worktree.path,
        prepared.worktree.timeout,
        prepared.worktree.operation_deadline,
    );
    match &prepared.plan.range_source {
        PlannedRangeBase::RemoteBranch { branch }
            if matches!(&prepared.plan.new_base, PlannedBase::Remote(_)) =>
        {
            verify_remote_head(&runner, "origin", &branch.name, &branch.oid)?;
        }
        PlannedRangeBase::RemoteBranch { .. } => {}
        PlannedRangeBase::HistoricalTargetBranch { branch, current }
        | PlannedRangeBase::HistoricalParentBranch { branch, current }
        | PlannedRangeBase::HistoricalSourceBranch { branch, current } => {
            retain_historical_target_base(&runner, branch, current)?;
        }
        PlannedRangeBase::PullRequestHead { pr, branch } => {
            fetch_exact_pull_request_head(&runner, "origin", *pr, branch)?;
        }
    }
    match &prepared.plan.new_base {
        PlannedBase::Remote(target) | PlannedBase::Simulated(target) => {
            verify_remote_head(&runner, "origin", &target.name, &target.oid)?;
        }
    }
    push_prepared(prepared)
}

fn push_prepared(prepared: &PreparedRebase) -> Result<RebaseReceipt, AppError> {
    if !prepared.plan.already_satisfied {
        let runner = process_runner(
            &prepared.worktree.path,
            prepared.worktree.timeout,
            prepared.worktree.operation_deadline,
        );
        let destination = format!("HEAD:refs/heads/{}", prepared.plan.branch);
        require_success(
            &runner,
            CommandSpec::new("git").args([
                "push",
                prepared.plan.lease.as_str(),
                "origin",
                destination.as_str(),
            ]),
            "rebase_stale_lease",
            "exact force-with-lease push failed; the branch may have moved and was not overwritten",
        )?;
    }
    Ok(RebaseReceipt {
        pr: prepared.plan.pr,
        branch: prepared.plan.branch.clone(),
        old_head_oid: prepared.plan.old_head_oid.clone(),
        new_head_oid: prepared.plan.new_head_oid.clone(),
        old_base_oid: prepared.plan.old_base_oid.clone(),
        new_base_branch: prepared.plan.new_base.branch().name.clone(),
        new_base_oid: prepared.plan.new_base.branch().oid.clone(),
        new_tree_oid: prepared.plan.new_tree_oid.clone(),
        commit_count: prepared.plan.commit_count,
        merge_topology: prepared.plan.merge_topology.clone(),
        squash_reconciliation: prepared.plan.squash_reconciliation.clone(),
        ci_trigger_workflows: prepared.plan.ci_trigger_workflows.clone(),
        lease: prepared.plan.lease.clone(),
        already_satisfied: prepared.plan.already_satisfied,
    })
}

fn preflight_ci_triggers(
    runner: &impl CommandRunner,
    source: &BranchSnapshot,
    target_branch: &str,
) -> Result<Vec<String>, AppError> {
    let listing = require_success(
        runner,
        CommandSpec::new("git").args([
            "ls-tree",
            "-r",
            "--name-only",
            source.oid.0.as_str(),
            ".github/workflows",
        ]),
        "rebase_ci_preflight_failed",
        "could not inspect workflows at the exact default revision",
    )?;
    let mut supported = Vec::new();
    for path in listing.stdout.lines().filter(|path| {
        Path::new(path).extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("yml") || extension.eq_ignore_ascii_case("yaml")
        })
    }) {
        let object = format!("{}:{path}", source.oid.0);
        let content = require_success(
            runner,
            CommandSpec::new("git").args(["show", object.as_str()]),
            "rebase_ci_preflight_failed",
            "could not read a workflow at the exact default revision",
        )?;
        let Ok(document) = serde_yaml::from_str::<serde_yaml::Value>(&content.stdout) else {
            continue;
        };
        let Some(root) = document.as_mapping() else {
            continue;
        };
        let Some(on) = yaml_get(root, "on").and_then(serde_yaml::Value::as_mapping) else {
            continue;
        };
        let Some(pull_request) = yaml_get(on, "pull_request") else {
            continue;
        };
        let Some(policy) = pull_request.as_mapping() else {
            // A null/scalar pull_request uses GitHub's default activity types,
            // which omit `edited` and therefore cannot prove the base retarget.
            continue;
        };
        let types = yaml_strings(yaml_get(policy, "types"));
        if !["opened", "synchronize", "reopened", "edited", "labeled"]
            .iter()
            .all(|required| types.iter().any(|item| item == required))
        {
            continue;
        }
        let branches = yaml_strings(yaml_get(policy, "branches"));
        if !branches.is_empty() || yaml_get(policy, "branches-ignore").is_some() {
            continue;
        }
        supported.push(path.to_owned());
    }
    if supported.is_empty() {
        return Err(decision(
            "rebase_ci_trigger_missing",
            "no exact-default pull_request workflow can run for the selected parent base on synchronize, edited, and labeled events",
            json!({
                "target_base": target_branch,
                "workflow_source_oid": source.oid,
                "required_types": ["opened", "synchronize", "reopened", "edited", "labeled"],
                "next": "enable a dedicated stack/full pull_request workflow without a branches filter and include types opened, synchronize, reopened, edited, labeled, and unlabeled; gate jobs on main base or the caravan PR label",
                "resumable": true
            }),
        ));
    }
    Ok(supported)
}

fn yaml_get<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a serde_yaml::Value> {
    mapping.get(serde_yaml::Value::String(key.to_owned()))
}

fn yaml_strings(value: Option<&serde_yaml::Value>) -> Vec<String> {
    match value {
        Some(serde_yaml::Value::String(value)) => vec![value.clone()],
        Some(serde_yaml::Value::Sequence(values)) => values
            .iter()
            .filter_map(serde_yaml::Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Reverify one exact remote branch snapshot at a whole-plan write barrier.
pub fn verify_branch_snapshot(
    repository_path: &Path,
    snapshot: &BranchSnapshot,
    timeout: Duration,
) -> Result<(), AppError> {
    verify_branch_snapshot_before(repository_path, snapshot, timeout, None)
}

pub(crate) fn verify_branch_snapshot_before(
    repository_path: &Path,
    snapshot: &BranchSnapshot,
    timeout: Duration,
    phase_deadline: Option<Instant>,
) -> Result<(), AppError> {
    validate_branch(&snapshot.name)?;
    validate_oid(&snapshot.oid)?;
    let runner = process_runner(repository_path, timeout, phase_deadline);
    verify_remote_head(&runner, "origin", &snapshot.name, &snapshot.oid)
}

fn verify_remote_head(
    runner: &impl CommandRunner,
    remote: &str,
    branch: &str,
    expected: &CommitOid,
) -> Result<(), AppError> {
    let reference = format!("refs/heads/{branch}");
    let advertised = require_success(
        runner,
        CommandSpec::new("git").args(["ls-remote", "--refs", remote, reference.as_str()]),
        "rebase_remote_head_unavailable",
        "could not verify the exact remote branch head",
    )?;
    let actual = advertised
        .stdout
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if actual != expected.0 {
        return Err(decision(
            "rebase_stale_lease",
            "remote branch moved since complete plan preflight",
            json!({"branch": branch, "expected_oid": expected, "actual_oid": actual, "resumable": true}),
        ));
    }
    Ok(())
}

fn fetch_exact_pull_request_head(
    runner: &impl CommandRunner,
    remote: &str,
    pr: PrNumber,
    snapshot: &BranchSnapshot,
) -> Result<(), AppError> {
    let reference = format!("refs/pull/{pr}/head");
    let advertised = require_success(
        runner,
        CommandSpec::new("git").args(["ls-remote", remote, reference.as_str()]),
        "rebase_remote_head_unavailable",
        "could not verify the merged predecessor pull-request head",
    )?;
    let actual = advertised
        .stdout
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if actual != snapshot.oid.0 {
        return Err(decision(
            "rebase_stale_lease",
            "merged predecessor pull-request head moved or does not match the retained boundary",
            json!({"pr": pr, "expected_oid": snapshot.oid, "actual_oid": actual, "resumable": true}),
        ));
    }
    require_success(
        runner,
        CommandSpec::new("git").args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            remote,
            reference.as_str(),
        ]),
        "rebase_exact_fetch_failed",
        "could not fetch the exact merged predecessor pull-request head",
    )?;
    Ok(())
}

fn retain_historical_target_base(
    runner: &impl CommandRunner,
    historical: &BranchSnapshot,
    current: &BranchSnapshot,
) -> Result<(), AppError> {
    let object = run(
        runner,
        CommandSpec::new("git").args(["cat-file", "-e", &format!("{}^{{commit}}", historical.oid)]),
    )?;
    if !object.is_success() {
        return Err(decision(
            "rebase_historical_base_missing",
            "the provider-retained old target generation is unavailable after fetching the current target branch",
            json!({
                "branch": historical.name,
                "historical_oid": historical.oid,
                "current_oid": current.oid,
                "resumable": true,
                "next": "rediscover provider state; do not guess or weaken the lease"
            }),
        ));
    }
    let ancestry = run(
        runner,
        CommandSpec::new("git").args([
            "merge-base",
            "--is-ancestor",
            historical.oid.0.as_str(),
            current.oid.0.as_str(),
        ]),
    )?;
    if !ancestry.is_success() {
        return Err(decision(
            "rebase_target_history_changed",
            "the current target branch is not a descendant of the PR's retained base generation",
            json!({
                "branch": historical.name,
                "historical_oid": historical.oid,
                "current_oid": current.oid,
                "resumable": true,
                "next": "inspect the default-branch rewrite and rediscover before retrying"
            }),
        ));
    }
    Ok(())
}

fn fetch_exact(
    runner: &impl CommandRunner,
    remote: &str,
    snapshot: &BranchSnapshot,
) -> Result<(), AppError> {
    let reference = format!("refs/heads/{}", snapshot.name);
    let advertised = require_success(
        runner,
        CommandSpec::new("git").args(["ls-remote", "--refs", remote, reference.as_str()]),
        "rebase_remote_head_unavailable",
        "could not verify the exact remote branch head",
    )?;
    let actual = advertised
        .stdout
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if actual != snapshot.oid.0 {
        return Err(decision(
            "rebase_stale_lease",
            "remote branch moved since discovery; refusing to simulate or force it",
            json!({"branch": snapshot.name, "expected_oid": snapshot.oid, "actual_oid": actual, "resumable": true}),
        ));
    }
    require_success(
        runner,
        CommandSpec::new("git").args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--refmap=",
            remote,
            reference.as_str(),
        ]),
        "rebase_exact_fetch_failed",
        "could not fetch the exact advertised branch object",
    )?;
    // Close the fetch race before any simulation or push.
    let after = require_success(
        runner,
        CommandSpec::new("git").args(["ls-remote", "--refs", remote, reference.as_str()]),
        "rebase_remote_head_unavailable",
        "could not reverify the exact remote branch head",
    )?;
    if after.stdout.split_whitespace().next().unwrap_or_default() != snapshot.oid.0 {
        return Err(decision(
            "rebase_stale_lease",
            "remote branch moved during exact fetch",
            json!({"branch": snapshot.name, "resumable": true}),
        ));
    }
    Ok(())
}

fn rev_parse(runner: &impl CommandRunner, revision: &str) -> Result<CommitOid, AppError> {
    let output = require_success(
        runner,
        CommandSpec::new("git").args(["rev-parse", "--verify", revision]),
        "rebase_result_invalid",
        "could not resolve the simulated rebase result",
    )?;
    let oid = CommitOid(output.stdout.trim().to_owned());
    validate_oid(&oid)?;
    Ok(oid)
}

fn validate_branch(branch: &str) -> Result<(), AppError> {
    if branch.is_empty() || branch.starts_with('-') || branch.contains(['\n', '\r', '\0']) {
        return Err(decision(
            "rebase_branch_invalid",
            "candidate head branch is not safe to pass to Git",
            json!({"branch": branch}),
        ));
    }
    Ok(())
}

fn validate_oid(oid: &CommitOid) -> Result<(), AppError> {
    if !matches!(oid.0.len(), 40 | 64) || !oid.0.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(decision(
            "rebase_oid_invalid",
            "provider returned an invalid full object ID",
            json!({"oid": oid}),
        ));
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn run(runner: &impl CommandRunner, command: CommandSpec) -> Result<CommandOutput, AppError> {
    runner.run(&command).map_err(|error| {
        decision(
            "rebase_command_failed",
            "could not run isolated cumulative rebase command",
            json!({"command": command.display(), "source": error.to_string(), "resumable": true}),
        )
    })
}

#[allow(clippy::needless_pass_by_value)]
fn require_success(
    runner: &impl CommandRunner,
    command: CommandSpec,
    code: &'static str,
    message: &'static str,
) -> Result<CommandOutput, AppError> {
    let output = run(runner, command.clone())?;
    if output.is_success() {
        Ok(output)
    } else {
        Err(decision(
            code,
            message,
            json!({"command": command.display(), "exit_code": output.code, "stderr": output.stderr, "resumable": true}),
        ))
    }
}

fn decision(code: &'static str, message: &'static str, details: serde_json::Value) -> AppError {
    AppError::structured(ErrorCategory::Validation, code, message, Some(details))
}

fn process_runner(
    directory: &Path,
    timeout: Duration,
    operation_deadline: Option<Instant>,
) -> ProcessRunner {
    let runner = ProcessRunner::in_directory(directory).with_timeout(timeout);
    operation_deadline.map_or(runner.clone(), |deadline| {
        runner.with_operation_deadline(deadline)
    })
}

struct TemporaryWorktree {
    repository: PathBuf,
    path: PathBuf,
    timeout: Duration,
    operation_deadline: Option<Instant>,
}

impl TemporaryWorktree {
    fn create(
        repository: &Path,
        head: &CommitOid,
        timeout: Duration,
        operation_deadline: Option<Instant>,
    ) -> Result<Self, AppError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("caravan-rebase-{}-{nonce}", std::process::id()));
        let runner = process_runner(repository, timeout, operation_deadline);
        require_success(
            &runner,
            CommandSpec::new("git").args([
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path.to_string_lossy().as_ref(),
                head.0.as_str(),
            ]),
            "rebase_worktree_failed",
            "could not create an isolated temporary worktree",
        )?;
        Ok(Self {
            repository: repository.to_path_buf(),
            path,
            timeout,
            operation_deadline,
        })
    }
}

impl Drop for TemporaryWorktree {
    fn drop(&mut self) {
        let runner = ProcessRunner::in_directory(&self.repository).with_timeout(self.timeout);
        let _ = runner.run(&CommandSpec::new("git").args([
            "worktree",
            "remove",
            "--force",
            self.path.to_string_lossy().as_ref(),
        ]));
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::process::Command;

    use super::*;
    use crate::model::{AutoMergeState, PullRequestState};
    use mcp_cli::StructuredError;

    // Physical-rebase fixtures intentionally exercise several real local Git
    // fetch/rebase/merge subprocesses. Keep their non-timeout assertions
    // resilient when the Nix build runs the suite under CPU/I/O contention;
    // timeout policy has separate focused coverage.
    const TEST_REBASE_BUDGET: Duration = Duration::from_secs(60);

    fn git(directory: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .output()
            .expect("run git fixture command");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 git output")
            .trim()
            .to_owned()
    }

    #[test]
    fn topology_count_refusal_explains_dropped_commits_and_recovery() {
        let topology = |oid: char| RebaseTopologyCommit {
            oid: CommitOid(oid.to_string().repeat(40)),
            parents: Vec::new(),
            tree_oid: CommitOid("f".repeat(40)),
        };
        let error = topology_commit_count_changed(
            PrNumber(42),
            &[topology('a'), topology('b')],
            &[topology('c')],
        );

        assert_eq!(error.code(), "rebase_topology_changed");
        assert!(error.message().contains("different number of commits"));
        let details = error.details().unwrap();
        assert_eq!(details["source_commit_count"], 2);
        assert_eq!(details["rebuilt_commit_count"], 1);
        assert_eq!(details["dropped_commit_count"], 1);
        assert_eq!(details["mutated"], false);
        assert!(
            details["safe_next_action"]
                .as_str()
                .unwrap()
                .contains("rebase the source")
        );
    }

    struct Fixture {
        _root: tempfile::TempDir,
        clone: PathBuf,
        repository: RepositoryId,
        old_main: CommitOid,
        new_main: CommitOid,
        feature: CommitOid,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remote.git");
        git(root.path(), &["init", "--bare", bare.to_str().unwrap()]);
        let clone = root.path().join("clone");
        git(
            root.path(),
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.name", "Caravan Test"]);
        git(&clone, &["config", "user.email", "caravan@example.invalid"]);
        git(&clone, &["checkout", "-b", "main"]);
        std::fs::write(clone.join("base"), "base\n").unwrap();
        std::fs::create_dir_all(clone.join(".github/workflows")).unwrap();
        std::fs::write(
            clone.join(".github/workflows/stack.yml"),
            "on:\n  pull_request:\n    types: [opened, synchronize, reopened, edited, labeled, unlabeled]\njobs: {}\n",
        )
        .unwrap();
        git(&clone, &["add", "base", ".github/workflows/stack.yml"]);
        git(&clone, &["commit", "-m", "base"]);
        let old_main = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "-u", "origin", "main"]);

        git(&clone, &["checkout", "-b", "feature"]);
        std::fs::write(clone.join("feature"), "candidate\n").unwrap();
        git(&clone, &["add", "feature"]);
        git(&clone, &["commit", "-m", "candidate"]);
        let feature = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "-u", "origin", "feature"]);

        git(&clone, &["checkout", "main"]);
        std::fs::write(clone.join("parent"), "parent\n").unwrap();
        git(&clone, &["add", "parent"]);
        git(&clone, &["commit", "-m", "parent"]);
        let new_main = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "origin", "main"]);
        git(&clone, &["checkout", "feature"]);

        Fixture {
            _root: root,
            clone,
            repository: RepositoryId {
                owner: "owner".to_owned(),
                name: "repo".to_owned(),
            },
            old_main,
            new_main,
            feature,
        }
    }

    fn branch(repository: &RepositoryId, name: &str, oid: &CommitOid) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository.clone(),
            name: name.to_owned(),
            oid: oid.clone(),
        }
    }

    /// A stacked candidate whose first member landed as **one** squash commit
    /// combining several pre-squash commits.
    ///
    /// This is the shape Git's own patch-equivalence detection cannot resolve:
    /// no individual pre-squash commit has the squash's patch id, so the
    /// sequencer replays them against content identical to what they produce.
    ///
    /// `divergent` reproduces the shape that must never be reconciled: the
    /// target moved past the equality point, so merge base, target tip, and
    /// candidate head are three distinct blobs for the same file.
    struct StackedFixture {
        _root: tempfile::TempDir,
        clone: PathBuf,
        repository: RepositoryId,
        old_main: CommitOid,
        new_main: CommitOid,
        landed_first: CommitOid,
        landed_head: CommitOid,
        feature: CommitOid,
    }

    fn stacked_squash_fixture(divergent: bool) -> StackedFixture {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remote.git");
        git(root.path(), &["init", "--bare", bare.to_str().unwrap()]);
        let clone = root.path().join("clone");
        git(
            root.path(),
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.name", "Caravan Test"]);
        git(&clone, &["config", "user.email", "caravan@example.invalid"]);
        git(&clone, &["checkout", "-b", "main"]);
        std::fs::create_dir_all(clone.join(".github/workflows")).unwrap();
        std::fs::write(
            clone.join(".github/workflows/stack.yml"),
            "on:\n  pull_request:\n    types: [opened, synchronize, reopened, edited, labeled, unlabeled]\njobs: {}\n",
        )
        .unwrap();
        std::fs::write(clone.join("app.rs"), "alpha\n").unwrap();
        git(&clone, &["add", "app.rs", ".github/workflows/stack.yml"]);
        git(&clone, &["commit", "-m", "base"]);
        let old_main = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "-u", "origin", "main"]);

        // The stacked member whose two commits later land as one squash.
        git(&clone, &["checkout", "-b", "feature"]);
        std::fs::write(clone.join("app.rs"), "beta\n").unwrap();
        git(&clone, &["add", "app.rs"]);
        git(&clone, &["commit", "-m", "member one part a"]);
        let landed_first = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        std::fs::write(clone.join("app.rs"), "gamma\n").unwrap();
        git(&clone, &["add", "app.rs"]);
        git(&clone, &["commit", "-m", "member one part b"]);
        let landed_head = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        std::fs::write(clone.join("app.rs"), "delta\n").unwrap();
        std::fs::write(clone.join("child.rs"), "child\n").unwrap();
        git(&clone, &["add", "app.rs", "child.rs"]);
        git(&clone, &["commit", "-m", "member two content"]);
        let feature = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "-u", "origin", "feature"]);

        git(&clone, &["checkout", "main"]);
        std::fs::write(clone.join("app.rs"), "gamma\n").unwrap();
        git(&clone, &["add", "app.rs"]);
        git(&clone, &["commit", "-m", "squash of member one"]);
        if divergent {
            std::fs::write(clone.join("app.rs"), "epsilon\n").unwrap();
            git(&clone, &["add", "app.rs"]);
            git(&clone, &["commit", "-m", "later independent landing"]);
        }
        let new_main = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "origin", "main"]);
        git(&clone, &["checkout", "feature"]);

        StackedFixture {
            _root: root,
            clone,
            repository: RepositoryId {
                owner: "owner".to_owned(),
                name: "repo".to_owned(),
            },
            old_main,
            new_main,
            landed_first,
            landed_head,
            feature,
        }
    }

    fn stacked_candidate(fixture: &StackedFixture) -> PullRequestSnapshot {
        PullRequestSnapshot {
            merge_state_status: None,
            number: crate::model::PrNumber(2227),
            title: "stacked tail".to_owned(),
            url: "https://example.invalid/2227".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &fixture.feature),
            base: branch(&fixture.repository, "main", &fixture.old_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        }
    }

    fn prepare_stacked(
        fixture: &StackedFixture,
        reconcile: bool,
    ) -> Result<PreparedRebase, AppError> {
        let candidate = stacked_candidate(fixture);
        let target = branch(&fixture.repository, "main", &fixture.new_main);
        prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            range_base_for_remote_target(&candidate, &target),
            PlannedBase::Remote(target.clone()),
            &target,
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET).with_squash_reconciliation(reconcile),
        )
    }

    #[test]
    fn replaying_squash_landed_history_conflicts_without_explicit_reconciliation() {
        let fixture = stacked_squash_fixture(false);

        let error = prepare_stacked(&fixture, false)
            .err()
            .expect("replaying already-landed stacked history conflicts");

        assert_eq!(error.code(), "rebase_conflict");
        let details = error.details().unwrap();
        assert_eq!(details["conflicting_paths"][0], "app.rs");
        // Detection alone never rewrites: the branch is untouched.
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .split_whitespace()
            .next(),
            Some(fixture.feature.0.as_str())
        );
    }

    #[test]
    fn opt_in_reconciliation_drops_only_proven_squash_landed_history() {
        let fixture = stacked_squash_fixture(false);

        let prepared = prepare_stacked(&fixture, true).expect("proven reconciliation replays");

        let reconciliation = prepared
            .plan
            .squash_reconciliation
            .as_ref()
            .expect("reconciliation receipt");
        assert_eq!(
            reconciliation.outcome,
            crate::squash_equivalence::SquashEquivalenceOutcome::Reconcilable
        );
        assert_eq!(
            reconciliation.dropped_commits(),
            [fixture.landed_first.clone(), fixture.landed_head.clone()]
        );
        assert_eq!(
            reconciliation.retained_commits(),
            std::slice::from_ref(&fixture.feature)
        );
        assert_eq!(reconciliation.affected_paths(), ["app.rs"]);
        assert_eq!(
            reconciliation.authorized_range_base(),
            Some(&fixture.landed_head)
        );
        // The replay is exactly the retained commit on top of the exact target,
        // and its tree is the independently proven cumulative tree.
        let replayed = git(
            &fixture.clone,
            &[
                "rev-list",
                "--count",
                &format!("{}..{}", fixture.new_main.0, prepared.plan.new_head_oid.0),
            ],
        );
        assert_eq!(replayed, "1");
        assert_eq!(
            prepared.plan.new_tree_oid,
            reconciliation
                .after
                .as_ref()
                .expect("reconciled merge evidence")
                .result_tree
        );
        assert_eq!(
            git(
                &fixture.clone,
                &["show", &format!("{}:app.rs", prepared.plan.new_head_oid.0)]
            ),
            "delta"
        );
        assert_eq!(
            git(
                &fixture.clone,
                &[
                    "show",
                    &format!("{}:child.rs", prepared.plan.new_head_oid.0)
                ]
            ),
            "child"
        );
        // Nothing was pushed while only preparing.
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .split_whitespace()
            .next(),
            Some(fixture.feature.0.as_str())
        );
    }

    #[test]
    fn opt_in_reconciliation_still_fails_closed_on_divergence_after_equality() {
        let fixture = stacked_squash_fixture(true);

        let error = prepare_stacked(&fixture, true)
            .err()
            .expect("genuine divergence is never reconciled");

        assert_eq!(error.code(), "rebase_conflict");
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .split_whitespace()
            .next(),
            Some(fixture.feature.0.as_str())
        );
    }

    fn remote_range(candidate: &PullRequestSnapshot) -> PlannedRangeBase {
        PlannedRangeBase::RemoteBranch {
            branch: candidate.base.clone(),
        }
    }

    #[test]
    fn rewrites_under_exact_lease_without_touching_caller_worktree() {
        let fixture = fixture();
        let before_head = git(&fixture.clone, &["rev-parse", "HEAD"]);
        let before_status = git(&fixture.clone, &["status", "--porcelain"]);
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: crate::model::PrNumber(7),
            title: "candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &fixture.feature),
            base: branch(&fixture.repository, "main", &fixture.new_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let receipt = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &branch(&fixture.repository, "main", &fixture.new_main),
            &branch(&fixture.repository, "main", &fixture.new_main),
            Duration::from_secs(10),
        )
        .unwrap();

        assert_eq!(receipt.old_head_oid, fixture.feature);
        assert_eq!(receipt.old_base_oid, fixture.old_main);
        assert_eq!(receipt.new_base_oid, fixture.new_main);
        assert_ne!(receipt.new_head_oid, receipt.old_head_oid);
        assert_eq!(receipt.commit_count, 1);
        assert!(receipt.lease.ends_with(&fixture.feature.0));
        assert_eq!(git(&fixture.clone, &["rev-parse", "HEAD"]), before_head);
        assert_eq!(
            git(&fixture.clone, &["status", "--porcelain"]),
            before_status
        );
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .split_whitespace()
            .next(),
            Some(receipt.new_head_oid.0.as_str())
        );
        git(
            &fixture.clone,
            &[
                "merge-base",
                "--is-ancestor",
                &fixture.new_main.0,
                &receipt.new_head_oid.0,
            ],
        );
    }

    #[test]
    fn external_default_advance_refreshes_range_without_weakening_head_lease() {
        let fixture = fixture();
        let mut candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: crate::model::PrNumber(7),
            title: "candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &fixture.feature),
            // GitHub retains the PR's old base generation even though the
            // moving default ref has advanced to `new_main`.
            base: branch(&fixture.repository, "main", &fixture.old_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let current_default = branch(&fixture.repository, "main", &fixture.new_main);

        let first = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &current_default,
            &current_default,
            Duration::from_secs(10),
        )
        .expect("fresh sync retains the historical boundary and rebases");
        assert_eq!(first.old_base_oid, fixture.old_main);
        assert_eq!(first.new_base_oid, fixture.new_main);
        assert_eq!(
            first.lease,
            format!("--force-with-lease=refs/heads/feature:{}", fixture.feature)
        );
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .split_whitespace()
            .next(),
            Some(first.new_head_oid.0.as_str())
        );

        // Provider rediscovery after the first push observes the fresh exact
        // head and current default. The same sync is then an idempotent no-op,
        // not an impossible stale-default retry loop.
        candidate.head.oid = first.new_head_oid.clone();
        candidate.base.oid = fixture.new_main.clone();
        let second = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &current_default,
            &current_default,
            Duration::from_secs(10),
        )
        .expect("rediscovery makes the second sync resumable");
        assert!(second.already_satisfied);
        assert_eq!(second.old_head_oid, first.new_head_oid);
        assert_eq!(second.new_head_oid, first.new_head_oid);
    }

    #[test]
    fn rewritten_default_history_is_not_treated_as_a_normal_advance() {
        let fixture = fixture();
        git(&fixture.clone, &["checkout", "--orphan", "rewritten-main"]);
        git(&fixture.clone, &["rm", "-rf", "."]);
        std::fs::write(fixture.clone.join("replacement"), "replacement\n").unwrap();
        git(&fixture.clone, &["add", "replacement"]);
        git(&fixture.clone, &["commit", "-m", "rewrite main"]);
        let rewritten = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(
            &fixture.clone,
            &["push", "--force", "origin", "HEAD:refs/heads/main"],
        );
        git(&fixture.clone, &["checkout", "feature"]);
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: crate::model::PrNumber(7),
            title: "candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &fixture.feature),
            base: branch(&fixture.repository, "main", &fixture.old_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let error = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &branch(&fixture.repository, "main", &rewritten),
            &branch(&fixture.repository, "main", &rewritten),
            Duration::from_secs(10),
        )
        .expect_err("force-rewritten default is a decision, not an advance");
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "rebase_target_history_changed"
        );
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .split_whitespace()
            .next(),
            Some(fixture.feature.0.as_str())
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn preserves_owned_two_parent_candidate_topology_and_tree() {
        let fixture = fixture();
        git(&fixture.clone, &["checkout", "-b", "candidate-side"]);
        std::fs::write(fixture.clone.join("side"), "side\n").unwrap();
        git(&fixture.clone, &["add", "side"]);
        git(&fixture.clone, &["commit", "-m", "candidate side"]);
        git(&fixture.clone, &["checkout", "feature"]);
        std::fs::write(fixture.clone.join("feature-two"), "feature two\n").unwrap();
        git(&fixture.clone, &["add", "feature-two"]);
        git(&fixture.clone, &["commit", "-m", "candidate two"]);
        git(
            &fixture.clone,
            &[
                "merge",
                "--no-ff",
                "candidate-side",
                "-m",
                "merge candidate side",
            ],
        );
        git(&fixture.clone, &["checkout", "-b", "candidate-side-two"]);
        std::fs::write(fixture.clone.join("side-two"), "side two\n").unwrap();
        git(&fixture.clone, &["add", "side-two"]);
        git(&fixture.clone, &["commit", "-m", "candidate side two"]);
        git(&fixture.clone, &["checkout", "feature"]);
        std::fs::write(fixture.clone.join("feature-three"), "feature three\n").unwrap();
        git(&fixture.clone, &["add", "feature-three"]);
        git(&fixture.clone, &["commit", "-m", "candidate three"]);
        git(
            &fixture.clone,
            &[
                "merge",
                "--no-ff",
                "candidate-side-two",
                "-m",
                "merge candidate side two",
            ],
        );
        let nonlinear_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "feature"]);
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(7),
            title: "nonlinear candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &nonlinear_head),
            base: branch(&fixture.repository, "main", &fixture.old_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let target = branch(&fixture.repository, "main", &fixture.new_main);
        let receipt = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &target,
            &target,
            Duration::from_secs(60),
        )
        .expect("owned two-parent topology is preserved");
        let topology = receipt.merge_topology.as_ref().expect("topology receipt");
        assert_eq!(topology.strategy, "git_rebase_merges_no_rebase_cousins_v1");
        assert_eq!(topology.old_commits.len(), topology.new_commits.len());
        assert_eq!(topology.mapping.len(), topology.old_commits.len());
        assert_eq!(
            topology
                .old_commits
                .iter()
                .filter(|commit| commit.parents.len() == 2)
                .count(),
            2
        );
        assert_eq!(
            topology
                .new_commits
                .iter()
                .filter(|commit| commit.parents.len() == 2)
                .count(),
            2
        );
        assert_eq!(receipt.new_tree_oid, topology.expected_merge_tree_oid);
        assert!(receipt.lease.ends_with(&nonlinear_head.0));

        let mut refreshed = candidate;
        refreshed.head.oid = receipt.new_head_oid.clone();
        refreshed.base.oid = fixture.new_main.clone();
        let second = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &refreshed,
            &target,
            &target,
            Duration::from_secs(60),
        )
        .expect("second tick is idempotent");
        assert!(second.already_satisfied);
        assert_eq!(second.new_head_oid, receipt.new_head_oid);
    }

    #[test]
    fn preserves_default_branch_merge_inside_candidate_history() {
        let fixture = fixture();
        git(&fixture.clone, &["checkout", "feature"]);
        git(
            &fixture.clone,
            &["merge", "--no-ff", "main", "-m", "merge main into feature"],
        );
        let nonlinear_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "feature"]);
        git(&fixture.clone, &["checkout", "main"]);
        std::fs::write(fixture.clone.join("target-two"), "target two\n").unwrap();
        git(&fixture.clone, &["add", "target-two"]);
        git(&fixture.clone, &["commit", "-m", "advance target again"]);
        let target_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "main"]);
        git(&fixture.clone, &["checkout", "feature"]);
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(7),
            title: "main-merge candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &nonlinear_head),
            base: branch(&fixture.repository, "main", &fixture.old_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let target = branch(&fixture.repository, "main", &target_head);
        let receipt = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &target,
            &target,
            Duration::from_secs(60),
        )
        .expect("default-branch merge is retained under current target ancestry");
        let topology = receipt.merge_topology.expect("merge topology");
        assert!(
            topology
                .old_commits
                .iter()
                .any(|commit| commit.parents.len() == 2)
        );
        assert!(
            topology
                .new_commits
                .iter()
                .any(|commit| commit.parents.len() == 2)
        );
        assert_eq!(receipt.new_tree_oid, topology.expected_merge_tree_oid);
    }

    #[test]
    fn nonlinear_parent_generation_feeds_exact_linear_child_plan() {
        let fixture = fixture();
        git(&fixture.clone, &["checkout", "-b", "parent-side"]);
        std::fs::write(fixture.clone.join("parent-side"), "side\n").unwrap();
        git(&fixture.clone, &["add", "parent-side"]);
        git(&fixture.clone, &["commit", "-m", "parent side"]);
        git(&fixture.clone, &["checkout", "feature"]);
        std::fs::write(fixture.clone.join("parent-two"), "two\n").unwrap();
        git(&fixture.clone, &["add", "parent-two"]);
        git(&fixture.clone, &["commit", "-m", "parent two"]);
        git(
            &fixture.clone,
            &["merge", "--no-ff", "parent-side", "-m", "merge parent side"],
        );
        let parent_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "feature"]);
        git(&fixture.clone, &["checkout", "-b", "child"]);
        std::fs::write(fixture.clone.join("child"), "child\n").unwrap();
        git(&fixture.clone, &["add", "child"]);
        git(&fixture.clone, &["commit", "-m", "child"]);
        let child_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "-u", "origin", "child"]);
        let parent = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(7),
            title: "parent".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &parent_head),
            base: branch(&fixture.repository, "main", &fixture.old_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let child = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(8),
            title: "child".to_owned(),
            url: "https://example.invalid/8".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "child", &child_head),
            base: branch(&fixture.repository, "feature", &parent_head),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let default = branch(&fixture.repository, "main", &fixture.new_main);
        let prepared_parent = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &parent,
            range_base_for_remote_target(&parent, &default),
            PlannedBase::Remote(default.clone()),
            &default,
            RebaseExecutionBudget::new(Duration::from_secs(60)),
        )
        .expect("nonlinear parent plan");
        let planned_parent = branch(
            &fixture.repository,
            "feature",
            &prepared_parent.plan.new_head_oid,
        );
        let prepared_child = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &child,
            PlannedRangeBase::RemoteBranch {
                branch: child.base.clone(),
            },
            PlannedBase::Simulated(planned_parent),
            &default,
            RebaseExecutionBudget::new(Duration::from_secs(60)),
        )
        .expect("child consumes retained planned parent");
        assert!(prepared_parent.plan.merge_topology.is_some());
        assert!(prepared_child.plan.merge_topology.is_none());
        let parent_receipt = apply_prepared(&prepared_parent).unwrap();
        let child_receipt = apply_prepared(&prepared_child).unwrap();
        let status = std::process::Command::new("git")
            .current_dir(&fixture.clone)
            .args([
                "merge-base",
                "--is-ancestor",
                parent_receipt.new_head_oid.0.as_str(),
                child_receipt.new_head_oid.0.as_str(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn child_provider_base_lag_uses_retained_historical_parent_generation() {
        let fixture = fixture();
        git(&fixture.clone, &["checkout", "feature"]);
        git(&fixture.clone, &["checkout", "-b", "child"]);
        std::fs::write(fixture.clone.join("child-lag"), "child\n").unwrap();
        git(&fixture.clone, &["add", "child-lag"]);
        git(&fixture.clone, &["commit", "-m", "child"]);
        let child_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "-u", "origin", "child"]);

        git(&fixture.clone, &["checkout", "feature"]);
        std::fs::write(fixture.clone.join("late-parent"), "late parent\n").unwrap();
        git(&fixture.clone, &["add", "late-parent"]);
        git(&fixture.clone, &["commit", "-m", "late parent"]);
        let current_parent = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "feature"]);

        let parent = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(7),
            title: "parent".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &current_parent),
            base: branch(&fixture.repository, "main", &fixture.old_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let child = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(8),
            title: "child".to_owned(),
            url: "https://example.invalid/8".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "child", &child_head),
            // GitHub can retain the old exact BaseRefOid briefly after the
            // named parent branch already advertises `current_parent`.
            base: branch(&fixture.repository, "feature", &fixture.feature),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let default = branch(&fixture.repository, "main", &fixture.new_main);
        let prepared_parent = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &parent,
            range_base_for_remote_target(&parent, &default),
            PlannedBase::Remote(default.clone()),
            &default,
            RebaseExecutionBudget::new(Duration::from_secs(60)),
        )
        .expect("current parent plan");
        let planned_parent = branch(
            &fixture.repository,
            "feature",
            &prepared_parent.plan.new_head_oid,
        );
        let range_source = range_base_for_rewritten_parent(&child, &parent.head);
        assert!(matches!(
            &range_source,
            PlannedRangeBase::HistoricalParentBranch { branch, current }
                if branch.oid == fixture.feature && current.oid == current_parent
        ));
        let prepared_child = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &child,
            range_source,
            PlannedBase::Simulated(planned_parent),
            &default,
            RebaseExecutionBudget::new(Duration::from_secs(60)),
        )
        .expect("old provider BaseRefOid remains an exact historical range boundary");

        verify_prepared(&prepared_parent).expect("parent global preflight");
        verify_prepared(&prepared_child).expect("child historical-parent preflight");
        let parent_receipt =
            apply_prepared_after_write_barrier(&prepared_parent).expect("parent exact lease");
        let child_receipt =
            apply_prepared_after_write_barrier(&prepared_child).expect("child exact lease");
        let status = std::process::Command::new("git")
            .current_dir(&fixture.clone)
            .args([
                "merge-base",
                "--is-ancestor",
                parent_receipt.new_head_oid.0.as_str(),
                child_receipt.new_head_oid.0.as_str(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn rejects_octopus_candidate_topology_before_remote_write() {
        let fixture = fixture();
        git(&fixture.clone, &["checkout", "-b", "side-one"]);
        std::fs::write(fixture.clone.join("side-one"), "one\n").unwrap();
        git(&fixture.clone, &["add", "side-one"]);
        git(&fixture.clone, &["commit", "-m", "side one"]);
        git(&fixture.clone, &["checkout", "feature"]);
        git(&fixture.clone, &["checkout", "-b", "side-two"]);
        std::fs::write(fixture.clone.join("side-two"), "two\n").unwrap();
        git(&fixture.clone, &["add", "side-two"]);
        git(&fixture.clone, &["commit", "-m", "side two"]);
        git(&fixture.clone, &["checkout", "feature"]);
        git(
            &fixture.clone,
            &["merge", "--no-ff", "side-one", "side-two", "-m", "octopus"],
        );
        let head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "feature"]);
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(7),
            title: "octopus".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &head),
            base: branch(&fixture.repository, "main", &fixture.old_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let error = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &branch(&fixture.repository, "main", &fixture.new_main),
            &branch(&fixture.repository, "main", &fixture.new_main),
            Duration::from_secs(60),
        )
        .expect_err("octopus topology is unsupported");
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "rebase_unsupported_octopus"
        );
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .split_whitespace()
            .next(),
            Some(head.0.as_str())
        );
    }

    #[test]
    fn rejects_merge_parent_outside_candidate_and_target_history() {
        let fixture = fixture();
        git(
            &fixture.clone,
            &["checkout", "--orphan", "external-history"],
        );
        git(&fixture.clone, &["rm", "-rf", "."]);
        std::fs::write(fixture.clone.join("external"), "external\n").unwrap();
        git(&fixture.clone, &["add", "external"]);
        git(&fixture.clone, &["commit", "-m", "external root"]);
        git(&fixture.clone, &["checkout", "feature"]);
        git(
            &fixture.clone,
            &[
                "merge",
                "--allow-unrelated-histories",
                "--no-ff",
                "external-history",
                "-m",
                "external merge",
            ],
        );
        let head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "feature"]);
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(7),
            title: "external candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &head),
            base: branch(&fixture.repository, "main", &fixture.old_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let error = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &branch(&fixture.repository, "main", &fixture.new_main),
            &branch(&fixture.repository, "main", &fixture.new_main),
            Duration::from_secs(60),
        )
        .expect_err("unowned cousin history must fail before push");
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "rebase_cousin_history"
        );
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .split_whitespace()
            .next(),
            Some(head.0.as_str())
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn five_member_plan_reuses_exact_simulated_parent_generations() {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remote.git");
        git(root.path(), &["init", "--bare", bare.to_str().unwrap()]);
        let clone = root.path().join("clone");
        git(
            root.path(),
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.name", "Caravan Test"]);
        git(&clone, &["config", "user.email", "caravan@example.invalid"]);
        git(&clone, &["checkout", "-b", "main"]);
        std::fs::create_dir_all(clone.join(".github/workflows")).unwrap();
        std::fs::write(clone.join("base"), "base\n").unwrap();
        std::fs::write(
            clone.join(".github/workflows/stack.yml"),
            "on:\n  pull_request:\n    types: [opened, synchronize, reopened, edited, labeled, unlabeled]\njobs: {}\n",
        )
        .unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-m", "base"]);
        let main = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "-u", "origin", "main"]);

        let names = ["a", "b", "c", "d", "e"];
        let pr_numbers = [1972, 1962, 1959, 1958, 1946];
        let mut old_heads = Vec::new();
        let mut parent = "main";
        for name in names {
            git(&clone, &["checkout", "-b", name, parent]);
            std::fs::write(clone.join(name), format!("{name}\n")).unwrap();
            git(&clone, &["add", name]);
            git(&clone, &["commit", "-m", name]);
            old_heads.push(CommitOid(git(&clone, &["rev-parse", "HEAD"])));
            git(&clone, &["push", "-u", "origin", name]);
            parent = name;
        }
        git(&clone, &["checkout", "a"]);
        std::fs::write(clone.join("a-repair"), "repair\n").unwrap();
        git(&clone, &["add", "a-repair"]);
        git(&clone, &["commit", "-m", "repair a"]);
        let repaired_a = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "origin", "a"]);

        let repository = RepositoryId {
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        };
        let mut candidates = Vec::new();
        for (index, name) in names.into_iter().enumerate() {
            let (base_name, base_oid) = if index == 0 {
                ("main", main.clone())
            } else if index == 1 {
                ("a", repaired_a.clone())
            } else {
                (names[index - 1], old_heads[index - 1].clone())
            };
            let head_oid = if index == 0 {
                repaired_a.clone()
            } else {
                old_heads[index].clone()
            };
            candidates.push(PullRequestSnapshot {
                merge_state_status: None,
                number: PrNumber(pr_numbers[index]),
                title: name.to_owned(),
                url: format!("https://example.invalid/{}", pr_numbers[index]),
                state: PullRequestState::Open,
                draft: false,
                head: branch(&repository, name, &head_oid),
                base: branch(&repository, base_name, &base_oid),
                cross_repository: false,
                labels: BTreeSet::from(["caravan".to_owned()]),
                auto_merge: AutoMergeState::disabled(),
                checks: Vec::new(),
                created_at: None,
                merged_at: None,
                updated_at: None,
            });
        }

        let default = branch(&repository, "main", &main);
        let mut target = PlannedBase::Remote(default.clone());
        let mut prepared = Vec::new();
        for candidate in &candidates {
            let item = prepare_candidate(
                &clone,
                &repository,
                candidate,
                remote_range(candidate),
                target,
                &default,
                RebaseExecutionBudget::new(TEST_REBASE_BUDGET),
            )
            .unwrap();
            target = PlannedBase::Simulated(branch(
                &repository,
                &candidate.head.name,
                &item.plan.new_head_oid,
            ));
            prepared.push(item);
        }
        assert_eq!(
            git(&clone, &["ls-remote", "origin", "refs/heads/e"]),
            format!("{}\trefs/heads/e", old_heads[4])
        );
        for item in &prepared {
            verify_prepared(item).unwrap();
        }
        let receipts = prepared
            .iter()
            .map(|item| apply_prepared(item).unwrap())
            .collect::<Vec<_>>();
        for pair in receipts.windows(2) {
            let status = std::process::Command::new("git")
                .current_dir(&clone)
                .args([
                    "merge-base",
                    "--is-ancestor",
                    pair[0].new_head_oid.0.as_str(),
                    pair[1].new_head_oid.0.as_str(),
                ])
                .status()
                .unwrap();
            assert!(status.success());
        }
        assert_eq!(receipts.len(), 5);
        assert_eq!(receipts[4].branch, "e");

        // A merged/deleted predecessor remains an exact range boundary through
        // GitHub's durable pull-request head ref, so B can be promoted to head.
        drop(prepared);
        git(&clone, &["checkout", "main"]);
        git(
            &clone,
            &["reset", "--hard", receipts[0].new_head_oid.0.as_str()],
        );
        git(&clone, &["push", "origin", "main"]);
        git(
            root.path(),
            &[
                "--git-dir",
                bare.to_str().unwrap(),
                "update-ref",
                "refs/pull/1972/head",
                receipts[0].new_head_oid.0.as_str(),
            ],
        );
        git(&clone, &["push", "origin", "--delete", "a"]);
        let mut promoted = candidates[1].clone();
        promoted.head.oid = receipts[1].new_head_oid.clone();
        promoted.base.oid = receipts[0].new_head_oid.clone();
        let promoted_plan = prepare_candidate(
            &clone,
            &repository,
            &promoted,
            PlannedRangeBase::PullRequestHead {
                pr: PrNumber(1972),
                branch: promoted.base.clone(),
            },
            PlannedBase::Remote(branch(&repository, "main", &receipts[0].new_head_oid)),
            &branch(&repository, "main", &receipts[0].new_head_oid),
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET),
        )
        .expect("merged predecessor PR ref retains exact range");
        assert_eq!(promoted_plan.plan.pr, PrNumber(1962));
    }

    #[test]
    fn planning_conflict_never_pushes_the_remote_branch() {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remote.git");
        git(root.path(), &["init", "--bare", bare.to_str().unwrap()]);
        let clone = root.path().join("clone");
        git(
            root.path(),
            &["clone", bare.to_str().unwrap(), clone.to_str().unwrap()],
        );
        git(&clone, &["config", "user.name", "Caravan Test"]);
        git(&clone, &["config", "user.email", "caravan@example.invalid"]);
        git(&clone, &["checkout", "-b", "main"]);
        std::fs::create_dir_all(clone.join(".github/workflows")).unwrap();
        std::fs::write(clone.join("shared"), "base\n").unwrap();
        std::fs::write(
            clone.join(".github/workflows/stack.yml"),
            "on:\n  pull_request:\n    types: [opened, synchronize, reopened, edited, labeled, unlabeled]\njobs: {}\n",
        )
        .unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-m", "base"]);
        git(&clone, &["push", "-u", "origin", "main"]);
        git(&clone, &["checkout", "-b", "feature"]);
        std::fs::write(clone.join("shared"), "feature\n").unwrap();
        git(&clone, &["commit", "-am", "feature"]);
        let feature = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "-u", "origin", "feature"]);
        git(&clone, &["checkout", "main"]);
        std::fs::write(clone.join("shared"), "parent\n").unwrap();
        git(&clone, &["commit", "-am", "parent"]);
        let main = CommitOid(git(&clone, &["rev-parse", "HEAD"]));
        git(&clone, &["push", "origin", "main"]);

        let repository = RepositoryId {
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        };
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(7),
            title: "feature".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&repository, "feature", &feature),
            base: branch(&repository, "main", &main),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let before = git(&clone, &["ls-remote", "origin", "refs/heads/feature"]);
        let error = prepare_candidate(
            &clone,
            &repository,
            &candidate,
            remote_range(&candidate),
            PlannedBase::Remote(branch(&repository, "main", &main)),
            &branch(&repository, "main", &main),
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET),
        )
        .err()
        .expect("conflict must fail planning");
        assert_eq!(mcp_cli::StructuredError::code(&error), "rebase_conflict");
        assert_eq!(
            mcp_cli::StructuredError::details(&error).unwrap()["conflicting_paths"],
            serde_json::json!(["shared"])
        );
        assert_eq!(
            git(&clone, &["ls-remote", "origin", "refs/heads/feature"]),
            before
        );
    }

    /// bd-1a4e28: worktree creation is a fixed setup cost proportional to tree
    /// size, so pricing it at the same per-command share as every replay step
    /// made macOS structurally unable to rebase at all: 12-23s measured there
    /// against an observed 10176ms sub-deadline, versus 0.7-3.2s on Linux.
    #[test]
    fn worktree_setup_is_not_priced_like_an_ordinary_command() {
        let tight = RebaseExecutionBudget::new(Duration::from_millis(10_176));

        assert!(
            tight.worktree_setup_timeout() >= Duration::from_secs(24),
            "the floor must clear the slowest measured macOS cold run (23.6s)"
        );

        // A generous configured timeout is never reduced by the floor.
        let generous = RebaseExecutionBudget::new(Duration::from_secs(300));
        assert_eq!(
            generous.worktree_setup_timeout(),
            Duration::from_secs(300),
            "the floor raises a tight budget, it never lowers a deliberate one"
        );
    }

    /// bd-cef612: replaying only after the evicted head drops precisely that
    /// member's commits from a descendant while keeping the descendant's own
    /// work. Without the explicit boundary the sequencer uses the target as its
    /// upstream and replays the evicted commit too, silently reintroducing
    /// discarded content.
    #[test]
    fn replaying_after_the_evicted_head_drops_only_that_patch() {
        let fixture = fixture();

        git(&fixture.clone, &["checkout", "-b", "evicted", "main"]);
        std::fs::write(fixture.clone.join("evicted"), "evicted\n").unwrap();
        git(&fixture.clone, &["add", "evicted"]);
        git(&fixture.clone, &["commit", "-m", "evicted work"]);
        let evicted_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "-u", "origin", "evicted"]);

        git(&fixture.clone, &["checkout", "-b", "descendant"]);
        std::fs::write(fixture.clone.join("descendant"), "descendant\n").unwrap();
        git(&fixture.clone, &["add", "descendant"]);
        git(&fixture.clone, &["commit", "-m", "descendant work"]);
        let descendant_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "-u", "origin", "descendant"]);

        let target = branch(&fixture.repository, "main", &fixture.new_main);
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(3),
            title: "descendant".to_owned(),
            url: "https://example.invalid/3".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "descendant", &descendant_head),
            base: branch(&fixture.repository, "evicted", &evicted_head),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };

        let prepared = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            PlannedRangeBase::RemoteBranch {
                branch: branch(&fixture.repository, "evicted", &evicted_head),
            },
            PlannedBase::Remote(target.clone()),
            &target,
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET).replaying_after(evicted_head.clone()),
        )
        .expect("descendant rebuilds without the evicted patch");
        let receipt = apply_prepared(&prepared).expect("apply the unwound descendant");

        let files = git(
            &fixture.clone,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                receipt.new_head_oid.0.as_str(),
            ],
        );
        assert!(
            files.contains("descendant"),
            "own patch must survive: {files}"
        );
        assert!(
            !files.contains("evicted"),
            "evicted patch must be dropped: {files}"
        );
        assert_eq!(
            git(
                &fixture.clone,
                &["rev-parse", &format!("{}^", receipt.new_head_oid.0)]
            ),
            fixture.new_main.0,
            "the unwound descendant must sit directly on the exact target"
        );
    }

    /// bd-85b71d live shape (Cacophony PR2215): the root merged the default
    /// branch into itself, resolving conflicts by hand. Replaying that history
    /// re-hits those conflicts and fails, even though the final merge tree is
    /// independently clean. When Cara owns the squash merge the history is
    /// discarded at landing, so the root is flattened onto the proven tree
    /// instead of replayed.
    #[test]
    fn a_squashed_root_is_flattened_instead_of_replaying_its_merges() {
        let fixture = fixture();

        // The fixture already advanced remote main with a `parent` file. The
        // candidate edits that same path, so replaying the in-branch merge of
        // main must conflict while the final tree stays clean.
        let target_head = fixture.new_main.clone();
        git(&fixture.clone, &["checkout", "feature"]);
        std::fs::write(fixture.clone.join("parent"), "candidate side\n").unwrap();
        git(&fixture.clone, &["add", "parent"]);
        git(&fixture.clone, &["commit", "-m", "candidate edits parent"]);
        // Resolve the conflict by hand inside the branch, exactly as PR2215 did.
        let merge = std::process::Command::new("git")
            .current_dir(&fixture.clone)
            .args([
                "merge",
                "--no-ff",
                "-m",
                "merge main into candidate",
                "main",
            ])
            .output()
            .expect("run merge");
        if !merge.status.success() {
            std::fs::write(fixture.clone.join("parent"), "resolved by hand\n").unwrap();
            git(&fixture.clone, &["add", "parent"]);
            git(&fixture.clone, &["commit", "--no-edit"]);
        }
        let head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "feature"]);

        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(2215),
            title: "cumulative root".to_owned(),
            url: "https://example.invalid/2215".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &head),
            base: branch(&fixture.repository, "main", &target_head),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let target = branch(&fixture.repository, "main", &target_head);

        let prepared = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            remote_range(&candidate),
            PlannedBase::Remote(target.clone()),
            &target,
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET).flattening_squashed_root(true),
        )
        .expect("a squashed root is flattened rather than replayed");

        // One commit, parented directly on the exact target, carrying the tree
        // the merge-tree proof already validated.
        assert_eq!(
            git(
                &fixture.clone,
                &[
                    "rev-list",
                    "--count",
                    &format!("{}..{}", target_head.0, prepared.plan.new_head_oid.0)
                ]
            ),
            "1"
        );
        assert_eq!(
            git(
                &fixture.clone,
                &["rev-parse", &format!("{}^", prepared.plan.new_head_oid.0)]
            ),
            target_head.0
        );
        assert_eq!(
            git(
                &fixture.clone,
                &[
                    "rev-parse",
                    &format!("{}^{{tree}}", prepared.plan.new_head_oid.0)
                ]
            ),
            prepared.plan.new_tree_oid.0
        );
    }

    /// Without the explicit authorization the same shape still fails closed, so
    /// a child is never silently flattened.
    #[test]
    fn a_merge_preserving_child_is_never_flattened() {
        let fixture = fixture();
        let target_head = fixture.new_main.clone();
        git(&fixture.clone, &["checkout", "feature"]);
        std::fs::write(fixture.clone.join("parent"), "candidate side\n").unwrap();
        git(&fixture.clone, &["add", "parent"]);
        git(&fixture.clone, &["commit", "-m", "candidate edits parent"]);
        let merge = std::process::Command::new("git")
            .current_dir(&fixture.clone)
            .args([
                "merge",
                "--no-ff",
                "-m",
                "merge main into candidate",
                "main",
            ])
            .output()
            .expect("run merge");
        if !merge.status.success() {
            std::fs::write(fixture.clone.join("parent"), "resolved by hand\n").unwrap();
            git(&fixture.clone, &["add", "parent"]);
            git(&fixture.clone, &["commit", "--no-edit"]);
        }
        let head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "feature"]);

        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(2216),
            title: "child".to_owned(),
            url: "https://example.invalid/2216".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &head),
            base: branch(&fixture.repository, "main", &target_head),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let target = branch(&fixture.repository, "main", &target_head);

        let error = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            remote_range(&candidate),
            PlannedBase::Remote(target.clone()),
            &target,
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET),
        );
        let Err(error) = error else {
            panic!("an unauthorized merge-preserving replay must fail closed");
        };

        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "rebase_merge_replay_conflict"
        );
    }

    #[test]
    fn nonlinear_merge_tree_conflict_writes_nothing() {
        let fixture = fixture();
        git(&fixture.clone, &["checkout", "main"]);
        std::fs::write(fixture.clone.join("base"), "target conflict\n").unwrap();
        git(&fixture.clone, &["commit", "-am", "target conflict"]);
        let target_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "main"]);
        git(&fixture.clone, &["checkout", "feature"]);
        std::fs::write(fixture.clone.join("base"), "candidate conflict\n").unwrap();
        git(&fixture.clone, &["commit", "-am", "candidate conflict"]);
        git(&fixture.clone, &["checkout", "-b", "conflict-side"]);
        std::fs::write(fixture.clone.join("side"), "side\n").unwrap();
        git(&fixture.clone, &["add", "side"]);
        git(&fixture.clone, &["commit", "-m", "side"]);
        git(&fixture.clone, &["checkout", "feature"]);
        std::fs::write(fixture.clone.join("feature-two"), "two\n").unwrap();
        git(&fixture.clone, &["add", "feature-two"]);
        git(&fixture.clone, &["commit", "-m", "feature two"]);
        git(
            &fixture.clone,
            &["merge", "--no-ff", "conflict-side", "-m", "internal merge"],
        );
        let head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "feature"]);
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(7),
            title: "conflict".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &head),
            base: branch(&fixture.repository, "main", &fixture.old_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let before = git(
            &fixture.clone,
            &["ls-remote", "origin", "refs/heads/feature"],
        );
        let error = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &branch(&fixture.repository, "main", &target_head),
            &branch(&fixture.repository, "main", &target_head),
            Duration::from_secs(60),
        )
        .expect_err("independent merge-tree conflict fails before push");
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "rebase_merge_tree_conflict"
        );
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            ),
            before
        );
    }

    #[test]
    fn apply_time_lease_race_preserves_the_external_head() {
        let fixture = fixture();
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(7),
            title: "candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &fixture.feature),
            base: branch(&fixture.repository, "main", &fixture.new_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let prepared = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            remote_range(&candidate),
            PlannedBase::Remote(branch(&fixture.repository, "main", &fixture.new_main)),
            &branch(&fixture.repository, "main", &fixture.new_main),
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET),
        )
        .unwrap();
        verify_prepared(&prepared).expect("global barrier");
        std::fs::write(fixture.clone.join("external-race"), "race\n").unwrap();
        git(&fixture.clone, &["add", "external-race"]);
        git(&fixture.clone, &["commit", "-m", "external race"]);
        let external = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "origin", "feature"]);

        let error = apply_prepared_after_write_barrier(&prepared)
            .expect_err("exact apply lease must detect the post-barrier writer race");

        assert_eq!(mcp_cli::StructuredError::code(&error), "rebase_stale_lease");
        assert!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .starts_with(&external.0)
        );
    }

    #[test]
    fn barrier_apply_revalidates_moved_default_before_branch_push() {
        let fixture = fixture();
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(7),
            title: "candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &fixture.feature),
            base: branch(&fixture.repository, "main", &fixture.new_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let prepared = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            remote_range(&candidate),
            PlannedBase::Remote(branch(&fixture.repository, "main", &fixture.new_main)),
            &branch(&fixture.repository, "main", &fixture.new_main),
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET),
        )
        .unwrap();
        verify_prepared(&prepared).expect("global barrier");
        git(&fixture.clone, &["checkout", "main"]);
        std::fs::write(fixture.clone.join("late-main"), "late main\n").unwrap();
        git(&fixture.clone, &["add", "late-main"]);
        git(&fixture.clone, &["commit", "-m", "late main"]);
        git(&fixture.clone, &["push", "origin", "main"]);

        let error = apply_prepared_after_write_barrier(&prepared)
            .expect_err("moved default must stop before candidate push");

        assert_eq!(mcp_cli::StructuredError::code(&error), "rebase_stale_lease");
        assert!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .starts_with(&fixture.feature.0)
        );
    }

    #[test]
    fn stale_snapshot_is_typed_and_never_overwrites_remote() {
        let fixture = fixture();
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: crate::model::PrNumber(7),
            title: String::new(),
            url: String::new(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&fixture.repository, "feature", &CommitOid("0".repeat(40))),
            base: branch(&fixture.repository, "main", &fixture.new_main),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        };
        let error = rewrite_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            &branch(&fixture.repository, "main", &fixture.new_main),
            &branch(&fixture.repository, "main", &fixture.new_main),
            TEST_REBASE_BUDGET,
        )
        .unwrap_err();
        assert_eq!(mcp_cli::StructuredError::code(&error), "rebase_stale_lease");
        assert_eq!(
            git(
                &fixture.clone,
                &["ls-remote", "origin", "refs/heads/feature"]
            )
            .split_whitespace()
            .next(),
            Some(fixture.feature.0.as_str())
        );
    }
}
