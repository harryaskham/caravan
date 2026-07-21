//! Resumable membership mutation state and exact optimistic primitives.

use super::{
    AppError, ControlLabelAudit, GitHubMutationReceipt, MembershipOperation, MembershipProvider,
    MergeMethod, MutationKind, MutationStep, MutationStepState, OperationId, OperationReceipt,
    PullRequestPrecondition, PullRequestSnapshot, RepositoryId, comment_error, mutation_error,
};

pub(super) struct ExecutionState {
    pub(super) operation_id: OperationId,
    pub(super) operation: MembershipOperation,
    pub(super) steps: Vec<MutationStep>,
    pub(super) provider_receipts: Vec<GitHubMutationReceipt>,
    pub(super) current: Option<PullRequestSnapshot>,
}

impl ExecutionState {
    pub(super) fn new(operation: MembershipOperation) -> Self {
        Self {
            operation_id: OperationId::new(),
            operation,
            steps: Vec::new(),
            provider_receipts: Vec::new(),
            current: None,
        }
    }

    pub(super) fn operation_receipt(&self) -> OperationReceipt {
        OperationReceipt {
            operation_id: self.operation_id.clone(),
            operation: self.operation.name().to_owned(),
            changed: self
                .steps
                .iter()
                .any(|step| step.state == MutationStepState::Completed),
            completed_steps: self.steps.clone(),
        }
    }

    pub(super) fn record(&mut self, receipt: GitHubMutationReceipt, summary: &str) {
        let number = receipt.after.number;
        self.current = Some(receipt.after.clone());
        self.steps.push(MutationStep {
            kind: receipt.kind,
            state: MutationStepState::Completed,
            pr: Some(number),
            summary: summary.to_owned(),
        });
        self.provider_receipts.push(receipt);
    }

    pub(super) fn already(&mut self, kind: MutationKind, summary: &str) {
        self.steps.push(MutationStep {
            kind,
            state: MutationStepState::AlreadySatisfied,
            pr: self
                .current
                .as_ref()
                .map(|pull_request| pull_request.number),
            summary: summary.to_owned(),
        });
    }

    pub(super) fn precondition(&self) -> PullRequestPrecondition {
        PullRequestPrecondition::from(
            self.current
                .as_ref()
                .expect("membership execution has current PR facts"),
        )
    }

    pub(super) fn ensure_base(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
        base: &str,
    ) -> Result<(), AppError> {
        if self.current.as_ref().expect("current PR").base.name == base {
            self.already(
                MutationKind::SetBase,
                "PR already targets the required base",
            );
            return Ok(());
        }
        let receipt = provider
            .set_base(repository, &self.precondition(), base)
            .map_err(|error| mutation_error(&error, self))?;
        self.record(receipt, "changed PR base branch");
        Ok(())
    }

    pub(super) fn ensure_label_present(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
        label: &str,
    ) -> Result<(), AppError> {
        if self.current.as_ref().expect("current PR").has_label(label) {
            self.already(
                MutationKind::AddLabel,
                &format!("label `{label}` already present"),
            );
            return Ok(());
        }
        let receipt = provider
            .add_label(repository, &self.precondition(), label)
            .map_err(|error| mutation_error(&error, self))?;
        self.record(receipt, &format!("added label `{label}`"));
        Ok(())
    }

    pub(super) fn ensure_label_absent(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
        label: &str,
    ) -> Result<(), AppError> {
        if !self.current.as_ref().expect("current PR").has_label(label) {
            self.already(
                MutationKind::RemoveLabel,
                &format!("label `{label}` already absent"),
            );
            return Ok(());
        }
        let receipt = provider
            .remove_label(repository, &self.precondition(), label)
            .map_err(|error| mutation_error(&error, self))?;
        self.record(receipt, &format!("removed label `{label}`"));
        Ok(())
    }

    pub(super) fn ensure_control_label_comment(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
        audit: &ControlLabelAudit,
    ) -> Result<(), AppError> {
        let receipt = provider
            .ensure_control_label_comment(repository, &self.precondition(), audit)
            .map_err(|error| comment_error(&error, self))?;
        let already = receipt
            .provider_output
            .as_deref()
            .is_some_and(|output| output.starts_with("existing GitHub comment"));
        if already {
            self.already(
                MutationKind::Comment,
                "control-label audit comment already present",
            );
            self.current = Some(receipt.after);
        } else {
            self.record(receipt, "posted durable control-label audit comment");
        }
        Ok(())
    }

    pub(super) fn ensure_squash_auto_merge(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
    ) -> Result<(), AppError> {
        let current = self.current.as_ref().expect("current PR");
        if current.auto_merge.enabled
            && current.auto_merge.merge_method == Some(MergeMethod::Squash)
        {
            self.already(
                MutationKind::EnableAutoMerge,
                "squash auto-merge already enabled",
            );
            return Ok(());
        }
        let receipt = provider
            .enable_squash_auto_merge(repository, &self.precondition())
            .map_err(|error| mutation_error(&error, self))?;
        self.record(receipt, "enabled squash auto-merge");
        Ok(())
    }

    pub(super) fn ensure_auto_merge_disabled(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
    ) -> Result<(), AppError> {
        if !self
            .current
            .as_ref()
            .expect("current PR")
            .auto_merge
            .enabled
        {
            self.already(
                MutationKind::DisableAutoMerge,
                "auto-merge already disabled",
            );
            return Ok(());
        }
        let receipt = provider
            .disable_auto_merge(repository, &self.precondition())
            .map_err(|error| mutation_error(&error, self))?;
        self.record(receipt, "disabled auto-merge");
        Ok(())
    }
}
