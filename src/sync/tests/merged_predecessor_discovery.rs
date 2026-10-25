//! Discovery-to-policy regression for bd-63294d. The provider boundary is fake;
//! graph analysis, exact check decisions, and sync convergence are real.
use super::*;
use crate::command::{CommandOutput, CommandRunError, CommandRunner, CommandSpec};
use crate::github::{DiscoveryOptions, GitHubDiscovery};
use serde_json::Value;
use std::rc::Rc;

struct DiscoveryFixture {
    open: Vec<PullRequestSnapshot>,
    merged: Vec<PullRequestSnapshot>,
    calls: Rc<RefCell<Vec<CommandSpec>>>,
}

fn provider_row(pull: &PullRequestSnapshot) -> Value {
    json!({
        "number": pull.number.0, "title": pull.title,
        "state": if pull.is_merged() { "MERGED" } else { "OPEN" },
        "isDraft": pull.draft,
        "headRefName": pull.head.name, "headRefOid": pull.head.oid,
        "headRepository": {"nameWithOwner": pull.head.repository.slug()},
        "isCrossRepository": false,
        "baseRefName": pull.base.name, "baseRefOid": pull.base.oid,
        "labels": pull.labels.iter().map(|label| json!({"name": label})).collect::<Vec<_>>(),
        "autoMergeRequest": if pull.auto_merge.enabled {
            json!({"mergeMethod": "SQUASH"})
        } else { Value::Null },
        "createdAt": pull.created_at, "mergedAt": pull.merged_at,
        "updatedAt": PUBLISHED_AT, "url": pull.url,
    })
}

impl CommandRunner for DiscoveryFixture {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
        self.calls.borrow_mut().push(command.clone());
        let args = &command.args;
        if command.program == "git" {
            return Ok(CommandOutput::success(match args[0].as_str() {
                "symbolic-ref" => "main".to_owned(),
                "rev-parse" => branch("main").oid.0,
                _ => panic!("unexpected Git operation: {command:?}"),
            }));
        }
        assert_eq!(command.program, "gh");
        let open = self.open.iter().map(provider_row).collect::<Vec<_>>();
        let response = if args.starts_with(&["repo".into(), "view".into()]) {
            json!({"nameWithOwner": repository().slug(), "defaultBranchRef": {"name": "main"}})
        } else if args
            .get(1)
            .is_some_and(|arg| arg.ends_with("/git/ref/heads/main"))
        {
            json!({"object": {"sha": branch("main").oid}})
        } else if args
            .iter()
            .any(|arg| arg.contains("pullRequests(states:OPEN"))
        {
            json!({"rows": open, "pageInfo": {"hasNextPage": false, "totalCount": open.len()}})
        } else if args.iter().any(|arg| arg == "number=2") {
            provider_row(&self.open[0])
        } else if args.iter().any(|arg| arg.contains("history(first:20)")) {
            let mut candidates = serde_json::Map::new();
            for (index, pull) in self
                .open
                .iter()
                .filter(|pull| pull.has_label("caravan"))
                .enumerate()
            {
                candidates.insert(
                    format!("c{index}"),
                    json!({
                        "oid": format!("merge-{}", pull.number), "tree": {"oid": "tree"},
                        "parents": {"nodes": [{"oid": pull.base.oid}, {"oid": pull.head.oid}]},
                    }),
                );
            }
            json!({"data": {"repository": candidates}})
        } else if args.starts_with(&["pr".into(), "list".into()]) {
            match args.windows(2).find(|pair| pair[0] == "--state").unwrap()[1].as_str() {
                "open" if args.iter().any(|arg| arg == "--label") => json!(
                    self.open
                        .iter()
                        .filter(|pull| pull.has_label("caravan"))
                        .map(provider_row)
                        .collect::<Vec<_>>()
                ),
                "open" => json!(open),
                "merged" => json!(self.merged.iter().map(provider_row).collect::<Vec<_>>()),
                "closed" => json!([]),
                _ => panic!("unexpected PR state: {command:?}"),
            }
        } else {
            panic!("unexpected provider operation: {command:?}");
        };
        Ok(CommandOutput::success(response.to_string()))
    }
}

fn discover(
    open: Vec<PullRequestSnapshot>,
    merged: Vec<PullRequestSnapshot>,
    historical: bool,
    focused: bool,
) -> RepositorySnapshot {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let discovery = GitHubDiscovery::new(DiscoveryFixture {
        open,
        merged,
        calls: Rc::clone(&calls),
    })
    .with_options(DiscoveryOptions {
        include_historical_pull_requests: historical,
        focus_pr: focused.then_some(PrNumber(2)),
        require_current_pr_resolution: false,
        ..DiscoveryOptions::default()
    });
    let snapshot = discovery.discover().expect("bounded discovery");
    let calls = calls.borrow();
    assert_eq!(
        calls.len(),
        7 + 2 * usize::from(focused) + 3 * usize::from(historical && !focused),
        "predecessor retention adds no requests"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call
                .args
                .windows(2)
                .any(|pair| pair == ["--state", "closed"]))
            .count(),
        3 * usize::from(historical && !focused),
        "hot paths never scan closed history"
    );
    let merged_read = calls
        .iter()
        .find(|call| {
            call.args
                .windows(2)
                .any(|pair| pair == ["--state", "merged"])
        })
        .unwrap();
    assert!(
        !merged_read
            .args
            .iter()
            .any(|arg| arg.contains("statusCheckRollup"))
    );
    snapshot
}

fn predecessor() -> PullRequestSnapshot {
    pull_request(
        1,
        "one",
        "main",
        PullRequestState::Merged,
        AutoMergeState::disabled(),
    )
}

fn successor() -> PullRequestSnapshot {
    pull_request(
        2,
        "two",
        "one",
        PullRequestState::Open,
        AutoMergeState::squash(),
    )
}

#[test]
fn merged_predecessor_discovery_agrees_for_status_check_and_sync_and_advances() {
    let mut expected_problems = None;
    for (historical, focused) in [(true, false), (false, true), (false, false)] {
        let snapshot = discover(vec![successor()], vec![predecessor()], historical, focused);
        assert!(
            snapshot
                .pull_requests
                .iter()
                .any(|pull| pull.number == PrNumber(1)),
            "the merged base is topology, not optional history: historical={historical}, focused={focused}"
        );
        let provider = FakeProvider::with_pull_requests(snapshot.pull_requests.clone());
        let before = status(snapshot.pull_requests, Some(PrNumber(2)), &clean);
        let check = read::check_analysis(
            &before,
            &crate::CheckInput {
                pr: Some(2),
                ..Default::default()
            },
            &clean,
        )
        .expect("remote check reports the pending advancement");
        assert!(!check.eligible);
        assert_eq!(check.next_action, crate::read::CandidateNextAction::Repair);
        let problems = serde_json::to_value(check.problems).unwrap();
        if let Some(expected) = &expected_problems {
            assert_eq!(
                &problems, expected,
                "all discovery paths must name the same predecessor"
            );
        } else {
            assert!(
                problems.to_string().contains("merged predecessor #1"),
                "{problems}"
            );
            expected_problems = Some(problems);
        }

        let progress = execute(&before, &provider, false, false, false)
            .expect("sync advances the discovered successor without eviction");
        assert_eq!(progress.head_advancements.len(), 1);
        assert_eq!(
            progress.head_advancements[0].merged_predecessor,
            PrNumber(1)
        );
        assert_eq!(progress.head_advancements[0].new_head, PrNumber(2));
        let after = status(
            provider.pulls.borrow().values().cloned().collect(),
            Some(PrNumber(2)),
            &clean,
        );
        assert_eq!(after.analysis.pull_requests[&PrNumber(2)].base.name, "main");
        assert!(after.analysis.pull_requests[&PrNumber(2)].is_active_caravan_member());
        assert!(
            read::check_analysis(
                &after,
                &crate::CheckInput {
                    pr: Some(2),
                    ..Default::default()
                },
                &clean
            )
            .expect("advanced member passes check")
            .eligible
        );
    }
}

#[test]
fn merged_predecessor_discovery_keeps_all_ambiguous_matches_and_refuses_mutation() {
    let mut reused = predecessor();
    reused.number = PrNumber(3);
    reused.head.oid = CommitOid("another-generation".into());
    let snapshot = discover(vec![successor()], vec![predecessor(), reused], false, false);
    assert_eq!(
        snapshot.pull_requests.len(),
        3,
        "never select one merged generation by branch name"
    );
    let provider = FakeProvider::with_pull_requests(snapshot.pull_requests.clone());
    let before = status(snapshot.pull_requests, Some(PrNumber(2)), &clean);
    execute(&before, &provider, false, false, false)
        .expect_err("ambiguous predecessor fails closed");
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn merged_predecessor_discovery_retains_unlabelled_candidate_base_but_not_unrelated_history() {
    let mut candidate = successor();
    candidate.labels.clear();
    candidate.auto_merge = AutoMergeState::disabled();
    let unrelated = pull_request(
        99,
        "unrelated",
        "main",
        PullRequestState::Merged,
        AutoMergeState::disabled(),
    );
    for focused in [false, true] {
        let snapshot = discover(
            vec![candidate.clone()],
            vec![predecessor(), unrelated.clone()],
            false,
            focused,
        );
        assert_eq!(
            snapshot
                .pull_requests
                .iter()
                .map(|pull| pull.number)
                .collect::<Vec<_>>(),
            vec![PrNumber(1), PrNumber(2)]
        );
        assert!(
            snapshot
                .generation_facts
                .iter()
                .any(|fact| fact.pr == PrNumber(99)),
            "unrelated merged generation facts still guard supersession"
        );
    }
}

#[test]
fn merged_predecessor_discovery_preserves_reused_active_branch_guard() {
    let mut reused = pull_request(
        3,
        "one",
        "main",
        PullRequestState::Open,
        AutoMergeState::squash(),
    );
    reused.head.oid = CommitOid("new-generation".into());
    let snapshot = discover(vec![successor(), reused], vec![predecessor()], false, false);
    let provider = FakeProvider::with_pull_requests(snapshot.pull_requests.clone());
    let before = status(snapshot.pull_requests, Some(PrNumber(2)), &clean);
    assert!(
        before
            .analysis
            .fleet
            .problems
            .iter()
            .any(|problem| problem.kind == GraphProblemKind::ReusedBranchProvenance)
    );
    execute(&before, &provider, false, false, false).expect_err("reused branch fails closed");
    assert!(provider.calls.borrow().is_empty());
}

#[test]
fn merged_predecessor_discovery_does_not_invent_missing_history() {
    let snapshot = discover(vec![successor()], vec![], false, false);
    let provider = FakeProvider::with_pull_requests(snapshot.pull_requests.clone());
    let before = status(snapshot.pull_requests, Some(PrNumber(2)), &clean);
    assert!(
        before
            .analysis
            .fleet
            .problems
            .iter()
            .any(|problem| problem.message.contains("non-member branch"))
    );
    execute(&before, &provider, false, false, false).expect_err("unknown base fails closed");
    assert!(provider.calls.borrow().is_empty());
}
