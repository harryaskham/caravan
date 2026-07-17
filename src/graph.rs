//! Pure graph derivation plus injectable mechanical compatibility validation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::AppError;
use crate::compatibility;
use crate::model::{
    BranchSnapshot, Caravan, CaravanFleet, CompatibilityOutcome, CompatibilityReport, GraphProblem,
    GraphProblemKind, MergeMethod, PrNumber, PullRequestSnapshot, PullRequestState,
    RepositorySnapshot,
};

/// Full read-only analysis shared by status, show, check, CLI JSON, and MCP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GraphAnalysis {
    pub fleet: CaravanFleet,
    /// Canonical PR facts keyed by PR number for rendering and follow-up checks.
    pub pull_requests: BTreeMap<PrNumber, PullRequestSnapshot>,
    /// Exact compatibility evidence collected while validating current chains.
    #[serde(default)]
    pub compatibility: Vec<CompatibilityReport>,
}

impl GraphAnalysis {
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.fleet.problems.is_empty()
    }
}

/// Injectable compatibility seam. Production uses worktree-free Git; tests can
/// focus on graph policy without subprocesses.
pub trait CompatibilityChecker {
    fn check(
        &self,
        candidate: &BranchSnapshot,
        target: &BranchSnapshot,
    ) -> Result<CompatibilityReport, AppError>;
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

/// Production compatibility checker for one repository and remote.
#[derive(Debug, Clone)]
pub struct GitCompatibilityChecker {
    repository: PathBuf,
    remote: String,
    timeout: Duration,
}

impl GitCompatibilityChecker {
    #[must_use]
    pub fn new(repository: impl AsRef<Path>, remote: impl Into<String>) -> Self {
        Self {
            repository: repository.as_ref().to_path_buf(),
            remote: remote.into(),
            timeout: crate::command::DEFAULT_COMMAND_TIMEOUT,
        }
    }

    /// Override the hard deadline for each exact-revision Git subprocess.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl CompatibilityChecker for GitCompatibilityChecker {
    fn check(
        &self,
        candidate: &BranchSnapshot,
        target: &BranchSnapshot,
    ) -> Result<CompatibilityReport, AppError> {
        compatibility::check_compatibility_with_timeout(
            &self.repository,
            &self.remote,
            candidate,
            target,
            self.timeout,
        )
    }
}

/// Derive structural chains without executing Git compatibility checks.
///
/// The phases intentionally stay together so one pass owns all intermediate
/// parent/child maps and cannot expose a partially-derived graph.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn derive(snapshot: &RepositorySnapshot) -> GraphAnalysis {
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
    for pull_request in pull_requests.values() {
        if pull_request.state == PullRequestState::Merged && pull_request.has_label("caravan") {
            merged_by_head.insert(pull_request.head.name.clone(), pull_request.number);
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
                problems.push(GraphProblem {
                    kind: GraphProblemKind::DanglingBase,
                    prs: std::iter::once(pull_request.number)
                        .chain(merged)
                        .collect(),
                    message: merged.map_or_else(
                        || {
                            format!(
                                "PR #{} targets non-member branch `{}`",
                                pull_request.number, pull_request.base.name
                            )
                        },
                        |merged| {
                            format!(
                                "PR #{} still targets merged predecessor #{}; sync must advance it to `{}`",
                                pull_request.number, merged, snapshot.default_branch.name
                            )
                        },
                    ),
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
    for caravan in &caravans {
        for (position, number) in caravan.members.iter().enumerate() {
            let pull_request = active
                .get(number)
                .expect("derived caravan members are active");
            let valid = if position == 0 {
                pull_request.auto_merge.enabled
                    && pull_request.auto_merge.merge_method == Some(MergeMethod::Squash)
            } else {
                !pull_request.auto_merge.enabled
            };
            if !valid {
                problems.push(GraphProblem {
                    kind: GraphProblemKind::AutoMergeInvariant,
                    prs: vec![*number],
                    message: if position == 0 {
                        format!("caravan head #{number} must have squash auto-merge enabled")
                    } else {
                        format!("non-head caravan PR #{number} must have auto-merge disabled")
                    },
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

    GraphAnalysis {
        fleet: CaravanFleet {
            repository: snapshot.repository.clone(),
            default_branch: snapshot.default_branch.clone(),
            caravans,
            unqueued,
            problems,
        },
        pull_requests,
        compatibility: Vec::new(),
    }
}

/// Derive and mechanically validate current chain/fleet invariants.
pub fn analyze(
    snapshot: &RepositorySnapshot,
    checker: &impl CompatibilityChecker,
) -> Result<GraphAnalysis, AppError> {
    let mut analysis = derive(snapshot);
    let caravans = analysis.fleet.caravans.clone();

    for caravan in &caravans {
        let Some(head_number) = caravan.head() else {
            continue;
        };
        let head_branch = analysis
            .pull_requests
            .get(&head_number)
            .expect("derived head")
            .head
            .clone();
        let report = checker.check(&head_branch, &snapshot.default_branch)?;
        record_compatibility(
            &mut analysis,
            report,
            vec![head_number],
            "caravan head does not merge cleanly into the current default branch",
        );
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
            let report = checker.check(&child_branch, &parent_branch)?;
            record_compatibility(
                &mut analysis,
                report,
                vec![pair[0], pair[1]],
                "adjacent caravan PRs are not mechanically compatible",
            );
        }
    }

    for (index, caravan) in caravans.iter().enumerate() {
        for (other_index, other) in caravans.iter().enumerate() {
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
            let report = checker.check(&head_branch, &tail_branch)?;
            record_compatibility(
                &mut analysis,
                report,
                vec![head_number, tail_number],
                "one caravan head cannot be attached after another caravan tail",
            );
        }
    }

    Ok(analysis)
}

fn record_compatibility(
    analysis: &mut GraphAnalysis,
    report: CompatibilityReport,
    prs: Vec<PrNumber>,
    message: &str,
) {
    if report.outcome != CompatibilityOutcome::Clean {
        analysis.fleet.problems.push(GraphProblem {
            kind: GraphProblemKind::Incompatible,
            prs,
            message: message.to_owned(),
        });
    }
    analysis.compatibility.push(report);
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
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: format!("https://example.invalid/{number}"),
            state: PullRequestState::Open,
            draft: false,
            head: branch(head),
            base: branch(base),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge: if base == "main" {
                AutoMergeState::squash()
            } else {
                AutoMergeState::disabled()
            },
            checks: Vec::<CheckSnapshot>::new(),
            merged_at: None,
            updated_at: None,
        }
    }

    fn snapshot(pull_requests: Vec<PullRequestSnapshot>) -> RepositorySnapshot {
        RepositorySnapshot {
            repository: repository(),
            default_branch: branch("main"),
            current_branch: None,
            current_pr: None,
            pull_requests,
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
}
