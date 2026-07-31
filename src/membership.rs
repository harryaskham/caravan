//! Membership policy for creating, renewing, joining, and rejoining caravans.
//!
//! The provider adapter owns exact optimistic commands. This module owns only
//! operation ordering, complete preflight, idempotent resume, and receipts.

use std::collections::{BTreeMap, BTreeSet};

use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::command::{CommandRunError, CommandRunner, CommandSpec, ProcessRunner};
use crate::github::{
    ControlLabelAudit, CreatePullRequestInput, DiscoveryError, GitHubMutationAdapter,
    GitHubMutationReceipt, MutationError, control_label_marker,
};
use crate::graph::{CompatibilityChecker, GitCompatibilityChecker};
use crate::hooks::{self, HookDelivery};
use crate::model::{
    Caravan, CaravanEvent, EventKind, MergeMethod, MutationKind, MutationStep, MutationStepState,
    OperationId, OperationReceipt, PrNumber, PullRequestPrecondition, PullRequestSnapshot,
    PullRequestState, RepositoryId,
};
use crate::operation_lock::OperationLock;
use crate::read::{self, CheckOutput, StatusOutput};
use crate::{AppContext, AppError, CheckInput, CreateInput, JoinInput};

mod execution;
mod policy;
use execution::ExecutionState;
use policy::{
    membership_audit, preflight_eligibility, preflight_repository, resolve_join_target,
    validate_operation_shape, validate_post_rebase_target,
};

const ACTIVE_LABEL: &str = "caravan";
const EVICTED_LABEL: &str = "caravan-evicted";
const FORCE_LABEL: &str = "caravan-force";
const SKIPPED_LABEL: &str = "caravan-join-skipped";

/// Provider commands that must still run after an irreversible branch rewrite:
/// the mandatory exact rediscovery plus the membership label/base writes.
const POST_REWRITE_COMMAND_RESERVE: u32 = 4;

/// Commands one candidate may spend before the tick reclaims the budget.
///
/// The exact-candidate binding read builds its OWN runner bounded by its own
/// `exact_deadline`, so that read is already contained. Assigning that deadline
/// back over the operation deadline collapsed the whole remaining operation to a
/// single command window, and the post-rewrite guard then priced
/// `POST_REWRITE_COMMAND_RESERVE * timeout` against `1 * timeout`. Four against
/// one cannot pass, at any value of `command_timeout_secs`, because raising it
/// scales both sides (bd-84f82d).
///
/// A candidate still must not consume the tick, so the deadline is bounded
/// rather than merely restored: the reserve it will need, plus the rewrite work
/// that precedes it, and never beyond the caller's tick deadline.
const CANDIDATE_OPERATION_COMMAND_BUDGET: u32 = POST_REWRITE_COMMAND_RESERVE * 2;

/// Bound one candidate's share without collapsing it to a single command.
fn candidate_operation_deadline(
    context: &AppContext,
    tick_deadline: std::time::Instant,
) -> std::time::Instant {
    let share = std::time::Duration::from_secs(context.config.command_timeout_secs)
        .saturating_mul(CANDIDATE_OPERATION_COMMAND_BUDGET);
    std::cmp::min(tick_deadline, std::time::Instant::now() + share)
}
const REQUIRED_LABELS: [&str; 3] = [ACTIVE_LABEL, EVICTED_LABEL, FORCE_LABEL];

/// Membership command kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MembershipOperation {
    New,
    Renew,
    Join,
    Rejoin,
}

impl MembershipOperation {
    fn name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Renew => "renew",
            Self::Join => "join",
            Self::Rejoin => "rejoin",
        }
    }

    const fn is_join(self) -> bool {
        matches!(self, Self::Join | Self::Rejoin)
    }

    const fn is_renewal(self) -> bool {
        matches!(self, Self::Renew | Self::Rejoin)
    }
}

/// Input normalized across the four membership commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipRequest {
    pub operation: MembershipOperation,
    pub create_pr: bool,
    pub tail_pr: Option<u64>,
    pub head_pr: Option<u64>,
    pub reason: Option<String>,
    pub priority_label: Option<String>,
    pub agent_priority_labels: Vec<String>,
}

/// Exact predecessor selected from the serialized live-tail snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JoinPredecessorReceipt {
    pub pr: PrNumber,
    pub branch: String,
    pub head_oid: crate::model::CommitOid,
}

/// Exact source patch and target generation bound before any provider mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JoinSourceReceipt {
    pub branch: String,
    pub head_oid: crate::model::CommitOid,
    pub parent: crate::model::BranchSnapshot,
    pub tree_oid: crate::model::CommitOid,
    pub patch_fingerprint: String,
    pub effective_patch_fingerprint: String,
    #[serde(default)]
    pub source_commits: Vec<crate::model::CommitOid>,
    #[serde(default)]
    pub already_landed_commits: Vec<crate::model::CommitOid>,
    pub source_title: String,
    pub selected_tail: JoinPredecessorReceipt,
    pub expected_result_tree_oid: crate::model::CommitOid,
}

/// Final exact candidate state after physical rewrite and provider admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JoinResultReceipt {
    pub head_oid: crate::model::CommitOid,
    pub base_ref: String,
    pub base_oid: crate::model::CommitOid,
    pub state: PullRequestState,
}

/// Durable PR-scoped force intent is preserved across membership and Cara-owned
/// history rewrites until explicit revoke, eviction, or successful merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JoinForceIntent {
    Absent,
    Preserved,
}

/// Versioned, additive contract consumed by Cacophony `pr_cara_join`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JoinReceipt {
    pub schema_version: u32,
    pub operation_id: OperationId,
    pub repository: RepositoryId,
    pub caravan_id: PrNumber,
    pub candidate_pr: PrNumber,
    pub candidate_source_head_oid: crate::model::CommitOid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<JoinSourceReceipt>,
    pub predecessor: JoinPredecessorReceipt,
    pub default_branch_oid: crate::model::CommitOid,
    pub result: JoinResultReceipt,
    pub rebase_on_join: bool,
    pub ancestry_verified: bool,
    pub membership_durable: bool,
    pub force_intent: JoinForceIntent,
    pub config_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebase_receipt: Option<crate::physical_rebase::RebaseReceipt>,
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    /// Deterministic process-independent hash of this receipt with this field omitted.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub receipt_hash: String,
}

/// Successful, resumable membership result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MembershipOutput {
    pub receipt: OperationReceipt,
    /// Exact old/new OIDs for the optional physical ancestry rewrite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebase_receipt: Option<crate::physical_rebase::RebaseReceipt>,
    /// Exact provider before/after facts for every completed remote mutation.
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    /// Stable exact-admission receipt for new/renew/join/rejoin. Root admissions
    /// encode the default branch as predecessor `pr=0` (bd-d15ba3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_receipt: Option<JoinReceipt>,
    pub pull_request: PullRequestSnapshot,
    pub caravan_id: PrNumber,
    /// Caravans that already existed when a ROOT admission created a separate
    /// one, so the caller can see the choice made on their behalf.
    ///
    /// Advisory only. `new` is often exactly right, and Cara cannot know the
    /// caller's intent, so this neither refuses nor recommends. It exists
    /// because every caravan on a live fleet was created with `new` while clean
    /// candidates queued behind an existing caravan nobody joined, and nothing
    /// in the output revealed that an alternative was available (bd-5891bb).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coexisting_caravans: Vec<PrNumber>,
    /// Typed intent-aware admission-order decision bound to this operation,
    /// including exact provider-mutation and idempotency evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_intent: Option<crate::admission::AdmissionIntentDecision>,
    /// Canonical events emitted after the complete membership operation.
    #[serde(default)]
    pub events: Vec<CaravanEvent>,
    /// Bounded status for configured hooks which consumed `events`.
    #[serde(default)]
    pub hook_deliveries: Vec<HookDelivery>,
}

/// Provider operations required by membership policy.
pub trait MembershipProvider {
    /// Fresh open generation facts used only by the membership domain's
    /// immediate pre-mutation generation guard.
    fn open_generation_facts(
        &self,
        _repository: &RepositoryId,
    ) -> Result<Vec<crate::model::PullRequestGenerationFact>, MutationError> {
        Ok(Vec::new())
    }

    fn compare_generation_commits(
        &self,
        _repository: &RepositoryId,
        _base: &crate::model::CommitOid,
        _head: &crate::model::CommitOid,
    ) -> Result<crate::generation::CommitRelation, MutationError> {
        Ok(crate::generation::CommitRelation::Unknown {
            reason: "generation comparison is unavailable in this provider".to_owned(),
        })
    }

    fn generation_comment_bodies(
        &self,
        _repository: &RepositoryId,
        _pr: PrNumber,
    ) -> Result<Vec<String>, MutationError> {
        Ok(Vec::new())
    }

    fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError>;

    fn refetch_pull_request(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<PullRequestSnapshot, MutationError>;

    fn branch_is_protected(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<bool, MutationError>;

    fn repository_allows_auto_merge(
        &self,
        repository: &RepositoryId,
    ) -> Result<bool, MutationError>;

    fn repository_labels(
        &self,
        repository: &RepositoryId,
    ) -> Result<BTreeSet<String>, MutationError>;

    fn create_pull_request(
        &self,
        repository: &RepositoryId,
        input: &CreatePullRequestInput,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn set_base(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        base: &str,
    ) -> Result<GitHubMutationReceipt, MutationError>;

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

    fn enable_squash_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError>;

    fn disable_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError>;
}

impl<R: crate::command::CommandRunner> MembershipProvider for GitHubMutationAdapter<R> {
    fn open_generation_facts(
        &self,
        repository: &RepositoryId,
    ) -> Result<Vec<crate::model::PullRequestGenerationFact>, MutationError> {
        self.open_generation_facts(repository)
    }

    fn compare_generation_commits(
        &self,
        repository: &RepositoryId,
        base: &crate::model::CommitOid,
        head: &crate::model::CommitOid,
    ) -> Result<crate::generation::CommitRelation, MutationError> {
        self.compare_commits(repository, base, head)
    }

    fn generation_comment_bodies(
        &self,
        repository: &RepositoryId,
        pr: PrNumber,
    ) -> Result<Vec<String>, MutationError> {
        self.pull_request_comment_bodies(repository, pr)
    }

    fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError> {
        self.verify_branch_head(repository, branch, expected)
    }

    fn refetch_pull_request(
        &self,
        repository: &RepositoryId,
        number: PrNumber,
    ) -> Result<PullRequestSnapshot, MutationError> {
        self.refetch_pull_request(repository, number)
    }

    fn branch_is_protected(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<bool, MutationError> {
        self.branch_is_protected(repository, branch)
    }

    fn repository_allows_auto_merge(
        &self,
        repository: &RepositoryId,
    ) -> Result<bool, MutationError> {
        self.repository_allows_auto_merge(repository)
    }

    fn repository_labels(
        &self,
        repository: &RepositoryId,
    ) -> Result<BTreeSet<String>, MutationError> {
        self.repository_labels(repository)
    }

    fn create_pull_request(
        &self,
        repository: &RepositoryId,
        input: &CreatePullRequestInput,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.create_pull_request(repository, input)
    }

    fn set_base(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        base: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.set_base(repository, expected, base)
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

    fn enable_squash_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.enable_squash_auto_merge(repository, expected)
    }

    fn disable_auto_merge(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.disable_auto_merge(repository, expected)
    }
}

/// Create a new one-PR caravan.
pub fn new(context: &AppContext, input: &CreateInput) -> Result<MembershipOutput, AppError> {
    execute_live(
        context,
        &MembershipRequest {
            operation: MembershipOperation::New,
            create_pr: input.create_pr,
            tail_pr: None,
            head_pr: None,
            reason: input.reason.clone(),
            priority_label: input.priority_label.clone(),
            agent_priority_labels: context.config.agent_priority_labels.clone(),
        },
        input.pr,
    )
}

/// Renew an evicted PR as a new caravan.
pub fn renew(context: &AppContext, input: &CreateInput) -> Result<MembershipOutput, AppError> {
    execute_live(
        context,
        &MembershipRequest {
            operation: MembershipOperation::Renew,
            create_pr: input.create_pr,
            tail_pr: None,
            head_pr: None,
            reason: input.reason.clone(),
            priority_label: input.priority_label.clone(),
            agent_priority_labels: context.config.agent_priority_labels.clone(),
        },
        input.pr,
    )
}

/// Append the current PR after a selected or uniquely inferred tail.
pub fn join(context: &AppContext, input: &JoinInput) -> Result<MembershipOutput, AppError> {
    execute_live(
        context,
        &MembershipRequest {
            operation: MembershipOperation::Join,
            create_pr: input.create_pr,
            tail_pr: input.tail_pr,
            head_pr: input.head_pr,
            reason: input.reason.clone(),
            priority_label: input.priority_label.clone(),
            agent_priority_labels: context.config.agent_priority_labels.clone(),
        },
        input.pr,
    )
}

/// Rejoin an evicted PR after a selected or uniquely inferred tail.
pub fn rejoin(context: &AppContext, input: &JoinInput) -> Result<MembershipOutput, AppError> {
    execute_live(
        context,
        &MembershipRequest {
            operation: MembershipOperation::Rejoin,
            create_pr: input.create_pr,
            tail_pr: input.tail_pr,
            head_pr: input.head_pr,
            reason: input.reason.clone(),
            priority_label: input.priority_label.clone(),
            agent_priority_labels: context.config.agent_priority_labels.clone(),
        },
        input.pr,
    )
}

fn execute_live(
    context: &AppContext,
    request: &MembershipRequest,
    candidate_pr: Option<u64>,
) -> Result<MembershipOutput, AppError> {
    let _lock = OperationLock::acquire(&context.repository_path, request.operation.name())?;
    execute_locked(context, request, candidate_pr, None, None, None, true)
}

/// Run one exact sync-owned new/join while the caller retains the repository lock.
pub(crate) fn auto_admit_locked(
    context: &AppContext,
    status: StatusOutput,
    candidate_pr: PrNumber,
    tail_pr: Option<PrNumber>,
    priority_label: Option<String>,
    operation_deadline: std::time::Instant,
    github_budget: &crate::command::GithubRequestBudget,
) -> Result<MembershipOutput, AppError> {
    let operation = if tail_pr.is_some() {
        MembershipOperation::Join
    } else {
        MembershipOperation::New
    };
    execute_locked(
        context,
        &MembershipRequest {
            operation,
            create_pr: false,
            tail_pr: tail_pr.map(|number| number.0),
            head_pr: None,
            reason: Some(format!(
                "sync-owned automatic admission using {}",
                crate::sync::AUTO_ADMISSION_HEURISTIC_VERSION
            )),
            priority_label,
            agent_priority_labels: context.config.agent_priority_labels.clone(),
        },
        Some(candidate_pr.0),
        Some(operation_deadline),
        Some(github_budget),
        Some(status),
        false,
    )
}

fn require_current_join_root(status: &StatusOutput, target: &JoinTarget) -> Result<(), AppError> {
    let root_number = target.caravan.head().ok_or_else(|| {
        AppError::validation("join_target_empty", "selected join caravan has no root")
    })?;
    let root = status
        .analysis
        .pull_requests
        .get(&root_number)
        .ok_or_else(|| {
            AppError::validation(
                "join_root_missing",
                "selected join root is absent from discovery",
            )
        })?;
    let current_default = &status.analysis.fleet.default_branch;
    if root.base.name == current_default.name && root.base.oid == current_default.oid {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "join_root_stale_default",
        "selected caravan root is not based on the exact current default generation",
        Some(json!({
            "mutated": false,
            "root_pr": root.number,
            "root_head": root.head,
            "observed_root_base": root.base,
            "required_default": current_default,
            "selected_tail_pr": target.tail.number,
            "selected_tail_head": target.tail.head,
            "safe_next_action": "run `cara sync --all` until the selected root is current, then retry the same join",
        })),
    ))
}

fn revalidate_generation_before_membership(
    status: &StatusOutput,
    candidate: PrNumber,
    provider: &impl MembershipProvider,
) -> Result<(), AppError> {
    if status
        .admission
        .generation_integrity
        .finding(candidate)
        .is_none()
    {
        // Ordinary repositories and legacy non-Cacophony PRs retain existing
        // behavior. A Cacophony-shaped PR with missing metadata has an explicit
        // invalid finding and therefore never takes this branch.
        return Ok(());
    }
    let mut facts = provider
        .open_generation_facts(&status.repository)
        .map_err(|error| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "generation_revalidation_failed",
                "could not re-read open Cacophony generations immediately before membership mutation",
                Some(json!({
                    "pr": candidate,
                    "error": error.to_string(),
                    "mutated": false,
                    "safe_next_action": "restore provider reads and rerun the same membership command",
                })),
            )
        })?;
    if !facts.iter().any(|fact| fact.pr == candidate) {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "generation_candidate_missing",
            "selected generation disappeared before membership mutation",
            Some(json!({
                "pr": candidate,
                "expected_generation_integrity": status.admission.generation_integrity,
                "mutated": false,
                "safe_next_action": "rediscover open PR generations; never mutate or close the missing candidate by assumption",
            })),
        ));
    }
    for pr in crate::generation::duplicate_stream_prs(&facts)
        .into_iter()
        .take(32)
    {
        if let Ok(comments) = provider.generation_comment_bodies(&status.repository, pr) {
            crate::generation::attach_reviewed_supersession_links(&mut facts, pr, &comments);
        }
    }
    let fresh = crate::generation::analyze(&facts, |base, head| {
        provider
            .compare_generation_commits(&status.repository, base, head)
            .unwrap_or_else(|error| crate::generation::CommitRelation::Unknown {
                reason: error.to_string(),
            })
    });
    let finding = fresh.finding(candidate).ok_or_else(|| {
        AppError::structured(
            ErrorCategory::Validation,
            "generation_metadata_disappeared",
            "selected PR no longer carries exact Cacophony generation metadata",
            Some(json!({
                "pr": candidate,
                "expected_generation_integrity": status.admission.generation_integrity,
                "actual_generation_integrity": fresh,
                "mutated": false,
                "safe_next_action": "repair or review provider metadata; do not admit the candidate",
            })),
        )
    })?;
    if finding.disposition != crate::generation::GenerationDisposition::CurrentGeneration {
        return crate::generation::require_admissible(&fresh, candidate);
    }
    Ok(())
}

fn membership_identity_matches(
    expected: &PullRequestSnapshot,
    actual: &PullRequestSnapshot,
) -> bool {
    expected.number == actual.number
        && expected.state == actual.state
        && expected.draft == actual.draft
        && expected.head == actual.head
        && expected.base == actual.base
        && expected.cross_repository == actual.cross_repository
        && expected.labels == actual.labels
        && expected.auto_merge == actual.auto_merge
}

fn membership_identity(pull: &PullRequestSnapshot) -> serde_json::Value {
    json!({
        "number": pull.number,
        "state": pull.state,
        "draft": pull.draft,
        "head": pull.head,
        "base": pull.base,
        "cross_repository": pull.cross_repository,
        "labels": pull.labels,
        "auto_merge": pull.auto_merge,
    })
}

fn membership_identity_changes(
    expected: &PullRequestSnapshot,
    actual: &PullRequestSnapshot,
) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if expected.state != actual.state {
        changed.push("state");
    }
    if expected.draft != actual.draft {
        changed.push("draft");
    }
    if expected.head != actual.head {
        changed.push("head");
    }
    if expected.base != actual.base {
        changed.push("base");
    }
    if expected.cross_repository != actual.cross_repository {
        changed.push("head_repository");
    }
    if expected.labels != actual.labels {
        changed.push("labels");
    }
    if expected.auto_merge != actual.auto_merge {
        changed.push("auto_merge");
    }
    changed
}

fn revalidate_join_root(
    status: &StatusOutput,
    target: &JoinTarget,
    provider: &impl MembershipProvider,
) -> Result<(), AppError> {
    let root_number = target.caravan.head().expect("join caravan is non-empty");
    let expected = status
        .analysis
        .pull_requests
        .get(&root_number)
        .expect("join root came from status");
    let actual = provider
        .refetch_pull_request(&status.repository, root_number)
        .map_err(|error| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "join_root_refetch_failed",
                "could not revalidate selected root before join mutation",
                Some(json!({"root_pr": root_number, "error": error.to_string(), "mutated": false})),
            )
        })?;
    if membership_identity_matches(expected, &actual)
        && actual.base == status.analysis.fleet.default_branch
    {
        return Ok(());
    }
    let changed_fields = membership_identity_changes(expected, &actual);
    Err(AppError::structured(
        ErrorCategory::Validation,
        "join_root_moved_before_apply",
        "selected caravan root changed after join preview",
        Some(json!({
            "root_pr": root_number,
            "changed_fields": changed_fields,
            "expected": membership_identity(expected),
            "actual": membership_identity(&actual),
            "required_default": status.analysis.fleet.default_branch,
            "ignored_check_churn": true,
            "mutated": false,
            "retryable": true,
            "retry_command": "rerun the same `cara join` command",
            "safe_next_action": "rediscover and retry the same join; run `cara sync --all` first only when root head/base/labels/auto-merge actually changed",
        })),
    ))
}

fn validate_membership_source_request(
    status: &StatusOutput,
    request: &MembershipRequest,
) -> Result<(), AppError> {
    if status.current_pr.is_none() && !request.create_pr {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "current_pr_not_found",
            "root admission requires an explicit --pr or a checkout with one unique open PR",
            Some(json!({
                "current_branch": status.current_branch,
                "mutated": false,
                "safe_next_action": "rerun with `new --pr <PR>` (or `renew --pr <PR>`) for a Saloon candidate",
            })),
        ));
    }
    if status.current_pr.is_none()
        && request.create_pr
        && status.current_branch.as_deref() == Some(status.default_branch.as_str())
    {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "create_pr_on_default_branch",
            "cannot create a pull request directly from the default branch",
            Some(json!({
                "branch": status.current_branch,
                "default_branch": status.default_branch,
                "mutated": false,
                "safe_next_action": "check out a topic branch with a unique patch, then rerun --create-pr",
            })),
        ));
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn source_command(
    runner: &impl CommandRunner,
    command: CommandSpec,
    code: &str,
    message: &str,
) -> Result<crate::command::CommandOutput, AppError> {
    let output = runner.run(&command).map_err(|error| {
        AppError::structured(
            if matches!(error, CommandRunError::Timeout { .. }) {
                ErrorCategory::Timeout
            } else {
                ErrorCategory::ExecutionFailure
            },
            code,
            format!("{message}: {error}"),
            Some(json!({"command": command.display(), "mutated": false})),
        )
    })?;
    if output.is_success() {
        Ok(output)
    } else {
        Err(AppError::structured(
            ErrorCategory::Validation,
            code,
            message,
            Some(json!({
                "command": command.display(),
                "code": output.code,
                "stdout": output.stdout,
                "stderr": output.stderr,
                "mutated": false,
            })),
        ))
    }
}

fn source_oid(
    runner: &impl CommandRunner,
    revision: &str,
    code: &str,
) -> Result<crate::model::CommitOid, AppError> {
    let output = source_command(
        runner,
        CommandSpec::new("git").args(["rev-parse", revision]),
        code,
        "could not resolve exact join source identity",
    )?;
    let oid = output.stdout.trim();
    if oid.len() != 40 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::validation(
            code,
            "join source identity is not one exact Git OID",
        ));
    }
    Ok(crate::model::CommitOid(oid.to_owned()))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
/// Prove enough budget remains for mandatory post-rewrite provider work.
///
/// bd-4e4615: a rewrite that lands remotely and then times out during the
/// required rediscovery leaves correct but incomplete receipts. Reserving the
/// phase up front turns that into a clean zero-mutation refusal instead.
fn require_post_rewrite_budget(
    context: &AppContext,
    operation_deadline: Option<std::time::Instant>,
    pr: PrNumber,
) -> Result<(), AppError> {
    let Some(deadline) = operation_deadline else {
        return Ok(());
    };
    // One rediscovery plus the membership writes that must follow it.
    let timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let required = timeout.saturating_mul(POST_REWRITE_COMMAND_RESERVE);
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining >= required {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "membership_post_rewrite_budget_insufficient",
        "not enough operation budget remains to complete mandatory post-rewrite rediscovery and membership writes; refusing to start an irreversible branch rewrite",
        Some(json!({
            "pr": pr,
            "required_ms": u64::try_from(required.as_millis()).unwrap_or(u64::MAX),
            "remaining_ms": u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
            "reserved_commands": POST_REWRITE_COMMAND_RESERVE,
            "mutated": false,
            "resumable": true,
            // The reserve is a MULTIPLE of `command_timeout_secs`, so raising it
            // raises the bar it is failing against and never enlarges what
            // remains. The advice used to say "raise command_timeout_secs",
            // which steers the reader deeper into the fault: an operator read it
            // and proposed 60 -> 300, which would have taken the requirement
            // from 240s to 1200s (bd-cef42d).
            "required_formula": "command_timeout_secs * POST_REWRITE_COMMAND_RESERVE",
            "safe_next_action": format!(
                "rerun with a fresh operation budget, or raise sync.max_duration_secs to enlarge the operation deadline, or LOWER command_timeout_secs: the reserve is {POST_REWRITE_COMMAND_RESERVE}x that value, so raising it increases this requirement rather than meeting it"
            ),
        })),
    ))
}

/// Refuse to create a pull request for a branch with no unique patch.
///
/// bd-032049: the empty-source refusal already existed, but it ran only after
/// `--create-pr` had already created the pull request, so a no-op candidate
/// still produced a real PR and its inferred "work" was an already-landed
/// default-branch advance. Proving the branch first keeps the whole operation
/// zero-mutation.
fn require_nonempty_source_branch(
    context: &AppContext,
    status: &StatusOutput,
    branch: &str,
    operation_deadline: std::time::Instant,
) -> Result<(), AppError> {
    let timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let runner = ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(timeout)
        .with_operation_deadline(operation_deadline);
    let default = &status.analysis.fleet.default_branch;
    let patch = source_command(
        &runner,
        CommandSpec::new("git").args(["diff", "--binary", &format!("{}...HEAD", default.oid.0)]),
        "join_source_patch_unavailable",
        "could not compute the exact source patch against the current default branch",
    )?;
    if patch.stdout.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "join_empty_source_noop",
            "source branch has no unique patch beyond current default; refusing to create a pull request for a zero-mutation no-op",
            Some(json!({
                "branch": branch,
                "default_branch": default,
                "mutated": false,
                "noop": true,
                "safe_next_action": "commit the intended change on this branch, or rerun without --create-pr against an existing pull request",
            })),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn preflight_join_source(
    context: &AppContext,
    provider: &impl MembershipProvider,
    status: &StatusOutput,
    source: &crate::model::BranchSnapshot,
    predecessor: &JoinPredecessorReceipt,
    operation_deadline: std::time::Instant,
) -> Result<JoinSourceReceipt, AppError> {
    let timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    provider
        .verify_branch_head(&status.repository, &source.name, &source.oid)
        .map_err(|error| {
            AppError::structured(
                ErrorCategory::Validation,
                "join_source_head_moved",
                "source branch moved before exact join planning",
                Some(json!({"source": source, "error": error.to_string(), "mutated": false})),
            )
        })?;
    let runner = ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(timeout)
        .with_operation_deadline(operation_deadline);
    let default = &status.analysis.fleet.default_branch;
    let tail = crate::model::BranchSnapshot {
        repository: status.repository.clone(),
        name: predecessor.branch.clone(),
        oid: predecessor.head_oid.clone(),
    };
    crate::compatibility::prepare_branch_snapshots_with_runner(
        &runner,
        "origin",
        &[default.clone(), tail.clone(), source.clone()],
    )?;
    let merge_bases = source_command(
        &runner,
        CommandSpec::new("git").args([
            "merge-base",
            "--all",
            default.oid.0.as_str(),
            source.oid.0.as_str(),
        ]),
        "join_source_parent_ambiguous",
        "could not derive one exact source/default patch boundary",
    )?;
    let boundaries = merge_bases.stdout.lines().collect::<Vec<_>>();
    if boundaries.len() != 1 {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "join_source_parent_ambiguous",
            "source and current default do not have one exact patch boundary",
            Some(
                json!({"source": source, "default": default, "merge_bases": boundaries, "mutated": false}),
            ),
        ));
    }
    let parent = crate::model::BranchSnapshot {
        repository: status.repository.clone(),
        name: default.name.clone(),
        oid: crate::model::CommitOid(boundaries[0].to_owned()),
    };
    let tree_oid = source_oid(
        &runner,
        &format!("{}^{{tree}}", source.oid.0),
        "join_source_tree_invalid",
    )?;
    let patch = source_command(
        &runner,
        CommandSpec::new("git").args([
            "diff",
            "--binary",
            parent.oid.0.as_str(),
            source.oid.0.as_str(),
        ]),
        "join_source_patch_failed",
        "could not derive the exact source-only patch",
    )?;
    let source_title = source_command(
        &runner,
        CommandSpec::new("git").args(["show", "-s", "--format=%s", source.oid.0.as_str()]),
        "join_source_title_failed",
        "could not bind source commit title provenance",
    )?
    .stdout
    .trim()
    .to_owned();
    let cherry = source_command(
        &runner,
        CommandSpec::new("git").args([
            "cherry",
            default.oid.0.as_str(),
            source.oid.0.as_str(),
            parent.oid.0.as_str(),
        ]),
        "join_source_patch_identity_failed",
        "could not compare source patch identities with current default",
    )?;
    let mut source_commits = Vec::new();
    let mut already_landed_commits = Vec::new();
    for line in cherry.stdout.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let sign = fields.next();
        let oid = fields.next().unwrap_or_default();
        if !matches!(sign, Some("+" | "-"))
            || oid.len() != 40
            || !oid.bytes().all(|byte| byte.is_ascii_hexdigit())
            || fields.next().is_some()
        {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "join_source_patch_identity_ambiguous",
                "Git returned an ambiguous source patch identity",
                Some(json!({"line": line, "source": source, "default": default, "mutated": false})),
            ));
        }
        let oid = crate::model::CommitOid(oid.to_owned());
        source_commits.push(oid.clone());
        if sign == Some("-") {
            already_landed_commits.push(oid);
        }
    }
    if !patch.stdout.is_empty() && source_commits.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "join_source_patch_identity_ambiguous",
            "non-empty source content has no bounded stable patch identity",
            Some(json!({"source": source, "parent": parent, "default": default, "mutated": false})),
        ));
    }
    let effective_default = source_command(
        &runner,
        CommandSpec::new("git").args([
            "merge-tree",
            "--write-tree",
            default.oid.0.as_str(),
            source.oid.0.as_str(),
        ]),
        "join_source_effective_patch_conflict",
        "source patch does not have one clean effective result on current default",
    )?;
    let effective_default_tree = crate::model::CommitOid(
        effective_default
            .stdout
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned(),
    );
    let effective_patch = source_command(
        &runner,
        CommandSpec::new("git").args([
            "diff",
            "--binary",
            default.oid.0.as_str(),
            effective_default_tree.0.as_str(),
        ]),
        "join_source_effective_patch_failed",
        "could not derive the source patch not already represented on current default",
    )?;
    let expected_result = if effective_patch.stdout.is_empty() {
        source_oid(
            &runner,
            &format!("{}^{{tree}}", tail.oid.0),
            "join_target_tree_invalid",
        )?
    } else {
        let merged = source_command(
            &runner,
            CommandSpec::new("git").args([
                "merge-tree",
                "--write-tree",
                tail.oid.0.as_str(),
                source.oid.0.as_str(),
            ]),
            "join_source_merge_conflict",
            "source-only patch does not have one clean result on the selected tail",
        )?;
        crate::model::CommitOid(
            merged
                .stdout
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned(),
        )
    };
    if expected_result.0.len() != 40
        || !expected_result
            .0
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(AppError::validation(
            "join_result_tree_invalid",
            "join source preflight did not produce one exact result tree",
        ));
    }
    let receipt = JoinSourceReceipt {
        branch: source.name.clone(),
        head_oid: source.oid.clone(),
        parent,
        tree_oid,
        patch_fingerprint: fnv1a64(patch.stdout.as_bytes()),
        effective_patch_fingerprint: fnv1a64(effective_patch.stdout.as_bytes()),
        source_commits,
        already_landed_commits,
        source_title,
        selected_tail: predecessor.clone(),
        expected_result_tree_oid: expected_result,
    };
    if effective_patch.stdout.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "join_empty_source_noop",
            "source branch has no unique patch beyond current default; join is a zero-mutation no-op",
            Some(json!({"source": receipt, "mutated": false, "noop": true})),
        ));
    }
    Ok(receipt)
}

#[allow(clippy::too_many_lines)]
fn execute_locked(
    context: &AppContext,
    request: &MembershipRequest,
    candidate_pr: Option<u64>,
    operation_deadline: Option<std::time::Instant>,
    github_budget: Option<&crate::command::GithubRequestBudget>,
    preloaded_status: Option<StatusOutput>,
    dispatch_hooks: bool,
) -> Result<MembershipOutput, AppError> {
    if candidate_pr.is_some() && request.create_pr {
        return Err(AppError::validation(
            "remote_candidate_create_conflict",
            "--pr selects an existing provider PR and cannot be combined with --create-pr",
        ));
    }
    if candidate_pr.is_some() && request.operation.is_join() && !context.config.rebase_on_join {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "atomic_join_requires_rebase_on_join",
            "checkout-free atomic join requires explicit `rebase_on_join: true`",
            Some(json!({
                "config_path": context.config_path,
                "candidate_pr": candidate_pr,
                "mutated": false,
                "safe_next_action": "commit `rebase_on_join: true`, then retry the same join command"
            })),
        ));
    }
    let timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let mut operation_deadline = operation_deadline.unwrap_or_else(|| {
        std::time::Instant::now()
            + std::time::Duration::from_secs(context.config.sync.max_duration_secs)
    });
    let mut status = if let Some(number) = candidate_pr {
        let bound = if let Some(status) = preloaded_status {
            read::bind_remote_candidate_from_status(
                context,
                status,
                PrNumber(number),
                github_budget,
            )?
        } else {
            read::status_for_remote_candidate_with_deadline(
                context,
                PrNumber(number),
                operation_deadline,
                github_budget,
            )?
        };
        // Not `bound.exact_deadline`: that is the binding read's own one-command
        // window, already applied to its own runner (bd-84f82d).
        operation_deadline = candidate_operation_deadline(context, operation_deadline);
        bound.status
    } else if request.create_pr {
        read::status_for_pr_creation(context, operation_deadline, github_budget)?
    } else {
        read::status_with_deadline_and_budget(context, operation_deadline, github_budget)?
    };
    let mut checker = GitCompatibilityChecker::new(&context.repository_path, "origin")
        .with_timeout(timeout)
        .with_operation_deadline(operation_deadline);
    let provider_runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(timeout)
        .with_operation_deadline(operation_deadline);
    let provider_runner = github_budget.map_or(provider_runner.clone(), |budget| {
        provider_runner.with_github_request_budget(budget.clone())
    });
    let mut provider = GitHubMutationAdapter::new(provider_runner);
    let repository = status.repository.clone();
    let failure_status = status.clone();
    let default_branch_oid = status.analysis.fleet.default_branch.oid.clone();
    let mut candidate_source_head_oid = status
        .current_pr
        .and_then(|number| status.analysis.pull_requests.get(&number))
        .map(|candidate| candidate.head.oid.clone());
    let initial_join_target = request
        .operation
        .is_join()
        .then(|| resolve_join_target(&status, request))
        .transpose()?;
    let selected_predecessor = initial_join_target.as_ref().map_or_else(
        || {
            Some(JoinPredecessorReceipt {
                // A root has no predecessor PR. Zero is the durable sentinel;
                // branch/OID carry the authoritative default-branch identity.
                pr: PrNumber(0),
                branch: status.default_branch.clone(),
                head_oid: default_branch_oid.clone(),
            })
        },
        |target| {
            Some(JoinPredecessorReceipt {
                pr: target.tail.number,
                branch: target.tail.head.name.clone(),
                head_oid: target.tail.head.oid.clone(),
            })
        },
    );
    let mut join_source_receipt = None;
    if context.config.rebase_on_join {
        validate_membership_source_request(&status, request)?;
        let target = initial_join_target.as_ref();
        if let Some(target) = target
            && let Err(error) = require_current_join_root(&status, target)
        {
            return Err(record_join_preflight_failure(
                context,
                &status,
                request,
                error,
                dispatch_hooks,
            ));
        }
        // Growing a chain past the size the configured deadline can guarantee
        // to drain converts a bounded prefix apply into a permanent refusal.
        // Refuse the join instead, and let the existing prefix keep draining.
        if let Some(target) = target
            && let Some(refusal) = crate::sync::caravan_capacity_refusal(
                context,
                &status,
                candidate_pr.map_or(PrNumber(0), PrNumber),
                Some(target.tail.number),
            )
        {
            return Err(record_join_preflight_failure(
                context,
                &status,
                request,
                crate::sync::caravan_capacity_error(&refusal),
                dispatch_hooks,
            ));
        }
        let predecessor = selected_predecessor
            .as_ref()
            .expect("join predecessor retained");
        let source = if let Some(number) = status.current_pr {
            status
                .analysis
                .pull_requests
                .get(&number)
                .map(|candidate| candidate.head.clone())
                .ok_or_else(|| {
                    AppError::validation(
                        "join_source_missing",
                        "selected join source PR is absent from discovery",
                    )
                })?
        } else {
            let branch = status.current_branch.clone().ok_or_else(|| {
                AppError::validation(
                    "current_branch_not_found",
                    "physical membership with --create-pr requires one named source branch",
                )
            })?;
            let runner = ProcessRunner::in_directory(&context.repository_path)
                .with_timeout(timeout)
                .with_operation_deadline(operation_deadline);
            crate::model::BranchSnapshot {
                repository: status.repository.clone(),
                name: branch,
                oid: source_oid(&runner, "HEAD", "join_source_head_invalid")?,
            }
        };
        join_source_receipt = Some(
            preflight_join_source(
                context,
                &provider,
                &status,
                &source,
                predecessor,
                operation_deadline,
            )
            .map_err(|error| {
                record_join_preflight_failure(context, &status, request, error, dispatch_hooks)
            })?,
        );
        if let Some(target) = target
            && let Err(error) = revalidate_join_root(&status, target, &provider)
        {
            return Err(record_join_preflight_failure(
                context,
                &status,
                request,
                error,
                dispatch_hooks,
            ));
        }
    }
    let mut creation_state = None;
    let mut rewrite_comment_receipt = None;
    let rebase_receipt = if context.config.rebase_on_join {
        if request.create_pr && status.current_pr.is_none() {
            let current_branch = status.current_branch.clone().ok_or_else(|| {
                AppError::validation(
                    "current_branch_not_found",
                    "--create-pr requires a named non-default current branch",
                )
            })?;
            if current_branch == status.default_branch {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "create_pr_on_default_branch",
                    "cannot create a pull request directly from the default branch",
                    Some(json!({
                        "branch": current_branch,
                        "default_branch": status.default_branch,
                        "safe_next_action": "create a topic branch, commit the intended changes, then rerun the same command"
                    })),
                ));
            }
            let desired_base = initial_join_target.as_ref().map_or_else(
                || status.default_branch.clone(),
                |target| target.tail.head.name.clone(),
            );
            let mut state = ExecutionState::new(request.operation);
            crate::initialization::require_ready(&status.initialization)?;
            preflight_repository(
                &provider,
                &status.repository,
                &status.default_branch,
                request.operation,
                &request.agent_priority_labels,
                context.config.sync.actions.join_unlabelled_prs,
                status.head_merge.actor.caravan(),
                &state,
            )?;
            // bd-032049: prove the branch carries unique work BEFORE creating a
            // pull request. Creating first let an empty candidate become a real
            // PR whose "work" was an already-landed default-branch advance,
            // which was then replayed onto the caravan tail.
            require_nonempty_source_branch(context, &status, &current_branch, operation_deadline)?;
            let receipt = provider
                .create_pull_request(
                    &status.repository,
                    &CreatePullRequestInput {
                        head: current_branch,
                        base: desired_base,
                        draft: false,
                    },
                )
                .map_err(|error| mutation_error(&error, &state))?;
            let created = receipt.after.clone();
            candidate_source_head_oid = Some(created.head.oid.clone());
            state.record(
                receipt,
                "created pull request before exact physical join preflight",
            );
            creation_state = Some(state);
            status =
                read::status_with_deadline_and_budget(context, operation_deadline, github_budget)?;
            if status.current_pr != Some(created.number) {
                return Err(AppError::structured(
                    ErrorCategory::ExecutionFailure,
                    "created_pr_rediscovery_failed",
                    "created pull request was not the current branch PR after exact rediscovery",
                    Some(json!({
                        "created_pr": created.number,
                        "current_pr": status.current_pr,
                        "resumable": true,
                        "safe_next_action": "inspect the preserved open PR and rerun the same membership command"
                    })),
                ));
            }
        }
        let number = status.current_pr.ok_or_else(|| {
            AppError::validation(
                "current_pr_not_found",
                "rebase_on_join requires the current branch to have an open PR",
            )
        })?;
        let candidate = status
            .analysis
            .pull_requests
            .get(&number)
            .cloned()
            .ok_or_else(|| {
                AppError::validation(
                    "current_pr_missing_from_snapshot",
                    "current PR was not included in discovery",
                )
            })?;
        if request
            .reason
            .as_deref()
            .is_some_and(|reason| reason.trim().is_empty())
            || (request.tail_pr.is_some() && request.head_pr.is_some())
        {
            return Err(AppError::validation(
                "rebase_membership_input_invalid",
                "membership input must pass complete validation before a branch rewrite",
            ));
        }
        if request.priority_label.as_deref().is_some_and(|label| {
            !request
                .agent_priority_labels
                .iter()
                .any(|configured| configured == label.trim())
        }) {
            return Err(AppError::validation(
                "priority_label_not_configured",
                "priority label is not an exact configured agent_priority_labels entry",
            ));
        }
        let join_target = initial_join_target.clone();
        // A root has no tail to follow; only a root is squash-merged by cara.
        let is_caravan_root = join_target.is_none();
        let desired_base = join_target.as_ref().map_or_else(
            || status.default_branch.clone(),
            |target| target.tail.head.name.clone(),
        );
        let preflight_state = ExecutionState::new(request.operation);
        crate::initialization::require_ready(&status.initialization)?;
        preflight_repository(
            &provider,
            &status.repository,
            &status.default_branch,
            request.operation,
            &request.agent_priority_labels,
            context.config.sync.actions.join_unlabelled_prs,
            status.head_merge.actor.caravan(),
            &preflight_state,
        )?;
        validate_operation_shape(&candidate, request, &desired_base)?;
        preflight_eligibility(&status, &candidate, request, join_target.as_ref(), &checker)?;
        let rewrite_reason = join_target.as_ref().map_or(
            crate::physical_rebase::BranchRewriteReason::CurrentDefaultAdvanced,
            |target| crate::physical_rebase::BranchRewriteReason::JoinedCaravan {
                parent_pr: target.tail.number,
            },
        );
        let target = join_target.map_or_else(
            || status.analysis.fleet.default_branch.clone(),
            |target| target.tail.head,
        );
        let mut planning_candidate = candidate.clone();
        if let Some(source) = join_source_receipt.as_ref() {
            planning_candidate.base.clone_from(&source.parent);
        }
        let range_source = join_source_receipt.as_ref().map_or_else(
            || crate::physical_rebase::range_base_for_remote_target(&planning_candidate, &target),
            |source| {
                if source.parent.name == status.analysis.fleet.default_branch.name
                    && source.parent.oid != status.analysis.fleet.default_branch.oid
                {
                    crate::physical_rebase::PlannedRangeBase::HistoricalSourceBranch {
                        branch: source.parent.clone(),
                        current: status.analysis.fleet.default_branch.clone(),
                    }
                } else {
                    crate::physical_rebase::range_base_for_remote_target(
                        &planning_candidate,
                        &target,
                    )
                }
            },
        );
        let prepared = crate::physical_rebase::prepare_candidate(
            &context.repository_path,
            &repository,
            &planning_candidate,
            range_source,
            crate::physical_rebase::PlannedBase::Remote(target.clone()),
            &status.analysis.fleet.default_branch,
            crate::physical_rebase::RebaseExecutionBudget::new(timeout)
                .with_deadline(operation_deadline)
                .because(rewrite_reason)
                // bd-abd929: bd-85b71d applied this reasoning inside sync but never
                // here: a caravan ROOT is squash-merged by cara, so its history
                // is discarded at landing and a merge-preserving replay proves
                // nothing. Flatten onto the exact target instead of refusing
                // with `rebase_merge_tree_mismatch`. A member joining behind a
                // tail is never flattened, because its ancestry must physically
                // follow the chain.
                .flattening_squashed_root(
                    is_caravan_root
                        && context.config.sync.resolved_head_merge_actor()
                            == crate::model::HeadMergeActor::Caravan,
                ),
        )?;
        if let Some(source) = join_source_receipt.as_ref() {
            let planned_base = match &prepared.plan.new_base {
                crate::physical_rebase::PlannedBase::Remote(base)
                | crate::physical_rebase::PlannedBase::Simulated(base) => base,
            };
            if prepared.plan.old_head_oid != source.head_oid
                || prepared.plan.old_base_oid != source.parent.oid
                || planned_base.name != source.selected_tail.branch
                || planned_base.oid != source.selected_tail.head_oid
                || prepared.plan.new_tree_oid != source.expected_result_tree_oid
            {
                let error = AppError::structured(
                    ErrorCategory::Validation,
                    "join_source_result_mismatch",
                    "physical join plan does not equal the exact source-only patch receipt",
                    Some(json!({
                        "source": source,
                        "plan": prepared.plan,
                        "mutated_branch": false,
                        "safe_next_action": "rediscover source/default/tail facts and retry without provider mutation",
                    })),
                );
                return Err(record_join_preflight_failure(
                    context,
                    &status,
                    request,
                    error,
                    dispatch_hooks,
                ));
            }
        }
        if let Some(target) = initial_join_target.as_ref()
            && let Err(error) = revalidate_join_root(&status, target, &provider)
        {
            return Err(record_join_preflight_failure(
                context,
                &status,
                request,
                error,
                dispatch_hooks,
            ));
        }
        revalidate_generation_before_membership(&status, candidate.number, &provider)?;
        // `caravan-force` is durable PR-scoped intent. Rewriting the branch does
        // not change PR identity and GitHub naturally retains its labels, so no
        // invalidate/restore transaction belongs before this push (bd-91e96a).
        // bd-4e4615: the branch rewrite is irreversible, and mandatory
        // post-rewrite rediscovery plus membership writes still have to run.
        // Starting the push with almost no budget left produced a live
        // github_mutation_timeout *after* the remote branch had already moved.
        require_post_rewrite_budget(context, Some(operation_deadline), candidate.number)?;
        let receipt = crate::physical_rebase::apply_prepared(&prepared)?;
        // GitHub is authoritative after a push. Never apply base/label changes
        // against the stale pre-rewrite PR snapshot.
        if let Some(candidate) = candidate_pr {
            let rediscovery_deadline = std::time::Instant::now() + timeout;
            let bound = read::status_for_remote_candidate_with_deadline(
                context,
                PrNumber(candidate),
                rediscovery_deadline,
                github_budget,
            )?;
            // Same as the binding above: keep a candidate-scoped share of the
            // tick, never a single command window (bd-84f82d).
            operation_deadline = candidate_operation_deadline(context, operation_deadline);
            status = bound.status;
            checker = GitCompatibilityChecker::new(&context.repository_path, "origin")
                .with_timeout(timeout)
                .with_operation_deadline(operation_deadline);
            let provider_runner =
                crate::command::ProcessRunner::in_directory(&context.repository_path)
                    .with_timeout(timeout)
                    .with_operation_deadline(operation_deadline);
            let provider_runner = github_budget.map_or(provider_runner.clone(), |budget| {
                provider_runner.with_github_request_budget(budget.clone())
            });
            provider = GitHubMutationAdapter::new(provider_runner);
        } else {
            status =
                read::status_with_deadline_and_budget(context, operation_deadline, github_budget)?;
        }
        let observed = status.analysis.pull_requests.get(&number).ok_or_else(|| {
            AppError::structured(
                ErrorCategory::ExecutionFailure,
                "rebase_rediscovery_failed",
                "rewritten PR was absent from post-push discovery",
                Some(json!({"rebase_receipt": receipt, "resumable": true})),
            )
        })?;
        if observed.head.oid != receipt.new_head_oid {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "rebase_rediscovery_stale",
                "provider did not report the exact pushed head after rewrite",
                Some(
                    json!({"rebase_receipt": receipt, "observed_oid": observed.head.oid, "resumable": true}),
                ),
            ));
        }
        rewrite_comment_receipt =
            crate::sync::ensure_branch_rewrite_comment(&provider, &repository, observed, &receipt)
                .map_err(|error| {
                    attach_rebase_receipt(mutation_error(&error, &preflight_state), Some(&receipt))
                })?;
        Some(receipt)
    } else {
        None
    };
    let execution = execute_with_rebase_guard(
        status,
        &checker,
        &provider,
        request.clone(),
        rebase_receipt.as_ref(),
        context.config.sync.actions.join_unlabelled_prs,
    )
    .map_err(|error| attach_rebase_receipt(error, rebase_receipt.as_ref()));
    let mut output = match execution {
        Ok(output) => output,
        Err(error) if request.operation.is_join() => {
            let event = join_failed_event(&failure_status, request, &error);
            let error = hooks::attach_events(error, std::slice::from_ref(&event));
            if dispatch_hooks {
                let deliveries = hooks::dispatch_events(context, std::slice::from_ref(&event))?;
                return Err(hooks::attach_deliveries(error, &deliveries));
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    if let Some(comment) = rewrite_comment_receipt {
        let already = comment
            .provider_output
            .as_deref()
            .is_some_and(|value| value.starts_with("existing GitHub comment"));
        output.receipt.completed_steps.push(MutationStep {
            kind: MutationKind::Comment,
            state: if already {
                MutationStepState::AlreadySatisfied
            } else {
                MutationStepState::Completed
            },
            pr: Some(comment.after.number),
            summary: "posted one-line branch rewrite reason".to_owned(),
        });
        output.provider_receipts.push(comment);
        output.receipt.changed = true;
    }
    if let Some(mut created) = creation_state {
        created.steps.append(&mut output.receipt.completed_steps);
        output.receipt.completed_steps = created.steps;
        output.receipt.changed = true;
        created
            .provider_receipts
            .append(&mut output.provider_receipts);
        output.provider_receipts = created.provider_receipts;
    }
    let kind = if request.operation.is_join() {
        EventKind::PrJoined
    } else {
        EventKind::CaravanCreated
    };
    if request.operation.is_join() || context.config.rebase_on_join {
        let join_receipt = build_join_receipt(
            context,
            &repository,
            &failure_status,
            JoinReceiptEvidence {
                predecessor: selected_predecessor,
                candidate_source_head_oid,
                source: join_source_receipt,
                default_branch_oid,
                rebase_receipt: rebase_receipt.as_ref(),
            },
            &output,
        )?;
        if context.config.rebase_on_join
            && (!join_receipt.ancestry_verified || !join_receipt.membership_durable)
        {
            return Err(AppError::structured(
                ErrorCategory::ExecutionFailure,
                "atomic_membership_receipt_incomplete",
                "membership completed without exact ancestry and durable membership proof",
                Some(json!({"join_receipt": join_receipt, "resumable": true})),
            ));
        }
        output.join_receipt = Some(join_receipt);
    }
    output.rebase_receipt = rebase_receipt;
    let event = hooks::event(
        kind,
        output.receipt.operation_id.clone(),
        repository,
        Some(output.caravan_id),
        vec![output.pull_request.number],
        None,
        None,
        BTreeMap::from([
            ("receipt".to_owned(), json!(output.receipt)),
            ("join_receipt".to_owned(), json!(output.join_receipt)),
        ]),
    );
    output.events.push(event);
    if dispatch_hooks {
        output.hook_deliveries = hooks::dispatch_events(context, &output.events)?;
    }
    Ok(output)
}

struct JoinReceiptEvidence<'a> {
    predecessor: Option<JoinPredecessorReceipt>,
    candidate_source_head_oid: Option<crate::model::CommitOid>,
    source: Option<JoinSourceReceipt>,
    default_branch_oid: crate::model::CommitOid,
    rebase_receipt: Option<&'a crate::physical_rebase::RebaseReceipt>,
}

fn build_join_receipt(
    context: &AppContext,
    repository: &RepositoryId,
    before: &StatusOutput,
    evidence: JoinReceiptEvidence<'_>,
    output: &MembershipOutput,
) -> Result<JoinReceipt, AppError> {
    let predecessor = evidence.predecessor.ok_or_else(|| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "join_receipt_predecessor_missing",
            "successful membership operation did not retain its exact predecessor receipt",
            Some(json!({"candidate_pr": output.pull_request.number})),
        )
    })?;
    let source_head = evidence.candidate_source_head_oid.ok_or_else(|| {
        AppError::structured(
            ErrorCategory::ExecutionFailure,
            "join_receipt_source_missing",
            "successful membership operation did not retain its source head",
            Some(json!({"candidate_pr": output.pull_request.number})),
        )
    })?;
    let source = evidence.source;
    if context.config.rebase_on_join && source.is_none() {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "join_receipt_source_provenance_missing",
            "physical membership did not retain exact source patch provenance",
            Some(json!({"candidate_pr": output.pull_request.number})),
        ));
    }
    let force_was_present = before
        .analysis
        .pull_requests
        .get(&output.pull_request.number)
        .is_some_and(|candidate| candidate.has_label(FORCE_LABEL));
    let force_intent = if force_was_present {
        JoinForceIntent::Preserved
    } else {
        JoinForceIntent::Absent
    };
    let ancestry_verified = evidence.rebase_receipt.is_some_and(|receipt| {
        source.as_ref().is_some_and(|source| {
            receipt.pr == output.pull_request.number
                && receipt.old_head_oid == source.head_oid
                && receipt.old_base_oid == source.parent.oid
                && receipt.new_head_oid == output.pull_request.head.oid
                && receipt.new_base_branch == predecessor.branch
                && receipt.new_base_oid == predecessor.head_oid
                && receipt.new_tree_oid == source.expected_result_tree_oid
        })
    });
    let membership_durable = output.pull_request.has_label(ACTIVE_LABEL)
        && output.pull_request.has_label(FORCE_LABEL) == force_was_present
        && output.pull_request.base.name == predecessor.branch
        && output.pull_request.base.oid == predecessor.head_oid;
    let mut receipt = JoinReceipt {
        schema_version: 1,
        operation_id: output.receipt.operation_id.clone(),
        repository: repository.clone(),
        caravan_id: output.caravan_id,
        candidate_pr: output.pull_request.number,
        candidate_source_head_oid: source_head,
        source,
        predecessor,
        default_branch_oid: evidence.default_branch_oid,
        result: JoinResultReceipt {
            head_oid: output.pull_request.head.oid.clone(),
            base_ref: output.pull_request.base.name.clone(),
            base_oid: output.pull_request.base.oid.clone(),
            state: output.pull_request.state,
        },
        rebase_on_join: context.config.rebase_on_join,
        ancestry_verified,
        membership_durable,
        force_intent,
        config_fingerprint: membership_config_fingerprint(context),
        rebase_receipt: evidence.rebase_receipt.cloned(),
        provider_receipts: output.provider_receipts.clone(),
        receipt_hash: String::new(),
    };
    let material = serde_json::to_vec(&receipt).expect("join receipt serializes");
    receipt.receipt_hash = fnv1a64(&material);
    Ok(receipt)
}

pub(crate) fn fnv1a64(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("fnv1a64:{hash:016x}")
}

fn membership_config_fingerprint(context: &AppContext) -> String {
    let contract = serde_json::to_vec(&json!({
        "version": context.config.version,
        "rebase_on_join": context.config.rebase_on_join,
        "force_merge": context.config.force_merge,
        "agent_priority_labels": &context.config.agent_priority_labels,
        "sync": &context.config.sync,
    }))
    .expect("validated config serializes");
    fnv1a64(&contract)
}

fn attach_rebase_receipt(
    error: AppError,
    receipt: Option<&crate::physical_rebase::RebaseReceipt>,
) -> AppError {
    let Some(receipt) = receipt else {
        return error;
    };
    let mut details = error.details().unwrap_or_else(|| json!({}));
    if let Some(object) = details.as_object_mut() {
        object.insert("rebase_receipt".to_owned(), json!(receipt));
        object.insert("resumable".to_owned(), json!(true));
        object.insert(
            "next".to_owned(),
            json!("rediscover provider state and rerun the same idempotent membership command"),
        );
    }
    AppError::structured(
        error.category(),
        error.code(),
        error.message(),
        Some(details),
    )
}

fn record_join_preflight_failure(
    context: &AppContext,
    status: &StatusOutput,
    request: &MembershipRequest,
    error: AppError,
    dispatch_hooks: bool,
) -> AppError {
    let event = join_failed_event(status, request, &error);
    let error = hooks::attach_events(error, std::slice::from_ref(&event));
    if !dispatch_hooks {
        return error;
    }
    match hooks::dispatch_events(context, &[event]) {
        Ok(deliveries) => hooks::attach_deliveries(error, &deliveries),
        Err(dispatch_error) => dispatch_error,
    }
}

fn join_failed_event(
    status: &StatusOutput,
    request: &MembershipRequest,
    error: &AppError,
) -> CaravanEvent {
    let mut prs = BTreeSet::new();
    prs.extend(status.current_pr);
    prs.extend(request.tail_pr.map(PrNumber));
    prs.extend(request.head_pr.map(PrNumber));
    let caravan_id = request.head_pr.map(PrNumber).or_else(|| {
        request
            .tail_pr
            .map(PrNumber)
            .and_then(|tail| status.analysis.fleet.containing(tail))
            .map(|caravan| caravan.id)
    });
    hooks::event(
        EventKind::JoinFailed,
        hooks::operation_id_from_error(error),
        status.repository.clone(),
        caravan_id,
        prs.into_iter().collect(),
        Some(status.analysis.fleet.clone()),
        Some(error.to_string()),
        BTreeMap::from([
            ("error_code".to_owned(), json!(error.code())),
            (
                "error_details".to_owned(),
                error.details().unwrap_or_default(),
            ),
        ]),
    )
}

#[cfg(test)]
fn execute(
    status: StatusOutput,
    checker: &impl CompatibilityChecker,
    provider: &impl MembershipProvider,
    request: MembershipRequest,
) -> Result<MembershipOutput, AppError> {
    execute_with_rebase_guard(status, checker, provider, request, None, false)
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
fn execute_with_rebase_guard(
    mut status: StatusOutput,
    checker: &impl CompatibilityChecker,
    provider: &impl MembershipProvider,
    request: MembershipRequest,
    expected_rebase: Option<&crate::physical_rebase::RebaseReceipt>,
    require_auto_admission_skip: bool,
) -> Result<MembershipOutput, AppError> {
    if request
        .reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err(AppError::validation(
            "membership_reason_empty",
            "--reason must contain a non-empty rationale when supplied",
        ));
    }
    if request.tail_pr.is_some() && request.head_pr.is_some() {
        return Err(AppError::validation(
            "ambiguous_target",
            "--tail-pr and --head-pr are mutually exclusive",
        ));
    }

    crate::initialization::require_ready(&status.initialization)?;
    let mut state = ExecutionState::new(request.operation);
    preflight_repository(
        provider,
        &status.repository,
        &status.default_branch,
        request.operation,
        &request.agent_priority_labels,
        require_auto_admission_skip,
        status.head_merge.actor.caravan(),
        &state,
    )?;
    let desired_priority_label = request.priority_label.as_deref().map(str::trim);
    if let Some(label) = desired_priority_label {
        if label.is_empty()
            || !request
                .agent_priority_labels
                .iter()
                .any(|item| item == label)
        {
            return Err(AppError::validation(
                "priority_label_not_configured",
                format!(
                    "priority label `{label}` is not an exact configured agent_priority_labels entry"
                ),
            ));
        }
    }

    let target = if request.operation.is_join() {
        Some(resolve_join_target(&status, &request)?)
    } else {
        None
    };
    if let Some(rebase) = expected_rebase {
        validate_post_rebase_target(&status, &request, target.as_ref(), rebase)?;
    }
    let desired_base = target.as_ref().map_or_else(
        || status.default_branch.clone(),
        |target| target.tail.head.name.clone(),
    );

    if status.current_pr.is_none() {
        if !request.create_pr {
            return Err(AppError::validation(
                "current_pr_not_found",
                "the current branch has no open PR; pass --create-pr to create one non-interactively",
            ));
        }
        let current_branch = status.current_branch.clone().ok_or_else(|| {
            AppError::validation(
                "current_branch_not_found",
                "--create-pr requires a named current branch",
            )
        })?;
        let receipt = provider
            .create_pull_request(
                &status.repository,
                &CreatePullRequestInput {
                    head: current_branch,
                    base: desired_base.clone(),
                    draft: false,
                },
            )
            .map_err(|error| mutation_error(&error, &state))?;
        let created = receipt.after.clone();
        status.current_pr = Some(created.number);
        status
            .analysis
            .pull_requests
            .insert(created.number, created.clone());
        state.record(receipt, "created pull request non-interactively");
    }

    let current_number = status.current_pr.expect("created or discovered current PR");
    let candidate = status
        .analysis
        .pull_requests
        .get(&current_number)
        .cloned()
        .ok_or_else(|| {
            AppError::validation(
                "current_pr_missing_from_snapshot",
                format!("PR #{current_number} was not included in discovery"),
            )
        })?;

    validate_operation_shape(&candidate, &request, &desired_base)?;
    let eligibility =
        preflight_eligibility(&status, &candidate, &request, target.as_ref(), checker)?;
    revalidate_generation_before_membership(&status, candidate.number, provider)?;
    let before_labels = candidate.labels.clone();
    let admission_priority_basis = desired_priority_label.map_or_else(
        || {
            status
                .admission
                .candidates
                .iter()
                .find(|item| item.pr == current_number)
                .map_or_else(
                    || "FIFO (oldest eligible PR first); no explicit agent priority".to_owned(),
                    |item| item.reason.clone(),
                )
        },
        |label| {
            let rank = request
                .agent_priority_labels
                .iter()
                .position(|item| item == label)
                .expect("validated label")
                + 1;
            format!("explicit configured agent priority label `{label}` (rank {rank}, 1 highest)")
        },
    );
    // Durable audit records why intent-aware order permitted this operation,
    // including any FIFO row bypassed only because it is still unjoined.
    let admission_priority_basis = eligibility.admission_intent.as_ref().map_or_else(
        || admission_priority_basis.clone(),
        |decision| {
            format!(
                "{admission_priority_basis}; selection={} intent={} order={:?} {}",
                decision.selection.name(),
                decision.intent.name(),
                decision.outcome,
                decision.reason
            )
        },
    );
    state.current = Some(candidate);

    state.ensure_base(provider, &status.repository, &desired_base)?;
    // Durable force intent follows the PR through base/position changes. Only
    // explicit revoke, eviction, or successful merge consumes it (bd-91e96a).
    // A generation-bound automatic skip is advisory only; every explicit
    // membership operation is a manual override and consumes it.
    state.ensure_label_absent(provider, &status.repository, SKIPPED_LABEL)?;
    for label in &request.agent_priority_labels {
        if Some(label.as_str()) != desired_priority_label {
            state.ensure_label_absent(provider, &status.repository, label)?;
        }
    }
    if let Some(label) = desired_priority_label {
        state.ensure_label_present(provider, &status.repository, label)?;
    }
    if request.operation.is_renewal() {
        state.ensure_label_absent(provider, &status.repository, EVICTED_LABEL)?;
    }
    state.ensure_label_present(provider, &status.repository, ACTIVE_LABEL)?;
    // Under the caravan-owned merge actor nobody delegates to the provider,
    // not even a fresh root: cara promotes and squashes it itself.
    if request.operation.is_join() || status.head_merge.actor.caravan() {
        state.ensure_auto_merge_disabled(provider, &status.repository)?;
    } else {
        state.ensure_squash_auto_merge(provider, &status.repository)?;
    }
    let audit = membership_audit(
        &request,
        &before_labels,
        &eligibility,
        state.current.as_ref().expect("current PR"),
        admission_priority_basis,
    );
    state.ensure_control_label_comment(provider, &status.repository, &audit)?;

    let receipt = state.operation_receipt();
    let pull_request = state
        .current
        .expect("membership operation has a current PR");
    let caravan_id = target
        .as_ref()
        .map_or(pull_request.number, |target| target.caravan.id);
    let mut admission_intent = eligibility.admission_intent;
    if let Some(decision) = admission_intent.as_mut() {
        decision.record_execution(receipt.changed);
    }
    // Only for a ROOT admission: a join already names its target, so the caller
    // has plainly seen the alternative.
    let coexisting_caravans = if request.operation.is_join() {
        Vec::new()
    } else {
        status
            .analysis
            .fleet
            .caravans
            .iter()
            .map(|caravan| caravan.id)
            .filter(|id| *id != caravan_id)
            .collect()
    };
    Ok(MembershipOutput {
        receipt,
        rebase_receipt: None,
        provider_receipts: state.provider_receipts,
        join_receipt: None,
        pull_request,
        caravan_id,
        coexisting_caravans,
        admission_intent,
        events: Vec::new(),
        hook_deliveries: Vec::new(),
    })
}

#[derive(Debug, Clone)]
struct JoinTarget {
    caravan: Caravan,
    tail: PullRequestSnapshot,
}

fn comment_error(error: &MutationError, state: &ExecutionState) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "github_comment_failed",
        format!("control labels changed but their durable GitHub comment failed: {error}"),
        Some(json!({
            "stage": "control_label_comment",
            "operation_id": state.operation_id,
            "completed_steps": state.steps,
            "provider_receipts": state.provider_receipts,
            "resumable": true,
            "dedupe": "deterministic GitHub-visible caravan-control-label-audit marker",
            "next": format!("rediscover and rerun `cara {}`", state.operation.name()),
        })),
    )
}

fn mutation_error(error: &MutationError, state: &ExecutionState) -> AppError {
    if let MutationError::Provider(DiscoveryError::Runner(CommandRunError::OutputLimit {
        command,
        code,
        stdout,
        stderr,
    })) = error
    {
        return AppError::structured(
            ErrorCategory::ExecutionFailure,
            "command_output_limit",
            error.to_string(),
            Some(json!({
                "stage": "github_mutation_output",
                "command": command.display(),
                "exit_code": code,
                "stdout": stdout,
                "stderr": stderr,
                "streams_combined": false,
                "operation_id": state.operation_id,
                "completed_steps": state.steps,
                "provider_receipts": state.provider_receipts,
                "resumable": true,
                "next": format!("reduce provider output, rediscover, and rerun `cara {}`", state.operation.name()),
            })),
        );
    }
    if let MutationError::Provider(DiscoveryError::Runner(
        CommandRunError::GithubRequestBudgetExceeded {
            command,
            limit,
            used,
        },
    )) = error
    {
        return AppError::structured(
            ErrorCategory::ExecutionFailure,
            "github_request_budget_exhausted",
            error.to_string(),
            Some(json!({
                "stage": "github_mutation",
                "command": command.display(),
                "limit": limit,
                "used": used,
                "operation_id": state.operation_id,
                "completed_steps": state.steps,
                "provider_receipts": state.provider_receipts,
                "resumable": true,
                "next": "rerun the same bounded sync tick to continue from fresh provider state",
            })),
        );
    }
    if let MutationError::Provider(DiscoveryError::Runner(CommandRunError::Timeout {
        command,
        timeout_ms,
        stdout,
        stderr,
        ..
    })) = error
    {
        return AppError::structured(
            ErrorCategory::Timeout,
            "github_mutation_timeout",
            error.to_string(),
            Some(json!({
                "stage": "github_mutation",
                "command": command.display(),
                "timeout_ms": timeout_ms,
                "stdout": stdout,
                "stderr": stderr,
                "operation_id": state.operation_id,
                "completed_steps": state.steps,
                "provider_receipts": state.provider_receipts,
                "resumable": true,
                "next": format!("rediscover and rerun `cara {}`", state.operation.name()),
            })),
        );
    }
    let (category, code) = if matches!(error, MutationError::StalePrecondition { .. }) {
        (ErrorCategory::Validation, "stale_precondition")
    } else {
        (ErrorCategory::ExecutionFailure, "github_mutation_failed")
    };
    AppError::structured(
        category,
        code,
        error.to_string(),
        Some(json!({
            "error": format!("{error:?}"),
            "operation_id": state.operation_id,
            "completed_steps": state.steps,
            "provider_receipts": state.provider_receipts,
            "resumable": true,
            "next": format!("rediscover and rerun `cara {}`", state.operation.name()),
        })),
    )
}

#[cfg(test)]
mod tests;
