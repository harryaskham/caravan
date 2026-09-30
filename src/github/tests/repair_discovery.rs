use super::*;

fn unlabelled_pr(number: u64, branch: &str) -> String {
    pr_object_json(number, branch, "acme/widgets")
        .replace(r#""labels":[{"name":"caravan"}]"#, r#""labels":[]"#)
}

#[test]
fn focused_repair_discovery_fetches_inactive_or_active_target_and_keeps_topology() {
    for target_active in [false, true] {
        let first = pr_object_json(1, "active", "acme/widgets");
        let target = if target_active {
            pr_object_json(3, "explicit-target", "acme/widgets")
        } else {
            unlabelled_pr(3, "explicit-target")
        };
        let active = if target_active {
            format!("[{first},{target}]")
        } else {
            format!("[{first}]")
        };
        let candidate = unlabelled_pr(2, "candidate");
        let unrelated = unlabelled_pr(99, "unrelated");
        let generation_rows = format!("[{first},{candidate},{target},{unrelated}]");
        let mut calls = successful_discovery_calls(&active);
        calls[1] = (current_branch_command(), CommandOutput::success("main\n"));
        calls.splice(
            4..5,
            [
                (
                    labeled_pr_command("acme/widgets", "open", "caravan", 1_000, false),
                    CommandOutput::success(active),
                ),
                (
                    pull_request_command(&repository(), "2"),
                    CommandOutput::success(candidate),
                ),
                (
                    pull_request_command(&repository(), "3"),
                    CommandOutput::success(target),
                ),
                (
                    open_generation_pr_command("acme/widgets", 1_000),
                    CommandOutput::success(generation_rows),
                ),
            ],
        );
        calls.truncate(calls.len() - 3);
        let discovery =
            GitHubDiscovery::new(FakeRunner::new(calls)).with_options(DiscoveryOptions {
                focus_pr: Some(PrNumber(2)),
                repair_target_pr: Some(PrNumber(3)),
                require_current_pr_resolution: false,
                include_historical_pull_requests: false,
                ..DiscoveryOptions::default()
            });
        let snapshot = discovery.discover().unwrap();
        assert_eq!(
            snapshot
                .pull_requests
                .iter()
                .map(|pull| pull.number)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([PrNumber(1), PrNumber(2), PrNumber(3)])
        );
        assert_eq!(snapshot.default_branch.name, "main");
        assert!(
            snapshot
                .generation_facts
                .iter()
                .any(|fact| fact.pr == PrNumber(99)),
            "lightweight unrelated lineage is retained, not full rollups"
        );
        assert!(
            snapshot
                .generation_facts
                .iter()
                .any(|fact| fact.pr == PrNumber(9)),
            "merged lineage evidence retained"
        );
        discovery.runner.assert_exhausted();
    }
}

#[test]
fn focused_repair_discovery_rejects_orphan_target_before_any_provider_read() {
    let discovery = GitHubDiscovery::new(FakeRunner::new(vec![])).with_options(DiscoveryOptions {
        repair_target_pr: Some(PrNumber(3)),
        ..DiscoveryOptions::default()
    });
    assert_eq!(
        discovery.discover().unwrap_err(),
        DiscoveryError::InvalidRepairFocus
    );
    discovery.runner.assert_exhausted();
}

fn pull() -> PullRequestSnapshot {
    serde_json::from_str::<PullRequestJson>(&pr_object_json(3, "target", "acme/widgets"))
        .unwrap()
        .into_snapshot(&repository())
        .unwrap()
}

#[test]
fn focused_repair_discovery_refuses_changed_identity_but_not_check_progress() {
    let original = pull();
    for case in 0..9 {
        let mut changed = original.clone();
        match case {
            0 => changed.number = PrNumber(99),
            1 => changed.head.oid = CommitOid("moved-head".to_owned()),
            2 => changed.base.oid = CommitOid("moved-base".to_owned()),
            3 => changed.head.repository.owner = "foreign".to_owned(),
            4 => changed.draft = true,
            5 => changed.state = model::PullRequestState::Closed,
            6 => changed.labels.clear(),
            7 => changed.updated_at = Some("2026-07-18T11:00:00Z".to_owned()),
            _ => changed.created_at = Some("2026-07-16T11:00:00Z".to_owned()),
        }
        let mut active = vec![original.clone()];
        assert_eq!(
            merge_repair_focus(&mut active, PrNumber(3), changed).unwrap_err(),
            DiscoveryError::RepairFocusChanged { pr: 3 }
        );
        assert_eq!(
            active,
            [original.clone()],
            "case {case} must preserve snapshot on refusal"
        );
    }
    let mut newer_checks = original.clone();
    newer_checks.checks.clear();
    let mut active = vec![original];
    merge_repair_focus(&mut active, PrNumber(3), newer_checks.clone()).unwrap();
    assert_eq!(active, [newer_checks]);
}
