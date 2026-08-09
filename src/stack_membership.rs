//! Native GitHub Stack convergence after ordinary Cara membership policy.
//!
//! Cara membership remains authority for candidacy, compatibility, labels,
//! bases, and audit receipts. This module maps the *completed* ordinary
//! singleton/new or join result to the exact Stack create/add operation needed
//! to represent the same caravan. A Stack failure is therefore a resumable
//! partial membership operation, never permission to roll back Cara policy.

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::command::CommandRunner;
use crate::github::{
    GitHubMutationAdapter, GitHubStackAddPlan, GitHubStackCreatePlan, GitHubStackEntryGeneration,
    GitHubStackMutationError, GitHubStackMutationReceipt, GitHubStackTopology,
};
use crate::membership::MembershipOutput;
use crate::model::{
    BranchSnapshot, Caravan, PrNumber, PullRequestSnapshot, PullRequestState, RepositoryId,
};
use crate::read::{NativeStackStatus, StackConsistency, StatusOutput};

/// Exact native provider action required after one ordinary membership result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum NativeMembershipPlan {
    /// GitHub Stack objects require at least two PRs. A new one-member caravan
    /// is represented by ordinary PR membership alone.
    AbsentSingleton {
        repository: RepositoryId,
        operation_id: String,
        caravan_id: PrNumber,
        member: PrNumber,
    },
    Create {
        plan: Box<GitHubStackCreatePlan>,
    },
    Add {
        repository: RepositoryId,
        operation_id: String,
        actor: String,
        stack_number: u64,
        expected_members: Vec<PrNumber>,
        candidate: Box<PullRequestSnapshot>,
    },
}

/// Successful native representation receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "disposition")]
pub enum NativeMembershipReceipt {
    AbsentSingleton {
        repository: RepositoryId,
        operation_id: String,
        caravan_id: PrNumber,
        member: PrNumber,
    },
    StackMutation {
        receipt: Box<GitHubStackMutationReceipt>,
    },
}

/// Durable, secret-free continuation written before a native Stack create/add.
/// Ordinary Cara membership may already be visible when the provider operation
/// fails; this checkpoint gives the scheduler an exact first-party recovery
/// target instead of asking it to re-admit a now-labelled PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NativeMembershipCheckpoint {
    pub schema_version: u32,
    pub caravan_id: PrNumber,
    pub plan: NativeMembershipPlan,
    pub evidence_hash: String,
}

impl NativeMembershipCheckpoint {
    fn from_plan(plan: &NativeMembershipPlan) -> Option<Self> {
        let caravan_id = plan.caravan_id()?;
        let mut checkpoint = Self {
            schema_version: 1,
            caravan_id,
            plan: plan.clone(),
            evidence_hash: String::new(),
        };
        checkpoint.evidence_hash = checkpoint.expected_hash();
        Some(checkpoint)
    }

    fn expected_hash(&self) -> String {
        let mut material = self.clone();
        material.evidence_hash.clear();
        crate::membership::fnv1a64(
            &serde_json::to_vec(&material).expect("native membership checkpoint serializes"),
        )
    }

    #[must_use]
    pub fn verify(&self) -> bool {
        !self.evidence_hash.is_empty() && self.evidence_hash == self.expected_hash()
    }
}

impl NativeMembershipPlan {
    #[must_use]
    pub fn caravan_id(&self) -> Option<PrNumber> {
        match self {
            Self::AbsentSingleton { .. } => None,
            Self::Create { plan } => plan.desired.entries.first().map(|entry| entry.pr),
            Self::Add {
                expected_members, ..
            } => expected_members.first().copied(),
        }
    }
}

pub(crate) fn persist_pending(
    repository: &Path,
    plan: &NativeMembershipPlan,
) -> Result<Option<NativeMembershipCheckpoint>, crate::AppError> {
    let Some(checkpoint) = NativeMembershipCheckpoint::from_plan(plan) else {
        return Ok(None);
    };
    crate::stack_checkpoint::write(repository, &pending_key(checkpoint.caravan_id), &checkpoint)?;
    Ok(Some(checkpoint))
}

pub(crate) fn load_pending(
    repository: &Path,
    caravan_id: PrNumber,
) -> Result<Option<NativeMembershipCheckpoint>, crate::AppError> {
    let checkpoint = crate::stack_checkpoint::load(repository, &pending_key(caravan_id))?;
    if checkpoint
        .as_ref()
        .is_some_and(|checkpoint: &NativeMembershipCheckpoint| !checkpoint.verify())
    {
        return Err(crate::AppError::validation(
            "github_stack_membership_checkpoint_invalid",
            "native membership checkpoint evidence hash is invalid",
        ));
    }
    Ok(checkpoint)
}

pub(crate) fn clear_pending(
    repository: &Path,
    caravan_id: PrNumber,
) -> Result<(), crate::AppError> {
    crate::stack_checkpoint::remove(repository, &pending_key(caravan_id))
}

fn pending_key(caravan_id: PrNumber) -> String {
    format!("membership-{}", caravan_id.0)
}

#[derive(Debug)]
pub enum NativeMembershipError {
    InvalidPlan { code: String, message: String },
    Stack(GitHubStackMutationError),
}

impl std::fmt::Display for NativeMembershipError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPlan { code, message } => write!(formatter, "{code}: {message}"),
            Self::Stack(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativeMembershipError {}

impl From<GitHubStackMutationError> for NativeMembershipError {
    fn from(error: GitHubStackMutationError) -> Self {
        Self::Stack(error)
    }
}

/// Exact pre-membership facts this bridge is allowed to consume.
#[derive(Debug, Clone)]
pub struct NativeMembershipFacts<'a> {
    pub repository: RepositoryId,
    pub default_branch: &'a BranchSnapshot,
    pub caravans: &'a [Caravan],
    pub pull_requests: &'a std::collections::BTreeMap<PrNumber, PullRequestSnapshot>,
    pub native_stacks: &'a [NativeStackStatus],
}

impl<'a> NativeMembershipFacts<'a> {
    #[must_use]
    pub fn from_status(status: &'a StatusOutput) -> Self {
        Self {
            repository: status.repository.clone(),
            default_branch: &status.analysis.fleet.default_branch,
            caravans: &status.analysis.fleet.caravans,
            pull_requests: &status.analysis.pull_requests,
            native_stacks: &status.stack_backend.native_stacks,
        }
    }
}

/// Map one completed ordinary membership operation to its exact native action.
pub fn plan_native_membership(
    facts: &NativeMembershipFacts<'_>,
    output: &MembershipOutput,
    actor: &str,
) -> Result<NativeMembershipPlan, NativeMembershipError> {
    let operation_id = output.receipt.operation_id.0.clone();
    let repository = facts.repository.clone();
    if output.receipt.operation == "new" || output.receipt.operation == "renew" {
        return Ok(NativeMembershipPlan::AbsentSingleton {
            repository,
            operation_id,
            caravan_id: output.caravan_id,
            member: output.pull_request.number,
        });
    }
    if output.receipt.operation != "join" && output.receipt.operation != "rejoin" {
        return Err(invalid_plan(
            "github_stack_membership_operation_invalid",
            "native membership supports only new/renew/join/rejoin receipts",
        ));
    }

    let caravan = facts
        .caravans
        .iter()
        .find(|caravan| caravan.id == output.caravan_id)
        .ok_or_else(|| {
            invalid_plan(
                "github_stack_membership_caravan_missing",
                "the target caravan is absent from the exact pre-membership status",
            )
        })?;
    if caravan.members.contains(&output.pull_request.number) {
        return Err(invalid_plan(
            "github_stack_membership_candidate_already_present",
            "the candidate already belonged to the pre-membership caravan",
        ));
    }

    if caravan.members.len() == 1 {
        let root = facts
            .pull_requests
            .get(&caravan.members[0])
            .ok_or_else(|| {
                invalid_plan(
                    "github_stack_membership_root_missing",
                    "the singleton root facts are absent",
                )
            })?;
        let topology = topology_from_members(facts.default_branch, [root, &output.pull_request])?;
        return Ok(NativeMembershipPlan::Create {
            plan: Box::new(GitHubStackCreatePlan {
                operation_id,
                actor: actor.to_owned(),
                desired: topology,
            }),
        });
    }

    let matching = facts
        .native_stacks
        .iter()
        .filter(|native| native.caravan_id == Some(caravan.id))
        .collect::<Vec<_>>();
    let [native] = matching.as_slice() else {
        return Err(invalid_plan(
            "github_stack_membership_mapping_ambiguous",
            "an existing multi-member caravan must map to exactly one provider Stack",
        ));
    };
    if native.consistency != StackConsistency::Exact {
        return Err(invalid_plan(
            "github_stack_membership_generation_drifted",
            "the existing provider Stack must be exact before append",
        ));
    }
    Ok(NativeMembershipPlan::Add {
        repository,
        operation_id,
        actor: actor.to_owned(),
        stack_number: native.stack.number,
        expected_members: caravan.members.clone(),
        candidate: Box::new(output.pull_request.clone()),
    })
}

impl<R: CommandRunner> GitHubMutationAdapter<R> {
    /// Execute exactly one planned Stack representation mutation.
    ///
    /// The CRUD adapter fresh-reads and leases the provider generation; exact
    /// retries become zero-write already-satisfied receipts.
    pub fn converge_native_membership(
        &self,
        plan: &NativeMembershipPlan,
    ) -> Result<NativeMembershipReceipt, NativeMembershipError> {
        match plan {
            NativeMembershipPlan::AbsentSingleton {
                repository,
                operation_id,
                caravan_id,
                member,
            } => Ok(NativeMembershipReceipt::AbsentSingleton {
                repository: repository.clone(),
                operation_id: operation_id.clone(),
                caravan_id: *caravan_id,
                member: *member,
            }),
            NativeMembershipPlan::Create { plan } => self
                .native_stack_create(&plan.desired.base.repository, plan)
                .map(|receipt| NativeMembershipReceipt::StackMutation {
                    receipt: Box::new(receipt),
                })
                .map_err(Into::into),
            NativeMembershipPlan::Add {
                repository,
                operation_id,
                actor,
                stack_number,
                expected_members,
                candidate,
            } => {
                let before = self
                    .native_stack_generation(repository, *stack_number)?
                    .ok_or_else(|| {
                        invalid_plan(
                            "github_stack_membership_stack_missing",
                            "the exact provider Stack disappeared before append",
                        )
                    })?;
                let actual = before
                    .topology
                    .entries
                    .iter()
                    .map(|entry| entry.pr)
                    .collect::<Vec<_>>();
                let expected_full = expected_members
                    .iter()
                    .copied()
                    .chain(std::iter::once(candidate.number))
                    .collect::<Vec<_>>();
                if &actual != expected_members && actual != expected_full {
                    return Err(invalid_plan(
                        "github_stack_membership_members_drifted",
                        "provider Stack members changed before append",
                    ));
                }
                // An exact full generation means the prior add response was
                // lost. Reconstruct its expected prefix and let the CRUD
                // adapter return a sealed AlreadySatisfied receipt rather than
                // rejecting the successful continuation as member drift.
                let (expected_before, desired) = if actual == expected_full {
                    let desired = before.topology.clone();
                    let mut expected_before = before.clone();
                    expected_before
                        .topology
                        .entries
                        .truncate(expected_members.len());
                    let expected_candidate = entry_from_pull(
                        expected_members.len(),
                        candidate,
                        expected_before
                            .topology
                            .entries
                            .last()
                            .map(|entry| &entry.head),
                    )?;
                    if desired.entries.get(expected_members.len()) != Some(&expected_candidate) {
                        return Err(invalid_plan(
                            "github_stack_membership_candidate_drifted",
                            "already-appended provider entry differs from the exact candidate generation",
                        ));
                    }
                    (expected_before, desired)
                } else {
                    let mut desired = before.topology.clone();
                    desired.entries.push(entry_from_pull(
                        desired.entries.len(),
                        candidate,
                        desired.entries.last().map(|entry| &entry.head),
                    )?);
                    (before, desired)
                };
                self.native_stack_add(
                    repository,
                    &GitHubStackAddPlan {
                        operation_id: operation_id.clone(),
                        actor: actor.clone(),
                        before: expected_before,
                        desired,
                    },
                )
                .map(|receipt| NativeMembershipReceipt::StackMutation {
                    receipt: Box::new(receipt),
                })
                .map_err(Into::into)
            }
        }
    }
}

pub(crate) fn topology_from_members<'a>(
    base: &crate::model::BranchSnapshot,
    members: impl IntoIterator<Item = &'a PullRequestSnapshot>,
) -> Result<GitHubStackTopology, NativeMembershipError> {
    let mut entries = Vec::new();
    for member in members {
        let predecessor = entries
            .last()
            .map(|entry: &GitHubStackEntryGeneration| &entry.head);
        entries.push(entry_from_pull(entries.len(), member, predecessor)?);
    }
    Ok(GitHubStackTopology {
        base: base.clone(),
        entries,
    })
}

fn entry_from_pull(
    position: usize,
    pull: &PullRequestSnapshot,
    predecessor: Option<&crate::model::BranchSnapshot>,
) -> Result<GitHubStackEntryGeneration, NativeMembershipError> {
    if pull.state != PullRequestState::Open || pull.draft {
        return Err(invalid_plan(
            "github_stack_membership_candidate_ineligible",
            "Stack members must be open and non-draft",
        ));
    }
    if predecessor.is_some_and(|predecessor| &pull.base != predecessor) {
        return Err(invalid_plan(
            "github_stack_membership_base_chain_invalid",
            "member does not target the exact predecessor generation",
        ));
    }
    Ok(GitHubStackEntryGeneration {
        position: u32::try_from(position).map_err(|_| {
            invalid_plan(
                "github_stack_membership_too_large",
                "Stack position exceeds u32",
            )
        })?,
        pr: pull.number,
        stack_state: "open".to_owned(),
        pull_request_state: pull.state,
        draft: pull.draft,
        merged_at: pull.merged_at.clone(),
        base: pull.base.clone(),
        head: pull.head.clone(),
    })
}

fn invalid_plan(code: &str, message: &str) -> NativeMembershipError {
    NativeMembershipError::InvalidPlan {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{
        AutoMergeState, BranchSnapshot, Caravan, CommitOid, OperationId, OperationReceipt,
    };

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "acme".to_owned(),
            name: "widgets".to_owned(),
        }
    }

    fn branch(name: &str, oid: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(oid.to_owned()),
        }
    }

    fn pull(number: u64, base: BranchSnapshot) -> PullRequestSnapshot {
        PullRequestSnapshot {
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: String::new(),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&format!("head-{number}"), &format!("{number}aaaaa")),
            base,
            cross_repository: false,
            labels: std::collections::BTreeSet::from(["caravan".to_owned()]),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
            merge_state_status: None,
        }
    }

    fn output(
        operation: &str,
        candidate: PullRequestSnapshot,
        caravan_id: PrNumber,
    ) -> MembershipOutput {
        MembershipOutput {
            receipt: OperationReceipt {
                operation_id: OperationId("membership-op".to_owned()),
                operation: operation.to_owned(),
                completed_steps: Vec::new(),
                changed: true,
            },
            provider_api: crate::model::GitHubApiTelemetry::default(),
            rebase_receipt: None,
            provider_receipts: Vec::new(),
            native_stack_receipt: None,
            join_receipt: None,
            pull_request: candidate,
            caravan_id,
            coexisting_caravans: Vec::new(),
            admission_intent: None,
            admission_compatibility_authorization: None,
            events: Vec::new(),
            hook_deliveries: Vec::new(),
        }
    }

    fn facts<'a>(
        default_branch: &'a BranchSnapshot,
        caravans: &'a [Caravan],
        pulls: &'a BTreeMap<PrNumber, PullRequestSnapshot>,
        native_stacks: &'a [NativeStackStatus],
    ) -> NativeMembershipFacts<'a> {
        NativeMembershipFacts {
            repository: repository(),
            default_branch,
            caravans,
            pull_requests: pulls,
            native_stacks,
        }
    }

    #[test]
    fn a_new_root_is_an_explicit_absent_singleton_without_provider_stack() {
        let main = branch("main", "main000");
        let candidate = pull(101, main.clone());
        let output = output("new", candidate, PrNumber(101));

        let plan =
            plan_native_membership(&facts(&main, &[], &BTreeMap::new(), &[]), &output, "cara")
                .expect("a new root has no provider Stack");

        assert!(matches!(
            plan,
            NativeMembershipPlan::AbsentSingleton {
                member: PrNumber(101),
                caravan_id: PrNumber(101),
                ..
            }
        ));
    }

    #[test]
    fn joining_a_singleton_creates_the_exact_two_member_stack() {
        let main = branch("main", "main000");
        let root = pull(101, main.clone());
        let child = pull(102, root.head.clone());
        let caravans = vec![Caravan::new(vec![PrNumber(101)]).unwrap()];
        let pulls = BTreeMap::from([(PrNumber(101), root.clone())]);
        let output = output("join", child.clone(), PrNumber(101));

        let plan = plan_native_membership(&facts(&main, &caravans, &pulls, &[]), &output, "cara")
            .expect("singleton plus child creates one Stack");
        let NativeMembershipPlan::Create { plan } = plan else {
            panic!("expected create")
        };
        assert_eq!(plan.desired.base, main);
        assert_eq!(
            plan.desired
                .entries
                .iter()
                .map(|entry| entry.pr)
                .collect::<Vec<_>>(),
            vec![PrNumber(101), PrNumber(102)]
        );
        assert_eq!(plan.desired.entries[1].base, root.head);
        assert_eq!(plan.desired.entries[1].head, child.head);
    }

    #[test]
    fn failed_native_create_has_a_durable_exact_continuation_checkpoint() {
        let repository_path = tempfile::tempdir().unwrap();
        let initialized = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository_path.path())
            .output()
            .unwrap();
        assert!(initialized.status.success());
        let main = branch("main", "main000");
        let root = pull(101, main.clone());
        let child = pull(102, root.head.clone());
        let caravans = vec![Caravan::new(vec![PrNumber(101)]).unwrap()];
        let pulls = BTreeMap::from([(PrNumber(101), root)]);
        let output = output("join", child, PrNumber(101));
        let plan =
            plan_native_membership(&facts(&main, &caravans, &pulls, &[]), &output, "cara").unwrap();

        let written = persist_pending(repository_path.path(), &plan)
            .unwrap()
            .expect("a native create carries a continuation");
        let loaded = load_pending(repository_path.path(), PrNumber(101))
            .unwrap()
            .expect("continuation survives process boundaries");

        assert!(loaded.verify());
        assert_eq!(loaded, written);
        assert_eq!(loaded.plan, plan);
        clear_pending(repository_path.path(), PrNumber(101)).unwrap();
        assert!(
            load_pending(repository_path.path(), PrNumber(101))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn an_existing_exact_stack_plans_append_against_its_number_and_members() {
        let main = branch("main", "main000");
        let root = pull(101, main.clone());
        let child = pull(102, root.head.clone());
        let tail = pull(103, child.head.clone());
        let caravans = vec![Caravan::new(vec![PrNumber(101), PrNumber(102)]).unwrap()];
        let pulls = BTreeMap::from([(PrNumber(101), root), (PrNumber(102), child)]);
        let native = NativeStackStatus {
            stack: crate::github::GitHubStackSnapshot {
                id: 1,
                number: 42,
                node_id: "S_membership".to_owned(),
                base: crate::github::GitHubStackBase {
                    ref_name: "main".to_owned(),
                },
                open: true,
                created_at: "2026-08-01T09:00:00Z".to_owned(),
                pull_requests: Vec::new(),
            },
            caravan_id: Some(PrNumber(101)),
            consistency: StackConsistency::Exact,
            problems: Vec::new(),
        };
        let output = output("join", tail.clone(), PrNumber(101));

        let plan =
            plan_native_membership(&facts(&main, &caravans, &pulls, &[native]), &output, "cara")
                .expect("exact multi-member Stack appends");
        let NativeMembershipPlan::Add {
            stack_number,
            expected_members,
            candidate,
            ..
        } = plan
        else {
            panic!("expected add")
        };
        assert_eq!(stack_number, 42);
        assert_eq!(expected_members, vec![PrNumber(101), PrNumber(102)]);
        assert_eq!(*candidate, tail);
    }

    #[test]
    fn ambiguous_or_drifted_existing_stack_is_refused_before_provider_access() {
        let main = branch("main", "main000");
        let root = pull(101, main.clone());
        let child = pull(102, root.head.clone());
        let tail = pull(103, child.head.clone());
        let caravans = vec![Caravan::new(vec![PrNumber(101), PrNumber(102)]).unwrap()];
        let pulls = BTreeMap::from([(PrNumber(101), root), (PrNumber(102), child)]);
        let output = output("join", tail, PrNumber(101));
        let mut native = NativeStackStatus {
            stack: crate::github::GitHubStackSnapshot {
                id: 1,
                number: 42,
                node_id: "S_membership".to_owned(),
                base: crate::github::GitHubStackBase {
                    ref_name: "main".to_owned(),
                },
                open: true,
                created_at: String::new(),
                pull_requests: Vec::new(),
            },
            caravan_id: Some(PrNumber(101)),
            consistency: StackConsistency::Drifted,
            problems: Vec::new(),
        };

        assert!(matches!(
            plan_native_membership(
                &facts(&main, &caravans, &pulls, std::slice::from_ref(&native)),
                &output,
                "cara",
            ),
            Err(NativeMembershipError::InvalidPlan { code, .. })
                if code == "github_stack_membership_generation_drifted"
        ));
        native.consistency = StackConsistency::Exact;
        assert!(matches!(
            plan_native_membership(
                &facts(&main, &caravans, &pulls, &[native.clone(), native]),
                &output,
                "cara",
            ),
            Err(NativeMembershipError::InvalidPlan { code, .. })
                if code == "github_stack_membership_mapping_ambiguous"
        ));
    }
}
