//! Exact reconciliation of squash-equivalent pre-squash history.
//!
//! A caravan member that lands is squash-merged: the target branch gains one
//! new commit whose *content* equals the member's cumulative content, but whose
//! commit identity is unrelated to the pre-squash commits the surviving stack
//! still carries. Every later tail therefore keeps replaying commits whose
//! result is already represented on the target, and a candidate rebased on the
//! current target can conflict against content that is textually identical to
//! what it is trying to introduce.
//!
//! This module answers exactly one question, worktree-free and without any
//! mutation: *is there a stacked prefix whose cumulative content the target
//! already holds byte-for-byte, and does replaying only the remaining commits
//! against that proven boundary merge cleanly?*
//!
//! The proof is deliberately narrow:
//!
//! * Only an ancestor-closed **linear prefix** of the candidate-only range is
//!   considered, so dropping it can never orphan later content.
//! * A prefix is represented only when **every** path its cumulative diff
//!   changes has an identical blob object *and* file mode on the target tip.
//!   Commit messages, patch text, subjects, authorship, and commit dates are
//!   never evidence.
//! * Reconciliation is authorized only when the retained commits, replayed with
//!   the proven boundary as merge base, produce an independently clean merge.
//!
//! Everything else fails closed: partial overlap, an identical patch with a
//! different resulting tree, a genuine three-way divergence after the equality
//! point, an ambiguous or absent merge base, or a range that exceeds the
//! audited bound. Failing closed never drops a commit and never proposes
//! "take either side".

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppError;
use crate::command::{CommandRunner, CommandSpec, DEFAULT_COMMAND_TIMEOUT, ProcessRunner};
use crate::compatibility;
use crate::model::{BranchSnapshot, CommitOid, CompatibilityOutcome};

/// Deterministic contract text bound into every emitted receipt.
pub const SQUASH_EQUIVALENCE_POLICY: &str = "A stacked pre-squash prefix is reconciled only with exact cumulative proof: every path its cumulative diff changes must have an identical blob object and file mode on the exact target tip, and replaying the retained commits with that proven boundary as merge base must be independently clean. Commit messages, patch text, subjects, and authorship are never proof. Partial representation, an equal patch with a different resulting tree, a genuine three-way divergence after the equality point, a vacuous match on paths the prefix never changed, and any indeterminate range all fail closed with exact evidence and drop nothing. Reconciliation evidence never authorizes rewriting a live provider branch on its own; that remains a separately reviewed owner action.";

/// Audited bound on the linear prefix considered for reconciliation.
pub const MAX_ANALYZED_PREFIX_COMMITS: usize = 256;

/// Audited bound on how many prefix boundaries are proven per evaluation.
///
/// Already-landed members are the *earliest* commits of a stacked range, so
/// the bound is applied to the earliest boundaries and the longest proven one
/// among them is selected. A range deeper than this reports that its
/// evaluation was bounded instead of silently claiming no equivalence exists.
pub const MAX_EVALUATED_BOUNDARIES: usize = 64;

/// Audited bound on per-receipt path evidence.
pub const MAX_RECEIPT_PATHS: usize = 512;

/// Whether a proven reconciliation exists for this exact candidate/target pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SquashEquivalenceOutcome {
    /// No stacked prefix is represented on the target with exact proof.
    NoEquivalence,
    /// A proven prefix exists and the retained commits merge cleanly against it.
    Reconcilable,
    /// A proven prefix exists, but the retained commits still conflict against
    /// the target. This is ordinary semantic divergence after the equality
    /// point and is never reconciled automatically.
    ResidualConflict,
    /// The range cannot be evaluated exactly (absent/ambiguous merge base,
    /// non-linear boundary, non-representable path, or bound exceeded).
    Indeterminate,
}

impl SquashEquivalenceOutcome {
    /// Stable `snake_case` name for receipts and hooks.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NoEquivalence => "no_equivalence",
            Self::Reconcilable => "reconcilable",
            Self::ResidualConflict => "residual_conflict",
            Self::Indeterminate => "indeterminate",
        }
    }

    /// Whether this outcome authorizes replaying from a reconciled boundary.
    #[must_use]
    pub const fn authorizes_reconciliation(self) -> bool {
        matches!(self, Self::Reconcilable)
    }
}

/// What reconciliation would do with one analyzed candidate-only commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationAction {
    /// Proven already represented on the target; dropped from the replay.
    Dropped,
    /// Kept and replayed. Genuinely new content is always retained.
    Retained,
}

impl ReconciliationAction {
    /// Stable `snake_case` name for receipts and hooks.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dropped => "dropped",
            Self::Retained => "retained",
        }
    }
}

/// Exact per-path proof that the target already holds the prefix's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepresentedPath {
    pub path: String,
    /// File mode at the proven boundary commit; identical on the target.
    pub mode: String,
    /// Blob object at the proven boundary commit; identical on the target.
    pub blob: String,
    /// Whether the prefix's cumulative effect on this path is a deletion which
    /// the target already performed.
    pub deleted: bool,
}

/// One analyzed candidate-only commit and its reconciliation disposition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnalyzedCommit {
    pub oid: CommitOid,
    pub tree_oid: CommitOid,
    pub action: ReconciliationAction,
    /// Paths this exact commit changed relative to its parent. Evidence only;
    /// authority comes from the cumulative per-path proof.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// Exact merge evidence for one three-way construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MergeEvidence {
    /// Merge base used for the construction.
    pub merge_base: CommitOid,
    pub outcome: CompatibilityOutcome,
    /// Cumulative tree `git merge-tree` constructed for this base.
    pub result_tree: CommitOid,
    #[serde(default)]
    pub conflicting_paths: Vec<String>,
}

/// Complete auditable receipt for one squash-equivalence evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SquashEquivalenceReport {
    pub schema_version: u32,
    pub candidate: BranchSnapshot,
    pub target: BranchSnapshot,
    pub candidate_oid: CommitOid,
    pub target_oid: CommitOid,
    /// Natural merge base of candidate and target, when one exists uniquely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_base: Option<CommitOid>,
    pub outcome: SquashEquivalenceOutcome,
    /// Cumulative merge evidence before reconciliation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<MergeEvidence>,
    /// Cumulative merge evidence after reconciliation, when a proven boundary
    /// exists. Present for both `reconcilable` and `residual_conflict`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<MergeEvidence>,
    /// Proven boundary commit, i.e. the reconciled replay base. Present
    /// whenever a prefix was proven, even if the residual replay conflicts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proven_boundary: Option<CommitOid>,
    /// Tree of the proven boundary commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_tree: Option<CommitOid>,
    /// Tree of the exact target tip which supplied the equality proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_tree: Option<CommitOid>,
    /// Analyzed linear prefix commits with their dispositions.
    #[serde(default)]
    pub commits: Vec<AnalyzedCommit>,
    /// Exact per-path equality proof backing the dropped prefix.
    #[serde(default)]
    pub represented_paths: Vec<RepresentedPath>,
    /// Whether path evidence was bounded for receipt size.
    #[serde(default)]
    pub represented_paths_truncated: bool,
    /// Total candidate-only commits observed in this range.
    #[serde(default)]
    pub candidate_commit_count: usize,
    /// Whether the analyzed linear prefix covered the whole candidate range.
    #[serde(default)]
    pub analyzed_prefix_complete: bool,
    /// Prefix boundaries actually proven, and whether that hit the audited bound.
    #[serde(default)]
    pub evaluated_boundaries: usize,
    #[serde(default)]
    pub evaluation_bounded: bool,
    pub reason: String,
    pub policy: String,
}

impl SquashEquivalenceReport {
    /// Boundary commit a reconciled replay may use as its range base.
    ///
    /// `None` unless the prefix is proven *and* the retained commits merge
    /// cleanly from that boundary, so callers cannot accidentally reconcile a
    /// genuine divergence.
    #[must_use]
    pub fn authorized_range_base(&self) -> Option<&CommitOid> {
        self.outcome
            .authorizes_reconciliation()
            .then_some(self.proven_boundary.as_ref())
            .flatten()
    }

    /// Whether the un-reconciled attachment currently conflicts.
    #[must_use]
    pub fn reconciliation_required(&self) -> bool {
        self.before
            .as_ref()
            .is_some_and(|evidence| evidence.outcome != CompatibilityOutcome::Clean)
    }

    /// Exact commits reconciliation would drop, in ancestry order.
    #[must_use]
    pub fn dropped_commits(&self) -> Vec<CommitOid> {
        self.commits
            .iter()
            .filter(|commit| commit.action == ReconciliationAction::Dropped)
            .map(|commit| commit.oid.clone())
            .collect()
    }

    /// Exact analyzed commits reconciliation retains, in ancestry order.
    #[must_use]
    pub fn retained_commits(&self) -> Vec<CommitOid> {
        self.commits
            .iter()
            .filter(|commit| commit.action == ReconciliationAction::Retained)
            .map(|commit| commit.oid.clone())
            .collect()
    }

    /// Every path the proven prefix contributed, in sorted order.
    #[must_use]
    pub fn affected_paths(&self) -> Vec<String> {
        self.represented_paths
            .iter()
            .map(|path| path.path.clone())
            .collect()
    }

    /// Bounded structured details for errors, receipts, and hooks.
    #[must_use]
    pub fn details(&self) -> serde_json::Value {
        json!({
            "schema_version": self.schema_version,
            "candidate": format!("{}@{}", self.candidate.name, self.candidate_oid.0),
            "target": format!("{}@{}", self.target.name, self.target_oid.0),
            "outcome": self.outcome.name(),
            "merge_base": self.merge_base.as_ref().map(|oid| oid.0.clone()),
            "proven_boundary": self.proven_boundary.as_ref().map(|oid| oid.0.clone()),
            "dropped_commits": self.dropped_commits().iter().map(|oid| oid.0.clone()).collect::<Vec<_>>(),
            "retained_commits": self.retained_commits().iter().map(|oid| oid.0.clone()).collect::<Vec<_>>(),
            "affected_paths": self.affected_paths(),
            "cumulative_tree_before": self.before.as_ref().map(|evidence| evidence.result_tree.0.clone()),
            "cumulative_tree_after": self.after.as_ref().map(|evidence| evidence.result_tree.0.clone()),
            "residual_conflicting_paths": self
                .after
                .as_ref()
                .map_or_else(Vec::new, |evidence| evidence.conflicting_paths.clone()),
            "reason": self.reason,
            "policy": self.policy,
        })
    }
}

/// Analyze one candidate against one target, fetching both exact revisions.
pub fn analyze(
    repository: impl AsRef<Path>,
    remote: &str,
    candidate: &BranchSnapshot,
    target: &BranchSnapshot,
) -> Result<SquashEquivalenceReport, AppError> {
    analyze_with_timeout(
        repository,
        remote,
        candidate,
        target,
        DEFAULT_COMMAND_TIMEOUT,
    )
}

/// Analyze with an explicit per-command hard deadline.
pub fn analyze_with_timeout(
    repository: impl AsRef<Path>,
    remote: &str,
    candidate: &BranchSnapshot,
    target: &BranchSnapshot,
    timeout: Duration,
) -> Result<SquashEquivalenceReport, AppError> {
    if candidate.repository != target.repository {
        return Err(AppError::validation(
            "cross_repository_reconciliation_unsupported",
            "squash-equivalence reconciliation requires candidate and target branches in one repository",
        ));
    }
    let runner = ProcessRunner::in_directory(repository).with_timeout(timeout);
    let target_oid = compatibility::resolve_branch_snapshot_with_runner(&runner, remote, target)?;
    let candidate_oid =
        compatibility::resolve_branch_snapshot_with_runner(&runner, remote, candidate)?;
    analyze_with_runner(&runner, candidate, target, &candidate_oid, &target_oid)
}

/// Analyze revisions already fetched and verified by a bounded preparation phase.
#[allow(clippy::too_many_lines)]
pub(crate) fn analyze_with_runner(
    runner: &impl CommandRunner,
    candidate: &BranchSnapshot,
    target: &BranchSnapshot,
    candidate_oid: &CommitOid,
    target_oid: &CommitOid,
) -> Result<SquashEquivalenceReport, AppError> {
    let mut report = SquashEquivalenceReport {
        schema_version: 1,
        candidate: candidate.clone(),
        target: target.clone(),
        candidate_oid: candidate_oid.clone(),
        target_oid: target_oid.clone(),
        merge_base: None,
        outcome: SquashEquivalenceOutcome::Indeterminate,
        before: None,
        after: None,
        proven_boundary: None,
        boundary_tree: None,
        target_tree: None,
        commits: Vec::new(),
        represented_paths: Vec::new(),
        represented_paths_truncated: false,
        candidate_commit_count: 0,
        analyzed_prefix_complete: false,
        evaluated_boundaries: 0,
        evaluation_bounded: false,
        reason: String::new(),
        policy: SQUASH_EQUIVALENCE_POLICY.to_owned(),
    };

    let Some(merge_base) = unique_merge_base(runner, candidate_oid, target_oid)? else {
        "candidate and target do not share exactly one merge base; no exact cumulative proof is possible"
            .clone_into(&mut report.reason);
        return Ok(report);
    };
    report.merge_base = Some(merge_base.clone());
    report.target_tree = Some(compatibility::commit_tree_with_runner(runner, target_oid)?);

    let (before_outcome, before_tree, before_conflicts) =
        compatibility::merge_tree_with_runner(runner, candidate_oid, target_oid)?;
    report.before = Some(MergeEvidence {
        merge_base: merge_base.clone(),
        outcome: before_outcome,
        result_tree: CommitOid(before_tree),
        conflicting_paths: before_conflicts,
    });

    let chain = linear_prefix_chain(runner, &merge_base, candidate_oid)?;
    report.candidate_commit_count = chain.total_commits;
    report.analyzed_prefix_complete =
        chain.commits.len() == chain.total_commits && !chain.commits.is_empty();
    if chain.commits.is_empty() {
        report.outcome = SquashEquivalenceOutcome::NoEquivalence;
        report.reason = if chain.total_commits == 0 {
            "candidate has no commits beyond the merge base; there is no stacked history to reconcile".to_owned()
        } else {
            "candidate history is not linear at the merge base; only an ancestor-closed linear range may be reconciled".to_owned()
        };
        return Ok(report);
    }
    if !report.analyzed_prefix_complete {
        // A non-linear candidate-only range is a deterministic unsupported
        // shape. Proving a prefix would still leave a replay caller deciding
        // how to reproduce merges, so reconciliation fails closed here rather
        // than authorizing a boundary nothing can safely consume.
        report.reason = format!(
            "candidate-only range is not an ancestor-closed linear range ({} of {} commits are linear from the merge base); reconciliation requires a linear range",
            chain.commits.len(),
            chain.total_commits
        );
        return Ok(report);
    }

    let Some(target_entries) = diff_entries(runner, &merge_base, target_oid)? else {
        "target diff contains a path Git could not represent exactly; failing closed"
            .clone_into(&mut report.reason);
        return Ok(report);
    };

    // Search from the longest evaluated prefix downwards: the longest proven
    // prefix drops the most already-represented history, and every shorter
    // prefix it contains is implied by the same cumulative evidence. The
    // evaluation is bounded because each boundary costs one exact Git diff and
    // this analysis shares one operation deadline with every other check.
    let evaluated = chain.commits.len().min(MAX_EVALUATED_BOUNDARIES);
    report.evaluated_boundaries = evaluated;
    report.evaluation_bounded = evaluated < chain.commits.len();
    let mut proven: Option<(usize, BTreeMap<String, DiffTarget>)> = None;
    for index in (0..evaluated).rev() {
        let boundary = &chain.commits[index];
        let Some(prefix_entries) = diff_entries(runner, &merge_base, boundary)? else {
            "candidate prefix diff contains a path Git could not represent exactly; failing closed"
                .clone_into(&mut report.reason);
            return Ok(report);
        };
        // A prefix that changes nothing proves nothing: a vacuous match on
        // files the prefix never touched is never squash-equivalence evidence.
        if prefix_entries.is_empty() {
            continue;
        }
        let represented = prefix_entries.iter().all(|(path, entry)| {
            target_entries
                .get(path)
                .is_some_and(|target_entry| target_entry == entry)
        });
        if represented {
            proven = Some((index, prefix_entries));
            break;
        }
    }

    let Some((boundary_index, prefix_entries)) = proven else {
        report.outcome = SquashEquivalenceOutcome::NoEquivalence;
        report.commits = describe_commits(
            runner,
            &merge_base,
            &chain.commits,
            usize::MAX,
            ReconciliationAction::Retained,
        )?;
        report.reason = format!(
            "no stacked prefix of the first {} of {} candidate-only commit(s) is byte-identical to {}@{}; every commit is retained",
            evaluated,
            chain.commits.len(),
            target.name,
            target_oid.0
        );
        return Ok(report);
    };

    let boundary = chain.commits[boundary_index].clone();
    report.proven_boundary = Some(boundary.clone());
    report.boundary_tree = Some(compatibility::commit_tree_with_runner(runner, &boundary)?);
    let (after_outcome, after_tree, after_conflicts) =
        compatibility::merge_tree_with_base_with_runner(
            runner,
            candidate_oid,
            target_oid,
            Some(&boundary),
        )?;
    report.after = Some(MergeEvidence {
        merge_base: boundary.clone(),
        outcome: after_outcome,
        result_tree: CommitOid(after_tree),
        conflicting_paths: after_conflicts.clone(),
    });

    let mut represented = prefix_entries
        .iter()
        .map(|(path, entry)| RepresentedPath {
            path: path.clone(),
            mode: entry.mode.clone(),
            blob: entry.blob.clone(),
            deleted: entry.deleted(),
        })
        .collect::<Vec<_>>();
    represented.sort_by(|left, right| left.path.cmp(&right.path));
    report.represented_paths_truncated = represented.len() > MAX_RECEIPT_PATHS;
    represented.truncate(MAX_RECEIPT_PATHS);
    report.represented_paths = represented;

    if after_outcome == CompatibilityOutcome::Clean {
        report.outcome = SquashEquivalenceOutcome::Reconcilable;
        report.commits = describe_commits(
            runner,
            &merge_base,
            &chain.commits,
            boundary_index,
            ReconciliationAction::Dropped,
        )?;
        report.reason = format!(
            "commits through {} are byte-identical to {}@{} on every one of the {} path(s) they change; replaying the {} retained commit(s) from that proven boundary merges cleanly",
            boundary.0,
            target.name,
            target_oid.0,
            report.represented_paths.len(),
            chain.commits.len() - boundary_index - 1
        );
    } else {
        report.outcome = SquashEquivalenceOutcome::ResidualConflict;
        report.commits = describe_commits(
            runner,
            &merge_base,
            &chain.commits,
            usize::MAX,
            ReconciliationAction::Retained,
        )?;
        report.reason = format!(
            "commits through {} are byte-identical to {}@{}, but replaying the retained commits from that proven boundary still conflicts in {}; this is ordinary three-way divergence after the equality point and nothing is dropped",
            boundary.0,
            target.name,
            target_oid.0,
            if after_conflicts.is_empty() {
                "the reconciled merge".to_owned()
            } else {
                after_conflicts.join(", ")
            }
        );
    }
    Ok(report)
}

/// One dst-side entry of an exact `git diff-tree --raw` record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffTarget {
    mode: String,
    blob: String,
}

impl DiffTarget {
    fn deleted(&self) -> bool {
        self.mode.bytes().all(|byte| byte == b'0')
    }
}

/// The linear, ancestor-closed prefix of the candidate-only range.
struct PrefixChain {
    commits: Vec<CommitOid>,
    total_commits: usize,
}

/// Exact unique merge base, or `None` when absent or ambiguous.
fn unique_merge_base(
    runner: &impl CommandRunner,
    candidate_oid: &CommitOid,
    target_oid: &CommitOid,
) -> Result<Option<CommitOid>, AppError> {
    let output = compatibility::git_output(
        runner,
        [
            "merge-base",
            "--all",
            target_oid.0.as_str(),
            candidate_oid.0.as_str(),
        ],
    )?;
    match output.code {
        Some(0) => {}
        Some(1) => return Ok(None),
        code => {
            return Err(compatibility::git_failure(
                "merge_base_failed",
                "git merge-base could not evaluate the candidate range boundary",
                code,
                &output,
            ));
        }
    }
    let mut bases = output.stdout.lines().filter(|line| !line.trim().is_empty());
    let Some(base) = bases.next() else {
        return Ok(None);
    };
    if bases.next().is_some() {
        return Ok(None);
    }
    let base = base.trim().to_owned();
    if !compatibility::valid_full_oid(&base) {
        return Err(compatibility::malformed_git_output(
            "merge_base_output_invalid",
            "git merge-base returned an invalid object ID",
            &output,
        ));
    }
    Ok(Some(CommitOid(base)))
}

/// Collect the linear prefix walking upward from the merge base.
///
/// The walk stops at the first commit that is not a single-parent continuation
/// of the previous one, so a merge commit or a fork point bounds the analysis
/// instead of silently reordering history.
fn linear_prefix_chain(
    runner: &impl CommandRunner,
    merge_base: &CommitOid,
    candidate_oid: &CommitOid,
) -> Result<PrefixChain, AppError> {
    let output = compatibility::git_output(
        runner,
        [
            "rev-list",
            "--reverse",
            "--topo-order",
            "--parents",
            candidate_oid.0.as_str(),
            &format!("^{}", merge_base.0),
        ],
    )?;
    if !output.is_success() {
        return Err(compatibility::git_failure(
            "rev_list_failed",
            "git rev-list could not enumerate the exact candidate-only range",
            output.code,
            &output,
        ));
    }
    let lines = output.stdout.lines().collect::<Vec<_>>();
    let total_commits = lines.len();
    let mut commits = Vec::new();
    let mut expected_parent = merge_base.clone();
    for line in lines {
        if commits.len() >= MAX_ANALYZED_PREFIX_COMMITS {
            break;
        }
        let mut fields = line.split_whitespace();
        let Some(oid) = fields.next() else { break };
        if !compatibility::valid_full_oid(oid) {
            return Err(compatibility::malformed_git_output(
                "rev_list_output_invalid",
                "git rev-list returned an invalid object ID",
                &output,
            ));
        }
        let parents = fields.collect::<Vec<_>>();
        if parents.len() != 1 || parents[0] != expected_parent.0 {
            break;
        }
        expected_parent = CommitOid(oid.to_owned());
        commits.push(expected_parent.clone());
    }
    Ok(PrefixChain {
        commits,
        total_commits,
    })
}

/// Exact dst-side `path -> (mode, blob)` map for one tree-to-tree diff.
///
/// Returns `None` when Git reported a path this process cannot represent
/// exactly, so a lossy comparison can never manufacture equality.
fn diff_entries(
    runner: &impl CommandRunner,
    from: &CommitOid,
    to: &CommitOid,
) -> Result<Option<BTreeMap<String, DiffTarget>>, AppError> {
    let output = compatibility::git_output(
        runner,
        [
            "diff-tree",
            "-r",
            "-z",
            "--no-commit-id",
            "--no-renames",
            from.0.as_str(),
            to.0.as_str(),
        ],
    )?;
    if !output.is_success() {
        return Err(compatibility::git_failure(
            "diff_tree_failed",
            "git diff-tree could not construct exact cumulative content evidence",
            output.code,
            &output,
        ));
    }
    let mut fields = output
        .stdout
        .as_bytes()
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut entries = BTreeMap::new();
    while let Some(meta) = fields.next() {
        let meta = compatibility::bytes_to_text(meta);
        let Some(record) = meta.strip_prefix(':') else {
            return Err(compatibility::malformed_git_output(
                "diff_tree_output_invalid",
                "git diff-tree returned a record without a raw-format marker",
                &output,
            ));
        };
        let columns = record.split_whitespace().collect::<Vec<_>>();
        if columns.len() < 5 {
            return Err(compatibility::malformed_git_output(
                "diff_tree_output_invalid",
                "git diff-tree returned an incomplete raw record",
                &output,
            ));
        }
        let Some(path) = fields.next().map(compatibility::bytes_to_text) else {
            return Err(compatibility::malformed_git_output(
                "diff_tree_output_invalid",
                "git diff-tree returned a raw record without a path",
                &output,
            ));
        };
        if path.contains('\u{fffd}') {
            return Ok(None);
        }
        entries.insert(
            path,
            DiffTarget {
                mode: columns[1].to_owned(),
                blob: columns[3].to_owned(),
            },
        );
    }
    Ok(Some(entries))
}

/// Build the per-commit receipt rows for one analyzed prefix.
///
/// Commits at or before `dropped_through` take `dropped_action`; every later
/// commit is retained. Passing `usize::MAX` retains everything.
fn describe_commits(
    runner: &impl CommandRunner,
    merge_base: &CommitOid,
    commits: &[CommitOid],
    dropped_through: usize,
    dropped_action: ReconciliationAction,
) -> Result<Vec<AnalyzedCommit>, AppError> {
    let trees = commit_trees(runner, commits)?;
    let mut described = Vec::with_capacity(commits.len());
    let mut parent = merge_base.clone();
    for ((index, oid), tree_oid) in commits.iter().enumerate().zip(trees) {
        let action = if index <= dropped_through {
            dropped_action
        } else {
            ReconciliationAction::Retained
        };
        let paths = if action == ReconciliationAction::Dropped {
            diff_entries(runner, &parent, oid)?
                .map(|entries| entries.into_keys().collect::<BTreeSet<_>>())
                .unwrap_or_default()
                .into_iter()
                .take(MAX_RECEIPT_PATHS)
                .collect()
        } else {
            Vec::new()
        };
        described.push(AnalyzedCommit {
            oid: oid.clone(),
            tree_oid,
            action,
            paths,
        });
        parent = oid.clone();
    }
    Ok(described)
}

/// Resolve every commit's exact tree in one batched Git object query.
///
/// One subprocess per analyzed range keeps receipt construction bounded even
/// for a deep stacked candidate.
fn commit_trees(
    runner: &impl CommandRunner,
    commits: &[CommitOid],
) -> Result<Vec<CommitOid>, AppError> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    let input = commits.iter().fold(String::new(), |mut input, commit| {
        use std::fmt::Write as _;
        writeln!(input, "{}^{{tree}}", commit.0).expect("writing to String cannot fail");
        input
    });
    let command = CommandSpec::new("git")
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"])
        .stdin(input);
    let output = runner
        .run(&command)
        .map_err(|error| compatibility::command_run_error(&error))?;
    if !output.is_success() {
        return Err(compatibility::git_failure(
            "commit_tree_batch_failed",
            "git cat-file could not resolve the exact commit trees",
            output.code,
            &output,
        ));
    }
    let mut trees = Vec::with_capacity(commits.len());
    for line in output.stdout.lines() {
        let mut fields = line.split_whitespace();
        let (Some(oid), Some("tree")) = (fields.next(), fields.next()) else {
            return Err(compatibility::malformed_git_output(
                "commit_tree_batch_invalid",
                "git cat-file did not resolve an expected commit tree",
                &output,
            ));
        };
        if !compatibility::valid_full_oid(oid) {
            return Err(compatibility::malformed_git_output(
                "commit_tree_batch_invalid",
                "git cat-file returned an invalid tree object ID",
                &output,
            ));
        }
        trees.push(CommitOid(oid.to_owned()));
    }
    if trees.len() != commits.len() {
        return Err(compatibility::malformed_git_output(
            "commit_tree_batch_invalid",
            "git cat-file returned an unexpected number of commit trees",
            &output,
        ));
    }
    Ok(trees)
}

/// Typed fail-closed error for callers which required an authorized boundary.
#[must_use]
pub fn unauthorized(report: &SquashEquivalenceReport) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        "squash_equivalence_unproven",
        format!(
            "squash-equivalence reconciliation is not authorized: {}",
            report.reason
        ),
        Some(report.details()),
    )
}

#[cfg(test)]
mod tests;
