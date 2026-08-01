//! Explicit, lease-protected physical branch chaining.
//!
//! This module is deliberately separate from compatibility checks: enabling
//! `rebase_on_join` authorizes history rewriting.  Every rewrite is first
//! completed in a detached temporary worktree, and the only remote mutation is
//! an exact force-with-lease push.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppError;
use crate::command::{CommandOutput, CommandRunner, CommandSpec, ProcessRunner};
use crate::model::{BranchSnapshot, CommitOid, PrNumber, PullRequestSnapshot, RepositoryId};
use crate::remote_lease::RemoteLeaseGuard;
use crate::squash_equivalence::{self, SquashEquivalenceReport};
use crate::writer_guard::WriterCommandRunner;

const MAX_MERGE_PRESERVING_COMMITS: usize = 256;
static WORKTREE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Shared child and whole-operation limits for one prepared generation.
/// Smallest timeout allowed for creating an isolated worktree.
///
/// Chosen from measurement rather than taste: the slowest observed cold run was
/// 23.6s on macOS for a 3801-file tree, so 60s leaves headroom for a larger
/// repository without ever being the thing that bounds a healthy tick. The
/// operation deadline remains the real bound.
const WORKTREE_SETUP_FLOOR: Duration = Duration::from_secs(60);

/// Why Caravan is authorized to replace one exact branch generation.
///
/// Selected before the rewrite and retained in the immutable plan/receipt, so
/// the GitHub-visible reason cannot be inferred incorrectly after publication
/// (bd-8e97bf).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BranchRewriteReason {
    #[default]
    Unspecified,
    CurrentDefaultAdvanced,
    ParentAdvanced {
        parent_pr: PrNumber,
    },
    JoinedCaravan {
        parent_pr: PrNumber,
    },
    ParentEvicted {
        parent_pr: PrNumber,
    },
    CaravanReshaped,
    ReviewedRepair {
        operation_id: String,
    },
}

impl BranchRewriteReason {
    /// One visible line plus a same-line hidden idempotency marker.
    #[must_use]
    pub fn comment(&self, receipt: &RebaseReceipt) -> Option<(String, String)> {
        if receipt.already_satisfied {
            return None;
        }
        let reason = match self {
            Self::Unspecified => return None,
            Self::CurrentDefaultAdvanced => "current default advanced".to_owned(),
            Self::ParentAdvanced { parent_pr } => format!("parent #{parent_pr} advanced"),
            Self::JoinedCaravan { parent_pr } => {
                format!("it joined the caravan behind parent #{parent_pr}")
            }
            Self::ParentEvicted { parent_pr } => format!("parent #{parent_pr} was evicted"),
            Self::CaravanReshaped => "the caravan was reshaped".to_owned(),
            Self::ReviewedRepair { operation_id } => {
                let bounded = operation_id
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(32)
                    .collect::<String>();
                format!("reviewed repair {bounded} was applied")
            }
        };
        let marker = format!(
            "<!-- caravan-branch-rewrite:v1:{}:{}:{} -->",
            receipt.pr, receipt.old_head_oid, receipt.new_head_oid
        );
        let short = |oid: &CommitOid| oid.0.chars().take(7).collect::<String>();
        let body = format!(
            "Caravan updated this branch because {reason}; branch {} → {}. {marker}",
            short(&receipt.old_head_oid),
            short(&receipt.new_head_oid),
        );
        Some((marker, body))
    }
}

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
    /// Immutable reason attached to the exact prepared generation.
    pub rewrite_reason: BranchRewriteReason,
    /// Operation-scoped remote fence retained through preparation and push.
    pub writer_fence: Option<Arc<RemoteLeaseGuard>>,
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
            rewrite_reason: BranchRewriteReason::Unspecified,
            writer_fence: None,
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

    /// Bind the operation's reason to this exact prepared generation.
    #[must_use]
    pub fn because(mut self, reason: BranchRewriteReason) -> Self {
        self.rewrite_reason = reason;
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

    #[must_use]
    pub fn with_writer_fence(mut self, fence: Option<Arc<RemoteLeaseGuard>>) -> Self {
        self.writer_fence = fence;
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

/// One exact old parent generation replaced by its same-batch planned successor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RebaseTopologyParentReplacement {
    pub old_parent: CommitOid,
    pub new_parent: CommitOid,
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
    /// Exact provider base replaced by the retained simulated parent in this
    /// same globally verified batch. Absent for ordinary remote targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_replacement: Option<RebaseTopologyParentReplacement>,
    /// Source merges Git safely elided because every external parent was already
    /// in exact target ancestry and the independently proven final tree still
    /// matched. This is explicit in the receipt rather than hidden as a changed
    /// commit count (bd-a720be).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elided_target_merges: Vec<CommitOid>,
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
    #[serde(default)]
    pub rewrite_reason: BranchRewriteReason,
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
    #[serde(default)]
    pub rewrite_reason: BranchRewriteReason,
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
        budget.writer_fence.as_ref(),
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
    // A simulated target is the same parent branch rewritten earlier in this
    // globally verified batch. A nonlinear child can legitimately carry the
    // exact old provider base as one merge parent; retain that exact replacement
    // proof while moving all candidate cousins onto the planned parent
    // (bd-b7568d, bd-a720be).
    let replay_parent_boundary =
        matches!(&new_base, PlannedBase::Simulated(_)).then(|| range_branch.oid.clone());
    let replaced_parent_boundary = replay_parent_boundary.as_ref().filter(|old_parent| {
        old_topology
            .iter()
            .any(|commit| commit.parents.contains(old_parent))
    });
    // A previous Cara generation may already contain the exact selected target
    // as a merge parent. v0.0.60 produced precisely that shape and then refused
    // its own first-parent cousin history on the next tick. Target ancestry plus
    // the independently proven result tree is sufficient to treat the branch as
    // already stacked (bd-a720be).
    let target_is_ancestor = is_ancestor(&runner, &target.oid, &candidate.head.oid)?;
    // Root flattening is work only while merge commits remain in the candidate
    // range. Once an earlier tick has produced a linear head directly on this
    // same target, excluding it from reuse solely because the policy flag stays
    // enabled regenerates identical trees under new timestamps forever and
    // continually invalidates exact-generation CI (bd-e500b0).
    let root_needs_flattening = budget.flatten_squashed_root && has_merges;
    let reuse_existing_head = target_is_ancestor
        && !root_needs_flattening
        && !budget.reconcile_squash_equivalent
        && budget.replay_upstream.is_none();
    let expected_merge_tree = if has_merges {
        if !target_is_ancestor {
            validate_merge_preserving_topology(
                &runner,
                &old_topology,
                target,
                replaced_parent_boundary,
                candidate.number,
            )?;
        }
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
        budget.writer_fence.clone(),
    )?;
    let worktree_runner = process_runner(
        &worktree.path,
        budget.command_timeout,
        budget.operation_deadline,
        budget.writer_fence.as_ref(),
    );
    // Reconciliation is opt-in and additionally requires exact proof, so a
    // routine rewrite never silently drops history. A merge-preserving range
    // is excluded: only an ancestor-closed linear range can be proven here.
    let reconciliation =
        if !reuse_existing_head && budget.reconcile_squash_equivalent && !has_merges {
            authorized_reconciliation(&runner, candidate, target, &range_base)?
        } else {
            None
        };
    // bd-85b71d: a merge-preserving root that Cara will squash-merge does not
    // need its history replayed. `expected_merge_tree` has already proven the
    // exact content that will land, so build that commit directly instead of
    // re-resolving conflicts the author already resolved by hand.
    let flattened = if !reuse_existing_head && has_merges && budget.flatten_squashed_root {
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
    let rebase = if reuse_existing_head || flattened.is_some() {
        CommandOutput::success(String::new())
    } else {
        run_rebase(
            &worktree_runner,
            &range_base,
            &target.oid,
            &candidate.head.oid,
            has_merges,
            budget
                .replay_upstream
                .as_ref()
                .or_else(|| {
                    reconciliation
                        .as_ref()
                        .and_then(SquashEquivalenceReport::authorized_range_base)
                })
                .or(replay_parent_boundary.as_ref()),
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
    if new_head != candidate.head.oid {
        let rewritten_topology = collect_range_topology(
            &worktree_runner,
            &[&target.oid],
            &new_head,
            "rebase_result_invalid",
        )?;
        validate_rewritten_metadata(
            &worktree_runner,
            &rewritten_topology,
            target,
            candidate.number,
        )?;
    }
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
                expected,
                replaced_parent_boundary,
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
        rewrite_reason: budget.rewrite_reason.clone(),
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
    replaced_parent_boundary: Option<&CommitOid>,
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
            if replaced_parent_boundary == Some(parent) {
                continue;
            }
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

#[derive(Debug)]
struct CommitMetadata {
    author_seconds: i64,
    committer_seconds: i64,
    subject: String,
}

fn commit_metadata(
    runner: &impl CommandRunner,
    oid: &CommitOid,
) -> Result<CommitMetadata, AppError> {
    let output = require_success(
        runner,
        CommandSpec::new("git").args(["show", "-s", "--format=%at%x1f%ct%x1f%s", oid.0.as_str()]),
        "rebase_metadata_unavailable",
        "rewritten commit metadata could not be read before push",
    )?;
    let mut fields = output.stdout.trim_end().splitn(3, '\u{1f}');
    let parse_seconds = |value: Option<&str>, field: &'static str| {
        value
            .unwrap_or_default()
            .parse::<i64>()
            .map_err(|error| {
                decision(
                    "rebase_metadata_unavailable",
                    "rewritten commit timestamp is not an integer",
                    json!({"commit": oid, "field": field, "error": error.to_string(), "mutated": false, "resumable": true}),
                )
            })
    };
    Ok(CommitMetadata {
        author_seconds: parse_seconds(fields.next(), "author_seconds")?,
        committer_seconds: parse_seconds(fields.next(), "committer_seconds")?,
        subject: fields.next().unwrap_or_default().to_owned(),
    })
}

/// Verify metadata on every object Cara just created before any push.
///
/// `--reset-author-date` supplies the intended values, but the postcondition is
/// the authority: a skewed clock or future-dated parent must fail closed rather
/// than publishing a commit that predates one of its own parents. A reconstructed
/// merge directly naming the selected target must likewise name that real branch
/// instead of retaining a stale `Merge branch 'main'` subject (bd-a720be).
fn validate_rewritten_metadata(
    runner: &impl CommandRunner,
    topology: &[RebaseTopologyCommit],
    target: &BranchSnapshot,
    pr: PrNumber,
) -> Result<(), AppError> {
    for commit in topology {
        let metadata = commit_metadata(runner, &commit.oid)?;
        let mut parent_floor = i64::MIN;
        for parent in &commit.parents {
            let parent_metadata = commit_metadata(runner, parent)?;
            parent_floor = parent_floor
                .max(parent_metadata.author_seconds)
                .max(parent_metadata.committer_seconds);
        }
        if metadata.author_seconds < parent_floor || metadata.committer_seconds < parent_floor {
            return Err(decision(
                "rebase_non_monotonic_timestamp",
                "Caravan generated a commit whose author or committer date predates one of its parents; the pull request owner cannot repair a Caravan-authored object",
                json!({
                    "pr": pr,
                    "commit": commit.oid,
                    "author_seconds": metadata.author_seconds,
                    "committer_seconds": metadata.committer_seconds,
                    "minimum_parent_seconds": parent_floor,
                    "responsible_component": "caravan",
                    "owner_actionable": false,
                    "mutated": false,
                    "resumable": true,
                    "safe_next_action": "upgrade or repair Caravan; do not route this generated-history defect to the pull request owner",
                }),
            ));
        }
        // In Git's ordered parent list the first parent is the checked-out
        // branch and the second is what was merged into it. Only the latter can
        // truthfully require the selected target's name; target as first parent
        // means some internal candidate side was merged, whose ref name is not
        // recoverable from the commit object.
        if commit.parents.len() == 2
            && commit.parents.get(1) == Some(&target.oid)
            && !metadata.subject.contains(&target.name)
        {
            return Err(decision(
                "rebase_misleading_merge_subject",
                "Caravan reconstructed a merge of the selected target but retained a subject naming a different branch; the pull request owner cannot repair a Caravan-authored object",
                json!({
                    "pr": pr,
                    "commit": commit.oid,
                    "subject": metadata.subject,
                    "actual_merged_branch": target.name,
                    "actual_merged_oid": target.oid,
                    "responsible_component": "caravan",
                    "owner_actionable": false,
                    "mutated": false,
                    "resumable": true,
                    "safe_next_action": "upgrade or repair Caravan; do not route this generated-history defect to the pull request owner",
                }),
            ));
        }
    }
    Ok(())
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
            // A stacked target is normally a cousin of the source head. Keeping
            // cousins on their original roots produced a Caravan-authored merge
            // whose first parent was still rooted on a stale release commit,
            // then the ancestry guard correctly refused that exact object. The
            // sequencer must move candidate cousins onto the selected target;
            // the independently proven result tree and topology proof below
            // remain the content/shape authority (bd-a720be).
            "--rebase-merges=rebase-cousins".to_owned(),
            "--reapply-cherry-picks".to_owned(),
            "--empty=keep".to_owned(),
        ]);
    }
    // Use the exact target as the upstream so target-side commits represented in
    // an old merge are excluded rather than replayed. The independently retained
    // range base still binds and validates source provenance; changing the
    // upstream to that older boundary replays target advances and grows the
    // candidate topology. The live stale-root defect was `no-rebase-cousins`,
    // not this target exclusion (bd-a720be).
    //
    // A reconciled upstream replaces the target only when squash-equivalence
    // analysis proved, path by path, that the target already holds that
    // boundary's cumulative content; eviction may likewise supply an explicit
    // boundary.
    let upstream = reconciled_upstream.unwrap_or(target);
    arguments.extend([
        // These are NEW Caravan-authored objects. Copying each old author date
        // into the committer date made a merge predate its newly selected parent.
        // Reset both dates at replay time; a postcondition below verifies every
        // emitted object is parent-monotonic before any push (bd-a720be).
        "--reset-author-date".to_owned(),
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

/// Select the source commits that still require one-to-one mapping after Git
/// safely removes target-only merge scaffolding.
fn mapped_replay_source<'a>(
    old_topology: &'a [RebaseTopologyCommit],
    new_topology: &[RebaseTopologyCommit],
    pr: PrNumber,
) -> Result<(Vec<&'a RebaseTopologyCommit>, Vec<CommitOid>), AppError> {
    if new_topology.len() == old_topology.len() {
        return Ok((old_topology.iter().collect(), Vec::new()));
    }
    let old_without_merges = old_topology
        .iter()
        .filter(|commit| commit.parents.len() != 2)
        .collect::<Vec<_>>();
    if new_topology.len() == old_without_merges.len()
        && new_topology.iter().all(|commit| commit.parents.len() != 2)
    {
        return Ok((
            old_without_merges,
            old_topology
                .iter()
                .filter(|commit| commit.parents.len() == 2)
                .map(|commit| commit.oid.clone())
                .collect(),
        ));
    }
    Err(topology_commit_count_changed(
        pr,
        old_topology,
        new_topology,
    ))
}

fn build_merge_topology_proof(
    runner: &impl CommandRunner,
    old_topology: &[RebaseTopologyCommit],
    target: &CommitOid,
    new_head: &CommitOid,
    expected_tree: CommitOid,
    replaced_parent_boundary: Option<&CommitOid>,
    pr: PrNumber,
) -> Result<MergePreservingTopology, AppError> {
    let new_tree = rev_parse(runner, &format!("{}^{{tree}}", new_head.0))?;
    if new_tree != expected_tree {
        return Err(decision(
            "rebase_merge_tree_mismatch",
            "merge-preserving replay tree differs from the independently computed clean merge tree",
            json!({"pr": pr, "expected_tree": expected_tree, "actual_tree": new_tree, "resumable": true}),
        ));
    }
    let new_topology =
        collect_range_topology(runner, &[target], new_head, "rebase_result_invalid")?;

    // `rebase-cousins` turns `stale-root fix -- merge old-main` into one
    // fix directly on the target. External-parent validation and exact tree
    // equality make that elision explicit and bounded (bd-a720be).
    let (mapped_old, elided_target_merges) = mapped_replay_source(old_topology, &new_topology, pr)?;
    let oid_mapping = mapped_old
        .iter()
        .zip(&new_topology)
        .map(|(old, new)| (old.oid.clone(), new.oid.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut mapping = Vec::with_capacity(mapped_old.len());
    for (old, new) in mapped_old.into_iter().zip(&new_topology) {
        if old.parents.len() != new.parents.len() {
            return Err(decision(
                "rebase_topology_changed",
                "merge-preserving replay changed a commit's parent cardinality",
                json!({"pr": pr, "old": old, "new": new, "resumable": true}),
            ));
        }
        for old_parent in &old.parents {
            if replaced_parent_boundary == Some(old_parent) {
                if !new.parents.contains(target) {
                    return Err(decision(
                        "rebase_topology_changed",
                        "merge-preserving replay did not replace the exact old provider parent with the retained planned parent",
                        json!({"pr": pr, "old": old, "new": new, "old_parent": old_parent, "expected_parent": target, "resumable": true}),
                    ));
                }
            } else if let Some(expected_parent) = oid_mapping.get(old_parent)
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
        strategy: if replaced_parent_boundary.is_some() {
            "git_rebase_merges_rebase_cousins_v3_replaced_parent"
        } else {
            "git_rebase_merges_rebase_cousins_v2"
        }
        .to_owned(),
        expected_merge_tree_oid: expected_tree,
        old_commits: old_topology.to_vec(),
        new_commits: new_topology,
        mapping,
        parent_replacement: replaced_parent_boundary.map(|old_parent| {
            RebaseTopologyParentReplacement {
                old_parent: old_parent.clone(),
                new_parent: target.clone(),
            }
        }),
        elided_target_merges,
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
        prepared.worktree.writer_fence.as_ref(),
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
        prepared.worktree.writer_fence.as_ref(),
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
        prepared.worktree.writer_fence.as_ref(),
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
            prepared.worktree.writer_fence.as_ref(),
        );
        let destination = format!("HEAD:refs/heads/{}", prepared.plan.branch);
        require_success(
            &runner,
            CommandSpec::new("git")
                .args([
                    "push",
                    prepared.plan.lease.as_str(),
                    "origin",
                    destination.as_str(),
                ])
                .git_write(),
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
        rewrite_reason: prepared.plan.rewrite_reason.clone(),
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
    let runner = process_runner(repository_path, timeout, phase_deadline, None);
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

fn unique_worktree_path(nonce: u128) -> PathBuf {
    let sequence = WORKTREE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "caravan-rebase-{}-{nonce}-{sequence}",
        std::process::id()
    ))
}

fn process_runner(
    directory: &Path,
    timeout: Duration,
    operation_deadline: Option<Instant>,
    writer_fence: Option<&Arc<RemoteLeaseGuard>>,
) -> WriterCommandRunner {
    let runner = ProcessRunner::in_directory(directory).with_timeout(timeout);
    let runner = operation_deadline.map_or(runner.clone(), |deadline| {
        runner.with_operation_deadline(deadline)
    });
    WriterCommandRunner::with_remote_fence(runner, writer_fence.cloned())
}

struct TemporaryWorktree {
    repository: PathBuf,
    path: PathBuf,
    timeout: Duration,
    operation_deadline: Option<Instant>,
    writer_fence: Option<Arc<RemoteLeaseGuard>>,
}

impl TemporaryWorktree {
    fn create(
        repository: &Path,
        head: &CommitOid,
        timeout: Duration,
        operation_deadline: Option<Instant>,
        writer_fence: Option<Arc<RemoteLeaseGuard>>,
    ) -> Result<Self, AppError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = unique_worktree_path(nonce);
        let runner = process_runner(
            repository,
            timeout,
            operation_deadline,
            writer_fence.as_ref(),
        );
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
            writer_fence,
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
    use std::sync::Mutex;

    use super::*;
    use crate::model::{AutoMergeState, PullRequestState};
    use crate::remote_lease::{
        RemoteLeaseAcquire, RemoteLeaseError, RemoteLeaseGrant, RemoteLeaseKey, RemoteWriterLease,
    };
    use mcp_cli::StructuredError;

    // Physical-rebase fixtures intentionally exercise several real local Git
    // fetch/rebase/merge subprocesses. Keep their non-timeout assertions
    // resilient when the Nix build runs the suite under CPU/I/O contention;
    // timeout policy has separate focused coverage.
    const TEST_REBASE_BUDGET: Duration = Duration::from_secs(60);

    #[derive(Default)]
    struct RevocableLease(Mutex<Option<RemoteLeaseGrant>>);

    impl RevocableLease {
        fn revoke(&self) {
            *self.0.lock().unwrap() = None;
        }
    }

    impl RemoteWriterLease for RevocableLease {
        fn acquire(
            &self,
            request: &RemoteLeaseAcquire,
        ) -> Result<RemoteLeaseGrant, RemoteLeaseError> {
            let grant = RemoteLeaseGrant {
                schema_version: 1,
                key: request.key.clone(),
                writer_owner: request.writer_owner.clone(),
                operation_id: request.operation_id.clone(),
                fencing_token: 1,
                heartbeat_due_unix_ms: request.now_unix_ms + request.heartbeat_ms,
                expires_unix_ms: request.now_unix_ms + request.ttl_ms,
                backend_revision: "revision-1".to_owned(),
            };
            *self.0.lock().unwrap() = Some(grant.clone());
            Ok(grant)
        }

        fn inspect(
            &self,
            _key: &RemoteLeaseKey,
        ) -> Result<Option<RemoteLeaseGrant>, RemoteLeaseError> {
            Ok(self.0.lock().unwrap().clone())
        }

        fn renew(
            &self,
            _grant: &RemoteLeaseGrant,
            _now_unix_ms: u64,
            _ttl_ms: u64,
            _heartbeat_ms: u64,
        ) -> Result<RemoteLeaseGrant, RemoteLeaseError> {
            Err(RemoteLeaseError::Execution("not used".to_owned()))
        }

        fn release(&self, _grant: &RemoteLeaseGrant) -> Result<bool, RemoteLeaseError> {
            self.0.lock().unwrap().take();
            Ok(true)
        }
    }

    fn test_writer_fence() -> (Arc<RevocableLease>, Arc<RemoteLeaseGuard>) {
        let now_unix_ms: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .try_into()
            .unwrap();
        let backend = Arc::new(RevocableLease::default());
        let backend_trait: Arc<dyn RemoteWriterLease> = backend.clone();
        let guard = RemoteLeaseGuard::acquire(
            backend_trait,
            &RemoteLeaseAcquire {
                key: RemoteLeaseKey {
                    host: "github.com".to_owned(),
                    owner: "owner".to_owned(),
                    repository: "repo".to_owned(),
                    installation_id: Some(42),
                },
                writer_owner: "host-a".to_owned(),
                operation_id: "physical-test".to_owned(),
                now_unix_ms,
                ttl_ms: 60_000,
                heartbeat_ms: 15_000,
            },
        )
        .unwrap();
        (backend, Arc::new(guard))
    }

    #[test]
    fn identical_clock_values_still_produce_unique_worktree_paths() {
        let paths = (0..256)
            .map(|_| unique_worktree_path(1_785_546_450_726_853_000))
            .collect::<BTreeSet<_>>();
        assert_eq!(paths.len(), 256);
    }

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

    fn amend_head_dates(directory: &Path, date: &str) {
        let output = Command::new("git")
            .current_dir(directory)
            .env("GIT_COMMITTER_DATE", date)
            .args(["commit", "--amend", "--no-edit", "--date", date])
            .output()
            .expect("amend fixture dates");
        assert!(
            output.status.success(),
            "date amend failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn rewrite_receipt(reason: BranchRewriteReason) -> RebaseReceipt {
        RebaseReceipt {
            pr: PrNumber(2330),
            branch: "child".to_owned(),
            old_head_oid: CommitOid("a".repeat(40)),
            new_head_oid: CommitOid("b".repeat(40)),
            old_base_oid: CommitOid("c".repeat(40)),
            new_base_branch: "parent".to_owned(),
            new_base_oid: CommitOid("d".repeat(40)),
            new_tree_oid: CommitOid("e".repeat(40)),
            commit_count: 1,
            merge_topology: None,
            squash_reconciliation: None,
            ci_trigger_workflows: Vec::new(),
            lease: "exact".to_owned(),
            already_satisfied: false,
            rewrite_reason: reason,
        }
    }

    #[test]
    fn rewrite_reason_is_one_visible_line_and_exact_generation_marked() {
        let receipt = rewrite_receipt(BranchRewriteReason::ParentAdvanced {
            parent_pr: PrNumber(2331),
        });
        let (marker, body) = receipt.rewrite_reason.comment(&receipt).unwrap();
        assert_eq!(body.lines().count(), 1);
        assert!(body.len() < 240);
        assert!(body.contains("parent #2331 advanced"));
        assert!(body.contains("aaaaaaa → bbbbbbb"));
        assert!(body.ends_with(&marker));
        assert!(marker.contains(":2330:"));
    }

    #[test]
    fn unspecified_or_already_satisfied_rewrite_never_claims_an_update() {
        let receipt = rewrite_receipt(BranchRewriteReason::Unspecified);
        assert!(receipt.rewrite_reason.comment(&receipt).is_none());
        let mut noop = rewrite_receipt(BranchRewriteReason::CurrentDefaultAdvanced);
        noop.already_satisfied = true;
        assert!(noop.rewrite_reason.comment(&noop).is_none());
    }

    fn assert_dates_not_before(directory: &Path, child: &CommitOid, parent: &CommitOid) {
        let dates = git(directory, &["show", "-s", "--format=%at %ct", &child.0]);
        let parent_dates = git(directory, &["show", "-s", "--format=%at %ct", &parent.0]);
        let floor = parent_dates
            .split_whitespace()
            .map(|value| value.parse::<i64>().unwrap())
            .max()
            .unwrap();
        assert!(
            dates
                .split_whitespace()
                .all(|value| value.parse::<i64>().unwrap() >= floor)
        );
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

    fn open_candidate(
        number: u64,
        title: &str,
        head: BranchSnapshot,
        base: BranchSnapshot,
    ) -> PullRequestSnapshot {
        PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(number),
            title: title.to_owned(),
            url: format!("https://example.invalid/{number}"),
            state: PullRequestState::Open,
            draft: false,
            head,
            base,
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn lost_operation_fence_after_prepare_stops_before_force_push() {
        let fixture = fixture();
        let candidate = open_candidate(
            42,
            "fenced candidate",
            branch(&fixture.repository, "feature", &fixture.feature),
            branch(&fixture.repository, "main", &fixture.old_main),
        );
        let target = branch(&fixture.repository, "main", &fixture.new_main);
        let (backend, fence) = test_writer_fence();
        let prepared = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            range_base_for_remote_target(&candidate, &target),
            PlannedBase::Remote(target.clone()),
            &target,
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET).with_writer_fence(Some(fence)),
        )
        .expect("preparation uses only read and dry-run commands");
        backend.revoke();

        let error = apply_prepared(&prepared).unwrap_err();
        assert_eq!(error.code(), "rebase_command_failed");
        assert!(
            error.details().unwrap()["source"]
                .as_str()
                .unwrap()
                .contains("remote GitWrite fence refused")
        );
        let remote = git(
            &fixture.clone,
            &["ls-remote", "origin", "refs/heads/feature"],
        );
        assert_eq!(
            remote.split_whitespace().next(),
            Some(fixture.feature.0.as_str())
        );
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
        assert_eq!(topology.strategy, "git_rebase_merges_rebase_cousins_v2");
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

    /// A merge whose external side is already exact target ancestry contributes
    /// no independent patch. Rebase-cousins may elide it only while the proven
    /// final tree remains identical, and the receipt names the elision
    /// (bd-a720be).
    #[test]
    fn elides_a_default_branch_merge_already_represented_by_the_target() {
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
                .all(|commit| commit.parents.len() != 2),
            "the redundant target merge is linearized onto the target"
        );
        assert_eq!(topology.elided_target_merges.len(), 1);
        assert_eq!(receipt.new_tree_oid, topology.expected_merge_tree_oid);
    }

    /// Exact shape emitted for Cacophony #2330 by v0.0.60 (bd-a720be):
    /// a fix rooted on stale main, a merge of then-current main, and a newer
    /// parent-generation branch selected as the stack target.
    #[test]
    fn stacked_main_merge_is_rebuilt_on_target_and_passes_the_next_guard() {
        let fixture = fixture();
        git(&fixture.clone, &["checkout", "feature"]);
        amend_head_dates(&fixture.clone, "2001-01-01T00:00:00Z");
        git(
            &fixture.clone,
            &[
                "merge",
                "--no-ff",
                "main",
                "-m",
                "Merge branch 'main' into feature",
            ],
        );
        amend_head_dates(&fixture.clone, "2001-01-01T00:00:00Z");
        let source_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "--force", "origin", "feature"]);

        git(&fixture.clone, &["checkout", "main"]);
        git(&fixture.clone, &["checkout", "-b", "parent-generation"]);
        std::fs::write(fixture.clone.join("parent-generation"), "parent\n").unwrap();
        git(&fixture.clone, &["add", "parent-generation"]);
        git(&fixture.clone, &["commit", "-m", "pending parent member"]);
        let parent_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(
            &fixture.clone,
            &["push", "-u", "origin", "parent-generation"],
        );
        git(&fixture.clone, &["checkout", "feature"]);

        let mut candidate = open_candidate(
            2330,
            "timed-out request latency",
            branch(&fixture.repository, "feature", &source_head),
            branch(&fixture.repository, "main", &fixture.new_main),
        );
        let target = branch(&fixture.repository, "parent-generation", &parent_head);
        let workflow_source = branch(&fixture.repository, "main", &fixture.new_main);
        let prepared = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            remote_range(&candidate),
            PlannedBase::Remote(target.clone()),
            &workflow_source,
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET).because(
                BranchRewriteReason::ParentAdvanced {
                    parent_pr: PrNumber(2331),
                },
            ),
        )
        .expect("Caravan's own stack output must pass its ancestry proof");

        assert!(!prepared.plan.already_satisfied);
        assert_eq!(
            prepared.plan.rewrite_reason,
            BranchRewriteReason::ParentAdvanced {
                parent_pr: PrNumber(2331)
            }
        );
        assert!(
            is_ancestor(
                &process_runner(&fixture.clone, TEST_REBASE_BUDGET, None, None),
                &parent_head,
                &prepared.plan.new_head_oid
            )
            .unwrap()
        );
        let topology = prepared.plan.merge_topology.as_ref().unwrap();
        assert_eq!(topology.elided_target_merges.len(), 1);
        assert!(
            topology
                .new_commits
                .iter()
                .all(|commit| commit.parents.len() == 1)
        );
        assert!(
            !git(
                &fixture.clone,
                &["show", "-s", "--format=%s", &prepared.plan.new_head_oid.0]
            )
            .contains("Merge branch 'main'")
        );
        assert_dates_not_before(&fixture.clone, &prepared.plan.new_head_oid, &parent_head);

        let published =
            apply_prepared(&prepared).expect("publish the exact prepared test generation");
        assert_eq!(published.rewrite_reason, prepared.plan.rewrite_reason);
        candidate.head.oid = prepared.plan.new_head_oid.clone();
        candidate.base = target.clone();
        let next = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            remote_range(&candidate),
            PlannedBase::Remote(target),
            &workflow_source,
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET),
        )
        .expect("next tick accepts the exact object Cara produced");
        assert!(next.plan.already_satisfied);
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

    /// bd-b7568d live PR2330 shape: the child was authored on an independent
    /// side and merged the exact old root generation as its second parent.
    /// Rewriting that root in the same batch must map only that provider base
    /// to the planned root; it is not unowned cousin history.
    #[test]
    fn nonlinear_child_maps_old_provider_parent_to_planned_parent() {
        let fixture = fixture();
        git(
            &fixture.clone,
            &["checkout", "-b", "child", fixture.old_main.0.as_str()],
        );
        std::fs::write(fixture.clone.join("child-side"), "child\n").unwrap();
        git(&fixture.clone, &["add", "child-side"]);
        git(&fixture.clone, &["commit", "-m", "child side"]);
        git(
            &fixture.clone,
            &[
                "merge",
                "--no-ff",
                "feature",
                "-m",
                "merge exact parent into child",
            ],
        );
        let child_head = CommitOid(git(&fixture.clone, &["rev-parse", "HEAD"]));
        git(&fixture.clone, &["push", "-u", "origin", "child"]);

        let parent = open_candidate(
            7,
            "parent",
            branch(&fixture.repository, "feature", &fixture.feature),
            branch(&fixture.repository, "main", &fixture.old_main),
        );
        let child = open_candidate(
            8,
            "child",
            branch(&fixture.repository, "child", &child_head),
            branch(&fixture.repository, "feature", &fixture.feature),
        );
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
        .expect("parent plan");
        let planned_parent = branch(
            &fixture.repository,
            "feature",
            &prepared_parent.plan.new_head_oid,
        );
        let prepared_child = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &child,
            range_base_for_rewritten_parent(&child, &parent.head),
            PlannedBase::Simulated(planned_parent.clone()),
            &default,
            RebaseExecutionBudget::new(Duration::from_secs(60)),
        )
        .expect("the old exact provider parent maps to its same-batch replacement");

        let topology = prepared_child
            .plan
            .merge_topology
            .as_ref()
            .expect("nonlinear topology receipt");
        assert_eq!(
            topology.strategy,
            "git_rebase_merges_rebase_cousins_v3_replaced_parent"
        );
        assert_eq!(
            topology.parent_replacement,
            Some(RebaseTopologyParentReplacement {
                old_parent: fixture.feature.clone(),
                new_parent: planned_parent.oid.clone(),
            })
        );
        let old_merge = topology
            .old_commits
            .iter()
            .find(|commit| commit.parents.contains(&fixture.feature))
            .expect("old merge names the provider base");
        assert!(
            topology.elided_target_merges.contains(&old_merge.oid),
            "the redundant old-parent merge is replaced by direct planned-parent ancestry"
        );
        assert_eq!(
            prepared_child.plan.new_tree_oid,
            topology.expected_merge_tree_oid
        );

        verify_prepared(&prepared_parent).expect("parent global preflight");
        verify_prepared(&prepared_child).expect("child global preflight");
        let parent_receipt = apply_prepared_after_write_barrier(&prepared_parent).unwrap();
        let child_receipt = apply_prepared_after_write_barrier(&prepared_child).unwrap();
        let runner = process_runner(&fixture.clone, TEST_REBASE_BUDGET, None, None);
        assert!(
            is_ancestor(
                &runner,
                &parent_receipt.new_head_oid,
                &child_receipt.new_head_oid
            )
            .unwrap()
        );
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

        // Publish and rediscover the exact flattened root. A later tick on the
        // unchanged target must retain this generation; replaying its now-linear
        // commits solely because root flattening is enabled changes timestamps,
        // cancels exact-generation CI, and can never reach merge readiness
        // (bd-e500b0).
        let first = apply_prepared(&prepared).expect("publish the flattened root");
        let mut rediscovered = candidate;
        rediscovered.head.oid = first.new_head_oid.clone();
        rediscovered.base.oid = target_head.clone();
        let second = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &rediscovered,
            remote_range(&rediscovered),
            PlannedBase::Remote(target.clone()),
            &target,
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET).flattening_squashed_root(true),
        )
        .expect("an already flattened root is idempotent on an unchanged target");
        assert!(second.plan.already_satisfied);
        assert_eq!(second.plan.new_head_oid, first.new_head_oid);
    }

    /// A child that already contains the exact selected target as a merge parent
    /// is already stacked. Ordinary sync retains that exact object instead of
    /// rejecting its other parent as cousin history or flattening it. This is
    /// the migration path for the first v0.0.60 stack (bd-a720be).
    #[test]
    fn a_child_with_the_exact_target_as_merge_parent_is_already_stacked() {
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

        let prepared = prepare_candidate(
            &fixture.clone,
            &fixture.repository,
            &candidate,
            remote_range(&candidate),
            PlannedBase::Remote(target.clone()),
            &target,
            RebaseExecutionBudget::new(TEST_REBASE_BUDGET),
        )
        .expect("the exact target merge parent is already a valid stack edge");

        assert!(prepared.plan.already_satisfied);
        assert_eq!(prepared.plan.new_head_oid, head);
        assert_eq!(prepared.plan.new_base.branch().oid, target_head);
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
