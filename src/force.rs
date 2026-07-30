//! Audited, exact-generation force-intent arm and revoke operations.
//!
//! These operations only manage the one-shot `caravan-force` intent label.
//! Normal sync remains the sole owner of final CI observation and administrator
//! squash merge execution.

use std::collections::BTreeSet;
use std::time::Duration;

use clap::Args;
use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::github::{
    ControlLabelAudit, GitHubMutationAdapter, GitHubMutationReceipt, MutationError,
    control_label_marker,
};
use crate::model::{
    BranchSnapshot, CheckSnapshot, CompatibilityOutcome, MutationKind, MutationStep,
    MutationStepState, OperationId, OperationReceipt, PrNumber, PullRequestPrecondition,
    PullRequestSnapshot, PullRequestState, RepositoryId,
};
use crate::operation_lock::OperationLock;
use crate::read::StatusOutput;
use crate::{AppContext, AppError};

const FORCE_LABEL: &str = "caravan-force";
const MAX_ACTOR_BYTES: usize = 500;
const MAX_REASON_BYTES: usize = 2_000;

/// Exact operator identity and rationale for force-intent mutation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Args)]
pub struct ForceIntentInput {
    /// Exact active caravan head PR.
    #[arg(long, value_name = "PR")]
    pub pr: u64,
    /// Audited operator identity (non-secret).
    #[arg(long, value_name = "ACTOR")]
    pub actor: String,
    /// Bounded operator rationale.
    #[arg(long, value_name = "TEXT")]
    pub reason: String,
}

/// Force intent transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ForceIntentOperation {
    Arm,
    Revoke,
}

impl ForceIntentOperation {
    fn name(self) -> &'static str {
        match self {
            Self::Arm => "force_arm",
            Self::Revoke => "force_revoke",
        }
    }
}

/// Stable exact receipt for one force intent transition.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ForceIntentOutput {
    pub receipt: OperationReceipt,
    pub operation: ForceIntentOperation,
    pub repository: RepositoryId,
    pub pr: PrNumber,
    pub head: BranchSnapshot,
    pub default_branch: BranchSnapshot,
    #[serde(default)]
    pub before_labels: BTreeSet<String>,
    #[serde(default)]
    pub after_labels: BTreeSet<String>,
    #[serde(default)]
    pub observed_checks: Vec<CheckSnapshot>,
    pub actor: String,
    pub reason: String,
    pub mutated: bool,
    pub intent_present: bool,
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    pub next: String,
}

trait ForceProvider {
    fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError>;
    fn viewer_permission(&self, repository: &RepositoryId) -> Result<String, MutationError>;
    fn add_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError>;
    fn remove_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError>;
    fn ensure_control_label_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        audit: &ControlLabelAudit,
    ) -> Result<GitHubMutationReceipt, MutationError>;
}

impl<R: crate::command::CommandRunner> ForceProvider for GitHubMutationAdapter<R> {
    fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError> {
        self.verify_branch_head(repository, branch, expected)
    }

    fn viewer_permission(&self, repository: &RepositoryId) -> Result<String, MutationError> {
        self.viewer_permission(repository)
    }

    fn add_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.add_label(repository, expected, label)
    }

    fn remove_label(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        label: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.remove_label(repository, expected, label)
    }

    fn ensure_control_label_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        audit: &ControlLabelAudit,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.ensure_control_label_comment(repository, expected, audit)
    }
}

struct ForceState {
    operation_id: OperationId,
    operation: ForceIntentOperation,
    current: PullRequestSnapshot,
    steps: Vec<MutationStep>,
    provider_receipts: Vec<GitHubMutationReceipt>,
}

impl ForceState {
    fn new(operation: ForceIntentOperation, current: PullRequestSnapshot) -> Self {
        Self {
            operation_id: OperationId::new(),
            operation,
            current,
            steps: Vec::new(),
            provider_receipts: Vec::new(),
        }
    }

    fn precondition(&self) -> PullRequestPrecondition {
        PullRequestPrecondition::from(&self.current)
    }

    fn already(&mut self, kind: MutationKind, summary: &str) {
        self.steps.push(MutationStep {
            kind,
            state: MutationStepState::AlreadySatisfied,
            pr: Some(self.current.number),
            summary: summary.to_owned(),
        });
    }

    fn record(&mut self, receipt: GitHubMutationReceipt, summary: &str) {
        self.current = receipt.after.clone();
        self.steps.push(MutationStep {
            kind: receipt.kind,
            state: MutationStepState::Completed,
            pr: Some(self.current.number),
            summary: summary.to_owned(),
        });
        self.provider_receipts.push(receipt);
    }

    fn operation_receipt(&self) -> OperationReceipt {
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
}

/// Arm exact-generation one-shot force intent.
pub fn arm(context: &AppContext, input: &ForceIntentInput) -> Result<ForceIntentOutput, AppError> {
    execute_live(context, input, ForceIntentOperation::Arm)
}

/// Revoke exact-generation one-shot force intent.
pub fn revoke(
    context: &AppContext,
    input: &ForceIntentInput,
) -> Result<ForceIntentOutput, AppError> {
    execute_live(context, input, ForceIntentOperation::Revoke)
}

fn execute_live(
    context: &AppContext,
    input: &ForceIntentInput,
    operation: ForceIntentOperation,
) -> Result<ForceIntentOutput, AppError> {
    validate_input(input)?;
    let _lock = OperationLock::acquire(&context.repository_path, operation.name())?;
    let timeout = Duration::from_secs(context.config.command_timeout_secs);
    let deadline =
        std::time::Instant::now() + Duration::from_secs(context.config.sync.max_duration_secs);
    let bound = crate::read::status_for_remote_candidate_with_deadline(
        context,
        PrNumber(input.pr),
        deadline,
        None,
    )?;
    let provider = GitHubMutationAdapter::new(
        crate::command::ProcessRunner::in_directory(&context.repository_path)
            .with_timeout(timeout)
            .with_operation_deadline(bound.exact_deadline),
    );
    execute(&bound.status, &provider, context, input, operation)
}

fn validate_input(input: &ForceIntentInput) -> Result<(), AppError> {
    let actor = input.actor.trim();
    let reason = input.reason.trim();
    if actor.is_empty() || actor.len() > MAX_ACTOR_BYTES {
        return Err(AppError::validation(
            "force_actor_invalid",
            format!("--actor must contain 1..={MAX_ACTOR_BYTES} bytes"),
        ));
    }
    if reason.is_empty() || reason.len() > MAX_REASON_BYTES {
        return Err(AppError::validation(
            "force_reason_invalid",
            format!("--reason must contain 1..={MAX_REASON_BYTES} bytes"),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute(
    status: &StatusOutput,
    provider: &impl ForceProvider,
    context: &AppContext,
    input: &ForceIntentInput,
    operation: ForceIntentOperation,
) -> Result<ForceIntentOutput, AppError> {
    crate::initialization::require_ready(&status.initialization)?;
    if operation == ForceIntentOperation::Arm && !context.config.force_merge {
        return Err(force_validation(
            "force_policy_disabled",
            "force intent cannot be armed while force_merge=false",
            status,
            input,
        ));
    }
    let pr = PrNumber(input.pr);
    let current = status
        .analysis
        .pull_requests
        .get(&pr)
        .cloned()
        .ok_or_else(|| {
            force_validation(
                "force_pr_not_found",
                "selected PR is not present in fresh provider discovery",
                status,
                input,
            )
        })?;
    if current.state != PullRequestState::Open
        || current.draft
        || !current.has_label("caravan")
        || current.has_label("caravan-evicted")
        || current.cross_repository
        || current.head.repository != status.repository
    {
        return Err(force_validation(
            "force_pr_ineligible",
            "force intent requires an open, non-draft, owned, non-evicted active head",
            status,
            input,
        ));
    }
    let caravan = status.analysis.fleet.containing(pr).ok_or_else(|| {
        force_validation(
            "force_pr_not_active_member",
            "force intent requires an active Caravan member",
            status,
            input,
        )
    })?;
    if caravan.head() != Some(pr) {
        return Err(force_validation(
            "force_pr_not_head",
            "force intent may be armed or revoked only on the current caravan head",
            status,
            input,
        ));
    }
    if !status.analysis.fleet.problems.is_empty() {
        return Err(force_validation(
            "force_graph_invalid",
            "force intent cannot bypass unresolved Caravan graph problems",
            status,
            input,
        ));
    }
    if status
        .pauses
        .iter()
        .any(|pause| pause.record.caravan_head == caravan.id && pause.state.is_effective())
    {
        return Err(force_validation(
            "force_caravan_held",
            "force intent cannot bypass an active or expired explicit Caravan hold",
            status,
            input,
        ));
    }
    let compatibility = status.analysis.compatibility.iter().find(|report| {
        report.candidate == current.head
            && report.target == status.analysis.fleet.default_branch
            && report.outcome == CompatibilityOutcome::Clean
    });
    if compatibility.is_none() {
        return Err(force_validation(
            "force_compatibility_not_clean",
            "force intent requires exact clean textual compatibility with the current default branch",
            status,
            input,
        ));
    }
    provider
        .verify_branch_head(
            &status.repository,
            &status.default_branch,
            &status.analysis.fleet.default_branch.oid,
        )
        .map_err(|error| force_provider_error(&error, status, input, None))?;
    let permission = provider
        .viewer_permission(&status.repository)
        .map_err(|error| force_provider_error(&error, status, input, None))?;
    if permission != "ADMIN" {
        return Err(force_validation(
            "force_permission_denied",
            "force intent requires authenticated ADMIN permission",
            status,
            input,
        ));
    }

    let observed_before_labels = current.labels.clone();
    let audit_before_labels = transition_before_labels(&current.labels, operation);
    let audit_after_labels = transition_after_labels(&current.labels, operation);
    let mut state = ForceState::new(operation, current.clone());
    let intent_present = current.has_label(FORCE_LABEL);
    match operation {
        ForceIntentOperation::Arm if intent_present => {
            state.already(
                MutationKind::AddLabel,
                "exact-generation force intent already armed",
            );
        }
        ForceIntentOperation::Arm => {
            let receipt = provider
                .add_label(&status.repository, &state.precondition(), FORCE_LABEL)
                .map_err(|error| force_provider_error(&error, status, input, Some(&state)))?;
            state.record(receipt, "armed exact-generation caravan-force intent");
        }
        ForceIntentOperation::Revoke if !intent_present => {
            state.already(
                MutationKind::RemoveLabel,
                "exact-generation force intent already absent",
            );
        }
        ForceIntentOperation::Revoke => {
            let receipt = provider
                .remove_label(&status.repository, &state.precondition(), FORCE_LABEL)
                .map_err(|error| force_provider_error(&error, status, input, Some(&state)))?;
            state.record(receipt, "revoked exact-generation caravan-force intent");
        }
    }

    let audit = force_audit(
        status,
        input,
        operation,
        &current,
        audit_before_labels,
        audit_after_labels,
    );
    let comment = provider
        .ensure_control_label_comment(&status.repository, &state.precondition(), &audit)
        .map_err(|error| force_comment_error(&error, status, input, &state))?;
    let existing = comment
        .provider_output
        .as_deref()
        .is_some_and(|output| output.starts_with("existing GitHub comment"));
    if existing {
        state.already(
            MutationKind::Comment,
            "force-intent audit comment already present",
        );
        state.current = comment.after;
    } else {
        state.record(
            comment,
            "posted durable exact-generation force-intent audit",
        );
    }

    let receipt = state.operation_receipt();
    let intent_present = state.current.has_label(FORCE_LABEL);
    Ok(ForceIntentOutput {
        operation,
        repository: status.repository.clone(),
        pr,
        head: state.current.head.clone(),
        default_branch: status.analysis.fleet.default_branch.clone(),
        before_labels: observed_before_labels,
        after_labels: state.current.labels.clone(),
        observed_checks: current.checks,
        actor: input.actor.trim().to_owned(),
        reason: input.reason.trim().to_owned(),
        mutated: receipt.changed,
        intent_present,
        provider_receipts: state.provider_receipts,
        next: match operation {
            ForceIntentOperation::Arm => {
                "run `cara sync` to revalidate and consume this one-shot exact-generation intent"
                    .to_owned()
            }
            ForceIntentOperation::Revoke => {
                "force intent is absent; normal CI and auto-merge policy applies".to_owned()
            }
        },
        receipt,
    })
}

fn transition_before_labels(
    current: &BTreeSet<String>,
    operation: ForceIntentOperation,
) -> BTreeSet<String> {
    let mut labels = current.clone();
    match operation {
        ForceIntentOperation::Arm => {
            labels.remove(FORCE_LABEL);
        }
        ForceIntentOperation::Revoke => {
            labels.insert(FORCE_LABEL.to_owned());
        }
    }
    labels
}

fn transition_after_labels(
    current: &BTreeSet<String>,
    operation: ForceIntentOperation,
) -> BTreeSet<String> {
    let mut labels = current.clone();
    match operation {
        ForceIntentOperation::Arm => {
            labels.insert(FORCE_LABEL.to_owned());
        }
        ForceIntentOperation::Revoke => {
            labels.remove(FORCE_LABEL);
        }
    }
    labels
}

fn force_audit(
    status: &StatusOutput,
    input: &ForceIntentInput,
    operation: ForceIntentOperation,
    current: &PullRequestSnapshot,
    before_labels: BTreeSet<String>,
    after_labels: BTreeSet<String>,
) -> ControlLabelAudit {
    let compatibility = status
        .analysis
        .compatibility
        .iter()
        .find(|report| {
            report.candidate == current.head
                && report.target == status.analysis.fleet.default_branch
        })
        .expect("force policy proved compatibility");
    ControlLabelAudit {
        operation: operation.name().to_owned(),
        marker: control_label_marker(
            operation.name(),
            current.number,
            &current.head.oid,
            &before_labels,
            &after_labels,
        ),
        before_labels,
        after_labels,
        actor: input.actor.trim().to_owned(),
        reason: format!(
            "{}; observed checks: {}",
            input.reason.trim(),
            serde_json::to_string(&current.checks).expect("checks serialize")
        ),
        reason_source: "explicit audited --actor/--reason input".to_owned(),
        compatibility_evidence: format!(
            "{}@{} -> {}@{} = {:?}",
            compatibility.candidate.name,
            compatibility.candidate.oid,
            compatibility.target.name,
            compatibility.target.oid,
            compatibility.outcome,
        ),
        clean_squash_evidence:
            "exact head/default compatibility is clean; force intent arms only normal sync's final ADMIN squash path"
                .to_owned(),
        admission_priority_basis:
            "not applicable: force intent never changes Caravan admission order".to_owned(),
    }
}

fn force_validation(
    code: &'static str,
    message: &'static str,
    status: &StatusOutput,
    input: &ForceIntentInput,
) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        code,
        message,
        Some(json!({
            "pr": input.pr,
            "repository": status.repository,
            "default_branch": status.analysis.fleet.default_branch,
            "graph_problems": status.analysis.fleet.problems,
            "mutated": false,
        })),
    )
}

fn force_provider_error(
    error: &MutationError,
    status: &StatusOutput,
    input: &ForceIntentInput,
    state: Option<&ForceState>,
) -> AppError {
    AppError::structured(
        if matches!(
            error,
            MutationError::StalePrecondition { .. } | MutationError::BranchHeadMismatch { .. }
        ) {
            ErrorCategory::Validation
        } else {
            ErrorCategory::ExecutionFailure
        },
        if matches!(
            error,
            MutationError::StalePrecondition { .. } | MutationError::BranchHeadMismatch { .. }
        ) {
            "force_stale_precondition"
        } else {
            "force_provider_failed"
        },
        error.to_string(),
        Some(json!({
            "pr": input.pr,
            "repository": status.repository,
            "completed_steps": state.map(|state| &state.steps),
            "provider_receipts": state.map(|state| &state.provider_receipts),
            "mutated": state.is_some_and(|state| state.operation_receipt().changed),
            "resumable": true,
            "next": "rediscover exact provider facts and rerun the same force command",
        })),
    )
}

fn force_comment_error(
    error: &MutationError,
    status: &StatusOutput,
    input: &ForceIntentInput,
    state: &ForceState,
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "force_audit_comment_failed",
        format!("force label transition completed but durable audit comment failed: {error}"),
        Some(json!({
            "pr": input.pr,
            "repository": status.repository,
            "completed_steps": state.steps,
            "provider_receipts": state.provider_receipts,
            "mutated": state.operation_receipt().changed,
            "resumable": true,
            "next": "rerun the same force command; the deterministic audit marker deduplicates completion",
        })),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use mcp_cli::StructuredError;

    use crate::graph;
    use crate::model::{AutoMergeState, CheckState, CommitOid, GraphProblem};

    struct FakeProvider {
        pulls: RefCell<BTreeMap<PrNumber, PullRequestSnapshot>>,
        permission: &'static str,
        branch_oid: RefCell<CommitOid>,
        comments: RefCell<BTreeSet<String>>,
        fail_comment: bool,
        calls: RefCell<Vec<MutationKind>>,
    }

    impl FakeProvider {
        fn new(pull: PullRequestSnapshot) -> Self {
            Self {
                pulls: RefCell::new(BTreeMap::from([(pull.number, pull)])),
                permission: "ADMIN",
                branch_oid: RefCell::new(branch("main").oid),
                comments: RefCell::new(BTreeSet::new()),
                fail_comment: false,
                calls: RefCell::new(Vec::new()),
            }
        }

        fn mutate(
            &self,
            expected: &PullRequestPrecondition,
            kind: MutationKind,
            change: impl FnOnce(&mut PullRequestSnapshot),
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.calls.borrow_mut().push(kind);
            let before = self.pulls.borrow()[&expected.number].clone();
            let actual = PullRequestPrecondition::from(&before);
            if actual != *expected {
                return Err(MutationError::StalePrecondition {
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual),
                    changed_fields: vec!["fake_race".to_owned()],
                });
            }
            let mut after = before.clone();
            change(&mut after);
            self.pulls.borrow_mut().insert(after.number, after.clone());
            Ok(GitHubMutationReceipt {
                kind,
                before: Some(before),
                after,
                provider_output: None,
            })
        }
    }

    impl ForceProvider for FakeProvider {
        fn verify_branch_head(
            &self,
            _repository: &RepositoryId,
            branch_name: &str,
            expected: &CommitOid,
        ) -> Result<(), MutationError> {
            let actual = self.branch_oid.borrow().clone();
            if &actual != expected {
                return Err(MutationError::BranchHeadMismatch {
                    branch: branch_name.to_owned(),
                    expected: expected.clone(),
                    actual,
                });
            }
            Ok(())
        }

        fn viewer_permission(&self, _repository: &RepositoryId) -> Result<String, MutationError> {
            Ok(self.permission.to_owned())
        }

        fn add_label(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            label: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::AddLabel, |pull| {
                pull.labels.insert(label.to_owned());
            })
        }

        fn remove_label(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            label: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::RemoveLabel, |pull| {
                pull.labels.remove(label);
            })
        }

        fn ensure_control_label_comment(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            audit: &ControlLabelAudit,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            if self.fail_comment {
                return Err(MutationError::Provider(
                    crate::github::DiscoveryError::CommandFailed {
                        command: crate::command::CommandSpec::new("fake"),
                        code: Some(1),
                        stderr: "comment failed".to_owned(),
                    },
                ));
            }
            let marker = audit.marker.clone();
            let existing = !self.comments.borrow_mut().insert(marker.clone());
            let mut receipt = self.mutate(expected, MutationKind::Comment, |_| {})?;
            if existing {
                receipt.provider_output = Some(format!("existing GitHub comment {marker}"));
            }
            Ok(receipt)
        }
    }

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        }
    }

    fn branch(name: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(format!("{name:0<40}")),
        }
    }

    fn pull() -> PullRequestSnapshot {
        PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(1),
            title: "head".to_owned(),
            url: "https://example.invalid/1".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch("head"),
            base: branch("main"),
            cross_repository: false,
            labels: BTreeSet::from(["caravan".to_owned()]),
            auto_merge: AutoMergeState::squash(),
            checks: vec![CheckSnapshot {
                name: "CI".to_owned(),
                state: CheckState::Failure,
                provider_state: Some("FAILURE".to_owned()),
                details_url: None,
            }],
            created_at: None,
            merged_at: None,
            updated_at: None,
        }
    }

    fn status(pull: PullRequestSnapshot) -> StatusOutput {
        let default = branch("main");
        let snapshot = crate::model::RepositorySnapshot {
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            repository: repository(),
            default_branch: default.clone(),
            current_branch: Some(pull.head.name.clone()),
            current_pr: Some(pull.number),
            pull_requests: vec![pull],
            generation_facts: Vec::new(),
            observed_at: None,
        };
        let checker = |_candidate: &BranchSnapshot, target: &BranchSnapshot| {
            Ok(crate::model::CompatibilityReport {
                candidate: branch("head"),
                target: target.clone(),
                outcome: CompatibilityOutcome::Clean,
                conflicting_paths: Vec::new(),
                diagnostic: None,
            })
        };
        let analysis = graph::analyze(&snapshot, &checker).unwrap();
        StatusOutput {
            config_provenance: None,
            head_merge: crate::read::HeadMergeStatus::default(),
            runtime: crate::read::RuntimeProvenance::default(),
            provider_api: crate::model::GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository(),
            rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
            auto_admission: crate::read::AutoAdmissionStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: snapshot.current_branch,
            current_pr: snapshot.current_pr,
            healthy: analysis.healthy(),
            initialization: crate::initialization::InitializationStatus::default(),
            admission: crate::read::resolve_admission(&analysis, &[]),
            analysis,
            pauses: Vec::new(),
            sync_budget: crate::sync::SyncBudgetStatus::default(),
        }
    }

    fn input() -> ForceIntentInput {
        ForceIntentInput {
            pr: 1,
            actor: "operator".to_owned(),
            reason: "accept known CI failure".to_owned(),
        }
    }

    fn context() -> AppContext {
        let mut context = AppContext::default();
        context.config.force_merge = true;
        context
    }

    #[test]
    fn arm_and_rerun_are_generation_bound_and_idempotent() {
        let pull = pull();
        let initial_status = status(pull.clone());
        let provider = FakeProvider::new(pull);
        let first = execute(
            &initial_status,
            &provider,
            &context(),
            &input(),
            ForceIntentOperation::Arm,
        )
        .unwrap();
        assert!(first.mutated);
        assert!(first.intent_present);
        assert!(first.after_labels.contains(FORCE_LABEL));
        assert_eq!(provider.comments.borrow().len(), 1);

        let refreshed = status(provider.pulls.borrow()[&PrNumber(1)].clone());
        let second = execute(
            &refreshed,
            &provider,
            &context(),
            &input(),
            ForceIntentOperation::Arm,
        )
        .unwrap();
        assert!(!second.mutated);
        assert!(second.before_labels.contains(FORCE_LABEL));
        assert!(second.after_labels.contains(FORCE_LABEL));
        assert_eq!(provider.comments.borrow().len(), 1);
        assert_eq!(
            provider.calls.borrow().as_slice(),
            [
                MutationKind::AddLabel,
                MutationKind::Comment,
                MutationKind::Comment,
            ]
        );
    }

    #[test]
    fn revoke_is_exact_audited_and_idempotent() {
        let mut armed = pull();
        armed.labels.insert(FORCE_LABEL.to_owned());
        armed.labels.insert("unrelated".to_owned());
        let initial_status = status(armed.clone());
        let provider = FakeProvider::new(armed);
        let first = execute(
            &initial_status,
            &provider,
            &context(),
            &input(),
            ForceIntentOperation::Revoke,
        )
        .unwrap();
        assert!(first.mutated);
        assert!(!first.intent_present);
        assert!(!first.after_labels.contains(FORCE_LABEL));
        assert!(first.after_labels.contains("unrelated"));

        let refreshed = status(provider.pulls.borrow()[&PrNumber(1)].clone());
        let second = execute(
            &refreshed,
            &provider,
            &context(),
            &input(),
            ForceIntentOperation::Revoke,
        )
        .unwrap();
        assert!(!second.mutated);
        assert!(!second.before_labels.contains(FORCE_LABEL));
        assert!(!second.after_labels.contains(FORCE_LABEL));
        assert_eq!(provider.comments.borrow().len(), 1);
    }

    #[test]
    fn arm_fails_closed_for_policy_permission_graph_and_hold() {
        let pull = pull();
        let status = status(pull.clone());
        let provider = FakeProvider::new(pull.clone());
        let mut disabled = context();
        disabled.config.force_merge = false;
        assert_eq!(
            execute(
                &status,
                &provider,
                &disabled,
                &input(),
                ForceIntentOperation::Arm,
            )
            .unwrap_err()
            .code(),
            "force_policy_disabled"
        );
        let denied = FakeProvider {
            permission: "WRITE",
            ..FakeProvider::new(pull.clone())
        };
        assert_eq!(
            execute(
                &status,
                &denied,
                &context(),
                &input(),
                ForceIntentOperation::Arm,
            )
            .unwrap_err()
            .code(),
            "force_permission_denied"
        );
        let mut invalid = status.clone();
        invalid.analysis.fleet.problems.push(GraphProblem {
            kind: crate::model::GraphProblemKind::Branching,
            prs: vec![PrNumber(1)],
            message: "invalid".to_owned(),
        });
        assert_eq!(
            execute(
                &invalid,
                &provider,
                &context(),
                &input(),
                ForceIntentOperation::Arm,
            )
            .unwrap_err()
            .code(),
            "force_graph_invalid"
        );
    }

    #[test]
    fn stale_head_and_default_fail_before_force_label_mutation() {
        let pull = pull();
        let current_status = status(pull.clone());
        let provider = FakeProvider::new(pull.clone());
        provider
            .pulls
            .borrow_mut()
            .get_mut(&PrNumber(1))
            .unwrap()
            .labels
            .insert("external-change".to_owned());
        let error = execute(
            &current_status,
            &provider,
            &context(),
            &input(),
            ForceIntentOperation::Arm,
        )
        .unwrap_err();
        assert_eq!(error.code(), "force_stale_precondition");
        assert_eq!(provider.calls.borrow().as_slice(), [MutationKind::AddLabel]);
        assert!(
            !provider.pulls.borrow()[&PrNumber(1)]
                .labels
                .contains(FORCE_LABEL)
        );

        let provider = FakeProvider::new(pull);
        *provider.branch_oid.borrow_mut() = CommitOid("moved-default".to_owned());
        let error = execute(
            &current_status,
            &provider,
            &context(),
            &input(),
            ForceIntentOperation::Arm,
        )
        .unwrap_err();
        assert_eq!(error.code(), "force_stale_precondition");
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn hold_and_ineligible_prs_fail_without_force_mutation() {
        let pull = pull();
        let mut held = status(pull.clone());
        held.pauses.push(crate::pause::PauseStatus {
            record: crate::pause::PauseRecord {
                version: 1,
                caravan_head: PrNumber(1),
                members: vec![PrNumber(1)],
                expected_head: PullRequestPrecondition::from(&pull),
                expected_checks: pull.checks.clone(),
                actor: "operator".to_owned(),
                reason: "incident".to_owned(),
                paused_unix_secs: 1,
                expires_unix_secs: None,
                external_reference: None,
                resume_authorized_by: None,
            },
            state: crate::pause::PauseState::Active,
            auto_merge_suspended: true,
            retired_state: None,
            safe_next_action: "resume".to_owned(),
        });
        let provider = FakeProvider::new(pull.clone());
        assert_eq!(
            execute(
                &held,
                &provider,
                &context(),
                &input(),
                ForceIntentOperation::Arm,
            )
            .unwrap_err()
            .code(),
            "force_caravan_held"
        );
        assert!(provider.calls.borrow().is_empty());

        let mut draft = pull;
        draft.draft = true;
        let draft_status = status(draft.clone());
        let provider = FakeProvider::new(draft);
        assert_eq!(
            execute(
                &draft_status,
                &provider,
                &context(),
                &input(),
                ForceIntentOperation::Arm,
            )
            .unwrap_err()
            .code(),
            "force_pr_ineligible"
        );
        assert!(provider.calls.borrow().is_empty());
    }

    #[test]
    fn empty_and_pending_ci_can_arm_intent_without_consuming_it() {
        for checks in [
            Vec::new(),
            vec![CheckSnapshot {
                name: "CI".to_owned(),
                state: CheckState::InProgress,
                provider_state: Some("IN_PROGRESS".to_owned()),
                details_url: None,
            }],
        ] {
            let mut candidate = pull();
            candidate.checks = checks;
            let current_status = status(candidate.clone());
            let provider = FakeProvider::new(candidate);
            let output = execute(
                &current_status,
                &provider,
                &context(),
                &input(),
                ForceIntentOperation::Arm,
            )
            .unwrap();
            assert!(output.intent_present);
            assert!(output.next.contains("cara sync"));
            assert_eq!(
                provider.pulls.borrow()[&PrNumber(1)].state,
                PullRequestState::Open
            );
        }
    }

    #[test]
    fn typo_label_is_not_force_intent_and_comment_failure_is_partial() {
        let mut typo = pull();
        typo.labels.insert("caravan-forced".to_owned());
        let status = status(typo.clone());
        let provider = FakeProvider {
            fail_comment: true,
            ..FakeProvider::new(typo)
        };
        let error = execute(
            &status,
            &provider,
            &context(),
            &input(),
            ForceIntentOperation::Arm,
        )
        .unwrap_err();
        assert_eq!(error.code(), "force_audit_comment_failed");
        let details = error.details().unwrap();
        assert_eq!(details["mutated"], true);
        assert!(
            provider.pulls.borrow()[&PrNumber(1)]
                .labels
                .contains(FORCE_LABEL)
        );
        assert!(
            provider.pulls.borrow()[&PrNumber(1)]
                .labels
                .contains("caravan-forced")
        );
    }
}
