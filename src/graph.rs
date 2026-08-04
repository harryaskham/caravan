//! Pure graph derivation plus injectable mechanical compatibility validation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::AppError;
use crate::compatibility;
use crate::model::{
    BranchSnapshot, Caravan, CaravanFleet, CompatibilityOutcome, CompatibilityReport,
    CumulativeTreeProof, GraphProblem, GraphProblemKind, HeadMergeActor, MergeMethod, PrNumber,
    PullRequestSnapshot, PullRequestState, RepositorySnapshot,
};
use crate::squash_equivalence::SquashEquivalenceReport;

/// Full read-only analysis shared by status, show, check, CLI JSON, and MCP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GraphAnalysis {
    pub fleet: CaravanFleet,
    /// Canonical PR facts keyed by PR number for rendering and follow-up checks.
    pub pull_requests: BTreeMap<PrNumber, PullRequestSnapshot>,
    /// Exact compatibility evidence collected while validating current chains.
    #[serde(default)]
    pub compatibility: Vec<CompatibilityReport>,
    /// Exact cumulative-tree evidence for every caravan root against the
    /// default branch. Consumed by the caravan-owned merge so a retargeted root
    /// only lands when its squash produces the already-validated head tree.
    #[serde(default)]
    pub cumulative_trees: Vec<CumulativeTreeProof>,
    /// Exact squash-equivalence evidence for every attachment that is *not*
    /// mechanically clean. A stacked candidate whose earliest commits were
    /// already squash-landed on the target conflicts against content identical
    /// to what it carries; this evidence states, with exact per-path blob
    /// proof, whether that is the case here and whether reconciling it would
    /// leave a clean replay. It is evidence only: it never authorizes a
    /// rewrite on its own.
    #[serde(default)]
    pub squash_reconciliations: Vec<SquashEquivalenceReport>,
}

/// Bounded evidence about status-only compatibility work.
///
/// Mutating operations continue to require the complete graph analysis. The
/// read-only status surface may stop at its dedicated analysis deadline and
/// return the current provider/structural snapshot with every unevaluated
/// mechanical proof named explicitly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompatibilityAnalysisProgress {
    pub complete: bool,
    pub revision_preparation_complete: bool,
    /// Provider PR rows in the current bounded snapshot.
    pub candidate_count: usize,
    /// Unenrolled rows which may participate in automatic admission.
    pub unqueued_candidate_count: usize,
    pub caravan_count: usize,
    pub branch_count: usize,
    pub planned_analyses: usize,
    pub completed_analyses: usize,
    #[serde(default)]
    pub deferred_analyses: Vec<String>,
    #[serde(default)]
    pub deferred_analyses_truncated: usize,
    #[serde(default)]
    pub skipped_analyses: Vec<String>,
    #[serde(default)]
    pub skipped_analyses_truncated: usize,
}

/// One graph plus the exact completeness of its mechanical analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedGraphAnalysis {
    pub analysis: GraphAnalysis,
    pub progress: CompatibilityAnalysisProgress,
}

impl GraphAnalysis {
    /// Whether one problem gates the active fleet rather than a quarantined
    /// parked caravan.
    ///
    /// Parking is the exact terminal-red repair route: the complete topology
    /// and its problems remain visible, but that caravan leaves convergence,
    /// capacity, tail selection, and independent admission until its owner
    /// repairs it. A problem spanning any unparked caravan still gates. A
    /// repository-global problem with no identifiable caravan also fails
    /// closed.
    #[must_use]
    pub fn problem_blocks_active_fleet(&self, problem: &GraphProblem) -> bool {
        if !problem.kind.blocks_fleet() {
            return false;
        }
        let mut parked_caravan_observed = false;
        for number in &problem.prs {
            if let Some(caravan) = self.fleet.containing(*number) {
                if !caravan.parked {
                    return true;
                }
                parked_caravan_observed = true;
            }
        }
        !parked_caravan_observed
    }

    #[must_use]
    pub fn healthy(&self) -> bool {
        // An unadmitted candidate's incompatibility says nothing about the health
        // of the fleet: it is advisory evidence about a PR that is not in any
        // caravan and blocks nothing. A parked caravan is likewise quarantined:
        // keep its exact repair evidence, but do not let it mark the active
        // fleet unhealthy and reject every independent candidate.
        !self
            .fleet
            .problems
            .iter()
            .any(|problem| self.problem_blocks_active_fleet(problem))
    }
}

/// Injectable compatibility seam. Production uses worktree-free Git; tests can
/// focus on graph policy without subprocesses.
pub trait CompatibilityChecker {
    /// Validate/fetch unique branches once before pairwise reports. Fakes keep
    /// the default no-op while production caches exact revisions.
    fn prepare(&self, _branches: &[BranchSnapshot]) -> Result<(), AppError> {
        Ok(())
    }

    fn check(
        &self,
        candidate: &BranchSnapshot,
        target: &BranchSnapshot,
    ) -> Result<CompatibilityReport, AppError>;

    /// Exact cumulative-tree proof for landing `candidate` on `target`.
    ///
    /// Fakes keep the default "unproven", which the caravan-owned merge treats
    /// as "do not merge yet" rather than as permission.
    fn cumulative_tree(
        &self,
        _candidate: &BranchSnapshot,
        _target: &BranchSnapshot,
    ) -> Result<Option<CumulativeTreeProof>, AppError> {
        Ok(None)
    }

    /// Exact squash-equivalence evidence for a non-clean attachment.
    ///
    /// Fakes keep the default "no evidence", so graph policy tests never claim
    /// a conflict is reconcilable without a real Git proof.
    fn squash_equivalence(
        &self,
        _candidate: &BranchSnapshot,
        _target: &BranchSnapshot,
    ) -> Result<Option<SquashEquivalenceReport>, AppError> {
        Ok(None)
    }
}

impl<F> CompatibilityChecker for F
where
    F: Fn(&BranchSnapshot, &BranchSnapshot) -> Result<CompatibilityReport, AppError>,
{
    fn check(
        &self,
        candidate: &BranchSnapshot,
        target: &BranchSnapshot,
    ) -> Result<CompatibilityReport, AppError> {
        self(candidate, target)
    }
}

type PreparedRevisionKey = (String, String);

/// Process-wide evidence that one exact provider revision was prepared in one
/// local object database. Provider discovery remains authoritative; this only
/// avoids refetching an unchanged exact OID during later reads in the same tick.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SharedPreparedRevisionKey {
    local_repository: PathBuf,
    remote: String,
    provider_repository: crate::model::RepositoryId,
    branch: String,
    oid: String,
}

#[derive(Debug, Clone)]
struct SharedPreparedRevision {
    inserted: Instant,
    resolved: crate::model::CommitOid,
}

static SHARED_PREPARED_REVISIONS: OnceLock<
    Mutex<BTreeMap<SharedPreparedRevisionKey, SharedPreparedRevision>>,
> = OnceLock::new();
const SHARED_PREPARED_REVISION_MAX_AGE: Duration = Duration::from_secs(600);
const MAX_SHARED_PREPARED_REVISIONS: usize = 4_096;

fn prepared_revision_key(branch: &BranchSnapshot) -> PreparedRevisionKey {
    (branch.name.clone(), branch.oid.0.clone())
}

/// Production compatibility checker for one repository and remote.
#[derive(Debug, Clone)]
pub struct GitCompatibilityChecker {
    repository: PathBuf,
    remote: String,
    timeout: Duration,
    operation_deadline: Option<std::time::Instant>,
    prepared: std::cell::RefCell<BTreeMap<PreparedRevisionKey, crate::model::CommitOid>>,
}

impl GitCompatibilityChecker {
    #[must_use]
    pub fn new(repository: impl AsRef<Path>, remote: impl Into<String>) -> Self {
        let repository = repository.as_ref();
        Self {
            repository: std::fs::canonicalize(repository)
                .unwrap_or_else(|_| repository.to_path_buf()),
            remote: remote.into(),
            timeout: crate::command::DEFAULT_COMMAND_TIMEOUT,
            operation_deadline: None,
            prepared: std::cell::RefCell::new(BTreeMap::new()),
        }
    }

    fn shared_key(&self, branch: &BranchSnapshot) -> SharedPreparedRevisionKey {
        SharedPreparedRevisionKey {
            local_repository: self.repository.clone(),
            remote: self.remote.clone(),
            provider_repository: branch.repository.clone(),
            branch: branch.name.clone(),
            oid: branch.oid.0.clone(),
        }
    }

    fn hydrate_from_shared_cache(&self, branches: &[BranchSnapshot]) {
        let cache = SHARED_PREPARED_REVISIONS.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut shared = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.retain(|_, entry| entry.inserted.elapsed() <= SHARED_PREPARED_REVISION_MAX_AGE);
        let mut local = self.prepared.borrow_mut();
        for branch in branches {
            if let Some(entry) = shared.get(&self.shared_key(branch)) {
                local.insert(prepared_revision_key(branch), entry.resolved.clone());
            }
        }
    }

    fn publish_to_shared_cache(
        &self,
        branches: &[BranchSnapshot],
        prepared: &BTreeMap<PreparedRevisionKey, crate::model::CommitOid>,
    ) {
        let cache = SHARED_PREPARED_REVISIONS.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut shared = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.retain(|_, entry| entry.inserted.elapsed() <= SHARED_PREPARED_REVISION_MAX_AGE);
        for branch in branches {
            let Some(resolved) = prepared.get(&prepared_revision_key(branch)) else {
                continue;
            };
            let key = self.shared_key(branch);
            if !shared.contains_key(&key)
                && shared.len() >= MAX_SHARED_PREPARED_REVISIONS
                && let Some(oldest) = shared
                    .iter()
                    .min_by_key(|(_, entry)| entry.inserted)
                    .map(|(key, _)| key.clone())
            {
                shared.remove(&oldest);
            }
            shared.insert(
                key,
                SharedPreparedRevision {
                    inserted: Instant::now(),
                    resolved: resolved.clone(),
                },
            );
        }
    }

    /// Override the hard deadline for each exact-revision Git subprocess.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Share the status operation's absolute deadline across every compatibility subprocess.
    #[must_use]
    pub fn with_operation_deadline(mut self, deadline: std::time::Instant) -> Self {
        self.operation_deadline = Some(deadline);
        self
    }
}

impl CompatibilityChecker for GitCompatibilityChecker {
    fn prepare(&self, branches: &[BranchSnapshot]) -> Result<(), AppError> {
        self.hydrate_from_shared_cache(branches);
        let missing = {
            let cache = self.prepared.borrow();
            branches
                .iter()
                .filter(|branch| !cache.contains_key(&prepared_revision_key(branch)))
                .cloned()
                .collect::<Vec<_>>()
        };
        if !missing.is_empty() {
            let runner = crate::command::ProcessRunner::in_directory(&self.repository)
                .with_timeout(self.timeout)
                .with_operation_deadline(
                    self.operation_deadline
                        .unwrap_or_else(|| std::time::Instant::now() + self.timeout),
                );
            let prepared = compatibility::prepare_branch_snapshots_with_runner(
                &runner,
                &self.remote,
                &missing,
            )?;
            self.publish_to_shared_cache(&missing, &prepared);
            self.prepared.borrow_mut().extend(prepared);
        }
        Ok(())
    }

    fn check(
        &self,
        candidate: &BranchSnapshot,
        target: &BranchSnapshot,
    ) -> Result<CompatibilityReport, AppError> {
        let cache = self.prepared.borrow();
        if let (Some(candidate_oid), Some(target_oid)) = (
            cache.get(&prepared_revision_key(candidate)),
            cache.get(&prepared_revision_key(target)),
        ) {
            let runner = crate::command::ProcessRunner::in_directory(&self.repository)
                .with_timeout(self.timeout)
                .with_operation_deadline(
                    self.operation_deadline
                        .unwrap_or_else(|| std::time::Instant::now() + self.timeout),
                );
            compatibility::check_resolved_compatibility_with_runner(
                &runner,
                candidate,
                target,
                candidate_oid,
                target_oid,
            )
        } else {
            let timeout = self.operation_deadline.map_or(self.timeout, |deadline| {
                self.timeout
                    .min(deadline.saturating_duration_since(std::time::Instant::now()))
            });
            compatibility::check_compatibility_with_timeout(
                &self.repository,
                &self.remote,
                candidate,
                target,
                timeout,
            )
        }
    }

    fn cumulative_tree(
        &self,
        candidate: &BranchSnapshot,
        target: &BranchSnapshot,
    ) -> Result<Option<CumulativeTreeProof>, AppError> {
        let cache = self.prepared.borrow();
        let (Some(candidate_oid), Some(target_oid)) = (
            cache.get(&prepared_revision_key(candidate)),
            cache.get(&prepared_revision_key(target)),
        ) else {
            return Ok(None);
        };
        let runner = crate::command::ProcessRunner::in_directory(&self.repository)
            .with_timeout(self.timeout)
            .with_operation_deadline(
                self.operation_deadline
                    .unwrap_or_else(|| std::time::Instant::now() + self.timeout),
            );
        compatibility::cumulative_tree_proof_with_runner(
            &runner,
            candidate,
            target,
            candidate_oid,
            target_oid,
        )
        .map(Some)
    }

    fn squash_equivalence(
        &self,
        candidate: &BranchSnapshot,
        target: &BranchSnapshot,
    ) -> Result<Option<SquashEquivalenceReport>, AppError> {
        let cache = self.prepared.borrow();
        let (Some(candidate_oid), Some(target_oid)) = (
            cache.get(&prepared_revision_key(candidate)),
            cache.get(&prepared_revision_key(target)),
        ) else {
            return Ok(None);
        };
        let runner = crate::command::ProcessRunner::in_directory(&self.repository)
            .with_timeout(self.timeout)
            .with_operation_deadline(
                self.operation_deadline
                    .unwrap_or_else(|| std::time::Instant::now() + self.timeout),
            );
        crate::squash_equivalence::analyze_with_runner(
            &runner,
            candidate,
            target,
            candidate_oid,
            target_oid,
        )
        .map(Some)
    }
}

/// Derive structural chains without executing Git compatibility checks.
///
/// The phases intentionally stay together so one pass owns all intermediate
/// parent/child maps and cannot expose a partially-derived graph.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn derive(snapshot: &RepositorySnapshot) -> GraphAnalysis {
    derive_for_actor(snapshot, HeadMergeActor::default())
}

/// Derive structural chains for one configured head merge actor.
///
/// The auto-merge invariant is *gated on the actor*. A repository that
/// deliberately disabled provider-native auto-merge so Cara can own the merge
/// must not report a permanent, unsatisfiable "head must have squash auto-merge
/// enabled" problem; under [`HeadMergeActor::Caravan`] the invariant inverts and
/// every member — root included — must keep native auto-merge off so there is
/// exactly one merge actor.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn derive_for_actor(snapshot: &RepositorySnapshot, actor: HeadMergeActor) -> GraphAnalysis {
    let mut problems = Vec::new();
    let mut pull_requests = BTreeMap::new();
    for pull_request in &snapshot.pull_requests {
        if pull_requests
            .insert(pull_request.number, pull_request.clone())
            .is_some()
        {
            problems.push(GraphProblem {
                kind: GraphProblemKind::DuplicateMember,
                prs: vec![pull_request.number],
                message: format!(
                    "PR #{} appeared more than once in one discovery snapshot",
                    pull_request.number
                ),
            });
        }
    }

    let mut active = BTreeMap::new();
    let mut merged_by_head = BTreeMap::new();
    // A caravan member that is CLOSED rather than merged has left the chain
    // without landing, which is what a republished generation looks like: the
    // author opens a successor PR on a new branch and closes this one. Membership
    // is recomputed from labels on every read, so the departure leaves no trace
    // of itself — the only evidence is a surviving child still based on the
    // branch. Remember those branches so the resulting dangling base can name
    // what left instead of reporting an anonymous non-member branch (bd-d897cc).
    let mut departed_by_head = BTreeMap::new();
    for pull_request in pull_requests.values() {
        if pull_request.state == PullRequestState::Merged && pull_request.has_label("caravan") {
            merged_by_head.insert(pull_request.head.name.clone(), pull_request.number);
        }
        // A member closed WITHOUT merging took its caravan with it. Membership is
        // derived from open-or-merged pull requests, so a closed one is simply
        // absent: where a tail survived it dangles and the fleet stops, but a
        // sole member leaves nothing behind and the caravan silently ceases to
        // exist. The label outlives the close, so the evidence is right here
        // (bd-461c8b).
        if pull_request.state == PullRequestState::Closed && pull_request.has_label("caravan") {
            problems.push(GraphProblem {
                kind: GraphProblemKind::DissolvedMember,
                prs: vec![pull_request.number],
                message: format!(
                    "caravan member #{} was closed without merging, so its caravan no longer exists; requeue the work through its successor or remove the `caravan` label to acknowledge the dissolution",
                    pull_request.number.0
                ),
            });
            // Remember the branch too, so the surviving tail bd-461c8b names
            // above can say WHICH member it lost rather than reporting an
            // anonymous non-member branch (bd-d897cc).
            departed_by_head.insert(pull_request.head.name.clone(), pull_request.number);
        }
        if pull_request.state == PullRequestState::Open
            && pull_request.has_label("caravan")
            && pull_request.has_label("caravan-evicted")
        {
            problems.push(GraphProblem {
                kind: GraphProblemKind::ActiveAndEvicted,
                prs: vec![pull_request.number],
                message: format!(
                    "PR #{} has both caravan and caravan-evicted labels",
                    pull_request.number
                ),
            });
        } else if pull_request.is_active_caravan_member() {
            active.insert(pull_request.number, pull_request);
        }
    }

    let mut heads_by_branch: BTreeMap<String, Vec<PrNumber>> = BTreeMap::new();
    for pull_request in active.values() {
        heads_by_branch
            .entry(pull_request.head.name.clone())
            .or_default()
            .push(pull_request.number);
        if pull_request.cross_repository || pull_request.head.repository != snapshot.repository {
            problems.push(GraphProblem {
                kind: GraphProblemKind::ForkOnlyPredecessor,
                prs: vec![pull_request.number],
                message: format!(
                    "PR #{} has a fork-only head branch which cannot be a GitHub PR base",
                    pull_request.number
                ),
            });
        }
    }
    for members in heads_by_branch.values_mut() {
        members.sort_unstable();
        if members.len() > 1 {
            problems.push(GraphProblem {
                kind: GraphProblemKind::DuplicateMember,
                prs: members.clone(),
                message: "multiple active PRs expose the same head branch".to_owned(),
            });
        }
    }

    let mut roots = BTreeSet::new();
    let mut parent_of = BTreeMap::new();
    let mut children: BTreeMap<PrNumber, Vec<PrNumber>> = BTreeMap::new();
    for pull_request in active.values() {
        if pull_request.base.name == snapshot.default_branch.name {
            roots.insert(pull_request.number);
            continue;
        }
        match heads_by_branch.get(&pull_request.base.name) {
            Some(parents) if parents.len() == 1 => {
                let parent = parents[0];
                // bd-1cf2e1: a branch name is not identity. If a merged caravan
                // member used this same branch name, an unrelated new PR that
                // reused it would silently become this child's parent, because
                // active heads take precedence over merged ones. Bind the exact
                // recorded base OID to the resolved parent head and fail closed
                // when they disagree.
                let reused = merged_by_head.get(&pull_request.base.name).copied();
                let parent_head = active.get(&parent).map(|parent| parent.head.oid.clone());
                let provenance_matches = parent_head
                    .as_ref()
                    .is_none_or(|head| *head == pull_request.base.oid);
                if reused.is_some() && !provenance_matches {
                    roots.insert(pull_request.number);
                    problems.push(GraphProblem {
                        kind: GraphProblemKind::ReusedBranchProvenance,
                        prs: std::iter::once(pull_request.number)
                            .chain(std::iter::once(parent))
                            .chain(reused)
                            .collect(),
                        message: format!(
                            "PR #{} records base `{}` at {} but active PR #{parent} now holds that branch; a merged caravan member also used it, so the predecessor is ambiguous",
                            pull_request.number,
                            pull_request.base.name,
                            pull_request.base.oid.0,
                        ),
                    });
                    continue;
                }
                parent_of.insert(pull_request.number, parent);
                children
                    .entry(parent)
                    .or_default()
                    .push(pull_request.number);
            }
            Some(parents) => {
                roots.insert(pull_request.number);
                problems.push(GraphProblem {
                    kind: GraphProblemKind::MultipleHeads,
                    prs: std::iter::once(pull_request.number)
                        .chain(parents.iter().copied())
                        .collect(),
                    message: format!(
                        "PR #{} targets ambiguous branch `{}`",
                        pull_request.number, pull_request.base.name
                    ),
                });
            }
            None => {
                roots.insert(pull_request.number);
                let merged = merged_by_head.get(&pull_request.base.name).copied();
                let departed = merged
                    .is_none()
                    .then(|| departed_by_head.get(&pull_request.base.name).copied())
                    .flatten();
                problems.push(GraphProblem {
                    kind: GraphProblemKind::DanglingBase,
                    prs: std::iter::once(pull_request.number)
                        .chain(merged)
                        .chain(departed)
                        .collect(),
                    message: match (merged, departed) {
                        (Some(merged), _) => format!(
                            "PR #{} still targets merged predecessor #{}; sync must advance it to `{}`",
                            pull_request.number, merged, snapshot.default_branch.name
                        ),
                        (None, Some(departed)) => format!(
                            "PR #{} is stranded on branch `{}` of caravan member #{departed}, which was closed without merging; \
                             the caravan lost that member and this PR must be rebased onto `{}` or evicted",
                            pull_request.number,
                            pull_request.base.name,
                            snapshot.default_branch.name
                        ),
                        (None, None) => format!(
                            "PR #{} targets non-member branch `{}`",
                            pull_request.number, pull_request.base.name
                        ),
                    },
                });
            }
        }
    }
    for child_list in children.values_mut() {
        child_list.sort_unstable();
        if child_list.len() > 1 {
            let parent = parent_of
                .iter()
                .find_map(|(child, parent)| child_list.contains(child).then_some(*parent))
                .expect("a child list always has a parent");
            problems.push(GraphProblem {
                kind: GraphProblemKind::Branching,
                prs: std::iter::once(parent)
                    .chain(child_list.iter().copied())
                    .collect(),
                message: format!("PR #{parent} has multiple active caravan children"),
            });
        }
    }

    detect_cycles(&active, &parent_of, &mut problems);

    let mut caravans = Vec::new();
    let mut visited = BTreeSet::new();
    for root in roots {
        if visited.contains(&root) {
            continue;
        }
        let members = walk_linear(root, &children, &mut visited);
        if let Some(caravan) = Caravan::new(members) {
            caravans.push(caravan);
        }
    }
    for number in active.keys().copied() {
        if visited.contains(&number) {
            continue;
        }
        let members = walk_linear(number, &children, &mut visited);
        problems.push(GraphProblem {
            kind: GraphProblemKind::MissingHead,
            prs: members.clone(),
            message: "active component has no unique default-branch head".to_owned(),
        });
        if let Some(caravan) = Caravan::new(members) {
            caravans.push(caravan);
        }
    }
    caravans.sort_by_key(|caravan| caravan.id);
    for caravan in &mut caravans {
        caravan.parked = active
            .get(&caravan.id)
            .is_some_and(|head| head.has_label("caravan-parked"));
    }
    for caravan in &caravans {
        for (position, number) in caravan.members.iter().enumerate() {
            let pull_request = active
                .get(number)
                .expect("derived caravan members are active");
            let native_squash_armed = pull_request.auto_merge.enabled
                && pull_request.auto_merge.merge_method == Some(MergeMethod::Squash);
            // Historical delegation arms exactly the root; caravan-owned
            // merging arms nobody, because there is exactly one merge actor.
            let root_must_be_armed = actor.github() && position == 0 && !caravan.parked;
            let valid = if root_must_be_armed {
                native_squash_armed
            } else {
                !pull_request.auto_merge.enabled
            };
            if !valid {
                let message = if root_must_be_armed {
                    format!("caravan head #{number} must have squash auto-merge enabled")
                } else if position == 0 {
                    format!(
                        "caravan head #{number} must not delegate merging to native auto-merge; cara is the single merge actor"
                    )
                } else {
                    format!("non-head caravan PR #{number} must have auto-merge disabled")
                };
                problems.push(GraphProblem {
                    kind: GraphProblemKind::AutoMergeInvariant,
                    prs: vec![*number],
                    message,
                });
            }
        }
    }

    let unqueued = pull_requests
        .values()
        .filter(|pull_request| {
            pull_request.state == PullRequestState::Open
                && !pull_request.draft
                && !pull_request.has_label("caravan")
                && !pull_request.has_label("caravan-evicted")
        })
        .map(|pull_request| pull_request.number)
        .collect();

    // Prior formation is already in the bounded snapshot: discovery fetches
    // recently merged labelled PRs. Reporting it beside the live list costs no
    // extra provider request and removes the "[] means never" misreading.
    let merged_members = snapshot
        .pull_requests
        .iter()
        .filter(|pull_request| {
            pull_request.state == PullRequestState::Merged && pull_request.has_label("caravan")
        })
        .collect::<Vec<_>>();
    let mut merged_timestamps = merged_members
        .iter()
        .filter_map(|pull_request| pull_request.merged_at.clone())
        .collect::<Vec<_>>();
    merged_timestamps.sort_unstable();
    // The label-filtered query returned these rows, so GitHub's index still
    // considers them caravan members while their own records carry no label.
    // That is the signature of labels stripped after the fact, and without it a
    // count of zero reads as "no caravan ever merged here" — the precise
    // misreading `history` exists to prevent (bd-47f0c7).
    let unlabelled_merged_rows = snapshot
        .pull_requests
        .iter()
        .filter(|pull_request| {
            pull_request.state == PullRequestState::Merged && !pull_request.has_label("caravan")
        })
        .count();
    let history = crate::model::CaravanHistory {
        merged_members_observed: merged_members.len(),
        earliest_merged_at: merged_timestamps.first().cloned(),
        latest_merged_at: merged_timestamps.last().cloned(),
        unlabelled_merged_rows,
    };

    GraphAnalysis {
        fleet: CaravanFleet {
            repository: snapshot.repository.clone(),
            default_branch: snapshot.default_branch.clone(),
            caravans,
            unqueued,
            problems,
            history,
        },
        pull_requests,
        compatibility: Vec::new(),
        cumulative_trees: Vec::new(),
        squash_reconciliations: Vec::new(),
    }
}

/// Derive and mechanically validate current chain/fleet invariants.
pub fn analyze(
    snapshot: &RepositorySnapshot,
    checker: &impl CompatibilityChecker,
) -> Result<GraphAnalysis, AppError> {
    analyze_for_actor(snapshot, checker, HeadMergeActor::default())
}

#[derive(Debug, Clone)]
enum CompatibilityAnalysisTask {
    Fleet {
        description: String,
        candidate: BranchSnapshot,
        target: BranchSnapshot,
        prs: Vec<PrNumber>,
        message: &'static str,
    },
    CumulativeTree {
        description: String,
        candidate: BranchSnapshot,
        target: BranchSnapshot,
    },
    AdmissionCandidate {
        description: String,
        candidate: BranchSnapshot,
        target: BranchSnapshot,
        pr: PrNumber,
    },
}

impl CompatibilityAnalysisTask {
    fn description(&self) -> &str {
        match self {
            Self::Fleet { description, .. }
            | Self::CumulativeTree { description, .. }
            | Self::AdmissionCandidate { description, .. } => description,
        }
    }

    fn execute(
        &self,
        analysis: &mut GraphAnalysis,
        checker: &impl CompatibilityChecker,
    ) -> Result<(), AppError> {
        match self {
            Self::Fleet {
                candidate,
                target,
                prs,
                message,
                ..
            } => {
                let report = checker.check(candidate, target)?;
                record_compatibility(analysis, checker, report, prs.clone(), message)
            }
            Self::CumulativeTree {
                candidate, target, ..
            } => {
                if let Some(proof) = checker.cumulative_tree(candidate, target)? {
                    analysis.cumulative_trees.push(proof);
                }
                Ok(())
            }
            Self::AdmissionCandidate {
                candidate,
                target,
                pr,
                ..
            } => {
                let report = checker.check(candidate, target)?;
                record_candidate_compatibility(
                    analysis,
                    report,
                    *pr,
                    "leading admission candidate does not merge cleanly into the current default branch",
                );
                Ok(())
            }
        }
    }
}

const MAX_ANALYSIS_PROGRESS_SAMPLES: usize = 64;

fn push_progress_sample(samples: &mut Vec<String>, truncated: &mut usize, sample: String) {
    if samples.len() < MAX_ANALYSIS_PROGRESS_SAMPLES {
        samples.push(sample);
    } else {
        *truncated = truncated.saturating_add(1);
    }
}

fn defer_tasks(progress: &mut CompatibilityAnalysisProgress, tasks: &[CompatibilityAnalysisTask]) {
    for task in tasks {
        push_progress_sample(
            &mut progress.deferred_analyses,
            &mut progress.deferred_analyses_truncated,
            task.description().to_owned(),
        );
    }
}

fn analysis_timed_out(error: &AppError) -> bool {
    mcp_cli::StructuredError::category(error) == mcp_cli::ErrorCategory::Timeout
}

#[allow(clippy::too_many_lines)]
fn compatibility_tasks(
    snapshot: &RepositorySnapshot,
    analysis: &GraphAnalysis,
    progress: &mut CompatibilityAnalysisProgress,
) -> Vec<CompatibilityAnalysisTask> {
    let caravans = &analysis.fleet.caravans;
    let mut tasks = Vec::new();
    for caravan in caravans {
        let Some(head_number) = caravan.head() else {
            continue;
        };
        let head_branch = analysis
            .pull_requests
            .get(&head_number)
            .expect("derived head")
            .head
            .clone();
        tasks.push(CompatibilityAnalysisTask::Fleet {
            description: format!("head_to_default:pr#{head_number}"),
            candidate: head_branch.clone(),
            target: snapshot.default_branch.clone(),
            prs: vec![head_number],
            message: "caravan head does not merge cleanly into the current default branch",
        });
        // The caravan-owned merge lands the root's *already-validated* tree or
        // nothing at all, so the proof is collected with the same revisions.
        tasks.push(CompatibilityAnalysisTask::CumulativeTree {
            description: format!("cumulative_tree:pr#{head_number}"),
            candidate: head_branch,
            target: snapshot.default_branch.clone(),
        });
        for pair in caravan.members.windows(2) {
            let parent_branch = analysis
                .pull_requests
                .get(&pair[0])
                .expect("derived parent")
                .head
                .clone();
            let child_branch = analysis
                .pull_requests
                .get(&pair[1])
                .expect("derived child")
                .head
                .clone();
            tasks.push(CompatibilityAnalysisTask::Fleet {
                description: format!("adjacent:pr#{}->pr#{}", pair[1], pair[0]),
                candidate: child_branch,
                target: parent_branch,
                prs: vec![pair[0], pair[1]],
                message: "adjacent caravan PRs are not mechanically compatible",
            });
        }
    }

    // A parked caravan is quarantined for exact repair. Keep its own
    // head/default and internal-edge evidence above, but do not compare it to
    // active caravans: cross-compatibility with a queue slot that cannot be a
    // target must not contaminate active-fleet health or admission.
    for (index, caravan) in caravans
        .iter()
        .enumerate()
        .filter(|(_, caravan)| !caravan.parked)
    {
        for (other_index, other) in caravans
            .iter()
            .enumerate()
            .filter(|(_, caravan)| !caravan.parked)
        {
            if index == other_index {
                continue;
            }
            let (Some(head_number), Some(tail_number)) = (caravan.head(), other.tail()) else {
                continue;
            };
            let head_branch = analysis
                .pull_requests
                .get(&head_number)
                .expect("derived head")
                .head
                .clone();
            let tail_branch = analysis
                .pull_requests
                .get(&tail_number)
                .expect("derived tail")
                .head
                .clone();
            tasks.push(CompatibilityAnalysisTask::Fleet {
                description: format!("cross_caravan:pr#{head_number}->pr#{tail_number}"),
                candidate: head_branch,
                target: tail_branch,
                prs: vec![head_number, tail_number],
                message: "one caravan head cannot be attached after another caravan tail",
            });
        }
    }

    // Only the first bounded non-skipped candidates compete for the front.
    // Everything intentionally omitted is named rather than looking completed.
    let mut selected = 0_usize;
    for number in analysis.fleet.unqueued.iter().copied() {
        let Some(candidate) = analysis.pull_requests.get(&number) else {
            continue;
        };
        let skipped_reason = if candidate.has_label("caravan-join-skipped") {
            Some("generation_bound_skip")
        } else if selected >= LEADING_CANDIDATE_COMPATIBILITY_BOUND {
            Some("leading_candidate_bound")
        } else {
            selected = selected.saturating_add(1);
            if candidate.cross_repository {
                Some("cross_repository")
            } else if candidate.draft {
                Some("draft")
            } else {
                None
            }
        };
        if let Some(reason) = skipped_reason {
            push_progress_sample(
                &mut progress.skipped_analyses,
                &mut progress.skipped_analyses_truncated,
                format!("candidate_to_default:pr#{number}:{reason}"),
            );
            continue;
        }
        tasks.push(CompatibilityAnalysisTask::AdmissionCandidate {
            description: format!("candidate_to_default:pr#{number}"),
            candidate: candidate.head.clone(),
            target: snapshot.default_branch.clone(),
            pr: number,
        });
    }
    tasks
}

fn analyze_for_actor_internal(
    snapshot: &RepositorySnapshot,
    checker: &impl CompatibilityChecker,
    actor: HeadMergeActor,
    analysis_deadline: Option<Instant>,
) -> Result<BoundedGraphAnalysis, AppError> {
    let mut analysis = derive_for_actor(snapshot, actor);
    let caravans = analysis.fleet.caravans.clone();
    let mut branches = vec![snapshot.default_branch.clone()];
    branches.extend(caravans.iter().flat_map(|caravan| {
        caravan.members.iter().filter_map(|number| {
            analysis
                .pull_requests
                .get(number)
                .map(|pull_request| pull_request.head.clone())
        })
    }));
    branches.sort_by(|left, right| (&left.name, &left.oid.0).cmp(&(&right.name, &right.oid.0)));
    branches.dedup_by(|left, right| left.name == right.name && left.oid == right.oid);

    let mut progress = CompatibilityAnalysisProgress {
        candidate_count: snapshot.pull_requests.len(),
        unqueued_candidate_count: analysis.fleet.unqueued.len(),
        caravan_count: caravans.len(),
        branch_count: branches.len(),
        ..CompatibilityAnalysisProgress::default()
    };
    let tasks = compatibility_tasks(snapshot, &analysis, &mut progress);
    progress.planned_analyses = tasks.len();

    if analysis_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        defer_tasks(&mut progress, &tasks);
        return Ok(BoundedGraphAnalysis { analysis, progress });
    }
    if let Err(error) = checker.prepare(&branches) {
        if analysis_deadline.is_some() && analysis_timed_out(&error) {
            defer_tasks(&mut progress, &tasks);
            return Ok(BoundedGraphAnalysis { analysis, progress });
        }
        return Err(error);
    }
    progress.revision_preparation_complete = true;

    for (index, task) in tasks.iter().enumerate() {
        if analysis_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            defer_tasks(&mut progress, &tasks[index..]);
            return Ok(BoundedGraphAnalysis { analysis, progress });
        }
        if let Err(error) = task.execute(&mut analysis, checker) {
            if analysis_deadline.is_some() && analysis_timed_out(&error) {
                defer_tasks(&mut progress, &tasks[index..]);
                return Ok(BoundedGraphAnalysis { analysis, progress });
            }
            return Err(error);
        }
        progress.completed_analyses = progress.completed_analyses.saturating_add(1);
    }
    progress.complete = true;
    Ok(BoundedGraphAnalysis { analysis, progress })
}

/// Derive and mechanically validate current chain/fleet invariants for one
/// configured head merge actor. Every proof is mandatory on mutation-capable
/// and exact-preflight paths.
pub fn analyze_for_actor(
    snapshot: &RepositorySnapshot,
    checker: &impl CompatibilityChecker,
    actor: HeadMergeActor,
) -> Result<GraphAnalysis, AppError> {
    Ok(analyze_for_actor_with_progress(snapshot, checker, actor)?.analysis)
}

/// Complete compatibility analysis plus diagnostic work counts.
///
/// Unlike the bounded status variant, a timeout remains an error. Exact
/// preflight and mutation-capable callers therefore retain all-or-nothing
/// compatibility semantics.
pub fn analyze_for_actor_with_progress(
    snapshot: &RepositorySnapshot,
    checker: &impl CompatibilityChecker,
    actor: HeadMergeActor,
) -> Result<BoundedGraphAnalysis, AppError> {
    analyze_for_actor_internal(snapshot, checker, actor, None)
}

/// Status-only compatibility analysis which yields at an absolute deadline.
///
/// The returned graph contains current provider and structural evidence plus
/// every proof completed before the deadline. Deferred proof names make the
/// receipt explicitly partial; callers must never consume it for mutation.
pub fn analyze_for_actor_bounded(
    snapshot: &RepositorySnapshot,
    checker: &impl CompatibilityChecker,
    actor: HeadMergeActor,
    analysis_deadline: Instant,
) -> Result<BoundedGraphAnalysis, AppError> {
    analyze_for_actor_internal(snapshot, checker, actor, Some(analysis_deadline))
}

/// How many leading unqueued candidates are proven against the default branch.
///
/// Electing a candidate that cannot merge starves every clean candidate behind
/// it, but proving every unqueued PR on every status would dominate provider
/// and Git latency. Bounding it keeps the front honest at fixed cost.
const LEADING_CANDIDATE_COMPATIBILITY_BOUND: usize = 3;

/// Record one compatibility report plus, for a non-clean attachment, the exact
/// squash-equivalence evidence for the same revisions.
///
/// The extra evidence is collected only on conflict, so healthy fleets never
/// pay for it, and it never changes the compatibility outcome: a conflict stays
/// a conflict until a reviewed operation acts on the proof.
/// Record an unqueued candidate's incompatibility as advisory evidence.
///
/// Never a decision point: the candidate is skipped by admission and the queue
/// advances past it. Squash-equivalence reconciliation is deliberately not
/// collected here, because nothing downstream may act on a candidate that has
/// not been admitted.
fn record_candidate_compatibility(
    analysis: &mut GraphAnalysis,
    report: CompatibilityReport,
    pr: PrNumber,
    message: &str,
) {
    if report.outcome != CompatibilityOutcome::Clean {
        analysis.fleet.problems.push(GraphProblem {
            kind: GraphProblemKind::CandidateIncompatible,
            prs: vec![pr],
            message: message.to_owned(),
        });
    }
    analysis.compatibility.push(report);
}

fn record_compatibility(
    analysis: &mut GraphAnalysis,
    checker: &impl CompatibilityChecker,
    report: CompatibilityReport,
    prs: Vec<PrNumber>,
    message: &str,
) -> Result<(), AppError> {
    if report.outcome != CompatibilityOutcome::Clean {
        analysis.fleet.problems.push(GraphProblem {
            kind: GraphProblemKind::Incompatible,
            prs,
            message: message.to_owned(),
        });
        if let Some(reconciliation) =
            checker.squash_equivalence(&report.candidate, &report.target)?
        {
            analysis.squash_reconciliations.push(reconciliation);
        }
    }
    analysis.compatibility.push(report);
    Ok(())
}

fn walk_linear(
    root: PrNumber,
    children: &BTreeMap<PrNumber, Vec<PrNumber>>,
    visited: &mut BTreeSet<PrNumber>,
) -> Vec<PrNumber> {
    let mut members = Vec::new();
    let mut current = root;
    loop {
        if !visited.insert(current) {
            break;
        }
        members.push(current);
        let Some(child_list) = children.get(&current) else {
            break;
        };
        if child_list.len() != 1 {
            break;
        }
        current = child_list[0];
    }
    members
}

fn detect_cycles(
    active: &BTreeMap<PrNumber, &PullRequestSnapshot>,
    parent_of: &BTreeMap<PrNumber, PrNumber>,
    problems: &mut Vec<GraphProblem>,
) {
    let mut recorded = BTreeSet::new();
    for start in active.keys().copied() {
        let mut path = Vec::new();
        let mut positions = BTreeMap::new();
        let mut current = start;
        loop {
            if let Some(position) = positions.get(&current).copied() {
                let mut cycle = path[position..].to_vec();
                cycle.sort_unstable();
                if recorded.insert(cycle.clone()) {
                    problems.push(GraphProblem {
                        kind: GraphProblemKind::Cycle,
                        prs: cycle,
                        message: "active caravan base graph contains a cycle".to_owned(),
                    });
                }
                break;
            }
            positions.insert(current, path.len());
            path.push(current);
            let Some(parent) = parent_of.get(&current).copied() else {
                break;
            };
            current = parent;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::model::{AutoMergeState, CheckSnapshot, CommitOid, PullRequestState, RepositoryId};
    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        }
    }

    fn branch(name: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(format!("{name:0<40}")),
        }
    }

    fn pull_request(number: u64, head: &str, base: &str) -> PullRequestSnapshot {
        PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: format!("https://example.invalid/{number}"),
            state: PullRequestState::Open,
            draft: false,
            head: branch(head),
            base: branch(base),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            // Absent configuration keeps the historical provider-native actor.
            auto_merge: if base == "main" {
                AutoMergeState::squash()
            } else {
                AutoMergeState::disabled()
            },
            checks: Vec::<CheckSnapshot>::new(),
            created_at: Some(format!("2026-01-01T00:00:{number:02}Z")),
            merged_at: None,
            updated_at: None,
        }
    }

    /// Live loss (cacophony #2287, bd-461c8b): the sole member of a caravan was
    /// closed after being republished as a new generation, and the caravan
    /// ceased to exist with NOTHING reported. It was found only by noticing that
    /// no open pull request carried the `caravan` label any more.
    ///
    /// Asserts BOTH shapes, because they fail differently and only one of them
    /// was ever detected:
    ///   - a MIDDLE member leaves a tail behind, which dangles, so the fleet
    ///     already stopped on `DanglingBase`;
    ///   - a SOLE member leaves nothing behind, so there was no dangling base,
    ///     no problem, and no caravan.
    #[test]
    fn a_member_closed_without_merging_is_reported_not_silently_absent() {
        let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Clean,
                conflicting_paths: Vec::new(),
                diagnostic: None,
            })
        };
        let dissolved = |analysis: &GraphAnalysis| -> Vec<u64> {
            analysis
                .fleet
                .problems
                .iter()
                .filter(|problem| problem.kind == GraphProblemKind::DissolvedMember)
                .flat_map(|problem| problem.prs.iter().map(|pr| pr.0))
                .collect()
        };

        // The shape that actually happened: one member, closed, no tail.
        let sole = {
            let mut pr = pull_request(2287, "member", "main");
            pr.state = PullRequestState::Closed;
            pr
        };
        let analysis = analyze(&snapshot(vec![sole]), &checker).expect("analysis runs");
        assert_eq!(
            dissolved(&analysis),
            vec![2287],
            "a sole closed member leaves nothing to dangle, so the dissolution must be reported directly"
        );
        // Reported, NOT gated. Wiring the detection to real discovery surfaced
        // three historical dissolutions at once; blocking on them turned the
        // live fleet unhealthy with no action available that would ever clear
        // it, because a closed pull request never changes (bd-61024a).
        assert!(
            analysis.healthy(),
            "a dissolution already happened: there is no chain left to protect, so it must not wedge the fleet"
        );

        // The shape with a survivor: still reported, and still dangling.
        let head = pull_request(1, "a", "main");
        let middle = {
            let mut pr = pull_request(2, "b", "a");
            pr.state = PullRequestState::Closed;
            pr
        };
        let tail = pull_request(3, "c", "b");
        let analysis =
            analyze(&snapshot(vec![head, middle, tail]), &checker).expect("analysis runs");
        assert_eq!(dissolved(&analysis), vec![2]);
        assert!(
            analysis
                .fleet
                .problems
                .iter()
                .any(|problem| problem.kind == GraphProblemKind::DanglingBase),
            "the orphaned tail must still dangle: that is what stops the fleet"
        );
    }

    fn snapshot(pull_requests: Vec<PullRequestSnapshot>) -> RepositorySnapshot {
        RepositorySnapshot {
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            repository: repository(),
            default_branch: branch("main"),
            current_branch: None,
            current_pr: None,
            pull_requests,
            generation_facts: Vec::new(),
            observed_at: None,
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn clean(
        candidate: &BranchSnapshot,
        target: &BranchSnapshot,
    ) -> Result<CompatibilityReport, AppError> {
        Ok(CompatibilityReport {
            candidate: candidate.clone(),
            target: target.clone(),
            outcome: CompatibilityOutcome::Clean,
            conflicting_paths: Vec::new(),
            diagnostic: None,
        })
    }

    /// Run a git command in a fixture repository.
    ///
    /// Hoisted out of the test that used it: as a nested item its body counted
    /// toward that function's length and pushed it past the 100-line lint, which
    /// landed on main red under `--all-targets`.
    fn git(repository: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(repository)
            .args(args)
            .output()
            .expect("fixture git command");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git output is utf-8")
            .trim()
            .to_owned()
    }

    /// bd-750d37: consecutive rediscoveries construct fresh checker instances.
    /// Exact prepared revisions survive that boundary, but never cross a local
    /// repository/remote identity and never outlive the bounded preparation TTL.
    #[test]
    fn compatibility_preparation_is_shared_by_exact_identity_and_expires() {
        let fixture = tempfile::tempdir().expect("fixture repository");
        git(
            fixture.path(),
            &["init", "--quiet", "--initial-branch=main"],
        );
        git(fixture.path(), &["config", "user.name", "Caravan Test"]);
        git(
            fixture.path(),
            &["config", "user.email", "caravan@example.invalid"],
        );
        std::fs::write(fixture.path().join("file"), "prepared\n").unwrap();
        git(fixture.path(), &["add", "file"]);
        git(fixture.path(), &["commit", "--quiet", "-m", "prepared"]);
        let oid = git(fixture.path(), &["rev-parse", "HEAD"]);
        let repository_path = fixture.path().to_string_lossy().into_owned();
        git(
            fixture.path(),
            &["remote", "add", "fixture", &repository_path],
        );
        let snapshot = BranchSnapshot {
            repository: repository(),
            name: "main".to_owned(),
            oid: CommitOid(oid),
        };

        let first = GitCompatibilityChecker::new(fixture.path(), "fixture");
        first
            .prepare(std::slice::from_ref(&snapshot))
            .expect("first instance prepares from the remote");
        git(fixture.path(), &["remote", "remove", "fixture"]);

        let second = GitCompatibilityChecker::new(fixture.path(), "fixture");
        second
            .prepare(std::slice::from_ref(&snapshot))
            .expect("fresh checker reuses the exact process-wide preparation");

        let wrong_remote = GitCompatibilityChecker::new(fixture.path(), "other");
        assert!(
            wrong_remote
                .prepare(std::slice::from_ref(&snapshot))
                .is_err(),
            "the remote is part of cache identity"
        );
        let mut wrong_provider = snapshot.clone();
        wrong_provider.repository.name = "another-repository".to_owned();
        assert!(
            GitCompatibilityChecker::new(fixture.path(), "fixture")
                .prepare(&[wrong_provider])
                .is_err(),
            "the provider repository is part of cache identity"
        );
        let mut wrong_branch = snapshot.clone();
        wrong_branch.name = "other-branch".to_owned();
        assert!(
            GitCompatibilityChecker::new(fixture.path(), "fixture")
                .prepare(&[wrong_branch])
                .is_err(),
            "the branch is part of cache identity"
        );
        let mut wrong_oid = snapshot.clone();
        wrong_oid.oid = CommitOid("a".repeat(40));
        assert!(
            GitCompatibilityChecker::new(fixture.path(), "fixture")
                .prepare(&[wrong_oid])
                .is_err(),
            "the exact OID is part of cache identity"
        );

        let other_repository = tempfile::tempdir().expect("other repository");
        git(
            other_repository.path(),
            &["init", "--quiet", "--initial-branch=main"],
        );
        let wrong_repository = GitCompatibilityChecker::new(other_repository.path(), "fixture");
        assert!(
            wrong_repository
                .prepare(std::slice::from_ref(&snapshot))
                .is_err(),
            "the local object database is part of cache identity"
        );

        let key = first.shared_key(&snapshot);
        SHARED_PREPARED_REVISIONS
            .get()
            .expect("cache initialized")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&key)
            .expect("exact cache entry")
            .inserted = Instant::now()
            .checked_sub(SHARED_PREPARED_REVISION_MAX_AGE + Duration::from_secs(1))
            .expect("TTL fits in monotonic clock");
        let expired = GitCompatibilityChecker::new(fixture.path(), "fixture");
        assert!(
            expired.prepare(&[snapshot]).is_err(),
            "an expired preparation must refetch instead of claiming a stale local fact"
        );
    }

    #[test]
    fn compatibility_prepares_unique_branches_once_then_checks_only_reports() {
        struct CountingChecker {
            prepared: std::cell::RefCell<Vec<BranchSnapshot>>,
            reports: std::cell::Cell<usize>,
        }
        impl CompatibilityChecker for CountingChecker {
            fn prepare(&self, branches: &[BranchSnapshot]) -> Result<(), AppError> {
                *self.prepared.borrow_mut() = branches.to_vec();
                Ok(())
            }

            fn check(
                &self,
                candidate: &BranchSnapshot,
                target: &BranchSnapshot,
            ) -> Result<CompatibilityReport, AppError> {
                self.reports.set(self.reports.get() + 1);
                clean(candidate, target)
            }
        }
        let checker = CountingChecker {
            prepared: std::cell::RefCell::new(Vec::new()),
            reports: std::cell::Cell::new(0),
        };
        analyze(
            &snapshot(vec![
                pull_request(1, "one", "main"),
                pull_request(2, "two", "one"),
                pull_request(3, "three", "two"),
            ]),
            &checker,
        )
        .unwrap();

        let prepared = checker.prepared.borrow();
        assert_eq!(prepared.len(), 4, "default plus three unique active heads");
        assert_eq!(
            checker.reports.get(),
            3,
            "head/default plus two adjacent reports"
        );
    }

    #[test]
    fn derives_multiple_linear_caravans_in_head_order() {
        let analysis = analyze(
            &snapshot(vec![
                pull_request(2, "two", "one"),
                pull_request(3, "three", "main"),
                pull_request(1, "one", "main"),
            ]),
            &clean,
        )
        .unwrap();
        assert!(analysis.healthy());
        assert_eq!(
            analysis.fleet.caravans,
            vec![
                Caravan::new(vec![PrNumber(1), PrNumber(2)]).unwrap(),
                Caravan::new(vec![PrNumber(3)]).unwrap(),
            ]
        );
        // Head/default (2), adjacent (1), and both ordered cross-caravan pairs.
        assert_eq!(analysis.compatibility.len(), 5);
    }

    #[test]
    fn conflicting_attachment_records_squash_equivalence_evidence_only_on_conflict() {
        struct ConflictingChecker {
            queried: std::cell::RefCell<Vec<(String, String)>>,
        }
        impl CompatibilityChecker for ConflictingChecker {
            fn check(
                &self,
                candidate: &BranchSnapshot,
                target: &BranchSnapshot,
            ) -> Result<CompatibilityReport, AppError> {
                if candidate.name == "two" && target.name == "one" {
                    return Ok(CompatibilityReport {
                        candidate: candidate.clone(),
                        target: target.clone(),
                        outcome: CompatibilityOutcome::Conflict,
                        conflicting_paths: vec!["app.rs".to_owned()],
                        diagnostic: None,
                    });
                }
                clean(candidate, target)
            }

            fn squash_equivalence(
                &self,
                candidate: &BranchSnapshot,
                target: &BranchSnapshot,
            ) -> Result<Option<SquashEquivalenceReport>, AppError> {
                self.queried
                    .borrow_mut()
                    .push((candidate.name.clone(), target.name.clone()));
                Ok(Some(SquashEquivalenceReport {
                    schema_version: 1,
                    candidate: candidate.clone(),
                    target: target.clone(),
                    candidate_oid: candidate.oid.clone(),
                    target_oid: target.oid.clone(),
                    merge_base: None,
                    outcome: crate::squash_equivalence::SquashEquivalenceOutcome::NoEquivalence,
                    before: None,
                    after: None,
                    proven_boundary: None,
                    boundary_tree: None,
                    target_tree: None,
                    commits: Vec::new(),
                    represented_paths: Vec::new(),
                    represented_paths_truncated: false,
                    candidate_commit_count: 0,
                    analyzed_prefix_complete: true,
                    evaluated_boundaries: 0,
                    evaluation_bounded: false,
                    reason: "fixture".to_owned(),
                    policy: crate::squash_equivalence::SQUASH_EQUIVALENCE_POLICY.to_owned(),
                }))
            }
        }

        let checker = ConflictingChecker {
            queried: std::cell::RefCell::new(Vec::new()),
        };
        let analysis = analyze(
            &snapshot(vec![
                pull_request(1, "one", "main"),
                pull_request(2, "two", "one"),
            ]),
            &checker,
        )
        .unwrap();

        assert!(!analysis.healthy());
        assert_eq!(
            checker.queried.borrow().as_slice(),
            [("two".to_owned(), "one".to_owned())],
            "evidence is collected exactly once, only for the conflicting pair"
        );
        assert_eq!(analysis.squash_reconciliations.len(), 1);
        let evidence = &analysis.squash_reconciliations[0];
        assert_eq!(evidence.candidate.name, "two");
        assert_eq!(evidence.target.name, "one");
        assert!(evidence.authorized_range_base().is_none());
    }

    #[test]
    fn healthy_fleet_collects_no_squash_equivalence_evidence() {
        let analysis = analyze(
            &snapshot(vec![
                pull_request(1, "one", "main"),
                pull_request(2, "two", "one"),
            ]),
            &clean,
        )
        .unwrap();

        assert!(analysis.healthy());
        assert!(analysis.squash_reconciliations.is_empty());
    }

    #[test]
    fn detects_branching_cycles_and_active_evicted_labels() {
        let mut first = pull_request(1, "one", "main");
        first.labels.insert("caravan-evicted".to_owned());
        let analysis = derive(&snapshot(vec![
            first,
            pull_request(2, "two", "three"),
            pull_request(3, "three", "two"),
            pull_request(4, "four", "parent"),
            pull_request(5, "five", "parent"),
            pull_request(6, "parent", "main"),
        ]));
        let kinds: BTreeSet<_> = analysis
            .fleet
            .problems
            .iter()
            .map(|problem| problem.kind)
            .collect();
        assert!(kinds.contains(&GraphProblemKind::ActiveAndEvicted));
        assert!(kinds.contains(&GraphProblemKind::Cycle));
        assert!(kinds.contains(&GraphProblemKind::Branching));
    }

    #[test]
    fn merged_predecessor_is_a_pending_head_advancement_problem() {
        let mut merged = pull_request(9, "old-head", "main");
        merged.state = PullRequestState::Merged;
        merged.merged_at = Some("2026-07-17T00:00:00Z".to_owned());
        let analysis = derive(&snapshot(vec![
            merged,
            pull_request(10, "next", "old-head"),
        ]));
        assert_eq!(analysis.fleet.caravans[0].id, PrNumber(10));
        assert!(analysis.fleet.problems.iter().any(|problem| {
            problem.kind == GraphProblemKind::DanglingBase
                && problem.prs == vec![PrNumber(10), PrNumber(9)]
        }));
    }

    #[test]
    fn unqueued_excludes_drafts_and_evicted_prs() {
        let mut ready = pull_request(1, "ready", "main");
        ready.labels.clear();
        let mut draft = pull_request(2, "draft", "main");
        draft.labels.clear();
        draft.draft = true;
        let mut evicted = pull_request(3, "evicted", "main");
        evicted.labels = BTreeSet::from(["caravan-evicted".to_owned()]);
        let analysis = derive(&snapshot(vec![ready, draft, evicted]));
        assert_eq!(analysis.fleet.unqueued, vec![PrNumber(1)]);
    }

    /// Live regression (bd-d897cc): the first caravan on Cacophony dissolved
    /// when its member was republished as a new generation — the author closed
    /// the member without merging and opened a successor on a new branch.
    /// Membership is recomputed from labels on every read, so the departure
    /// left no trace, and any surviving child reported only that it targeted an
    /// anonymous "non-member branch". A stranded tail must name the member that
    /// left, or the operator cannot tell a republished chain apart from a PR
    /// pointed at a branch that never belonged to a caravan.
    #[test]
    fn a_child_stranded_by_a_closed_member_names_the_member_that_left() {
        let mut departed = pull_request(2287, "first-caravan", "main");
        departed.state = PullRequestState::Closed;
        let analysis = derive(&snapshot(vec![
            departed,
            pull_request(2288, "stacked-on-it", "first-caravan"),
        ]));

        let problem = analysis
            .fleet
            .problems
            .iter()
            .find(|problem| problem.kind == GraphProblemKind::DanglingBase)
            .expect("a child of a departed member is a dangling base");
        assert_eq!(problem.prs, vec![PrNumber(2288), PrNumber(2287)]);
        assert!(
            problem.message.contains("#2287"),
            "the departed member must be named, got: {}",
            problem.message
        );
        assert!(
            problem.message.contains("closed without merging"),
            "the reason for the strand must be stated, got: {}",
            problem.message
        );
    }

    /// A base branch that never belonged to a caravan stays anonymous: only a
    /// departed *member* earns the named diagnosis above.
    #[test]
    fn a_child_of_an_unrelated_branch_is_still_an_anonymous_dangling_base() {
        let analysis = derive(&snapshot(vec![pull_request(11, "next", "someones-branch")]));
        let problem = analysis
            .fleet
            .problems
            .iter()
            .find(|problem| problem.kind == GraphProblemKind::DanglingBase)
            .expect("an unrelated base is still dangling");
        assert_eq!(problem.prs, vec![PrNumber(11)]);
        assert!(problem.message.contains("non-member branch"));
    }

    #[test]
    fn merged_force_head_leaves_no_active_caravan_or_auto_merge_problem() {
        // Post-merge recovery case: the provider already merged a force-labelled
        // head that carried a long check history. Even if durable checkpoint
        // publication failed, fresh discovery must retire it from active
        // topology and never demand auto-merge repair on a merged PR.
        let mut merged = pull_request(2101, "tui-remote-pane", "main");
        merged.state = PullRequestState::Merged;
        merged.auto_merge = AutoMergeState::disabled();
        merged.merged_at = Some("2026-07-25T01:07:18Z".to_owned());
        merged.labels = BTreeSet::from(["caravan".to_owned(), "caravan-force".to_owned()]);
        merged.checks = (0..500)
            .map(|index| CheckSnapshot {
                name: format!("check-{index}"),
                state: crate::model::CheckState::Success,
                provider_state: Some("SUCCESS".to_owned()),
                details_url: Some(format!("https://example.invalid/runs/{index}")),
                ..crate::model::CheckSnapshot::default()
            })
            .collect();

        let analysis = derive(&snapshot(vec![merged.clone()]));

        assert!(analysis.fleet.caravans.is_empty());
        assert!(analysis.fleet.unqueued.is_empty());
        assert!(analysis.fleet.problems.is_empty());
        assert!(analysis.healthy());
        // The merge receipt stays observable for evidence and dashboards.
        let observed = &analysis.pull_requests[&PrNumber(2101)];
        assert_eq!(observed.state, PullRequestState::Merged);
        assert_eq!(observed.merged_at.as_deref(), Some("2026-07-25T01:07:18Z"));
    }

    /// Live regression: recording an unqueued candidate conflict as an ordinary
    /// `Incompatible` problem made `decision_for_problem` classify it as a
    /// `HeadConflict` — the fleet-blocking decision meant for a caravan member —
    /// so one conflicting candidate aborted the entire sync tick before any
    /// caravan was touched.
    #[test]
    fn a_candidate_conflict_is_never_a_fleet_blocking_problem() {
        let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/lib.rs".to_owned()],
                diagnostic: None,
            })
        };
        let mut unqueued = pull_request(2234, "stuck", "main");
        unqueued.labels.clear();

        let analysis = analyze(&snapshot(vec![unqueued]), &checker).expect("advisory, not fatal");

        assert!(
            !analysis
                .fleet
                .problems
                .iter()
                .any(|problem| problem.kind == GraphProblemKind::Incompatible),
            "an unadmitted candidate must never produce a fleet-blocking Incompatible: {:?}",
            analysis.fleet.problems
        );
    }

    /// The kind emitted for an unadmitted candidate conflict is a WIRE CONTRACT
    /// between graph analysis and everything that reads it. `cara queue
    /// --status conflict` silently returned nothing for weeks because it
    /// compared against `Incompatible`, the variant the producer had stopped
    /// emitting; unit tests missed it because they injected the kind by hand
    /// instead of asking the real analysis what it produces.
    ///
    /// This asserts the producer's actual output, so any consumer can be checked
    /// against a fact rather than an assumption.
    #[test]
    fn an_unadmitted_candidate_conflict_is_emitted_as_candidate_scoped() {
        let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/lib.rs".to_owned()],
                diagnostic: None,
            })
        };
        let mut unqueued = pull_request(2245, "stuck", "main");
        unqueued.labels.clear();

        let analysis = analyze(&snapshot(vec![unqueued]), &checker).expect("advisory evidence");

        let emitted: Vec<_> = analysis
            .fleet
            .problems
            .iter()
            .filter(|problem| problem.prs.contains(&PrNumber(2245)))
            .map(|problem| problem.kind)
            .collect();

        assert_eq!(
            emitted,
            vec![GraphProblemKind::CandidateIncompatible],
            "consumers filter on this exact kind; changing it silently breaks them"
        );
        assert!(
            emitted.iter().all(|kind| kind.is_candidate_scoped()),
            "and it must classify as candidate-scoped, or it blocks the fleet"
        );
    }

    /// Live operator report: `caravan-join-skipped` was added to a conflicting
    /// PR and Cara kept naming it, so the label read as if it had done nothing.
    /// Admission had excluded it correctly all along; this detection loop was
    /// re-proving and re-reporting the operator's own decision every tick.
    #[test]
    fn an_explicitly_skipped_candidate_is_not_reproved_or_reported() {
        let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/lib.rs".to_owned()],
                diagnostic: None,
            })
        };
        let mut skipped = pull_request(2234, "stuck", "main");
        skipped.labels.clear();
        skipped.labels.insert("caravan-join-skipped".to_owned());

        let analysis = analyze(&snapshot(vec![skipped]), &checker)
            .expect("a skipped candidate is not an execution failure");

        assert!(
            !analysis
                .fleet
                .problems
                .iter()
                .any(|problem| problem.prs.contains(&PrNumber(2234))),
            "an explicitly skipped PR must not be re-reported: {:?}",
            analysis.fleet.problems
        );
    }

    /// bd-e9fcd7: a leading candidate that cannot merge into the default branch
    /// is recorded as a compatibility problem, so admission excludes it instead
    /// of electing it and starving every clean candidate behind it.
    #[test]
    fn a_conflicting_leading_candidate_is_recorded_not_elected() {
        let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/lib.rs".to_owned()],
                diagnostic: None,
            })
        };
        let mut unqueued = pull_request(2117, "stuck", "main");
        unqueued.labels.clear();

        let analysis = analyze(&snapshot(vec![unqueued]), &checker)
            .expect("a conflicting candidate is evidence, not an execution failure");

        assert!(
            analysis.fleet.problems.iter().any(|problem| {
                problem.kind == GraphProblemKind::CandidateIncompatible
                    && problem.prs.contains(&PrNumber(2117))
            }),
            "expected the leading candidate conflict to be recorded as candidate-scoped, never as a fleet-blocking Incompatible: {:?}",
            analysis.fleet.problems
        );
    }

    /// bd-1cf2e1: a merged member's branch name reused by an unrelated active
    /// PR must not silently become the child's parent.
    #[test]
    fn reused_branch_name_fails_closed_instead_of_reparenting() {
        let mut merged_parent = pull_request(1, "shared", "main");
        merged_parent.state = PullRequestState::Merged;
        // The child still records the merged parent's exact head OID.
        let child = pull_request(2, "child", "shared");
        // An unrelated active PR has since taken the same branch name, with a
        // different head OID.
        let mut reuser = pull_request(3, "shared", "main");
        reuser.head.oid = CommitOid("reused-head".to_owned());

        let analysis = derive(&snapshot(vec![merged_parent, child, reuser]));

        assert!(
            analysis.fleet.problems.iter().any(|problem| {
                problem.kind == GraphProblemKind::ReusedBranchProvenance
                    && problem.prs.contains(&PrNumber(2))
            }),
            "expected a reused-branch provenance problem: {:?}",
            analysis.fleet.problems
        );
        assert!(!analysis.healthy());
    }

    #[test]
    fn enforces_auto_merge_only_on_the_head() {
        let mut non_head = pull_request(2, "two", "one");
        non_head.auto_merge = AutoMergeState::squash();
        let analysis = derive(&snapshot(vec![pull_request(1, "one", "main"), non_head]));
        assert!(analysis.fleet.problems.iter().any(|problem| {
            problem.kind == GraphProblemKind::AutoMergeInvariant && problem.prs == vec![PrNumber(2)]
        }));
    }

    #[test]
    fn the_auto_merge_invariant_is_gated_on_the_configured_merge_actor() {
        // Absent configuration keeps the historical actor, so `derive` itself
        // still requires the root to be armed.
        assert_eq!(HeadMergeActor::default(), HeadMergeActor::Github);
        // A repository that deliberately disabled provider-native auto-merge so
        // cara can own the merge must not report a permanently unsatisfiable
        // "head must have squash auto-merge enabled" problem. Under the default
        // caravan-owned actor the invariant inverts instead: an armed root is a
        // second merge actor and is the problem.
        let mut disarmed_root = pull_request(1, "one", "main");
        disarmed_root.auto_merge = AutoMergeState::disabled();
        let disarmed = snapshot(vec![disarmed_root, pull_request(2, "two", "one")]);
        assert!(
            derive_for_actor(&disarmed, HeadMergeActor::Caravan)
                .fleet
                .problems
                .is_empty(),
            "a disarmed caravan is healthy when cara owns the merge"
        );
        assert!(
            derive_for_actor(&disarmed, HeadMergeActor::Github)
                .fleet
                .problems
                .iter()
                .any(
                    |problem| problem.kind == GraphProblemKind::AutoMergeInvariant
                        && problem.prs == vec![PrNumber(1)]
                ),
            "native delegation still requires the root to be armed"
        );

        let armed = snapshot(vec![
            pull_request(1, "one", "main"),
            pull_request(2, "two", "one"),
        ]);
        assert!(
            derive_for_actor(&armed, HeadMergeActor::Github)
                .fleet
                .problems
                .is_empty(),
            "native delegation is satisfied by an armed root"
        );
        let caravan_problem = derive_for_actor(&armed, HeadMergeActor::Caravan)
            .fleet
            .problems
            .iter()
            .find(|problem| problem.kind == GraphProblemKind::AutoMergeInvariant)
            .cloned()
            .expect("an armed root is a second merge actor");
        assert_eq!(caravan_problem.prs, vec![PrNumber(1)]);
        assert!(
            caravan_problem.message.contains("single merge actor"),
            "{}",
            caravan_problem.message
        );
    }

    #[test]
    fn records_conflicts_as_graph_problems() {
        let checker = |candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/lib.rs".to_owned()],
                diagnostic: None,
            })
        };
        let analysis = analyze(&snapshot(vec![pull_request(1, "one", "main")]), &checker)
            .expect("conflict is evidence, not execution failure");
        assert!(!analysis.healthy());
        assert!(
            analysis
                .fleet
                .problems
                .iter()
                .any(|problem| problem.kind == GraphProblemKind::Incompatible)
        );
    }

    /// bd-8c9916: an empty live list was repeatedly read as "no caravan has
    /// ever formed" on a repository that had merged members for eleven days.
    /// The live field was always right, so re-measuring never exposed the
    /// error; only carrying the historical answer beside it can.
    #[test]
    fn an_empty_live_fleet_still_proves_prior_formation() {
        let mut merged = pull_request(2008, "landed", "main");
        merged.state = PullRequestState::Merged;
        merged.merged_at = Some("2026-07-18T01:38:58Z".to_owned());
        let mut later = pull_request(2234, "landed-later", "main");
        later.state = PullRequestState::Merged;
        later.merged_at = Some("2026-07-28T17:27:45Z".to_owned());

        let analysis = derive(&snapshot(vec![merged, later]));

        assert!(
            analysis.fleet.caravans.is_empty(),
            "nothing is in flight right now"
        );
        let history = &analysis.fleet.history;
        assert!(history.has_formed_before());
        assert_eq!(history.merged_members_observed, 2);
        assert_eq!(
            history.earliest_merged_at.as_deref(),
            Some("2026-07-18T01:38:58Z")
        );
        assert_eq!(
            history.latest_merged_at.as_deref(),
            Some("2026-07-28T17:27:45Z")
        );
    }

    /// bd-47f0c7: labels were stripped from merged pull requests as an urgent
    /// workaround, GitHub's label index still returned the rows, and history
    /// collapsed from 24 to 0. A bare zero renders "never merged" and "evidence
    /// removed" identically, which is the exact misreading history exists to
    /// prevent.
    #[test]
    fn stripped_labels_make_history_unproven_rather_than_empty() {
        let mut stripped = pull_request(2200, "landed", "main");
        stripped.state = PullRequestState::Merged;
        stripped.merged_at = Some("2026-07-30T11:05:54Z".to_owned());
        stripped.labels.clear();

        let analysis = derive(&snapshot(vec![stripped]));
        let history = &analysis.fleet.history;

        assert_eq!(history.merged_members_observed, 0);
        assert_eq!(history.unlabelled_merged_rows, 1);
        assert!(!history.has_formed_before());
        assert!(
            history.evidence_may_be_stripped(),
            "an unproven history must never render as a proven absence"
        );
    }

    #[test]
    fn a_repository_without_merged_members_claims_no_history() {
        let analysis = derive(&snapshot(vec![pull_request(1, "open", "main")]));

        assert!(!analysis.fleet.history.has_formed_before());
        assert_eq!(analysis.fleet.history.merged_members_observed, 0);
        assert!(analysis.fleet.history.latest_merged_at.is_none());
    }
}
