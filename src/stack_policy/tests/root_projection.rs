//! bd-2d125b: root readiness uses the live Stack base, not the PR projection.
use super::*;
use crate::github::plan_github_stack_ready_prefix;

fn fixture() -> (
    GitHubStackGeneration,
    BTreeMap<PrNumber, PullRequestSnapshot>,
    BTreeMap<PrNumber, MergeCandidateIdentity>,
) {
    let mut stack = stack();
    stack.topology.entries[0].base.oid = CommitOid("historical-main".to_owned());
    let pulls = pull_requests(&stack);
    let mut candidates = merge_candidates(&stack);
    let root = candidates.get_mut(&PrNumber(101)).unwrap();
    root.compared_base = Some(stack.topology.base.clone());
    root.synthetic.as_mut().unwrap().parents[0] = stack.topology.base.oid.clone();
    root.freshness = MergeCandidateFreshness::StaleBase;
    root.stale_base = true;
    root.stale_reasons = vec!["recorded PR base is superseded by current main".to_owned()];
    (stack, pulls, candidates)
}

#[test]
fn current_root_synthetic_selects_prefix_without_rewriting_raw_generation() {
    for cumulative in [false, true] {
        let (stack, pulls, mut candidates) = fixture();
        if cumulative {
            let root_oid = candidates[&PrNumber(101)]
                .synthetic
                .as_ref()
                .unwrap()
                .oid
                .clone();
            let child = candidates.get_mut(&PrNumber(102)).unwrap();
            child.synthetic.as_mut().unwrap().parents[0] = root_oid;
            child.freshness = MergeCandidateFreshness::StaleBase;
            child.stale_base = true;
        }
        let preserved = candidates.clone();
        let evidence = stack_merge_evidence(
            facts(&pulls, &candidates, &[], &BTreeSet::new()),
            &stack,
            &ready,
        );
        assert!(
            evidence.iter().all(|entry| entry.blockers.is_empty()),
            "{evidence:?}"
        );
        let prefix = plan_github_stack_ready_prefix(&stack, &evidence).unwrap();
        assert_eq!(prefix.selected, stack.topology.entries);
        let plan = prefix
            .direct_squash_plan("projection-fixture", "fixture-owner")
            .unwrap();
        assert_eq!(
            plan.before, stack,
            "writer lease retains the complete raw generation"
        );
        assert_eq!(plan.selected[0].base.oid.0, "historical-main");
        assert_eq!(plan.before.topology.base.oid.0, "base000");
        assert_eq!(
            candidates, preserved,
            "projection staleness remains visible"
        );
    }
}

#[test]
fn stale_or_inexact_root_cannot_authorize_any_prefix() {
    for defect in [
        "old-parent",
        "wrong-source-parent",
        "malformed-parents",
        "stale-head",
        "missing-synthetic",
        "wrong-compared-ref",
        "unknown-freshness",
        "missing-stale-flag",
        "wrong-repository",
        "wrong-root-ref",
        "moved-default",
        "moved-head",
        "changed-raw-base",
    ] {
        let (mut stack, mut pulls, mut candidates) = fixture();
        let root = candidates.get_mut(&PrNumber(101)).unwrap();
        match defect {
            "old-parent" => {
                root.synthetic.as_mut().unwrap().parents[0] = root.base.oid.clone();
            }
            "wrong-source-parent" => {
                root.synthetic.as_mut().unwrap().parents[1] = CommitOid("other-head".to_owned());
            }
            "malformed-parents" => {
                root.synthetic
                    .as_mut()
                    .unwrap()
                    .parents
                    .push(CommitOid("third-parent".to_owned()));
            }
            "stale-head" => {
                root.stale_head = true;
            }
            "missing-synthetic" => {
                root.synthetic = None;
            }
            "wrong-compared-ref" => {
                root.compared_base.as_mut().unwrap().name = "other".to_owned();
            }
            "unknown-freshness" => {
                root.freshness = MergeCandidateFreshness::Unknown;
            }
            "missing-stale-flag" => {
                root.stale_base = false;
            }
            "wrong-repository" => {
                stack.topology.base.repository.owner = "foreign".to_owned();
            }
            "wrong-root-ref" => {
                stack.topology.entries[0].base.name = "other".to_owned();
                pulls.get_mut(&PrNumber(101)).unwrap().base.name = "other".to_owned();
                root.base.name = "other".to_owned();
            }
            "moved-default" => {
                stack.topology.base.oid = CommitOid("moved-main".to_owned());
            }
            "moved-head" => {
                stack.topology.entries[0].head.oid = CommitOid("moved-head".to_owned());
            }
            "changed-raw-base" => {
                root.base.oid = stack.topology.base.oid.clone();
            }
            _ => unreachable!(),
        }
        let evidence = stack_merge_evidence(
            facts(&pulls, &candidates, &[], &BTreeSet::new()),
            &stack,
            &ready,
        );
        let prefix = plan_github_stack_ready_prefix(&stack, &evidence).unwrap();
        assert!(
            prefix.selected.is_empty(),
            "{defect} unexpectedly permitted landing"
        );
        assert_eq!(prefix.first_blocked.as_ref().unwrap().pr, PrNumber(101));
        assert!(
            prefix
                .direct_squash_plan("refused", "fixture-owner")
                .is_err()
        );
    }
}

#[test]
fn projection_does_not_waive_required_ci_holds_or_conflicts() {
    let (stack, pulls, candidates) = fixture();
    let conflict = CompatibilityReport {
        candidate: stack.topology.entries[0].head.clone(),
        target: stack.topology.entries[0].base.clone(),
        outcome: CompatibilityOutcome::Conflict,
        conflicting_paths: vec!["src/example.rs".to_owned()],
        diagnostic: None,
    };
    for blocker in [
        GitHubStackMergeBlocker::RequiredChecksNotReady,
        GitHubStackMergeBlocker::Held,
        GitHubStackMergeBlocker::MechanicallyBlocked,
    ] {
        let held = if blocker == GitHubStackMergeBlocker::Held {
            BTreeSet::from([PrNumber(101)])
        } else {
            BTreeSet::new()
        };
        let reports = if blocker == GitHubStackMergeBlocker::MechanicallyBlocked {
            vec![conflict.clone()]
        } else {
            Vec::new()
        };
        let evidence =
            stack_merge_evidence(facts(&pulls, &candidates, &reports, &held), &stack, &|pr| {
                if pr == PrNumber(101) && blocker == GitHubStackMergeBlocker::RequiredChecksNotReady
                {
                    StackEntryCi::NotReady
                } else {
                    StackEntryCi::Ready
                }
            });
        assert!(evidence[0].blockers.contains(&blocker));
        assert!(
            plan_github_stack_ready_prefix(&stack, &evidence)
                .unwrap()
                .selected
                .is_empty()
        );
    }
}

#[test]
fn root_projection_exception_does_not_extend_to_stale_child_base() {
    let (mut stack, mut pulls, mut candidates) = fixture();
    let root_oid = candidates[&PrNumber(101)]
        .synthetic
        .as_ref()
        .unwrap()
        .oid
        .clone();
    let child = candidates.get_mut(&PrNumber(102)).unwrap();
    // Even an apparently cumulative synthetic cannot hide a changed source-base
    // lease on a child. Only the exact first Stack entry gets root treatment.
    child.base.oid = CommitOid("old-predecessor".to_owned());
    child.freshness = MergeCandidateFreshness::StaleBase;
    child.stale_base = true;
    child.synthetic.as_mut().unwrap().parents[0] = root_oid;
    stack.topology.entries[1].base = child.base.clone();
    pulls.get_mut(&PrNumber(102)).unwrap().base = child.base.clone();
    let evidence = stack_merge_evidence(
        facts(&pulls, &candidates, &[], &BTreeSet::new()),
        &stack,
        &ready,
    );
    let prefix = plan_github_stack_ready_prefix(&stack, &evidence).unwrap();
    assert_eq!(prefix.selected, vec![stack.topology.entries[0].clone()]);
    assert!(
        evidence[1]
            .blockers
            .contains(&GitHubStackMergeBlocker::SyntheticCandidateStale)
    );
}
