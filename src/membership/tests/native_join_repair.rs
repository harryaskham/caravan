//! bd-28ae57: real membership + native discovery + leased Git replay, offline.

use super::*;
use crate::github::{GitHubStackInventory, GitHubStackSnapshot};
use crate::physical_rebase::{self, PlannedBase, RebaseExecutionBudget};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Inventory {
    path: PathBuf,
    stack: GitHubStackSnapshot,
}

impl crate::read::NativeStackInventoryProvider for Inventory {
    fn native_stack_inventory(
        &self,
        _: &RepositoryId,
    ) -> Result<GitHubStackInventory, crate::github::GitHubStackReadError> {
        Ok(GitHubStackInventory {
            stacks: vec![self.stack.clone()],
            truncated: false,
        })
    }

    fn compare_stack_commits(
        &self,
        _: &RepositoryId,
        parent: &CommitOid,
        child: &CommitOid,
    ) -> Result<crate::generation::CommitRelation, String> {
        let common = git(&self.path, &["merge-base", &parent.0, &child.0]);
        Ok(if common == parent.0 {
            crate::generation::CommitRelation::Ahead
        } else {
            crate::generation::CommitRelation::Diverged
        })
    }
}

/// Join B (branched from main) behind active A without a join-time rewrite.
/// Exercise both an immediately visible Stack and the lagging strict-prefix
/// inventory that previously looked membership-only and skipped routine repair.
#[test]
#[allow(clippy::too_many_lines)]
fn native_join_behind_active_head_is_repaired_linearly_without_quarantining_slot() {
    for lagging_prefix in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let bare = directory.path().join("remote.git");
        let work = directory.path().join("work");
        git(
            directory.path(),
            &["init", "--bare", bare.to_str().unwrap()],
        );
        git(
            directory.path(),
            &["clone", bare.to_str().unwrap(), work.to_str().unwrap()],
        );
        git(&work, &["config", "user.name", "Caravan Test"]);
        git(&work, &["config", "user.email", "test@example.invalid"]);
        git(&work, &["checkout", "-b", "main"]);
        std::fs::write(work.join("base"), "base\n").unwrap();
        std::fs::create_dir_all(work.join(".github/workflows")).unwrap();
        std::fs::write(
            work.join(".github/workflows/stack.yml"),
            "on:\n  pull_request:\n    types: [opened, synchronize, reopened, edited, labeled, unlabeled]\njobs: {}\n",
        ).unwrap();
        git(&work, &["add", "base", ".github/workflows/stack.yml"]);
        git(&work, &["commit", "-m", "base"]);
        let main = CommitOid(git(&work, &["rev-parse", "HEAD"]));
        for name in ["root", "child"] {
            git(&work, &["checkout", "-b", name, "main"]);
            std::fs::write(work.join(name), format!("{name}\n")).unwrap();
            git(&work, &["add", name]);
            git(&work, &["commit", "-m", name]);
        }
        git(&work, &["push", "origin", "main", "root", "child"]);
        git(&work, &["checkout", "main"]);
        let mut root = pull_request(1, "root", "main", &[ACTIVE_LABEL]);
        let mut child = pull_request(2, "child", "main", &[]);
        root.head.oid = CommitOid(git(&work, &["rev-parse", "root"]));
        child.head.oid = CommitOid(git(&work, &["rev-parse", "child"]));
        root.base.oid = main.clone();
        child.base.oid = main.clone();
        let old_child = child.head.oid.clone();
        let mut before = status(child.clone(), vec![root.clone()]);
        before.analysis.fleet.default_branch.oid = main;
        before.stack_backend.configured = crate::config::StackType::Github;
        let provider = FakeProvider::with_pull_requests(vec![root.clone(), child]);
        provider.branch_heads.borrow_mut().extend([
            (
                "main".to_owned(),
                before.analysis.fleet.default_branch.oid.clone(),
            ),
            ("root".to_owned(), root.head.oid.clone()),
            ("child".to_owned(), old_child.clone()),
        ]);
        let joined = execute(
            before.clone(),
            &clean,
            &provider,
            MembershipRequest {
                expected_admission: None,
                operation: MembershipOperation::Join,
                create_pr: false,
                tail_pr: Some(1),
                head_pr: None,
                reason: None,
                priority_label: None,
                agent_priority_labels: Vec::new(),
            },
        )
        .expect("B joins behind A through ordinary native-mode membership");
        assert_eq!(joined.pull_request.head.oid, old_child);
        assert_eq!(joined.pull_request.base, root.head);
        let native = crate::stack_membership::plan_native_membership(
            &crate::stack_membership::NativeMembershipFacts::from_status(&before),
            &joined,
            "operator",
        )
        .unwrap();
        let crate::stack_membership::NativeMembershipPlan::Create { plan } = native else {
            panic!("joining a singleton creates a native Stack");
        };
        let rows = plan
            .desired
            .entries
            .iter()
            .map(|entry| crate::github::GitHubStackPullRequest {
                number: entry.pr.0,
                state: "open".to_owned(),
                draft: false,
                merged_at: None,
                head: crate::github::GitHubStackPullRequestHead {
                    ref_name: entry.head.name.clone(),
                    sha: entry.head.oid.clone(),
                },
            })
            .collect::<Vec<_>>();
        let mut inventory = Inventory {
            path: work.clone(),
            stack: GitHubStackSnapshot {
                id: 42,
                number: 42,
                node_id: "S_42".to_owned(),
                open: true,
                created_at: String::new(),
                base: crate::github::GitHubStackBase {
                    ref_name: "main".to_owned(),
                },
                pull_requests: rows.clone(),
            },
        };
        if lagging_prefix {
            inventory.stack.pull_requests.truncate(1);
        }
        let mut observed = status(joined.pull_request.clone(), vec![root.clone()]);
        observed.analysis.fleet.default_branch = before.analysis.fleet.default_branch.clone();
        observed.stack_backend = crate::read::stack_backend_status(
            crate::config::StackType::Github,
            &inventory,
            &repository(),
            &observed.analysis,
        );
        let mut context = AppContext {
            repository_path: work.clone(),
            ..AppContext::default()
        };
        context.config.stack_type = crate::config::StackType::Github;
        context.config.rebase_on_join = false;
        context.config.sync.max_caravans = 1;
        context.config.stack_rollout.mutations_opt_in = true;
        context.config.stack_rollout.reviewed_by = "operator".to_owned();
        let plans = crate::native_stack_rebase::pending_sync_repair_plans(
            &context,
            &observed,
            crate::native_stack_rebase::SyncRepairKind::RoutineDrift,
        )
        .unwrap();
        assert_eq!(
            plans.len(),
            1,
            "the queue selects the joined suffix, lag={lagging_prefix}"
        );
        let repair = &plans[0].1;
        assert!(repair.verify());
        assert!(repair.members.iter().any(|member| member.pr == PrNumber(2)));
        let mut prepared = Vec::new();
        for member in &repair.members {
            let pull = &observed.analysis.pull_requests[&member.pr];
            let parent = BranchSnapshot {
                repository: repository(),
                name: member.parent_branch.clone(),
                oid: member.parent_head.clone(),
            };
            let target = prepared.last().map_or_else(
                || PlannedBase::Remote(parent.clone()),
                |item: &physical_rebase::PreparedRebase| {
                    PlannedBase::Simulated(BranchSnapshot {
                        repository: repository(),
                        name: item.plan.branch.clone(),
                        oid: item.plan.new_head_oid.clone(),
                    })
                },
            );
            prepared.push(
                physical_rebase::prepare_candidate(
                    &work,
                    &repository(),
                    pull,
                    physical_rebase::range_base_for_rewritten_parent(pull, &parent),
                    target,
                    &observed.analysis.fleet.default_branch,
                    RebaseExecutionBudget::new(Duration::from_secs(60)),
                )
                .expect("the exact source range replays cleanly"),
            );
        }
        let applied =
            physical_rebase::apply_prepared_atomically(&prepared.iter().collect::<Vec<_>>())
                .unwrap();
        assert!(applied.atomic);
        for receipt in &applied.receipts {
            observed
                .analysis
                .pull_requests
                .get_mut(&receipt.pr)
                .unwrap()
                .head
                .oid = receipt.new_head_oid.clone();
        }
        let pulls = crate::native_stack_rebase::project_linear_bases(
            &observed,
            &[PrNumber(1), PrNumber(2)],
        )
        .unwrap();
        // Rebuild the provider representation from the exact leased heads, as
        // the production repair does. Neither old CI nor a source merge is used.
        inventory.stack.pull_requests = rows;
        for (row, pull) in inventory.stack.pull_requests.iter_mut().zip(&pulls) {
            row.head.sha = pull.head.oid.clone();
        }
        observed.analysis.pull_requests =
            pulls.into_iter().map(|pull| (pull.number, pull)).collect();
        observed.stack_backend = crate::read::stack_backend_status(
            crate::config::StackType::Github,
            &inventory,
            &repository(),
            &observed.analysis,
        );
        assert!(observed.stack_backend.problems.is_empty());
        assert!(observed.stack_backend.quarantined_caravans.is_empty());
        crate::sync::require_native_stack_backend_healthy(&observed).unwrap();
        assert!(crate::native_stack_rebase::drift_repair_plans(&context, &observed).is_empty());
        let new_child = git(&bare, &["rev-parse", "refs/heads/child"]);
        assert_ne!(new_child, old_child.0);
        assert_eq!(
            git(&bare, &["merge-base", &root.head.oid.0, &new_child]),
            root.head.oid.0
        );
        assert_eq!(
            git(&bare, &["rev-parse", "refs/heads/root"]),
            root.head.oid.0
        );
        for name in ["base", "root", "child"] {
            assert_eq!(git(&bare, &["show", &format!("{new_child}:{name}")]), name);
        }
    }
}
