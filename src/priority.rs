//! Audited automatic-admission priority controls for unenrolled pull requests.

use std::collections::BTreeSet;
use std::time::Duration;

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::command::ProcessRunner;
use crate::github::{
    ControlLabelAudit, GitHubMutationAdapter, GitHubMutationReceipt, MutationError,
    control_label_marker,
};
use crate::model::{
    BranchSnapshot, PrNumber, PullRequestPrecondition, PullRequestSnapshot, PullRequestState,
    RepositoryId,
};
use crate::read::StatusOutput;
use crate::{AppContext, AppError};

const ACTIVE_LABEL: &str = "caravan";
const EVICTED_LABEL: &str = "caravan-evicted";
const PRIORITY_NAMESPACE: &str = "caravan-priority:";

/// Set one configured automatic-admission priority on an exact unenrolled PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct PrioritySetInput {
    /// Exact provider pull request.
    #[arg(long, value_name = "PR")]
    pub pr: u64,
    /// Exact configured `caravan-priority:*` label.
    #[arg(long, value_name = "LABEL")]
    pub label: String,
    /// Audited operator or agent identity.
    #[arg(long)]
    pub actor: String,
    /// Audited reason for changing automatic order.
    #[arg(long)]
    pub reason: String,
}

/// Clear configured automatic-admission priority from an exact unenrolled PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::Args)]
pub struct PriorityClearInput {
    /// Exact provider pull request.
    #[arg(long, value_name = "PR")]
    pub pr: u64,
    /// Audited operator or agent identity.
    #[arg(long)]
    pub actor: String,
    /// Audited reason for restoring FIFO admission.
    #[arg(long)]
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PriorityOperation<'a> {
    Set(&'a str),
    Clear,
}

/// Exact, idempotent priority-control receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PriorityOutput {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub pr: PrNumber,
    pub head: BranchSnapshot,
    pub base: BranchSnapshot,
    pub default_branch: BranchSnapshot,
    pub config_fingerprint: String,
    pub actor: String,
    pub reason: String,
    pub permission: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_rank: Option<usize>,
    pub before_labels: BTreeSet<String>,
    pub after_labels: BTreeSet<String>,
    pub labels_changed: bool,
    pub mutated: bool,
    pub audit_durable: bool,
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
}

trait PriorityProvider {
    fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError>;

    fn repository_labels(
        &self,
        repository: &RepositoryId,
    ) -> Result<BTreeSet<String>, MutationError>;

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

impl<R: crate::command::CommandRunner> PriorityProvider for GitHubMutationAdapter<R> {
    fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError> {
        self.verify_branch_head(repository, branch, expected)
    }

    fn repository_labels(
        &self,
        repository: &RepositoryId,
    ) -> Result<BTreeSet<String>, MutationError> {
        self.repository_labels(repository)
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

/// Set one exact configured priority label.
pub fn set(context: &AppContext, input: &PrioritySetInput) -> Result<PriorityOutput, AppError> {
    execute_live(
        context,
        input.pr,
        &input.actor,
        &input.reason,
        PriorityOperation::Set(&input.label),
    )
}

/// Clear all configured priority metadata and return the PR to FIFO ordering.
pub fn clear(context: &AppContext, input: &PriorityClearInput) -> Result<PriorityOutput, AppError> {
    execute_live(
        context,
        input.pr,
        &input.actor,
        &input.reason,
        PriorityOperation::Clear,
    )
}

fn execute_live(
    context: &AppContext,
    pr: u64,
    actor: &str,
    reason: &str,
    operation: PriorityOperation<'_>,
) -> Result<PriorityOutput, AppError> {
    validate_audit_text(actor, reason)?;
    if let PriorityOperation::Set(label) = operation {
        validate_requested_label(label, &context.config.agent_priority_labels)?;
    }
    let lock = context.acquire_writer_operation("priority_control")?;
    let status = crate::read::status_for_remote_candidate(context, PrNumber(pr))?;
    ensure_config_unchanged(context)?;
    let provider = GitHubMutationAdapter::new(
        ProcessRunner::in_directory(&context.repository_path)
            .with_timeout(Duration::from_secs(context.config.command_timeout_secs)),
    );
    let output = execute(
        &status,
        &provider,
        &context.config.agent_priority_labels,
        config_fingerprint(context),
        actor,
        reason,
        operation,
    )?;
    lock.release()?;
    Ok(output)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn execute(
    status: &StatusOutput,
    provider: &impl PriorityProvider,
    configured_priorities: &[String],
    config_fingerprint: String,
    actor: &str,
    reason: &str,
    operation: PriorityOperation<'_>,
) -> Result<PriorityOutput, AppError> {
    crate::initialization::require_ready(&status.initialization)?;
    let selected_label = match operation {
        PriorityOperation::Set(label) => {
            validate_requested_label(label, configured_priorities)?;
            Some(label)
        }
        PriorityOperation::Clear => None,
    };
    let number = status.current_pr.ok_or_else(|| {
        AppError::validation(
            "priority_pr_not_found",
            "exact priority PR was not discovered",
        )
    })?;
    let candidate = status
        .analysis
        .pull_requests
        .get(&number)
        .cloned()
        .ok_or_else(|| {
            AppError::validation(
                "priority_pr_not_found",
                format!("PR #{number} is absent from the exact provider snapshot"),
            )
        })?;
    validate_candidate(status, &candidate, configured_priorities)?;

    let repository_labels = provider
        .repository_labels(&status.repository)
        .map_err(|error| mutation_error("priority_label_inventory_failed", &error, &[]))?;
    let missing = configured_priorities
        .iter()
        .filter(|label| !repository_labels.contains(*label))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "required_priority_labels_missing",
            "configured priority labels must exist before priority mutation",
            Some(
                json!({"missing_labels": missing, "mutated": false, "next": "run `cara init`, then retry"}),
            ),
        ));
    }
    let permission = provider
        .viewer_permission(&status.repository)
        .map_err(|error| mutation_error("priority_permission_check_failed", &error, &[]))?;
    if !matches!(permission.as_str(), "ADMIN" | "MAINTAIN" | "WRITE") {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "priority_permission_denied",
            "priority mutation requires repository write permission",
            Some(json!({"required": "WRITE", "actual": permission, "mutated": false})),
        ));
    }
    provider
        .verify_branch_head(
            &status.repository,
            &status.analysis.fleet.default_branch.name,
            &status.analysis.fleet.default_branch.oid,
        )
        .map_err(|error| mutation_error("priority_default_moved", &error, &[]))?;

    let before_labels = candidate.labels.clone();
    let mut current = candidate;
    let mut receipts = Vec::new();
    for label in configured_priorities {
        if Some(label.as_str()) == selected_label || !current.has_label(label) {
            continue;
        }
        let receipt = provider
            .remove_label(
                &status.repository,
                &PullRequestPrecondition::from(&current),
                label,
            )
            .map_err(|error| mutation_error("priority_remove_failed", &error, &receipts))?;
        current = receipt.after.clone();
        receipts.push(receipt);
    }
    if let Some(label) = selected_label
        && !current.has_label(label)
    {
        let receipt = provider
            .add_label(
                &status.repository,
                &PullRequestPrecondition::from(&current),
                label,
            )
            .map_err(|error| mutation_error("priority_add_failed", &error, &receipts))?;
        current = receipt.after.clone();
        receipts.push(receipt);
    }
    let operation_name = if selected_label.is_some() {
        "priority_set"
    } else {
        "priority_clear"
    };
    let selected_rank = selected_label.and_then(|label| {
        configured_priorities
            .iter()
            .position(|configured| configured == label)
            .map(|rank| rank + 1)
    });
    let audit = ControlLabelAudit {
        operation: operation_name.to_owned(),
        marker: control_label_marker(
            operation_name,
            current.number,
            &current.head.oid,
            &before_labels,
            &current.labels,
        ),
        before_labels: before_labels.clone(),
        after_labels: current.labels.clone(),
        actor: actor.to_owned(),
        reason: reason.to_owned(),
        reason_source: "explicit cara priority command".to_owned(),
        compatibility_evidence:
            "not applicable: priority metadata changes ordering, never compatibility authority"
                .to_owned(),
        clean_squash_evidence:
            "not applicable: priority control performs no merge or membership mutation".to_owned(),
        admission_priority_basis: selected_label.map_or_else(
            || "FIFO (all configured automatic-admission priority labels cleared)".to_owned(),
            |label| {
                format!(
                    "explicit configured priority `{label}` rank {} (1 highest)",
                    selected_rank.expect("selected configured priority has a rank")
                )
            },
        ),
    };
    let comment = provider
        .ensure_control_label_comment(
            &status.repository,
            &PullRequestPrecondition::from(&current),
            &audit,
        )
        .map_err(|error| mutation_error("priority_comment_failed", &error, &receipts))?;
    let audit_mutated = !comment
        .provider_output
        .as_deref()
        .is_some_and(|output| output.starts_with("existing GitHub comment"));
    current = comment.after.clone();
    receipts.push(comment);
    let labels_changed = before_labels != current.labels;

    Ok(PriorityOutput {
        schema_version: 1,
        repository: status.repository.clone(),
        pr: current.number,
        head: current.head.clone(),
        base: current.base.clone(),
        default_branch: status.analysis.fleet.default_branch.clone(),
        config_fingerprint,
        actor: actor.to_owned(),
        reason: reason.to_owned(),
        permission,
        selected_label: selected_label.map(str::to_owned),
        selected_rank,
        before_labels: before_labels.clone(),
        after_labels: current.labels.clone(),
        labels_changed,
        mutated: labels_changed || audit_mutated,
        audit_durable: true,
        provider_receipts: receipts,
    })
}

fn validate_candidate(
    status: &StatusOutput,
    candidate: &PullRequestSnapshot,
    configured_priorities: &[String],
) -> Result<(), AppError> {
    if candidate.state != PullRequestState::Open || candidate.draft {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "priority_pr_ineligible",
            "priority control requires an open non-draft pull request",
            Some(
                json!({"pr": candidate.number, "state": candidate.state, "draft": candidate.draft, "mutated": false}),
            ),
        ));
    }
    if candidate.cross_repository
        || candidate.head.repository != status.repository
        || candidate.base.repository != status.repository
    {
        return Err(AppError::validation(
            "priority_fork_unsupported",
            "priority control requires a same-repository PR head",
        ));
    }
    if candidate.has_label(ACTIVE_LABEL) || candidate.has_label(EVICTED_LABEL) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "priority_membership_ineligible",
            "priority control applies only to unenrolled, non-evicted PRs",
            Some(json!({"pr": candidate.number, "labels": candidate.labels, "mutated": false})),
        ));
    }
    let priority_namespace = candidate
        .labels
        .iter()
        .filter(|label| label.starts_with(PRIORITY_NAMESPACE))
        .collect::<Vec<_>>();
    let unknown = priority_namespace
        .iter()
        .filter(|label| !configured_priorities.contains(label))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let configured_count = priority_namespace.len().saturating_sub(unknown.len());
    if !unknown.is_empty() || configured_count > 1 {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "priority_metadata_invalid",
            "existing priority metadata is unknown or conflicting; inspect before mutation",
            Some(json!({
                "pr": candidate.number,
                "priority_labels": priority_namespace,
                "unknown_labels": unknown,
                "mutated": false,
            })),
        ));
    }
    Ok(())
}

fn validate_requested_label(label: &str, configured: &[String]) -> Result<(), AppError> {
    if configured.iter().any(|candidate| candidate == label) {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "priority_label_not_configured",
        "priority label must exactly match one configured agent_priority_labels entry",
        Some(json!({
            "requested": label,
            "configured": configured,
            "mutated": false,
        })),
    ))
}

fn validate_audit_text(actor: &str, reason: &str) -> Result<(), AppError> {
    if actor.trim().is_empty() || reason.trim().is_empty() {
        return Err(AppError::validation(
            "priority_audit_required",
            "priority control requires non-empty --actor and --reason",
        ));
    }
    if actor.len() > 256 || reason.len() > 2_048 {
        return Err(AppError::validation(
            "priority_audit_too_large",
            "priority actor/reason exceeds the bounded audit size",
        ));
    }
    Ok(())
}

fn ensure_config_unchanged(context: &AppContext) -> Result<(), AppError> {
    let config_path = if context.config_path.is_absolute() {
        context.config_path.clone()
    } else {
        context.repository_path.join(&context.config_path)
    };
    let actual = if context.config_existed {
        crate::config::CaravanConfig::load(&config_path)
            .map_err(|error| AppError::validation("priority_config_invalid", error.to_string()))?
    } else if config_path.exists() {
        return Err(AppError::validation(
            "priority_config_changed",
            "repository config appeared after command context was loaded",
        ));
    } else {
        crate::config::CaravanConfig::default()
    };
    if actual != context.config {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "priority_config_changed",
            "priority configuration changed during exact preflight",
            Some(
                json!({"mutated": false, "next": "reload config and review automatic order before retrying"}),
            ),
        ));
    }
    Ok(())
}

fn config_fingerprint(context: &AppContext) -> String {
    let material = serde_json::to_vec(&json!({
        "schema_version": 1,
        "repository": context.repository_path,
        "path": context.config_path,
        "config": context.config,
    }))
    .unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(material))
}

fn mutation_error(
    code: &str,
    error: &MutationError,
    receipts: &[GitHubMutationReceipt],
) -> AppError {
    AppError::structured(
        if matches!(error, MutationError::StalePrecondition { .. }) {
            ErrorCategory::Validation
        } else {
            ErrorCategory::ExecutionFailure
        },
        if matches!(error, MutationError::StalePrecondition { .. }) {
            "priority_stale_precondition"
        } else {
            code
        },
        error.to_string(),
        Some(json!({
            "provider_receipts": receipts,
            "mutated": !receipts.is_empty(),
            "next": "rediscover the exact PR/config and retry the same idempotent priority command",
        })),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::graph::GraphAnalysis;
    use crate::model::{AutoMergeState, CaravanFleet, CommitOid, GitHubApiTelemetry, MutationKind};

    #[derive(Debug)]
    struct FakeProvider {
        current: RefCell<PullRequestSnapshot>,
        repository_labels: BTreeSet<String>,
        permission: String,
        comments: RefCell<usize>,
        fail_comment: Cell<bool>,
    }

    impl PriorityProvider for FakeProvider {
        fn verify_branch_head(
            &self,
            _repository: &RepositoryId,
            _branch: &str,
            _expected: &crate::model::CommitOid,
        ) -> Result<(), MutationError> {
            Ok(())
        }

        fn repository_labels(
            &self,
            _repository: &RepositoryId,
        ) -> Result<BTreeSet<String>, MutationError> {
            Ok(self.repository_labels.clone())
        }

        fn viewer_permission(&self, _repository: &RepositoryId) -> Result<String, MutationError> {
            Ok(self.permission.clone())
        }

        fn add_label(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            label: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::AddLabel, |current| {
                current.labels.insert(label.to_owned());
            })
        }

        fn remove_label(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            label: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::RemoveLabel, |current| {
                current.labels.remove(label);
            })
        }

        fn ensure_control_label_comment(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            _audit: &ControlLabelAudit,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            if self.fail_comment.get() {
                return Err(MutationError::PermissionDenied {
                    required: "comment".to_owned(),
                    actual: "denied".to_owned(),
                });
            }
            *self.comments.borrow_mut() += 1;
            self.mutate(expected, MutationKind::Comment, |_| {})
        }
    }

    impl FakeProvider {
        fn mutate(
            &self,
            expected: &PullRequestPrecondition,
            kind: MutationKind,
            update: impl FnOnce(&mut PullRequestSnapshot),
        ) -> Result<GitHubMutationReceipt, MutationError> {
            let before = self.current.borrow().clone();
            let actual = PullRequestPrecondition::from(&before);
            if actual != *expected {
                return Err(MutationError::StalePrecondition {
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual),
                    changed_fields: vec!["test".to_owned()],
                });
            }
            let mut after = before.clone();
            update(&mut after);
            *self.current.borrow_mut() = after.clone();
            Ok(GitHubMutationReceipt {
                kind,
                before: Some(before),
                after,
                provider_output: None,
            })
        }
    }

    fn fixture() -> (StatusOutput, FakeProvider, Vec<String>) {
        let repository = RepositoryId {
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        };
        let branch = |name: &str, oid: &str| BranchSnapshot {
            repository: repository.clone(),
            name: name.to_owned(),
            oid: CommitOid(oid.to_owned()),
        };
        let candidate = PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(7),
            title: "candidate".to_owned(),
            url: "https://example.invalid/7".to_owned(),
            state: PullRequestState::Open,
            draft: false,
            head: branch("feature", "head-7"),
            base: branch("main", "main-old"),
            cross_repository: false,
            labels: BTreeSet::new(),
            auto_merge: AutoMergeState::disabled(),
            checks: Vec::new(),
            created_at: Some("2026-01-01T00:00:00Z".to_owned()),
            merged_at: None,
            updated_at: None,
        };
        let priorities = vec![
            "caravan-priority:high".to_owned(),
            "caravan-priority:low".to_owned(),
        ];
        let analysis = GraphAnalysis {
            fleet: CaravanFleet {
                repository: repository.clone(),
                default_branch: branch("main", "main-current"),
                caravans: Vec::new(),
                unqueued: vec![candidate.number],
                problems: Vec::new(),
                history: crate::model::CaravanHistory::default(),
            },
            pull_requests: BTreeMap::from([(candidate.number, candidate.clone())]),
            compatibility: Vec::new(),
            cumulative_trees: Vec::new(),
            squash_reconciliations: Vec::new(),
        };
        let status = StatusOutput {
            config_provenance: None,
            head_merge: crate::read::HeadMergeStatus::default(),
            runtime: crate::read::RuntimeProvenance::default(),
            provider_api: GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository.clone(),
            rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
            stack_backend: crate::read::StackBackendStatus::default(),
            auto_admission: crate::read::AutoAdmissionStatus::default(),
            default_branch: "main".to_owned(),
            current_branch: None,
            current_pr: Some(candidate.number),
            healthy: true,
            initialization: crate::initialization::InitializationStatus {
                ready: true,
                missing_labels: Vec::new(),
                mismatched_labels: Vec::new(),
                next: None,
                mutation_blocker: None,
            },
            admission: crate::read::resolve_admission(&analysis, &priorities),
            analysis,
            pauses: Vec::new(),
            sync_budget: crate::sync::SyncBudgetStatus::default(),
        };
        let provider = FakeProvider {
            current: RefCell::new(candidate),
            repository_labels: priorities.iter().cloned().collect(),
            permission: "WRITE".to_owned(),
            comments: RefCell::new(0),
            fail_comment: Cell::new(false),
        };
        (status, provider, priorities)
    }

    #[test]
    fn set_and_clear_priority_are_exact_audited_transitions() {
        let (status, provider, priorities) = fixture();
        let set = execute(
            &status,
            &provider,
            &priorities,
            "sha256:config".to_owned(),
            "operator",
            "urgent ordering",
            PriorityOperation::Set("caravan-priority:high"),
        )
        .unwrap();
        assert!(set.labels_changed);
        assert_eq!(set.selected_rank, Some(1));
        assert!(set.after_labels.contains("caravan-priority:high"));
        assert!(set.audit_durable);

        let mut current_status = status.clone();
        current_status
            .analysis
            .pull_requests
            .insert(set.pr, provider.current.borrow().clone());
        let clear = execute(
            &current_status,
            &provider,
            &priorities,
            "sha256:config".to_owned(),
            "operator",
            "return to FIFO",
            PriorityOperation::Clear,
        )
        .unwrap();
        assert!(clear.labels_changed);
        assert!(clear.selected_label.is_none());
        assert!(
            !clear
                .after_labels
                .iter()
                .any(|label| label.starts_with(PRIORITY_NAMESPACE))
        );
        assert_eq!(*provider.comments.borrow(), 2);
    }

    #[test]
    fn exact_retry_is_label_noop_but_keeps_audit_durable() {
        let (mut status, provider, priorities) = fixture();
        let first = execute(
            &status,
            &provider,
            &priorities,
            "sha256:config".to_owned(),
            "operator",
            "urgent ordering",
            PriorityOperation::Set("caravan-priority:high"),
        )
        .unwrap();
        status
            .analysis
            .pull_requests
            .insert(first.pr, provider.current.borrow().clone());
        let retry = execute(
            &status,
            &provider,
            &priorities,
            "sha256:config".to_owned(),
            "operator",
            "urgent ordering",
            PriorityOperation::Set("caravan-priority:high"),
        )
        .unwrap();
        assert!(!retry.labels_changed);
        assert_eq!(retry.provider_receipts.len(), 1);
        assert_eq!(retry.provider_receipts[0].kind, MutationKind::Comment);
        assert_eq!(*provider.comments.borrow(), 2);
    }

    #[test]
    fn comment_failure_preserves_partial_label_receipt() {
        let (status, provider, priorities) = fixture();
        provider.fail_comment.set(true);
        let error = execute(
            &status,
            &provider,
            &priorities,
            "sha256:config".to_owned(),
            "operator",
            "urgent ordering",
            PriorityOperation::Set("caravan-priority:high"),
        )
        .unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "priority_comment_failed"
        );
        let details = mcp_cli::StructuredError::details(&error).unwrap();
        assert_eq!(details["mutated"], true);
        assert_eq!(details["provider_receipts"].as_array().unwrap().len(), 1);
        assert!(
            provider
                .current
                .borrow()
                .labels
                .contains("caravan-priority:high")
        );
    }

    #[test]
    fn unknown_or_active_priority_state_fails_before_mutation() {
        let (mut status, provider, priorities) = fixture();
        let candidate = status.analysis.pull_requests.get_mut(&PrNumber(7)).unwrap();
        candidate.labels.insert(ACTIVE_LABEL.to_owned());
        let error = execute(
            &status,
            &provider,
            &priorities,
            "sha256:config".to_owned(),
            "operator",
            "should fail",
            PriorityOperation::Set("caravan-priority:high"),
        )
        .unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "priority_membership_ineligible"
        );
        assert!(provider.current.borrow().labels.is_empty());
        assert_eq!(*provider.comments.borrow(), 0);
    }

    #[test]
    fn unconfigured_requested_label_fails_before_provider_reads() {
        let (status, provider, priorities) = fixture();
        let error = execute(
            &status,
            &provider,
            &priorities,
            "sha256:config".to_owned(),
            "operator",
            "bad label",
            PriorityOperation::Set("caravan-priority:surprise"),
        )
        .unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "priority_label_not_configured"
        );
        assert!(provider.current.borrow().labels.is_empty());
        assert_eq!(*provider.comments.borrow(), 0);
    }
}
