//! Ordered, lock-fenced native Stack landing transaction.
//!
//! The provider adapters below this layer are deliberately policy-free: each
//! performs one exact provider step. This module owns the *order* those steps
//! must occur in, and the one property no individual adapter can guarantee —
//! that the complete-group ruleset lock is released exactly once, only after
//! terminal provider proof, and never while an outcome is still unresolved.
//!
//! Nothing here opens the workflow fence. `github_stack_backend_read_only` (and
//! the repository opt-in and capability gates ahead of it) still decide whether
//! a caller may reach this transaction at all.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    GitHubMutationAdapter, GitHubStackAsyncMergePlan, GitHubStackBranchLockError,
    GitHubStackBranchLockGeneration, GitHubStackBranchLockReceipt,
    GitHubStackLockedMergeCheckpoint, GitHubStackLockedMergeReceipt, GitHubStackMergeError,
    GitHubStackMergeStatus,
};
use crate::command::CommandRunner;
use crate::model::RepositoryId;

const SCHEMA_VERSION: u32 = 1;

/// Ordered landing phase. The order is the safety property: a lock is never
/// released before terminal proof, and terminal proof is never reached without
/// a verified lock.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GitHubStackLandPhase {
    Planned,
    Locked,
    Submitted,
    Terminal,
    Released,
}

/// Resumable landing state. Self-contained so a new process can continue from
/// durable evidence alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackLandCheckpoint {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub plan: GitHubStackAsyncMergePlan,
    pub phase: GitHubStackLandPhase,
    /// Always false: unstack/lock/merge/release are separate provider calls.
    pub provider_atomic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_lock: Option<GitHubStackBranchLockGeneration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<GitHubStackLockedMergeCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_status: Option<GitHubStackMergeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_release: Option<GitHubStackBranchLockReceipt>,
    pub evidence_hash: String,
}

impl GitHubStackLandCheckpoint {
    fn seal(mut self) -> Self {
        self.evidence_hash.clear();
        let material = serde_json::to_vec(&self).expect("Stack land checkpoint serializes");
        self.evidence_hash = crate::membership::fnv1a64(&material);
        self
    }

    #[must_use]
    pub fn verify(&self) -> bool {
        let expected = self.evidence_hash.clone();
        let mut material = self.clone();
        material.evidence_hash.clear();
        self.schema_version == SCHEMA_VERSION
            && !self.provider_atomic
            && phase_shape(self).is_ok()
            && serde_json::to_vec(&material)
                .ok()
                .is_some_and(|bytes| crate::membership::fnv1a64(&bytes) == expected)
    }

    /// A lock this transaction still owns and must eventually release.
    ///
    /// Present for every non-terminal-and-unreleased state, so an interrupted
    /// run can always be identified as lock-holding rather than silently
    /// leaving a repository ruleset behind.
    #[must_use]
    pub fn outstanding_lock(&self) -> Option<&GitHubStackBranchLockGeneration> {
        match self.phase {
            GitHubStackLandPhase::Planned | GitHubStackLandPhase::Released => None,
            GitHubStackLandPhase::Locked
            | GitHubStackLandPhase::Submitted
            | GitHubStackLandPhase::Terminal => self.branch_lock.as_ref(),
        }
    }
}

/// Typed refusal from the ordered landing transaction.
#[derive(Debug)]
pub enum GitHubStackLandError {
    InvalidCheckpoint {
        diagnostic: String,
    },
    OutOfOrder {
        expected: String,
        actual: GitHubStackLandPhase,
    },
    Lock(Box<GitHubStackBranchLockError>),
    Merge(Box<GitHubStackMergeError>),
}

impl std::fmt::Display for GitHubStackLandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCheckpoint { diagnostic } => {
                write!(formatter, "invalid Stack land checkpoint: {diagnostic}")
            }
            Self::OutOfOrder { expected, actual } => write!(
                formatter,
                "Stack land step requires {expected}, observed {actual:?}"
            ),
            Self::Lock(error) => error.fmt(formatter),
            Self::Merge(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GitHubStackLandError {}

impl From<GitHubStackBranchLockError> for GitHubStackLandError {
    fn from(error: GitHubStackBranchLockError) -> Self {
        Self::Lock(Box::new(error))
    }
}

impl From<GitHubStackMergeError> for GitHubStackLandError {
    fn from(error: GitHubStackMergeError) -> Self {
        Self::Merge(Box::new(error))
    }
}

impl<R: CommandRunner> GitHubMutationAdapter<R> {
    /// Begin one landing transaction from an exact reviewed merge plan.
    #[must_use]
    pub fn native_stack_land_begin(
        repository: &RepositoryId,
        plan: &GitHubStackAsyncMergePlan,
    ) -> GitHubStackLandCheckpoint {
        GitHubStackLandCheckpoint {
            schema_version: SCHEMA_VERSION,
            repository: repository.clone(),
            plan: plan.clone(),
            phase: GitHubStackLandPhase::Planned,
            provider_atomic: false,
            branch_lock: None,
            merge: None,
            terminal_status: None,
            lock_release: None,
            evidence_hash: String::new(),
        }
        .seal()
    }

    /// Acquire the complete-group ruleset lock before any merge submission.
    pub fn native_stack_land_lock(
        &self,
        repository: &RepositoryId,
        checkpoint: &GitHubStackLandCheckpoint,
    ) -> Result<GitHubStackLandCheckpoint, GitHubStackLandError> {
        validate(repository, checkpoint)?;
        if checkpoint.phase >= GitHubStackLandPhase::Locked {
            return Ok(checkpoint.clone());
        }
        let receipt =
            self.native_stack_branch_lock_acquire(repository, &checkpoint.plan.branch_lock_plan())?;
        let lock = receipt
            .lock
            .ok_or_else(|| GitHubStackLandError::InvalidCheckpoint {
                diagnostic: "lock acquisition returned no exact ruleset generation".to_owned(),
            })?;
        let mut next = checkpoint.clone();
        next.phase = GitHubStackLandPhase::Locked;
        next.branch_lock = Some(lock);
        Ok(next.seal())
    }

    /// Submit the merge under the verified lock.
    pub fn native_stack_land_submit(
        &self,
        repository: &RepositoryId,
        checkpoint: &GitHubStackLandCheckpoint,
    ) -> Result<GitHubStackLandCheckpoint, GitHubStackLandError> {
        validate(repository, checkpoint)?;
        if checkpoint.phase >= GitHubStackLandPhase::Submitted {
            return Ok(checkpoint.clone());
        }
        let lock = match checkpoint.phase {
            GitHubStackLandPhase::Locked => checkpoint
                .branch_lock
                .as_ref()
                .expect("a locked checkpoint carries its exact lock"),
            actual => {
                return Err(GitHubStackLandError::OutOfOrder {
                    expected: "an acquired complete-group lock".to_owned(),
                    actual,
                });
            }
        };
        let receipt = self.native_stack_merge_submit_locked(repository, &checkpoint.plan, lock)?;
        Ok(advance(checkpoint, &receipt))
    }

    /// Poll the async result under the same verified lock.
    pub fn native_stack_land_poll(
        &self,
        repository: &RepositoryId,
        checkpoint: &GitHubStackLandCheckpoint,
    ) -> Result<GitHubStackLandCheckpoint, GitHubStackLandError> {
        validate(repository, checkpoint)?;
        if checkpoint.phase >= GitHubStackLandPhase::Terminal {
            return Ok(checkpoint.clone());
        }
        let merge = match (checkpoint.phase, checkpoint.merge.as_ref()) {
            (GitHubStackLandPhase::Submitted, Some(merge)) => merge,
            (actual, _) => {
                return Err(GitHubStackLandError::OutOfOrder {
                    expected: "a submitted merge with a durable UUID checkpoint".to_owned(),
                    actual,
                });
            }
        };
        let receipt = self.native_stack_merge_poll_locked(repository, merge)?;
        Ok(advance(checkpoint, &receipt))
    }

    /// Release the lock, and only after terminal provider proof.
    ///
    /// A pending or still-submitted transaction is refused here rather than
    /// quietly unlocking the selected refs mid-flight, which is exactly the
    /// lower-head race the lock exists to prevent.
    pub fn native_stack_land_release(
        &self,
        repository: &RepositoryId,
        checkpoint: &GitHubStackLandCheckpoint,
    ) -> Result<GitHubStackLandCheckpoint, GitHubStackLandError> {
        validate(repository, checkpoint)?;
        if checkpoint.phase == GitHubStackLandPhase::Released {
            return Ok(checkpoint.clone());
        }
        let lock = match (checkpoint.phase, checkpoint.branch_lock.as_ref()) {
            (GitHubStackLandPhase::Terminal, Some(lock)) => lock,
            (actual, _) => {
                return Err(GitHubStackLandError::OutOfOrder {
                    expected: "terminal provider proof for every selected entry".to_owned(),
                    actual,
                });
            }
        };
        let receipt = self.native_stack_branch_lock_release(
            repository,
            &checkpoint.plan.branch_lock_plan(),
            lock,
        )?;
        let mut next = checkpoint.clone();
        next.phase = GitHubStackLandPhase::Released;
        next.lock_release = Some(receipt);
        Ok(next.seal())
    }
}

fn advance(
    checkpoint: &GitHubStackLandCheckpoint,
    receipt: &GitHubStackLockedMergeReceipt,
) -> GitHubStackLandCheckpoint {
    let mut next = checkpoint.clone();
    next.branch_lock = Some(receipt.branch_lock.clone());
    next.merge.clone_from(&receipt.checkpoint);
    match receipt.merge.status {
        // Terminal in the provider's own vocabulary, including the sealed
        // `indeterminate` outcome, which must still release its lock rather
        // than leaving a repository ruleset behind forever.
        GitHubStackMergeStatus::Merged
        | GitHubStackMergeStatus::Failed
        | GitHubStackMergeStatus::Indeterminate => {
            next.phase = GitHubStackLandPhase::Terminal;
            next.terminal_status = Some(receipt.merge.status);
        }
        GitHubStackMergeStatus::Submitted
        | GitHubStackMergeStatus::Pending
        | GitHubStackMergeStatus::Enqueued => {
            next.phase = GitHubStackLandPhase::Submitted;
        }
    }
    next.seal()
}

fn validate(
    repository: &RepositoryId,
    checkpoint: &GitHubStackLandCheckpoint,
) -> Result<(), GitHubStackLandError> {
    if &checkpoint.repository != repository || !checkpoint.verify() {
        return Err(GitHubStackLandError::InvalidCheckpoint {
            diagnostic: "schema, repository, phase evidence, or hash is invalid".to_owned(),
        });
    }
    Ok(())
}

fn phase_shape(checkpoint: &GitHubStackLandCheckpoint) -> Result<(), ()> {
    let ok = match checkpoint.phase {
        GitHubStackLandPhase::Planned => {
            checkpoint.branch_lock.is_none()
                && checkpoint.merge.is_none()
                && checkpoint.terminal_status.is_none()
                && checkpoint.lock_release.is_none()
        }
        GitHubStackLandPhase::Locked => {
            checkpoint.branch_lock.is_some()
                && checkpoint.merge.is_none()
                && checkpoint.terminal_status.is_none()
                && checkpoint.lock_release.is_none()
        }
        GitHubStackLandPhase::Submitted => {
            checkpoint.branch_lock.is_some()
                && checkpoint.merge.is_some()
                && checkpoint.terminal_status.is_none()
                && checkpoint.lock_release.is_none()
        }
        GitHubStackLandPhase::Terminal => {
            checkpoint.branch_lock.is_some()
                && checkpoint.terminal_status.is_some()
                && checkpoint.lock_release.is_none()
        }
        GitHubStackLandPhase::Released => {
            checkpoint.branch_lock.is_some()
                && checkpoint.terminal_status.is_some()
                && checkpoint.lock_release.is_some()
        }
    };
    if ok { Ok(()) } else { Err(()) }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;
    use crate::command::{CommandOutput, CommandRunError, CommandSpec};
    use crate::github::{
        GitHubStackEntryGeneration, GitHubStackGeneration, GitHubStackMergeCheckpoint,
        GitHubStackMergeReceipt, GitHubStackMergeRequestIdentity, GitHubStackTopology,
    };
    use crate::model::{BranchSnapshot, CommitOid, PrNumber, PullRequestState};

    struct NeverRuns;

    impl CommandRunner for NeverRuns {
        fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            panic!("ordering must be refused before provider access: {command:?}");
        }
    }

    struct Fake(RefCell<VecDeque<CommandOutput>>);

    impl CommandRunner for Fake {
        fn run(&self, _command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            Ok(self.0.borrow_mut().pop_front().expect("unexpected command"))
        }
    }

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

    fn entry(position: u32, number: u64, base: BranchSnapshot) -> GitHubStackEntryGeneration {
        GitHubStackEntryGeneration {
            position,
            pr: PrNumber(number),
            stack_state: "open".to_owned(),
            pull_request_state: PullRequestState::Open,
            draft: false,
            merged_at: None,
            base,
            head: branch(&format!("head-{number}"), &format!("{number}aaaaaa")),
        }
    }

    fn plan() -> GitHubStackAsyncMergePlan {
        let base = branch("main", "base000");
        let root = entry(0, 101, base.clone());
        let child = entry(1, 102, root.head.clone());
        let topology = GitHubStackTopology {
            base: base.clone(),
            entries: vec![root.clone(), child.clone()],
        };
        GitHubStackAsyncMergePlan {
            operation_id: "land-op".to_owned(),
            actor: "cara-test".to_owned(),
            before: GitHubStackGeneration {
                id: 5_001,
                number: 42,
                node_id: "S_land".to_owned(),
                open: true,
                created_at: "2026-08-01T09:00:00Z".to_owned(),
                topology,
            },
            selected: vec![root, child],
            merge_method: crate::github::GitHubStackMergeMethod::Squash,
            merge_action: crate::github::GitHubStackMergeAction::DirectMerge,
        }
    }

    fn lock() -> GitHubStackBranchLockGeneration {
        let plan = plan();
        let mut selected_refs = plan
            .selected
            .iter()
            .map(|entry| format!("refs/heads/{}", entry.head.name))
            .collect::<Vec<_>>();
        selected_refs.sort();
        GitHubStackBranchLockGeneration {
            id: 77,
            node_id: "RRS_land".to_owned(),
            name: "cara-stack-merge-lock-land".to_owned(),
            repository: repository(),
            source: repository().slug(),
            source_type: "Repository".to_owned(),
            target: "branch".to_owned(),
            enforcement: "active".to_owned(),
            selected_pull_requests: plan.selected.iter().map(|entry| entry.pr).collect(),
            selected_refs,
            current_user_can_bypass: "never".to_owned(),
            created_at: "2026-08-01T10:00:00Z".to_owned(),
            updated_at: "2026-08-01T10:00:00Z".to_owned(),
        }
    }

    fn request() -> GitHubStackMergeRequestIdentity {
        let plan = plan();
        GitHubStackMergeRequestIdentity {
            method: "PUT".to_owned(),
            path: format!("repos/{}/stacks/42/merge-async", repository().slug()),
            selected_top: PrNumber(102),
            selected_top_sha: plan
                .selected
                .last()
                .expect("a non-empty selected prefix")
                .head
                .oid
                .clone(),
            ordered_pull_requests: plan.selected.iter().map(|entry| entry.pr).collect(),
            ordered_heads: plan
                .selected
                .iter()
                .map(|entry| entry.head.oid.clone())
                .collect(),
            github_request_id: None,
        }
    }

    fn merge_checkpoint() -> GitHubStackLockedMergeCheckpoint {
        GitHubStackLockedMergeCheckpoint {
            schema_version: 1,
            repository: repository(),
            merge: GitHubStackMergeCheckpoint {
                schema_version: 1,
                repository: repository(),
                plan: plan(),
                request: request(),
                uuid: "0f2ec4a1-3f9d-4a1a-9f7e-1c2d3e4f5a6b".to_owned(),
                initial_provider_status: "submitted".to_owned(),
                evidence_hash: String::new(),
            },
            branch_lock: lock(),
            evidence_hash: String::new(),
        }
    }

    fn locked_receipt(status: GitHubStackMergeStatus) -> GitHubStackLockedMergeReceipt {
        GitHubStackLockedMergeReceipt {
            schema_version: 1,
            merge: GitHubStackMergeReceipt {
                schema_version: 1,
                repository: repository(),
                plan: plan(),
                request: request(),
                checkpoint: Some(merge_checkpoint().merge),
                status,
                provider_status: None,
                provider_message: None,
                provider_sha: None,
                observation: None,
                evidence_hash: String::new(),
            },
            branch_lock: lock(),
            branch_lock_verified: true,
            checkpoint: Some(merge_checkpoint()),
            evidence_hash: String::new(),
        }
    }

    fn begin() -> GitHubStackLandCheckpoint {
        GitHubMutationAdapter::<NeverRuns>::native_stack_land_begin(&repository(), &plan())
    }

    #[test]
    fn a_pending_merge_never_releases_the_complete_group_lock() {
        let submitted = advance(&begin(), &locked_receipt(GitHubStackMergeStatus::Submitted));
        assert_eq!(submitted.phase, GitHubStackLandPhase::Submitted);
        assert!(submitted.outstanding_lock().is_some());

        let pending = advance(&submitted, &locked_receipt(GitHubStackMergeStatus::Pending));
        assert_eq!(pending.phase, GitHubStackLandPhase::Submitted);
        assert!(pending.verify());

        let adapter = GitHubMutationAdapter::new(NeverRuns);
        assert!(matches!(
            adapter.native_stack_land_release(&repository(), &pending),
            Err(GitHubStackLandError::OutOfOrder { .. })
        ));
    }

    #[test]
    fn every_terminal_outcome_including_indeterminate_must_release_its_lock() {
        for status in [
            GitHubStackMergeStatus::Merged,
            GitHubStackMergeStatus::Failed,
            GitHubStackMergeStatus::Indeterminate,
        ] {
            let terminal = advance(&begin(), &locked_receipt(status));
            assert_eq!(terminal.phase, GitHubStackLandPhase::Terminal);
            assert_eq!(terminal.terminal_status, Some(status));
            assert!(
                terminal.outstanding_lock().is_some(),
                "a terminal transaction still owns its ruleset until release"
            );
            assert!(terminal.verify());
        }
    }

    #[test]
    fn submission_and_polling_are_refused_out_of_order_before_provider_access() {
        let adapter = GitHubMutationAdapter::new(NeverRuns);
        let planned = begin();
        assert_eq!(planned.phase, GitHubStackLandPhase::Planned);
        assert!(planned.outstanding_lock().is_none());
        assert!(!planned.provider_atomic);

        assert!(matches!(
            adapter.native_stack_land_submit(&repository(), &planned),
            Err(GitHubStackLandError::OutOfOrder { .. })
        ));
        assert!(matches!(
            adapter.native_stack_land_poll(&repository(), &planned),
            Err(GitHubStackLandError::OutOfOrder { .. })
        ));
        assert!(matches!(
            adapter.native_stack_land_release(&repository(), &planned),
            Err(GitHubStackLandError::OutOfOrder { .. })
        ));
    }

    #[test]
    fn a_tampered_or_foreign_checkpoint_is_rejected_before_provider_access() {
        let adapter = GitHubMutationAdapter::new(NeverRuns);
        let mut tampered = begin();
        tampered.provider_atomic = true;
        assert!(matches!(
            adapter.native_stack_land_lock(&repository(), &tampered),
            Err(GitHubStackLandError::InvalidCheckpoint { .. })
        ));

        let foreign = RepositoryId {
            owner: "other".to_owned(),
            name: "repo".to_owned(),
        };
        assert!(matches!(
            adapter.native_stack_land_lock(&foreign, &begin()),
            Err(GitHubStackLandError::InvalidCheckpoint { .. })
        ));

        // A phase that claims progress it has no evidence for cannot be sealed
        // into a valid checkpoint.
        let mut lying = begin();
        lying.phase = GitHubStackLandPhase::Terminal;
        assert!(!lying.verify());
    }

    #[test]
    fn completed_steps_are_idempotent_replays_without_provider_writes() {
        let adapter = GitHubMutationAdapter::new(Fake(RefCell::new(VecDeque::new())));
        let terminal = advance(&begin(), &locked_receipt(GitHubStackMergeStatus::Merged));

        let replayed = adapter
            .native_stack_land_lock(&repository(), &terminal)
            .expect("an already-locked transaction replays");
        assert_eq!(replayed, terminal);
        let replayed = adapter
            .native_stack_land_submit(&repository(), &terminal)
            .expect("an already-submitted transaction replays");
        assert_eq!(replayed, terminal);
        let replayed = adapter
            .native_stack_land_poll(&repository(), &terminal)
            .expect("an already-terminal transaction replays");
        assert_eq!(replayed, terminal);
    }
}
