use super::*;
use crate::github::{GitHubStackEntryGeneration, GitHubStackGeneration};

struct RetainedProvider {
    inner: FakeRecoveryProvider,
    rows: GitHubStackGeneration,
    reads: std::cell::Cell<u32>,
}

impl NativeStackRecoveryProvider for RetainedProvider {
    fn native_stack_generation(
        &self,
        _: &RepositoryId,
        number: u64,
    ) -> Result<Option<GitHubStackGeneration>, GitHubStackMutationError> {
        assert_eq!(number, self.rows.number);
        self.reads.set(self.reads.get() + 1);
        Ok(Some(self.rows.clone()))
    }
    fn verify_precondition_with_checks(
        &self,
        _: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<PullRequestSnapshot, MutationError> {
        self.inner.verify(expected, true)
    }
    fn verify_precondition(
        &self,
        _: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<PullRequestSnapshot, MutationError> {
        self.inner.verify(expected, false)
    }
    fn converge_membership(
        &self,
        plan: &NativeMembershipPlan,
    ) -> Result<NativeMembershipReceipt, NativeMembershipError> {
        self.inner.converge_membership(plan)
    }
}

struct Fixture {
    main: BranchSnapshot,
    caravans: Vec<Caravan>,
    backend: StackBackendStatus,
    provider: RetainedProvider,
}

fn project(rows: &GitHubStackGeneration) -> NativeStackStatus {
    let mut native = exact_native_stack(rows.number, &[]);
    native.stack.id = rows.id;
    native.stack.node_id.clone_from(&rows.node_id);
    native.stack.created_at.clone_from(&rows.created_at);
    native
        .stack
        .base
        .ref_name
        .clone_from(&rows.topology.base.name);
    native.stack.pull_requests = rows
        .topology
        .entries
        .iter()
        .map(|entry| crate::github::GitHubStackPullRequest {
            number: entry.pr.0,
            state: entry.stack_state.clone(),
            draft: entry.draft,
            merged_at: entry.merged_at.clone(),
            head: crate::github::GitHubStackPullRequestHead {
                ref_name: entry.head.name.clone(),
                sha: entry.head.oid.clone(),
            },
        })
        .collect();
    native.consistency = StackConsistency::Drifted;
    native
}

fn fixture(history_count: usize, active_count: usize) -> Fixture {
    let main = branch("main", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let root = pull(101, main.clone());
    let child = pull(102, root.head.clone());
    let mut tail = pull(103, child.head.clone());
    tail.checks[0].state = CheckState::InProgress;
    let pulls = BTreeMap::from([
        (root.number, root),
        (child.number, child),
        (tail.number, tail),
    ]);
    let desired = crate::stack_membership::topology_from_members(&main, pulls.values()).unwrap();
    let mut entries = Vec::new();
    for index in 0..history_count {
        let number = 90 + u64::try_from(index).unwrap();
        entries.push(GitHubStackEntryGeneration {
            position: u32::try_from(index).unwrap(),
            pr: PrNumber(number),
            stack_state: "closed".to_owned(),
            pull_request_state: PullRequestState::Merged,
            draft: false,
            merged_at: Some("2026-09-24T10:00:00Z".to_owned()),
            base: main.clone(),
            head: branch(&format!("old-{number}"), &format!("{number:040x}")),
        });
    }
    for row in desired.entries.iter().take(active_count) {
        let mut row = row.clone();
        row.position = u32::try_from(entries.len()).unwrap();
        entries.push(row);
    }
    let rows = GitHubStackGeneration {
        id: 4077,
        number: 4077,
        node_id: "S_4077".to_owned(),
        open: true,
        created_at: "2026-09-24T09:00:00Z".to_owned(),
        topology: GitHubStackTopology {
            base: main.clone(),
            entries,
        },
    };
    let backend = stack_backend_fixture(vec![project(&rows)]);
    Fixture {
        main,
        caravans: vec![Caravan::new(vec![PrNumber(101), PrNumber(102), PrNumber(103)]).unwrap()],
        backend,
        provider: RetainedProvider {
            inner: FakeRecoveryProvider {
                pulls,
                disposition: GitHubStackMutationDisposition::Completed,
                creates: std::cell::Cell::new(0),
            },
            rows,
            reads: std::cell::Cell::new(0),
        },
    }
}

fn evidence(f: &Fixture) -> RecoveryFacts<'_> {
    facts(&f.main, &f.caravans, &f.provider.inner.pulls, &f.backend)
}

#[test]
fn closed_prefix_auto_reforms_multiple_closures_and_partial_suffix_without_checkpoint() {
    for history in [1, 2] {
        for active in [1, 2] {
            let directory = tempfile::tempdir().unwrap();
            let context = test_context(directory.path());
            let mut f = fixture(history, active);
            let result = auto_recover_from_facts(&context, &evidence(&f), &f.provider)
                .unwrap()
                .unwrap();
            assert!(result.source_heads_unchanged);
            assert!(result.plan.pending_membership_checkpoint_hash.is_none());
            let NativeMembershipPlan::RecoveryAdd { plan, accepted } = &result.plan.action else {
                panic!("not an exact retained append")
            };
            assert_eq!(plan.before, f.provider.rows);
            assert_eq!(
                &plan.desired.entries[..history + active],
                f.provider.rows.topology.entries.as_slice()
            );
            assert_eq!(plan.desired.entries.len(), history + 3);
            assert_eq!(
                accepted
                    .entries
                    .iter()
                    .map(|entry| entry.pr)
                    .collect::<Vec<_>>(),
                f.caravans[0].members
            );
            assert_eq!(f.provider.reads.get(), 1);
            assert_eq!(f.provider.inner.creates.get(), 1);
            assert!(
                crate::stack_membership::load_pending(directory.path(), PrNumber(101))
                    .unwrap()
                    .is_none()
            );
            // Complete provider membership is authoritative on the next tick;
            // local response/checkpoint loss cannot cause a repeated append.
            f.provider.rows.topology.clone_from(&plan.desired);
            f.backend.native_stacks = vec![project(&f.provider.rows)];
            f.backend.native_stacks[0].consistency = StackConsistency::Exact;
            assert!(
                auto_recover_from_facts(&context, &evidence(&f), &f.provider)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(f.provider.inner.creates.get(), 1);
        }
    }
}

#[test]
fn closed_prefix_same_raw_and_logical_count_recovers_one_missing_tail() {
    let directory = tempfile::tempdir().unwrap();
    let context = test_context(directory.path());
    let mut f = fixture(1, 1);
    f.caravans = vec![Caravan::new(vec![PrNumber(101), PrNumber(102)]).unwrap()];
    f.provider.inner.pulls.remove(&PrNumber(103));
    // Stack4077's shape: [merged history, open root], desired [root, tail].
    assert_eq!(
        f.provider.rows.topology.entries.len(),
        f.caravans[0].members.len()
    );
    let output = auto_recover_from_facts(&context, &evidence(&f), &f.provider)
        .unwrap()
        .unwrap();
    let NativeMembershipPlan::RecoveryAdd { plan, .. } = output.plan.action else {
        panic!("must not create a replacement Stack")
    };
    assert_eq!(plan.before.number, 4077);
    assert_eq!(plan.desired.entries.len(), 3);
    assert_eq!(plan.desired.entries[2].pr, PrNumber(102));
    assert_eq!(f.provider.inner.creates.get(), 1);
}

#[test]
fn closed_prefix_auto_rejects_fresh_history_identity_and_base_races() {
    for mutation in 0..5 {
        let directory = tempfile::tempdir().unwrap();
        let context = test_context(directory.path());
        let mut f = fixture(1, 1);
        match mutation {
            0 => {
                f.provider.rows.topology.entries[0].head.oid =
                    CommitOid("moved-history".to_owned());
            }
            1 => f.provider.rows.node_id.push_str("-replacement"),
            2 => f.provider.rows.topology.entries[0].merged_at = Some("changed".to_owned()),
            3 => f.provider.rows.topology.entries[1].base = branch("wrong-base", "wrong-head"),
            _ => f.provider.rows.topology.base.oid = CommitOid("new-default".to_owned()),
        }
        assert!(auto_recover_from_facts(&context, &evidence(&f), &f.provider).is_err());
        assert_eq!(f.provider.inner.creates.get(), 0);
    }
}

#[test]
fn closed_prefix_auto_refuses_closed_middle_tail_unmerged_and_reopened_history() {
    for mutation in 0..5 {
        let directory = tempfile::tempdir().unwrap();
        let context = test_context(directory.path());
        let mut f = fixture(1, 2);
        let rows = &mut f.backend.native_stacks[0].stack.pull_requests;
        match mutation {
            0 => rows.swap(0, 1),                   // interleaved closed middle
            1 => rows.swap(0, 2),                   // closed tail
            2 => rows[0].merged_at = None,          // closed, not merged
            3 => rows[0].state = "OPEN".to_owned(), // contradictory reopen
            _ => rows[0].state = "future-state".to_owned(),
        }
        let error = auto_recover_from_facts(&context, &evidence(&f), &f.provider).unwrap_err();
        assert_eq!(error.code(), "github_stack_closed_history_requires_owner");
        assert_eq!(f.provider.reads.get(), 0);
        assert_eq!(f.provider.inner.creates.get(), 0);
    }
}

#[test]
fn closed_prefix_auto_refuses_history_intersection_and_truncated_inventory() {
    let directory = tempfile::tempdir().unwrap();
    let context = test_context(directory.path());
    let mut f = fixture(1, 1);
    let mut other = f.backend.native_stacks[0].clone();
    other.stack.number = 999;
    other.stack.pull_requests.truncate(1);
    other.caravan_id = None;
    f.backend.native_stacks.push(other);
    let error = auto_recover_from_facts(&context, &evidence(&f), &f.provider).unwrap_err();
    assert_eq!(error.code(), "github_stack_recovery_history_intersection");
    assert_eq!(f.provider.inner.creates.get(), 0);
    f.backend.provider_stacks_truncated = true;
    assert!(
        auto_recover_from_facts(&context, &evidence(&f), &f.provider)
            .unwrap()
            .is_none()
    );
    assert_eq!(f.provider.inner.creates.get(), 0);
}
