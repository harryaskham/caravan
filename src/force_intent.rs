//! Reviewed exact-head force-intent preview, apply, and revoke contract.
//!
//! This is the machine contract consumed by Cacophony's controller-gated
//! `caco cara force-*` surface.  It is deliberately separate from the compact
//! human `cara force` operation: every invocation binds an exact provider head,
//! complete Caravan membership generation, current CI-decision fingerprint,
//! bounded expiry, and the squash-only provider transition.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, ValueEnum};
use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::github::{GitHubMutationAdapter, GitHubMutationReceipt, MutationError};
use crate::model::{
    BranchSnapshot, CheckSnapshot, CheckState, CompatibilityOutcome, GraphProblemKind, MergeMethod,
    MutationKind, MutationStep, MutationStepState, OperationId, OperationReceipt, PrNumber,
    PullRequestPrecondition, PullRequestSnapshot, PullRequestState, RepositoryId,
};
use crate::operation_lock::OperationLock;
use crate::read::StatusOutput;
use crate::{AppContext, AppError};

const FORCE_LABEL: &str = "caravan-force";
const MAX_REASON_BYTES: usize = 2_000;
const MAX_GENERATION_BYTES: usize = 256;
const MAX_AUTHORITY_LIFETIME_MS: u64 = 24 * 60 * 60 * 1_000;

/// The only merge method reviewed force authority may arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ReviewedAutoMerge {
    Squash,
}

/// Exact authority supplied by the reviewed controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
pub struct ReviewedForceIntentInput {
    /// Exact active Caravan head PR.
    #[arg(long, value_name = "PR")]
    pub pr: u64,
    /// Exact 40-character provider head OID.
    #[arg(long, value_name = "OID")]
    pub head: String,
    /// Exact membership generation obtained from current Cara evidence.
    #[arg(long, value_name = "GENERATION")]
    pub membership_generation: String,
    /// Exact current CI-decision fingerprint obtained from current Cara evidence.
    #[arg(long, value_name = "FNV1A64")]
    pub failure_fingerprint: String,
    /// Bounded reviewed rationale.
    #[arg(long, value_name = "TEXT")]
    pub reason: String,
    /// Absolute Unix epoch expiry in milliseconds.
    #[arg(long, value_name = "MILLISECONDS")]
    pub expires_at_ms: u64,
    /// Reviewed native auto-merge method; only squash is accepted.
    #[arg(long, value_enum)]
    pub auto_merge: ReviewedAutoMerge,
}

/// Reviewed force-intent action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewedForceIntentAction {
    Preview,
    Apply,
    Revoke,
}

impl ReviewedForceIntentAction {
    fn name(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Apply => "apply",
            Self::Revoke => "revoke",
        }
    }

    fn operation_name(self) -> &'static str {
        match self {
            Self::Preview => "force_intent_preview",
            Self::Apply => "force_intent_apply",
            Self::Revoke => "force_intent_revoke",
        }
    }
}

/// One exact member incorporated into the membership generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForceIntentMemberEvidence {
    pub pr: PrNumber,
    pub head: BranchSnapshot,
    pub base: BranchSnapshot,
}

/// Exact current Caravan membership and its deterministic generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForceIntentMembershipEvidence {
    pub generation: String,
    pub caravan_id: PrNumber,
    pub default_branch: BranchSnapshot,
    #[serde(default)]
    pub members: Vec<ForceIntentMemberEvidence>,
}

/// Exact check rollup consumed by the decision fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForceIntentRequiredChecksEvidence {
    #[serde(default)]
    pub checks: Vec<CheckSnapshot>,
    pub state: String,
    pub failure_fingerprint: String,
}

/// Exact current queue decision consumed by the shared fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForceIntentDecisionEvidence {
    pub kind: String,
    pub state: String,
    pub pr: PrNumber,
    pub head: String,
    pub failure_fingerprint: String,
}

/// Stable Cara evidence consumed verbatim by Cacophony's validator.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewedForceIntentOutput {
    pub action: String,
    pub pr: u64,
    pub head: String,
    pub provider_head: String,
    pub membership_generation: String,
    pub membership: ForceIntentMembershipEvidence,
    pub failure_fingerprint: String,
    pub required_checks: ForceIntentRequiredChecksEvidence,
    pub current_decision: ForceIntentDecisionEvidence,
    pub expires_at_ms: u64,
    pub force_intent_applied: bool,
    pub squash_auto_merge_enabled: bool,
    pub atomic_provider_transaction: bool,
    pub mutated: bool,
    pub reason: String,
    pub receipt: OperationReceipt,
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    pub next: String,
}

#[derive(Debug, Clone)]
struct CurrentEvidence {
    pull: PullRequestSnapshot,
    membership: ForceIntentMembershipEvidence,
    checks: ForceIntentRequiredChecksEvidence,
    decision: ForceIntentDecisionEvidence,
    failure_fingerprint: String,
}

/// Result of one provider-level exact precondition plus single GraphQL mutation.
pub(crate) struct AtomicForceProviderReceipt {
    pub receipt: GitHubMutationReceipt,
    pub transaction_performed: bool,
}

trait ReviewedForceProvider {
    fn verify_precondition(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<PullRequestSnapshot, MutationError>;
    fn verify_branch_head(
        &self,
        repository: &RepositoryId,
        branch: &str,
        expected: &crate::model::CommitOid,
    ) -> Result<(), MutationError>;
    fn viewer_permission(&self, repository: &RepositoryId) -> Result<String, MutationError>;
    fn update_force_state(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        force_intent_present: bool,
        ensure_squash_auto_merge: bool,
    ) -> Result<AtomicForceProviderReceipt, MutationError>;
    fn ensure_marked_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        marker: &str,
        body: &str,
    ) -> Result<GitHubMutationReceipt, MutationError>;
}

impl<R: crate::command::CommandRunner> ReviewedForceProvider for GitHubMutationAdapter<R> {
    fn verify_precondition(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
    ) -> Result<PullRequestSnapshot, MutationError> {
        self.verify_precondition(repository, expected)
    }

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

    fn update_force_state(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        force_intent_present: bool,
        ensure_squash_auto_merge: bool,
    ) -> Result<AtomicForceProviderReceipt, MutationError> {
        let (receipt, transaction_performed) = self.atomic_label_and_squash_auto_merge(
            repository,
            expected,
            FORCE_LABEL,
            force_intent_present,
            ensure_squash_auto_merge,
        )?;
        Ok(AtomicForceProviderReceipt {
            receipt,
            transaction_performed,
        })
    }

    fn ensure_marked_comment(
        &self,
        repository: &RepositoryId,
        expected: &PullRequestPrecondition,
        marker: &str,
        body: &str,
    ) -> Result<GitHubMutationReceipt, MutationError> {
        self.ensure_marked_comment(repository, expected, marker, body)
    }
}

/// Re-read and return exact force evidence without provider mutation.
pub fn preview(
    context: &AppContext,
    input: &ReviewedForceIntentInput,
) -> Result<ReviewedForceIntentOutput, AppError> {
    execute_live(context, input, ReviewedForceIntentAction::Preview)
}

/// Apply exact-head force intent and squash auto-merge through one provider mutation.
pub fn apply(
    context: &AppContext,
    input: &ReviewedForceIntentInput,
) -> Result<ReviewedForceIntentOutput, AppError> {
    execute_live(context, input, ReviewedForceIntentAction::Apply)
}

/// Revoke exact-generation force intent, including after authority expiry.
pub fn revoke(
    context: &AppContext,
    input: &ReviewedForceIntentInput,
) -> Result<ReviewedForceIntentOutput, AppError> {
    execute_live(context, input, ReviewedForceIntentAction::Revoke)
}

fn execute_live(
    context: &AppContext,
    input: &ReviewedForceIntentInput,
    action: ReviewedForceIntentAction,
) -> Result<ReviewedForceIntentOutput, AppError> {
    validate_input(input, action, now_ms())?;
    let _lock = OperationLock::acquire(&context.repository_path, action.operation_name())?;
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
    execute(&bound.status, &provider, context, input, action, now_ms())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn validate_input(
    input: &ReviewedForceIntentInput,
    action: ReviewedForceIntentAction,
    now: u64,
) -> Result<(), AppError> {
    if input.pr == 0 {
        return Err(AppError::validation(
            "force_intent_pr_invalid",
            "--pr must be non-zero",
        ));
    }
    if input.head.len() != 40 || !input.head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::validation(
            "force_intent_head_invalid",
            "--head must be one exact 40-character hexadecimal provider OID",
        ));
    }
    let generation = input.membership_generation.trim();
    if generation.is_empty() || generation.len() > MAX_GENERATION_BYTES {
        return Err(AppError::validation(
            "force_intent_membership_generation_invalid",
            format!("--membership-generation must contain 1..={MAX_GENERATION_BYTES} bytes"),
        ));
    }
    let fingerprint = input
        .failure_fingerprint
        .strip_prefix("fnv1a64:")
        .filter(|value| value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if fingerprint.is_none() {
        return Err(AppError::validation(
            "force_intent_failure_fingerprint_invalid",
            "--failure-fingerprint must be one exact fnv1a64: plus 16 hexadecimal digits",
        ));
    }
    let reason = input.reason.trim();
    if reason.is_empty() || reason.len() > MAX_REASON_BYTES {
        return Err(AppError::validation(
            "force_intent_reason_invalid",
            format!("--reason must contain 1..={MAX_REASON_BYTES} bytes"),
        ));
    }
    if action == ReviewedForceIntentAction::Apply {
        if input.expires_at_ms <= now {
            return Err(AppError::validation(
                "force_intent_expired",
                "reviewed force authority has expired; obtain fresh exact evidence",
            ));
        }
        if input.expires_at_ms.saturating_sub(now) > MAX_AUTHORITY_LIFETIME_MS {
            return Err(AppError::validation(
                "force_intent_expiry_too_distant",
                "--expires-at-ms may be at most 24 hours in the future",
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute(
    status: &StatusOutput,
    provider: &impl ReviewedForceProvider,
    context: &AppContext,
    input: &ReviewedForceIntentInput,
    action: ReviewedForceIntentAction,
    now: u64,
) -> Result<ReviewedForceIntentOutput, AppError> {
    validate_input(input, action, now)?;
    crate::initialization::require_ready(&status.initialization)?;
    if action == ReviewedForceIntentAction::Apply && !context.config.force_merge {
        return Err(force_validation(
            "force_intent_policy_disabled",
            "reviewed force intent requires force_merge=true",
            status,
            input,
            None,
        ));
    }

    let selected_number = PrNumber(input.pr);
    let selected = status
        .analysis
        .pull_requests
        .get(&selected_number)
        .ok_or_else(|| {
            force_validation(
                "force_intent_pr_not_found",
                "selected PR is absent from fresh provider discovery",
                status,
                input,
                None,
            )
        })?;
    let member_numbers = status
        .analysis
        .fleet
        .containing(selected_number)
        .map_or_else(|| vec![selected_number], |caravan| caravan.members.clone());
    let mut exact_members = BTreeMap::new();
    for number in member_numbers {
        let discovered = status.analysis.pull_requests.get(&number).ok_or_else(|| {
            force_validation(
                "force_intent_membership_incomplete",
                "fresh membership is missing exact member provider facts",
                status,
                input,
                Some(json!({"missing_pr": number})),
            )
        })?;
        let exact = provider
            .verify_precondition(
                &status.repository,
                &PullRequestPrecondition::from(discovered),
            )
            .map_err(|error| provider_error(&error, status, input, &[], &[]))?;
        exact_members.insert(number, exact);
    }
    provider
        .verify_branch_head(
            &status.repository,
            &status.default_branch,
            &status.analysis.fleet.default_branch.oid,
        )
        .map_err(|error| provider_error(&error, status, input, &[], &[]))?;
    let mut exact_status = status.clone();
    exact_status.analysis.pull_requests.extend(exact_members);
    debug_assert!(selected.number == selected_number);
    let status = &exact_status;
    let mut evidence = current_evidence(status, input)?;
    validate_supplied_evidence(status, input, &evidence)?;
    if action == ReviewedForceIntentAction::Apply {
        validate_apply_policy(status, input, &evidence)?;
    }

    let operation_id = OperationId::new();
    let mut steps = Vec::new();
    let mut receipts = Vec::new();
    if action == ReviewedForceIntentAction::Preview {
        return Ok(output(
            action,
            input,
            evidence,
            operation_id,
            steps,
            receipts,
            false,
            false,
        ));
    }

    if action == ReviewedForceIntentAction::Apply {
        let permission = provider
            .viewer_permission(&status.repository)
            .map_err(|error| provider_error(&error, status, input, &steps, &receipts))?;
        if permission != "ADMIN" {
            return Err(force_validation(
                "force_intent_permission_denied",
                "reviewed force intent requires authenticated ADMIN permission",
                status,
                input,
                Some(json!({"required": "ADMIN", "actual": permission})),
            ));
        }
    }

    let desired_present = action == ReviewedForceIntentAction::Apply;
    // The queue-owned squash postcondition only exists while the *provider* is
    // the merge actor. Under caravan-owned merging the forced head is landed by
    // cara's own audited administrator squash, so arming here would install a
    // second merge actor on an intentionally non-green head.
    let desired_squash_auto_merge =
        action == ReviewedForceIntentAction::Apply && status.head_merge.actor.github();
    let atomic = provider
        .update_force_state(
            &status.repository,
            &PullRequestPrecondition::from(&evidence.pull),
            desired_present,
            desired_squash_auto_merge,
        )
        .map_err(|error| provider_error(&error, status, input, &steps, &receipts))?;
    let atomic_changed = atomic.transaction_performed;
    steps.push(MutationStep {
        kind: MutationKind::ForceIntentTransaction,
        state: if atomic_changed {
            MutationStepState::Completed
        } else {
            MutationStepState::AlreadySatisfied
        },
        pr: Some(PrNumber(input.pr)),
        summary: match action {
            ReviewedForceIntentAction::Apply if desired_squash_auto_merge => {
                "atomically converged exact-head force intent plus squash auto-merge"
            }
            ReviewedForceIntentAction::Apply => {
                "converged exact-head force intent; cara remains the single merge actor"
            }
            ReviewedForceIntentAction::Revoke => {
                "revoked exact-generation force intent without changing queue-owned auto-merge"
            }
            ReviewedForceIntentAction::Preview => unreachable!(),
        }
        .to_owned(),
    });
    let mut current = atomic.receipt.after.clone();
    receipts.push(atomic.receipt);

    let marker = audit_marker(action, input);
    let body = audit_body(action, input, &evidence, &marker);
    let comment = provider
        .ensure_marked_comment(
            &status.repository,
            &PullRequestPrecondition::from(&current),
            &marker,
            &body,
        )
        .map_err(|error| audit_error(&error, status, input, &steps, &receipts))?;
    let existing = comment
        .provider_output
        .as_deref()
        .is_some_and(|value| value.starts_with("existing GitHub comment"));
    steps.push(MutationStep {
        kind: MutationKind::Comment,
        state: if existing {
            MutationStepState::AlreadySatisfied
        } else {
            MutationStepState::Completed
        },
        pr: Some(PrNumber(input.pr)),
        summary: "persisted reviewed exact-generation force-intent audit".to_owned(),
    });
    current = comment.after.clone();
    receipts.push(comment);

    if current.head.oid.0 != input.head
        || current.has_label(FORCE_LABEL) != desired_present
        || (desired_squash_auto_merge
            && !(current.auto_merge.enabled
                && current.auto_merge.merge_method == Some(MergeMethod::Squash)))
    {
        return Err(AppError::structured(
            ErrorCategory::ExecutionFailure,
            "force_intent_postcondition_failed",
            "provider state did not retain the reviewed exact-head force postcondition",
            Some(json!({
                "expected_head": input.head,
                "actual": current,
                "completed_steps": steps,
                "provider_receipts": receipts,
                "mutated": true,
                "resumable": true,
                "next": "re-read exact provider state and rerun the same force-intent action",
            })),
        ));
    }
    evidence.pull = current;
    let mutated = atomic_changed || !existing;
    Ok(output(
        action,
        input,
        evidence,
        operation_id,
        steps,
        receipts,
        mutated,
        true,
    ))
}

#[allow(clippy::too_many_lines)]
fn current_evidence(
    status: &StatusOutput,
    input: &ReviewedForceIntentInput,
) -> Result<CurrentEvidence, AppError> {
    let pr = PrNumber(input.pr);
    let pull = status
        .analysis
        .pull_requests
        .get(&pr)
        .cloned()
        .ok_or_else(|| {
            force_validation(
                "force_intent_pr_not_found",
                "selected PR is absent from fresh provider discovery",
                status,
                input,
                None,
            )
        })?;
    if pull.state != PullRequestState::Open
        || pull.draft
        || pull.cross_repository
        || pull.head.repository != status.repository
        || !pull.has_label("caravan")
        || pull.has_label("caravan-evicted")
    {
        return Err(force_validation(
            "force_intent_pr_ineligible",
            "reviewed force intent requires an open, non-draft, owned active Caravan PR",
            status,
            input,
            Some(json!({"current_pr": pull})),
        ));
    }
    let caravan = status.analysis.fleet.containing(pr).ok_or_else(|| {
        force_validation(
            "force_intent_membership_missing",
            "selected PR is not in the current Caravan fleet",
            status,
            input,
            None,
        )
    })?;
    if caravan.head() != Some(pr) {
        return Err(force_validation(
            "force_intent_pr_not_head",
            "reviewed force intent is scoped only to the current Caravan head",
            status,
            input,
            Some(json!({"caravan": caravan})),
        ));
    }
    let members = caravan
        .members
        .iter()
        .map(|number| {
            let member = status.analysis.pull_requests.get(number).ok_or_else(|| {
                force_validation(
                    "force_intent_membership_incomplete",
                    "fresh membership is missing exact member provider facts",
                    status,
                    input,
                    Some(json!({"missing_pr": number, "caravan": caravan})),
                )
            })?;
            Ok(ForceIntentMemberEvidence {
                pr: *number,
                head: member.head.clone(),
                base: member.base.clone(),
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    let membership_material = json!({
        "schema_version": 1,
        "repository": status.repository,
        "caravan_id": caravan.id,
        "default_branch": status.analysis.fleet.default_branch,
        "members": members,
    });
    let generation = crate::membership::fnv1a64(
        &serde_json::to_vec(&membership_material).expect("membership evidence serializes"),
    );
    let membership = ForceIntentMembershipEvidence {
        generation: generation.clone(),
        caravan_id: caravan.id,
        default_branch: status.analysis.fleet.default_branch.clone(),
        members,
    };
    let mut checks = pull.checks.clone();
    checks.sort_by_key(canonical_check_key);
    let decision_state = decision_state(&checks);
    let fingerprint_material = json!({
        "schema_version": 1,
        "pr": pr,
        "head": pull.head.oid,
        "membership_generation": generation,
        "required_checks": checks,
        "current_decision": decision_state,
    });
    let failure_fingerprint = crate::membership::fnv1a64(
        &serde_json::to_vec(&fingerprint_material).expect("force decision evidence serializes"),
    );
    let required_checks = ForceIntentRequiredChecksEvidence {
        checks,
        state: decision_state.to_owned(),
        failure_fingerprint: failure_fingerprint.clone(),
    };
    let decision = ForceIntentDecisionEvidence {
        kind: if decision_state == "failure" {
            "ci_failure"
        } else if decision_state == "waiting" {
            "ci_waiting"
        } else {
            "ci_passing"
        }
        .to_owned(),
        state: decision_state.to_owned(),
        pr,
        head: pull.head.oid.0.clone(),
        failure_fingerprint: failure_fingerprint.clone(),
    };
    Ok(CurrentEvidence {
        pull,
        membership,
        checks: required_checks,
        decision,
        failure_fingerprint,
    })
}

fn canonical_check_key(check: &CheckSnapshot) -> (String, String, String, String) {
    (
        check.name.clone(),
        serde_json::to_string(&check.state).expect("check state serializes"),
        check.provider_state.clone().unwrap_or_default(),
        check.details_url.clone().unwrap_or_default(),
    )
}

fn decision_state(checks: &[CheckSnapshot]) -> &'static str {
    if checks.is_empty()
        || checks.iter().any(|check| {
            matches!(
                check.state,
                CheckState::Expected | CheckState::Queued | CheckState::InProgress
            )
        })
    {
        "waiting"
    } else if checks.iter().any(|check| {
        matches!(
            check.state,
            CheckState::Failure
                | CheckState::Cancelled
                | CheckState::TimedOut
                | CheckState::ActionRequired
                | CheckState::Unknown
        )
    }) {
        "failure"
    } else {
        "passing"
    }
}

fn validate_supplied_evidence(
    status: &StatusOutput,
    input: &ReviewedForceIntentInput,
    evidence: &CurrentEvidence,
) -> Result<(), AppError> {
    let mut drift = Vec::new();
    if evidence.pull.head.oid.0 != input.head {
        drift.push("head");
    }
    if evidence.membership.generation != input.membership_generation {
        drift.push("membership_generation");
    }
    if evidence.failure_fingerprint != input.failure_fingerprint {
        drift.push("failure_fingerprint");
    }
    if drift.is_empty() {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "force_intent_evidence_drift",
        format!("reviewed force evidence drifted: {}", drift.join(", ")),
        Some(json!({
            "pr": input.pr,
            "repository": status.repository,
            "drifted_fields": drift,
            "expected": {
                "head": input.head,
                "membership_generation": input.membership_generation,
                "failure_fingerprint": input.failure_fingerprint,
            },
            "current": {
                "provider_head": evidence.pull.head.oid,
                "membership": evidence.membership,
                "required_checks": evidence.checks,
                "current_decision": evidence.decision,
                "failure_fingerprint": evidence.failure_fingerprint,
            },
            "mutated": false,
            "safe_next_action": "review current evidence and issue a new exact bounded authority; never reuse stale force intent",
        })),
    ))
}

fn validate_apply_policy(
    status: &StatusOutput,
    input: &ReviewedForceIntentInput,
    evidence: &CurrentEvidence,
) -> Result<(), AppError> {
    let pr = PrNumber(input.pr);
    let unrelated_problems = status.analysis.fleet.problems.iter().filter(|problem| {
        problem.kind.blocks_fleet()
            && (problem.kind != GraphProblemKind::AutoMergeInvariant || !problem.prs.contains(&pr))
    });
    let problems = unrelated_problems.cloned().collect::<Vec<_>>();
    if !problems.is_empty() {
        return Err(force_validation(
            "force_intent_graph_invalid",
            "reviewed force intent cannot bypass unrelated Caravan graph problems",
            status,
            input,
            Some(json!({"blocking_problems": problems})),
        ));
    }
    let caravan = status
        .analysis
        .fleet
        .containing(pr)
        .expect("current evidence proved membership");
    if status
        .pauses
        .iter()
        .any(|pause| pause.record.caravan_head == caravan.id && pause.state.is_effective())
    {
        return Err(force_validation(
            "force_intent_caravan_held",
            "reviewed force intent cannot bypass an active or expired explicit hold",
            status,
            input,
            None,
        ));
    }
    let clean = status.analysis.compatibility.iter().any(|report| {
        report.candidate == evidence.pull.head
            && report.target == status.analysis.fleet.default_branch
            && report.outcome == CompatibilityOutcome::Clean
    });
    if !clean {
        return Err(force_validation(
            "force_intent_compatibility_not_clean",
            "reviewed force intent requires exact clean head/default compatibility",
            status,
            input,
            None,
        ));
    }
    if evidence.decision.state != "failure" {
        return Err(force_validation(
            "force_intent_current_decision_not_failed",
            "reviewed force authority applies only to a current exact CI failure decision",
            status,
            input,
            Some(json!({
                "required_checks": evidence.checks,
                "current_decision": evidence.decision,
            })),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn output(
    action: ReviewedForceIntentAction,
    input: &ReviewedForceIntentInput,
    evidence: CurrentEvidence,
    operation_id: OperationId,
    steps: Vec<MutationStep>,
    provider_receipts: Vec<GitHubMutationReceipt>,
    mutated: bool,
    atomic_provider_transaction: bool,
) -> ReviewedForceIntentOutput {
    let receipt = OperationReceipt {
        operation_id,
        operation: action.operation_name().to_owned(),
        changed: mutated,
        completed_steps: steps,
    };
    ReviewedForceIntentOutput {
        action: action.name().to_owned(),
        pr: input.pr,
        head: input.head.clone(),
        provider_head: evidence.pull.head.oid.0.clone(),
        membership_generation: evidence.membership.generation.clone(),
        membership: evidence.membership,
        failure_fingerprint: evidence.failure_fingerprint,
        required_checks: evidence.checks,
        current_decision: evidence.decision,
        expires_at_ms: input.expires_at_ms,
        force_intent_applied: evidence.pull.has_label(FORCE_LABEL),
        squash_auto_merge_enabled: evidence.pull.auto_merge.enabled
            && evidence.pull.auto_merge.merge_method == Some(MergeMethod::Squash),
        atomic_provider_transaction,
        mutated,
        reason: input.reason.trim().to_owned(),
        receipt,
        provider_receipts,
        next: match action {
            ReviewedForceIntentAction::Preview => {
                "review this exact evidence, then apply the identical bounded authority before expiry"
            }
            ReviewedForceIntentAction::Apply => {
                "run normal Cara sync; it remains the sole consumer of this one-shot force intent"
            }
            ReviewedForceIntentAction::Revoke => {
                "force intent is absent; normal queue ownership and CI policy remain authoritative"
            }
        }
        .to_owned(),
    }
}

fn audit_marker(action: ReviewedForceIntentAction, input: &ReviewedForceIntentInput) -> String {
    let material = serde_json::to_vec(&json!({
        "schema_version": 1,
        "action": action,
        "pr": input.pr,
        "head": input.head,
        "membership_generation": input.membership_generation,
        "failure_fingerprint": input.failure_fingerprint,
        "reason": input.reason.trim(),
        "expires_at_ms": input.expires_at_ms,
        "auto_merge": input.auto_merge,
    }))
    .expect("audit marker material serializes");
    format!(
        "<!-- caravan-force-intent:v1:{}:{}:{} -->",
        action.name(),
        input.pr,
        crate::membership::fnv1a64(&material)
    )
}

fn audit_body(
    action: ReviewedForceIntentAction,
    input: &ReviewedForceIntentInput,
    evidence: &CurrentEvidence,
    marker: &str,
) -> String {
    format!(
        "{marker}\n### Reviewed Caravan force intent: `{}`\n\n- **PR/head:** #{} `{}`\n- **Membership generation:** `{}`\n- **Failure fingerprint:** `{}`\n- **Expiry (Unix ms):** `{}`\n- **Auto-merge:** `squash`\n- **Reason:** {}\n- **Current decision:** `{}`\n\nThis authority is exact-generation, one-shot, and does not bypass compatibility, holds, ownership, permission, or lease checks.\n",
        action.name(),
        input.pr,
        input.head,
        evidence.membership.generation,
        evidence.failure_fingerprint,
        input.expires_at_ms,
        input.reason.trim(),
        evidence.decision.kind,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn force_validation(
    code: &'static str,
    message: &'static str,
    status: &StatusOutput,
    input: &ReviewedForceIntentInput,
    extra: Option<Value>,
) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        code,
        message,
        Some(json!({
            "pr": input.pr,
            "repository": status.repository,
            "provider_head": status.analysis.pull_requests.get(&PrNumber(input.pr)).map(|pull| &pull.head.oid),
            "extra": extra,
            "mutated": false,
        })),
    )
}

fn provider_error(
    error: &MutationError,
    status: &StatusOutput,
    input: &ReviewedForceIntentInput,
    steps: &[MutationStep],
    receipts: &[GitHubMutationReceipt],
) -> AppError {
    let stale = matches!(
        error,
        MutationError::StalePrecondition { .. } | MutationError::BranchHeadMismatch { .. }
    );
    let (partial_mutated, provider_evidence) = match error {
        MutationError::AtomicTransactionIncomplete {
            operation,
            before,
            after,
            desired_label_present,
            desired_squash_auto_merge,
            provider_error,
        } => (
            before.as_ref() != after.as_ref(),
            json!({
                "kind": "atomic_transaction_incomplete",
                "operation": operation,
                "before": before,
                "after": after,
                "desired_label_present": desired_label_present,
                "desired_squash_auto_merge": desired_squash_auto_merge,
                "provider_error": provider_error,
                "transaction_complete": false,
            }),
        ),
        MutationError::StalePrecondition {
            expected,
            actual,
            changed_fields,
        } => (
            false,
            json!({
                "kind": "stale_precondition",
                "expected": expected,
                "actual": actual,
                "changed_fields": changed_fields,
            }),
        ),
        MutationError::BranchHeadMismatch {
            branch,
            expected,
            actual,
        } => (
            false,
            json!({
                "kind": "branch_head_mismatch",
                "branch": branch,
                "expected": expected,
                "actual": actual,
            }),
        ),
        _ => (
            false,
            json!({"kind": "provider_error", "message": error.to_string()}),
        ),
    };
    AppError::structured(
        if stale {
            ErrorCategory::Validation
        } else {
            ErrorCategory::ExecutionFailure
        },
        if stale {
            "force_intent_stale_precondition"
        } else {
            "force_intent_provider_transaction_failed"
        },
        error.to_string(),
        Some(json!({
            "pr": input.pr,
            "repository": status.repository,
            "completed_steps": steps,
            "provider_receipts": receipts,
            "provider_error": provider_evidence,
            "mutated": partial_mutated || !receipts.is_empty(),
            "resumable": true,
            "next": "re-read exact provider head, membership, checks, and decision before retrying",
        })),
    )
}

fn audit_error(
    error: &MutationError,
    status: &StatusOutput,
    input: &ReviewedForceIntentInput,
    steps: &[MutationStep],
    receipts: &[GitHubMutationReceipt],
) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "force_intent_audit_failed",
        format!("force provider state converged but durable reviewed audit failed: {error}"),
        Some(json!({
            "pr": input.pr,
            "repository": status.repository,
            "completed_steps": steps,
            "provider_receipts": receipts,
            "mutated": true,
            "resumable": true,
            "next": "rerun the identical force-intent action; the exact audit marker deduplicates completion",
        })),
    )
}

/// Build deterministic evidence for a sync CI decision so its public decision
/// fingerprint can be reused by the reviewed force-intent contract.
pub(crate) fn sync_decision_evidence(status: &StatusOutput, pr: PrNumber) -> Option<Value> {
    let pull = status.analysis.pull_requests.get(&pr)?;
    let provisional = ReviewedForceIntentInput {
        pr: pr.0,
        head: pull.head.oid.0.clone(),
        membership_generation: "provisional".to_owned(),
        failure_fingerprint: "fnv1a64:0000000000000000".to_owned(),
        reason: "sync decision evidence".to_owned(),
        expires_at_ms: u64::MAX,
        auto_merge: ReviewedAutoMerge::Squash,
    };
    let evidence = current_evidence(status, &provisional).ok()?;
    Some(json!({
        "membership": evidence.membership,
        "required_checks": evidence.checks,
        "current_decision": evidence.decision,
        "failure_fingerprint": evidence.failure_fingerprint,
    }))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;

    use super::*;
    use crate::graph;
    use crate::model::AutoMergeState;
    use crate::model::{CommitOid, GraphProblem};
    use mcp_cli::StructuredError;

    struct FakeProvider {
        pull: RefCell<PullRequestSnapshot>,
        branch_oid: RefCell<crate::model::CommitOid>,
        permission: &'static str,
        transactions: Cell<usize>,
        comments: RefCell<BTreeSet<String>>,
        fail_comment: bool,
    }

    impl FakeProvider {
        fn new(pull: PullRequestSnapshot) -> Self {
            Self {
                pull: RefCell::new(pull),
                branch_oid: RefCell::new(branch("main").oid),
                permission: "ADMIN",
                transactions: Cell::new(0),
                comments: RefCell::new(BTreeSet::new()),
                fail_comment: false,
            }
        }
    }

    impl ReviewedForceProvider for FakeProvider {
        fn verify_precondition(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<PullRequestSnapshot, MutationError> {
            let actual_pull = self.pull.borrow().clone();
            let actual = PullRequestPrecondition::from(&actual_pull);
            if actual.mutation_identity_eq(expected) {
                Ok(actual_pull)
            } else {
                Err(MutationError::StalePrecondition {
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual),
                    changed_fields: vec!["fake_race".to_owned()],
                })
            }
        }

        fn verify_branch_head(
            &self,
            _repository: &RepositoryId,
            branch: &str,
            expected: &CommitOid,
        ) -> Result<(), MutationError> {
            let actual = self.branch_oid.borrow().clone();
            if &actual == expected {
                Ok(())
            } else {
                Err(MutationError::BranchHeadMismatch {
                    branch: branch.to_owned(),
                    expected: expected.clone(),
                    actual,
                })
            }
        }

        fn viewer_permission(&self, _repository: &RepositoryId) -> Result<String, MutationError> {
            Ok(self.permission.to_owned())
        }

        fn update_force_state(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            force_intent_present: bool,
            ensure_squash_auto_merge: bool,
        ) -> Result<AtomicForceProviderReceipt, MutationError> {
            let before = self.pull.borrow().clone();
            let actual = PullRequestPrecondition::from(&before);
            if actual != *expected {
                return Err(MutationError::StalePrecondition {
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual),
                    changed_fields: vec!["fake_race".to_owned()],
                });
            }
            let already = before.has_label(FORCE_LABEL) == force_intent_present
                && (!ensure_squash_auto_merge
                    || (before.auto_merge.enabled
                        && before.auto_merge.merge_method == Some(MergeMethod::Squash)));
            let mut after = before.clone();
            if force_intent_present {
                after.labels.insert(FORCE_LABEL.to_owned());
            } else {
                after.labels.remove(FORCE_LABEL);
            }
            if ensure_squash_auto_merge {
                after.auto_merge = AutoMergeState::squash();
            }
            if !already {
                self.transactions.set(self.transactions.get() + 1);
            }
            *self.pull.borrow_mut() = after.clone();
            Ok(AtomicForceProviderReceipt {
                receipt: GitHubMutationReceipt {
                    kind: MutationKind::ForceIntentTransaction,
                    before: Some(before),
                    after,
                    provider_output: None,
                },
                transaction_performed: !already,
            })
        }

        fn ensure_marked_comment(
            &self,
            _repository: &RepositoryId,
            _expected: &PullRequestPrecondition,
            marker: &str,
            _body: &str,
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
            let existing = !self.comments.borrow_mut().insert(marker.to_owned());
            Ok(GitHubMutationReceipt {
                kind: MutationKind::Comment,
                before: Some(self.pull.borrow().clone()),
                after: self.pull.borrow().clone(),
                provider_output: existing.then(|| format!("existing GitHub comment {marker}")),
            })
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
            oid: CommitOid(format!("{:0<40}", name.replace('m', "a").replace('h', "b"))),
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
            auto_merge: AutoMergeState::disabled(),
            checks: vec![CheckSnapshot {
                name: "CI".to_owned(),
                state: CheckState::Failure,
                provider_state: Some("FAILURE".to_owned()),
                details_url: Some("https://example.invalid/run/1".to_owned()),
            }],
            created_at: None,
            merged_at: None,
            updated_at: None,
        }
    }

    fn make_status(pull: PullRequestSnapshot) -> StatusOutput {
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
        // The historical reviewed-force fixtures exercise provider-native
        // delegation, where force intent also owns the squash postcondition.
        let analysis =
            graph::analyze_for_actor(&snapshot, &checker, crate::model::HeadMergeActor::Github)
                .unwrap();
        StatusOutput {
            config_provenance: None,
            head_merge: crate::read::HeadMergeStatus {
                actor: crate::model::HeadMergeActor::Github,
                ..crate::read::HeadMergeStatus::default()
            },
            runtime: crate::read::RuntimeProvenance::default(),
            provider_api: crate::model::GitHubApiTelemetry::default(),
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            timing: None,
            repository: repository(),
            rebase_on_join: crate::read::RebaseOnJoinStatus::default(),
            stack_backend: crate::read::StackBackendStatus::default(),
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

    fn context() -> AppContext {
        let mut context = AppContext::default();
        context.config.force_merge = true;
        context
    }

    fn exact_input(status: &StatusOutput, now: u64) -> ReviewedForceIntentInput {
        let provisional = ReviewedForceIntentInput {
            pr: 1,
            head: status.analysis.pull_requests[&PrNumber(1)]
                .head
                .oid
                .0
                .clone(),
            membership_generation: "provisional".to_owned(),
            failure_fingerprint: "fnv1a64:0000000000000000".to_owned(),
            reason: "known provider infrastructure failure".to_owned(),
            expires_at_ms: now + 60_000,
            auto_merge: ReviewedAutoMerge::Squash,
        };
        let evidence = current_evidence(status, &provisional).unwrap();
        ReviewedForceIntentInput {
            membership_generation: evidence.membership.generation,
            failure_fingerprint: evidence.failure_fingerprint,
            ..provisional
        }
    }

    #[test]
    fn preview_is_zero_write_and_returns_shared_exact_evidence() {
        let now = 1_000_000;
        let status = make_status(pull());
        let input = exact_input(&status, now);
        let provider = FakeProvider::new(pull());
        let output = execute(
            &status,
            &provider,
            &context(),
            &input,
            ReviewedForceIntentAction::Preview,
            now,
        )
        .unwrap();
        assert_eq!(output.action, "preview");
        assert_eq!(output.provider_head, input.head);
        assert_eq!(output.membership.generation, input.membership_generation);
        assert_eq!(
            output.required_checks.failure_fingerprint,
            input.failure_fingerprint
        );
        assert_eq!(
            output.current_decision.failure_fingerprint,
            input.failure_fingerprint
        );
        assert!(!output.mutated);
        let sync_evidence = sync_decision_evidence(&status, PrNumber(1)).unwrap();
        assert_eq!(
            sync_evidence["membership"]["generation"],
            output.membership_generation
        );
        assert_eq!(
            sync_evidence["failure_fingerprint"],
            output.failure_fingerprint
        );
        assert_eq!(provider.transactions.get(), 0);
        assert!(provider.comments.borrow().is_empty());
    }

    #[test]
    fn apply_converges_force_and_squash_in_one_provider_transaction_and_replays() {
        let now = 1_000_000;
        let initial = pull();
        let status = make_status(initial.clone());
        let input = exact_input(&status, now);
        let provider = FakeProvider::new(initial);
        let first = execute(
            &status,
            &provider,
            &context(),
            &input,
            ReviewedForceIntentAction::Apply,
            now,
        )
        .unwrap();
        assert!(first.force_intent_applied);
        assert!(first.squash_auto_merge_enabled);
        assert!(first.atomic_provider_transaction);
        let caco_evidence = serde_json::to_value(&first).unwrap();
        assert_eq!(caco_evidence["action"], "apply");
        assert_eq!(caco_evidence["pr"], input.pr);
        assert_eq!(caco_evidence["head"], input.head);
        assert_eq!(caco_evidence["provider_head"], input.head);
        assert_eq!(
            caco_evidence["membership_generation"],
            input.membership_generation
        );
        assert_eq!(
            caco_evidence["membership"]["generation"],
            input.membership_generation
        );
        assert_eq!(
            caco_evidence["failure_fingerprint"],
            input.failure_fingerprint
        );
        assert_eq!(
            caco_evidence["required_checks"]["failure_fingerprint"],
            input.failure_fingerprint
        );
        assert_eq!(
            caco_evidence["current_decision"]["failure_fingerprint"],
            input.failure_fingerprint
        );
        assert_eq!(caco_evidence["expires_at_ms"], input.expires_at_ms);
        assert_eq!(caco_evidence["force_intent_applied"], true);
        assert_eq!(caco_evidence["squash_auto_merge_enabled"], true);
        assert_eq!(caco_evidence["atomic_provider_transaction"], true);
        assert_eq!(provider.transactions.get(), 1);
        assert_eq!(provider.comments.borrow().len(), 1);

        let refreshed = make_status(provider.pull.borrow().clone());
        let replay = execute(
            &refreshed,
            &provider,
            &context(),
            &input,
            ReviewedForceIntentAction::Apply,
            now,
        )
        .unwrap();
        assert!(!replay.mutated);
        assert!(replay.atomic_provider_transaction);
        assert_eq!(provider.transactions.get(), 1);
        assert_eq!(provider.comments.borrow().len(), 1);
    }

    #[test]
    fn stale_head_membership_and_failure_fingerprint_fail_before_provider_writes() {
        let now = 1_000_000;
        let initial = pull();
        let status = make_status(initial.clone());
        let provider = FakeProvider::new(initial);
        for field in ["head", "membership", "fingerprint"] {
            let mut input = exact_input(&status, now);
            match field {
                "head" => input.head = "c".repeat(40),
                "membership" => input.membership_generation = "fnv1a64:1111111111111111".into(),
                "fingerprint" => input.failure_fingerprint = "fnv1a64:2222222222222222".into(),
                _ => unreachable!(),
            }
            let error = execute(
                &status,
                &provider,
                &context(),
                &input,
                ReviewedForceIntentAction::Apply,
                now,
            )
            .unwrap_err();
            assert_eq!(error.code(), "force_intent_evidence_drift", "{field}");
        }
        assert_eq!(provider.transactions.get(), 0);
        assert!(provider.comments.borrow().is_empty());
    }

    #[test]
    fn exact_provider_check_reread_rejects_decision_drift_before_mutation() {
        let now = 1_000_000;
        let discovered = pull();
        let status = make_status(discovered.clone());
        let input = exact_input(&status, now);
        let mut changed = discovered;
        changed.checks[0].details_url = Some("https://example.invalid/run/2".to_owned());
        let provider = FakeProvider::new(changed);

        let error = execute(
            &status,
            &provider,
            &context(),
            &input,
            ReviewedForceIntentAction::Apply,
            now,
        )
        .unwrap_err();

        assert_eq!(error.code(), "force_intent_evidence_drift");
        assert_eq!(provider.transactions.get(), 0);
        assert!(provider.comments.borrow().is_empty());
    }

    #[test]
    fn revoke_is_expiry_safe_idempotent_and_preserves_queue_auto_merge() {
        let now = 1_000_000;
        let mut armed = pull();
        armed.labels.insert(FORCE_LABEL.to_owned());
        armed.auto_merge = AutoMergeState::squash();
        let status = make_status(armed.clone());
        let mut input = exact_input(&status, now);
        input.expires_at_ms = now - 1;
        let provider = FakeProvider::new(armed);
        let expired_preview = execute(
            &status,
            &provider,
            &context(),
            &input,
            ReviewedForceIntentAction::Preview,
            now,
        )
        .expect("Caco's mandatory fresh preview remains available for expired revoke");
        assert!(!expired_preview.mutated);
        let first = execute(
            &status,
            &provider,
            &context(),
            &input,
            ReviewedForceIntentAction::Revoke,
            now,
        )
        .unwrap();
        assert!(!first.force_intent_applied);
        assert!(first.squash_auto_merge_enabled);
        assert_eq!(provider.transactions.get(), 1);

        let refreshed = make_status(provider.pull.borrow().clone());
        let replay = execute(
            &refreshed,
            &provider,
            &context(),
            &input,
            ReviewedForceIntentAction::Revoke,
            now,
        )
        .unwrap();
        assert!(!replay.mutated);
        assert_eq!(provider.transactions.get(), 1);
    }

    #[test]
    fn apply_rejects_nonfailure_and_distant_or_expired_authority() {
        let now = 1_000_000;
        let mut passing = pull();
        passing.checks[0].state = CheckState::Success;
        passing.checks[0].provider_state = Some("SUCCESS".into());
        let status = make_status(passing.clone());
        let provider = FakeProvider::new(passing);
        let input = exact_input(&status, now);
        let error = execute(
            &status,
            &provider,
            &context(),
            &input,
            ReviewedForceIntentAction::Apply,
            now,
        )
        .unwrap_err();
        assert_eq!(error.code(), "force_intent_current_decision_not_failed");

        let failed_status = make_status(pull());
        let mut expired = exact_input(&failed_status, now);
        expired.expires_at_ms = now;
        assert_eq!(
            validate_input(&expired, ReviewedForceIntentAction::Apply, now)
                .unwrap_err()
                .code(),
            "force_intent_expired"
        );
        expired.expires_at_ms = now + MAX_AUTHORITY_LIFETIME_MS + 1;
        assert_eq!(
            validate_input(&expired, ReviewedForceIntentAction::Apply, now)
                .unwrap_err()
                .code(),
            "force_intent_expiry_too_distant"
        );
    }

    #[test]
    fn audit_failure_preserves_atomic_provider_receipt_for_exact_retry() {
        let now = 1_000_000;
        let initial = pull();
        let status = make_status(initial.clone());
        let input = exact_input(&status, now);
        let mut provider = FakeProvider::new(initial);
        provider.fail_comment = true;
        let error = execute(
            &status,
            &provider,
            &context(),
            &input,
            ReviewedForceIntentAction::Apply,
            now,
        )
        .unwrap_err();
        assert_eq!(error.code(), "force_intent_audit_failed");
        assert!(provider.pull.borrow().has_label(FORCE_LABEL));
        assert!(provider.pull.borrow().auto_merge.enabled);
        let details = error.details().unwrap();
        assert_eq!(details["provider_receipts"].as_array().unwrap().len(), 1);
        assert_eq!(details["mutated"], true);
    }

    #[test]
    fn partial_atomic_provider_error_exposes_before_after_and_mutated_state() {
        let status = make_status(pull());
        let input = exact_input(&status, 1_000_000);
        let before = pull();
        let mut after = before.clone();
        after.labels.insert(FORCE_LABEL.to_owned());
        let error = provider_error(
            &MutationError::AtomicTransactionIncomplete {
                operation: "label_and_squash_auto_merge".to_owned(),
                before: Box::new(before),
                after: Box::new(after),
                desired_label_present: true,
                desired_squash_auto_merge: true,
                provider_error: Some("auto-merge field failed".to_owned()),
            },
            &status,
            &input,
            &[],
            &[],
        );
        assert_eq!(error.code(), "force_intent_provider_transaction_failed");
        let details = error.details().unwrap();
        assert_eq!(details["mutated"], true);
        assert_eq!(
            details["provider_error"]["kind"],
            "atomic_transaction_incomplete"
        );
        assert_eq!(
            details["provider_error"]["after"]["labels"],
            json!(["caravan", "caravan-force"])
        );
    }

    #[test]
    fn unrelated_graph_problem_remains_a_hard_stop_but_selected_auto_merge_gap_is_repairable() {
        let now = 1_000_000;
        let initial = pull();
        let mut status = make_status(initial.clone());
        assert!(
            status
                .analysis
                .fleet
                .problems
                .iter()
                .any(|problem| problem.kind == GraphProblemKind::AutoMergeInvariant)
        );
        let input = exact_input(&status, now);
        let provider = FakeProvider::new(initial.clone());
        execute(
            &status,
            &provider,
            &context(),
            &input,
            ReviewedForceIntentAction::Apply,
            now,
        )
        .expect("selected auto-merge invariant is repaired atomically");

        status.analysis.fleet.problems.push(GraphProblem {
            kind: GraphProblemKind::Cycle,
            prs: vec![PrNumber(1)],
            message: "cycle".to_owned(),
        });
        let provider = FakeProvider::new(initial);
        let error = execute(
            &status,
            &provider,
            &context(),
            &input,
            ReviewedForceIntentAction::Apply,
            now,
        )
        .unwrap_err();
        assert_eq!(error.code(), "force_intent_graph_invalid");
    }
}
