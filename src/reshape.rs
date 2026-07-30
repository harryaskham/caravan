//! Safe, resumable caravan eviction and splitting policy.

use std::collections::BTreeMap;

use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::github::{
    ControlLabelAudit, GitHubMutationAdapter, GitHubMutationReceipt, MutationError,
    control_label_marker,
};
use crate::graph::{CompatibilityChecker, GitCompatibilityChecker, analyze};
use crate::hooks::{self, HookDelivery};
use crate::membership::MembershipProvider;
use crate::model::{
    AutoMergeState, BranchSnapshot, CaravanEvent, CaravanFleet, EventKind, MergeMethod,
    MutationKind, MutationStep, MutationStepState, OperationId, OperationReceipt, PrNumber,
    PullRequestPrecondition, PullRequestSnapshot, PullRequestState, RepositorySnapshot,
};
use crate::operation_lock::OperationLock;
use crate::read::{self, StatusOutput};
use crate::{AppContext, AppError, EvictInput, SplitInput};

const ACTIVE_LABEL: &str = "caravan";
const EVICTED_LABEL: &str = "caravan-evicted";
const FORCE_LABEL: &str = "caravan-force";

/// Reshape operation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReshapeOperation {
    Evict,
    Split,
}

impl ReshapeOperation {
    const fn name(self) -> &'static str {
        match self {
            Self::Evict => "evict",
            Self::Split => "split",
        }
    }
}

/// Successful reshape receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReshapeOutput {
    pub operation: ReshapeOperation,
    pub pr: PrNumber,
    /// Members that were physically rebased onto the evicted PR and therefore
    /// still carry its commits after retargeting (bd-cef612).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub descendants_inheriting_evicted_patch: Vec<PrNumber>,
    /// Exact per-descendant rewrites that removed the evicted patch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub descendant_rewrites: Vec<crate::physical_rebase::RebaseReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub receipt: OperationReceipt,
    #[serde(default)]
    pub provider_receipts: Vec<GitHubMutationReceipt>,
    #[serde(default)]
    pub affected_prs: Vec<PrNumber>,
    /// Fleet expected after every recorded provider step completes.
    pub resulting_fleet: CaravanFleet,
    /// Canonical events emitted after the complete reshape operation.
    #[serde(default)]
    pub events: Vec<CaravanEvent>,
    /// Bounded status for configured hooks which consumed `events`.
    #[serde(default)]
    pub hook_deliveries: Vec<HookDelivery>,
}

/// Evict a PR selected explicitly or from the current branch.
pub fn evict(context: &AppContext, input: &EvictInput) -> Result<ReshapeOutput, AppError> {
    if input.reason.trim().is_empty() {
        return Err(AppError::validation(
            "eviction_reason_required",
            "--reason must contain a non-empty eviction rationale",
        ));
    }
    if input.cascade && input.all {
        return Err(AppError::validation(
            "eviction_scope_ambiguous",
            "--cascade and --all are mutually exclusive: choose the tail suffix or the whole caravan",
        ));
    }
    if !input.cascade && !input.all {
        return execute_live(
            context,
            ReshapeOperation::Evict,
            input.pr.map(PrNumber),
            Some(input.reason.clone()),
        );
    }
    evict_many(context, input)
}

/// Release several members tail-first (bd-e9187e).
///
/// A single eviction has to close the gap it opens by re-linking the evicted
/// member's child onto its predecessor, and that new edge can be incompatible.
/// Removing the current tail never re-links anything, so an ordered sequence of
/// tail removals dissolves a chain that no single eviction could touch. Each
/// step is an ordinary audited eviction with its own receipts, and the sequence
/// stops at the first refusal rather than leaving a half-dissolved chain
/// unreported.
fn evict_many(context: &AppContext, input: &EvictInput) -> Result<ReshapeOutput, AppError> {
    let status = read::status(context)?;
    let selected = input
        .pr
        .map(PrNumber)
        .or(status.current_pr)
        .ok_or_else(|| {
            AppError::validation(
                "eviction_pr_required",
                "no PR was selected and the current branch has no unique open pull request",
            )
        })?;
    let caravan = status
        .analysis
        .fleet
        .containing(selected)
        .ok_or_else(|| {
            AppError::validation(
                "eviction_pr_not_active",
                format!("PR #{selected} is not an active caravan member"),
            )
        })?
        .clone();
    let index = caravan
        .members
        .iter()
        .position(|member| *member == selected)
        .expect("containing caravan holds the selected member");
    let ordered = if input.all {
        caravan.members.clone()
    } else {
        caravan.members[index..].to_vec()
    };

    let mut released = Vec::new();
    let mut last = None;
    // Tail-first: the last member always has no child, so no gap is opened.
    for member in ordered.iter().rev().copied() {
        match execute_live(
            context,
            ReshapeOperation::Evict,
            Some(member),
            Some(input.reason.clone()),
        ) {
            Ok(output) => {
                released.push(member);
                last = Some(output);
            }
            Err(error) => {
                return Err(partial_cascade_error(&error, &ordered, &released, member));
            }
        }
    }
    let mut output = last.ok_or_else(|| {
        AppError::validation(
            "eviction_empty_scope",
            "the selected scope contained no caravan members",
        )
    })?;
    output.affected_prs = released;
    Ok(output)
}

/// Report an interrupted cascade honestly, naming what was already released.
fn partial_cascade_error(
    error: &AppError,
    ordered: &[PrNumber],
    released: &[PrNumber],
    failed: PrNumber,
) -> AppError {
    AppError::structured(
        error.category(),
        "eviction_cascade_interrupted",
        format!("cascading eviction stopped at PR #{failed}: {error}"),
        Some(json!({
            "requested_members": ordered,
            "released_members": released,
            "failed_member": failed,
            "source": error.details(),
            "resumable": true,
            "safe_next_action": "the released members are already evicted; inspect the reported failure and rerun the same command to continue from the remaining tail",
        })),
    )
}

/// Split before a non-head PR selected explicitly or from the current branch.
pub fn split(context: &AppContext, input: &SplitInput) -> Result<ReshapeOutput, AppError> {
    execute_live(
        context,
        ReshapeOperation::Split,
        input.pr.map(PrNumber),
        None,
    )
}

fn execute_live(
    context: &AppContext,
    operation: ReshapeOperation,
    selected: Option<PrNumber>,
    reason: Option<String>,
) -> Result<ReshapeOutput, AppError> {
    let _lock = OperationLock::acquire(&context.repository_path, operation.name())?;
    let timeout = std::time::Duration::from_secs(context.config.command_timeout_secs);
    let status = read::status(context)?;
    let repository = status.repository.clone();
    let failure_status = status.clone();
    let checker =
        GitCompatibilityChecker::new(&context.repository_path, "origin").with_timeout(timeout);
    let provider = GitHubMutationAdapter::new(
        crate::command::ProcessRunner::in_directory(&context.repository_path).with_timeout(timeout),
    );
    let requested_reason = reason.clone();
    let rewrite = RewriteContext {
        repository_path: &context.repository_path,
        timeout,
        enabled: context.config.rebase_on_join,
    };
    let mut output = match execute(
        status,
        &checker,
        &provider,
        operation,
        selected,
        reason,
        Some(&rewrite),
    ) {
        Ok(output) => output,
        Err(error) if operation == ReshapeOperation::Evict => {
            let event = eviction_failed_event(
                &failure_status,
                selected,
                requested_reason.as_deref(),
                &error,
            );
            let error = hooks::attach_events(error, std::slice::from_ref(&event));
            let deliveries = hooks::dispatch_events(context, std::slice::from_ref(&event))?;
            return Err(hooks::attach_deliveries(error, &deliveries));
        }
        Err(error) => return Err(error),
    };
    let kind = match operation {
        ReshapeOperation::Evict => EventKind::Evicted,
        ReshapeOperation::Split => EventKind::Split,
    };
    let event = hooks::event(
        kind,
        output.receipt.operation_id.clone(),
        repository,
        output
            .resulting_fleet
            .containing(output.pr)
            .map(|caravan| caravan.id),
        output.affected_prs.clone(),
        Some(output.resulting_fleet.clone()),
        output.reason.clone(),
        BTreeMap::from([("receipt".to_owned(), json!(output.receipt))]),
    );
    output.events.push(event);
    output.hook_deliveries = hooks::dispatch_events(context, &output.events)?;
    Ok(output)
}

fn eviction_failed_event(
    status: &StatusOutput,
    selected: Option<PrNumber>,
    requested_reason: Option<&str>,
    error: &AppError,
) -> CaravanEvent {
    let number = selected.or(status.current_pr);
    let caravan_id = number
        .and_then(|number| status.analysis.fleet.containing(number))
        .map(|caravan| caravan.id);
    let mut metadata = BTreeMap::from([("error_code".to_owned(), json!(error.code()))]);
    if let Some(reason) = requested_reason {
        metadata.insert("requested_reason".to_owned(), json!(reason));
    }
    hooks::event(
        EventKind::EvictionFailed,
        hooks::operation_id_from_error(error),
        status.repository.clone(),
        caravan_id,
        number.into_iter().collect(),
        Some(status.analysis.fleet.clone()),
        Some(error.to_string()),
        metadata,
    )
}

// Owning one immutable discovery snapshot prevents a multi-step operation from
// accidentally swapping in partially refreshed facts mid-transaction.
#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_lines)]
/// Repository access required to physically unwind descendants (bd-cef612).
struct RewriteContext<'a> {
    repository_path: &'a std::path::Path,
    timeout: std::time::Duration,
    enabled: bool,
}

#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn execute(
    status: StatusOutput,
    checker: &impl CompatibilityChecker,
    provider: &impl MembershipProvider,
    operation: ReshapeOperation,
    selected: Option<PrNumber>,
    reason: Option<String>,
    rewrite: Option<&RewriteContext<'_>>,
) -> Result<ReshapeOutput, AppError> {
    let number = selected.or(status.current_pr).ok_or_else(|| {
        AppError::validation(
            "reshape_pr_not_selected",
            "select a PR with --pr or run from a branch with one unique open PR",
        )
    })?;
    let target = status
        .analysis
        .pull_requests
        .get(&number)
        .cloned()
        .ok_or_else(|| missing_pr(number))?;
    if target.state != PullRequestState::Open {
        return Err(AppError::validation(
            "reshape_pr_not_open",
            format!("PR #{number} is not open"),
        ));
    }

    let (virtual_status, plan) = match operation {
        ReshapeOperation::Evict => plan_eviction(&status, &target)?,
        ReshapeOperation::Split => plan_split(&status, &target)?,
    };
    preflight_result(&status, &virtual_status, checker)?;
    if plan.creates_head {
        preflight_new_head(provider, &status)?;
    }

    let mut state = ReshapeState::new(operation, status.analysis.pull_requests.clone());
    match operation {
        ReshapeOperation::Evict => {
            let before_labels = state.current(number).labels.clone();
            state.ensure_auto_merge_disabled(provider, &status.repository, number)?;
            state.ensure_label_absent(provider, &status.repository, number, FORCE_LABEL)?;
            state.ensure_label_absent(provider, &status.repository, number, ACTIVE_LABEL)?;
            state.ensure_label_present(provider, &status.repository, number, EVICTED_LABEL)?;
            if let Some(child) = plan.child {
                let desired = plan
                    .child_base
                    .as_ref()
                    .expect("an eviction child has a desired base");
                state.ensure_base(provider, &status.repository, child, desired)?;
                if plan.creates_head && status.head_merge.actor.github() {
                    state.ensure_squash_auto_merge(provider, &status.repository, child)?;
                } else {
                    state.ensure_auto_merge_disabled(provider, &status.repository, child)?;
                }
            }
            let after = state.current(number);
            let audit = ControlLabelAudit {
                operation: operation.name().to_owned(),
                marker: control_label_marker(
                    operation.name(),
                    number,
                    &after.head.oid,
                    &before_labels,
                    &after.labels,
                ),
                before_labels,
                after_labels: after.labels.clone(),
                actor: "authenticated GitHub actor invoked through cara CLI/JSON/MCP".to_owned(),
                reason: reason.clone().expect("eviction reason validated by caller"),
                reason_source: "explicit --reason input".to_owned(),
                compatibility_evidence: "complete resulting-fleet compatibility preflight passed"
                    .to_owned(),
                clean_squash_evidence: if plan.creates_head {
                    "replacement head passed compatibility preflight and has squash auto-merge enabled".to_owned()
                } else {
                    "no replacement head required, or replacement remains a non-head with auto-merge disabled".to_owned()
                },
                admission_priority_basis:
                    "not applicable: eviction preserves relative queue priority".to_owned(),
            };
            state.ensure_control_label_comment(provider, &status.repository, number, &audit)?;
        }
        ReshapeOperation::Split => {
            state.ensure_base(
                provider,
                &status.repository,
                number,
                &status.analysis.fleet.default_branch,
            )?;
            if status.head_merge.actor.github() {
                state.ensure_squash_auto_merge(provider, &status.repository, number)?;
            } else {
                state.ensure_auto_merge_disabled(provider, &status.repository, number)?;
            }
        }
    }

    // bd-cef612: retargeting a child does not remove the evicted member's
    // commits from it. Physical joins rebased each member onto its predecessor,
    // so every descendant still carries the evicted patch and would silently
    // reintroduce discarded content when it lands. Record exactly which members
    // are affected instead of letting the receipt imply a clean removal.
    let descendants = if operation == ReshapeOperation::Evict {
        descendants_of(&status, number)
    } else {
        Vec::new()
    };
    // bd-cef612: physically drop the evicted patch from each descendant, so a
    // descendant that later lands cannot reintroduce discarded content.
    let descendant_rewrites = match rewrite {
        Some(rewrite) if rewrite.enabled && !descendants.is_empty() => unwind_descendants(
            rewrite,
            &status,
            number,
            &descendants,
            plan.child_base.as_ref(),
        )?,
        _ => Vec::new(),
    };
    let descendants_inheriting_evicted_patch = if descendant_rewrites.is_empty() {
        descendants
    } else {
        Vec::new()
    };
    Ok(ReshapeOutput {
        operation,
        descendants_inheriting_evicted_patch,
        descendant_rewrites,
        pr: number,
        reason,
        receipt: state.receipt(),
        provider_receipts: state.provider_receipts,
        affected_prs: plan.affected_prs,
        resulting_fleet: virtual_status.analysis.fleet,
        events: Vec::new(),
        hook_deliveries: Vec::new(),
    })
}

#[derive(Debug, Clone)]
struct ReshapePlan {
    child: Option<PrNumber>,
    child_base: Option<BranchSnapshot>,
    creates_head: bool,
    affected_prs: Vec<PrNumber>,
}

/// Rebuild each descendant without the evicted member's commits.
///
/// The sequencer normally replays everything in `target..head`, which would
/// re-apply the evicted patch. Replaying strictly after the evicted head drops
/// exactly that member's commits while preserving each owner's own work, and
/// the chain is rebuilt in order so a later descendant stacks on the rewritten
/// earlier one. Every rewrite is proven before any is published, so a stack is
/// never left half-unwound.
fn unwind_descendants(
    rewrite: &RewriteContext<'_>,
    status: &StatusOutput,
    evicted: PrNumber,
    descendants: &[PrNumber],
    child_base: Option<&BranchSnapshot>,
) -> Result<Vec<crate::physical_rebase::RebaseReceipt>, AppError> {
    let evicted_head = status
        .analysis
        .pull_requests
        .get(&evicted)
        .expect("evicted PR has a snapshot")
        .head
        .clone();
    let default = &status.analysis.fleet.default_branch;
    let mut target = crate::physical_rebase::PlannedBase::Remote(
        child_base.cloned().unwrap_or_else(|| default.clone()),
    );
    let mut boundary = evicted_head;
    let mut prepared = Vec::new();
    for number in descendants {
        let candidate = status
            .analysis
            .pull_requests
            .get(number)
            .expect("descendant has a snapshot");
        let item = crate::physical_rebase::prepare_candidate(
            rewrite.repository_path,
            &status.repository,
            candidate,
            crate::physical_rebase::PlannedRangeBase::RemoteBranch {
                branch: candidate.base.clone(),
            },
            target,
            default,
            crate::physical_rebase::RebaseExecutionBudget::new(rewrite.timeout)
                .replaying_after(boundary.oid.clone()),
        )?;
        target = crate::physical_rebase::PlannedBase::Simulated(BranchSnapshot {
            repository: status.repository.clone(),
            name: candidate.head.name.clone(),
            oid: item.plan.new_head_oid.clone(),
        });
        boundary = candidate.head.clone();
        prepared.push(item);
    }
    for item in &prepared {
        crate::physical_rebase::verify_prepared(item)?;
    }
    prepared
        .iter()
        .map(crate::physical_rebase::apply_prepared)
        .collect()
}

/// Members physically stacked after `number` in its caravan.
fn descendants_of(status: &StatusOutput, number: PrNumber) -> Vec<PrNumber> {
    status
        .analysis
        .fleet
        .containing(number)
        .and_then(|caravan| {
            caravan.position(number).map(|position| {
                caravan
                    .members
                    .iter()
                    .skip(position + 1)
                    .copied()
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default()
}

fn plan_eviction(
    status: &StatusOutput,
    target: &PullRequestSnapshot,
) -> Result<(StatusOutput, ReshapePlan), AppError> {
    let active_caravan = status.analysis.fleet.containing(target.number);
    let already_evicted = target.has_label(EVICTED_LABEL) && !target.has_label(ACTIVE_LABEL);
    if active_caravan.is_none() && !already_evicted {
        return Err(AppError::validation(
            "eviction_pr_not_active",
            format!(
                "PR #{} is neither active nor a resumable evicted PR",
                target.number
            ),
        ));
    }

    let (parent, child) = if let Some(caravan) = active_caravan {
        let position = caravan
            .position(target.number)
            .expect("containing caravan has target");
        (
            position
                .checked_sub(1)
                .and_then(|index| caravan.members.get(index).copied()),
            caravan.members.get(position + 1).copied(),
        )
    } else {
        let parent = status
            .analysis
            .pull_requests
            .values()
            .find(|pull_request| pull_request.head.name == target.base.name)
            .map(|pull_request| pull_request.number);
        let child = status
            .analysis
            .pull_requests
            .values()
            .find(|pull_request| {
                pull_request.is_active_caravan_member()
                    && pull_request.base.name == target.head.name
            })
            .map(|pull_request| pull_request.number);
        (parent, child)
    };

    let child_base = child.map(|_| {
        parent.map_or_else(
            || status.analysis.fleet.default_branch.clone(),
            |parent| {
                status
                    .analysis
                    .pull_requests
                    .get(&parent)
                    .expect("eviction parent has a snapshot")
                    .head
                    .clone()
            },
        )
    });
    let creates_head = child.is_some() && parent.is_none();
    let mut pull_requests = status.analysis.pull_requests.clone();
    let virtual_target = pull_requests
        .get_mut(&target.number)
        .expect("target is present");
    virtual_target.auto_merge = AutoMergeState::disabled();
    virtual_target.labels.remove(ACTIVE_LABEL);
    virtual_target.labels.remove(FORCE_LABEL);
    virtual_target.labels.insert(EVICTED_LABEL.to_owned());
    if let (Some(child), Some(base)) = (child, &child_base) {
        let virtual_child = pull_requests.get_mut(&child).expect("child is present");
        virtual_child.base = base.clone();
        virtual_child.auto_merge = if creates_head && status.head_merge.actor.github() {
            AutoMergeState::squash()
        } else {
            AutoMergeState::disabled()
        };
    }
    let virtual_status = virtual_status(status, pull_requests);
    let mut affected_prs = vec![target.number];
    if let Some(child) = child {
        affected_prs.push(child);
    }
    Ok((
        virtual_status,
        ReshapePlan {
            child,
            child_base,
            creates_head,
            affected_prs,
        },
    ))
}

fn plan_split(
    status: &StatusOutput,
    target: &PullRequestSnapshot,
) -> Result<(StatusOutput, ReshapePlan), AppError> {
    if !target.is_active_caravan_member() {
        return Err(AppError::validation(
            "split_pr_not_active",
            format!("PR #{} is not an active caravan member", target.number),
        ));
    }
    let caravan = status
        .analysis
        .fleet
        .containing(target.number)
        .expect("active target has a caravan");
    let position = caravan
        .position(target.number)
        .expect("containing caravan has target");
    // "Already a head" is whatever the configured merge actor makes it. Under
    // provider-native delegation that is a root carrying squash auto-merge;
    // under caravan-owned merging nobody is armed, so the exact fact is a root
    // already targeting the default branch.
    let already_head = position == 0
        && if status.head_merge.actor.github() {
            target.auto_merge.enabled && target.auto_merge.merge_method == Some(MergeMethod::Squash)
        } else {
            target.base == status.analysis.fleet.default_branch
        };
    if already_head {
        return Err(AppError::validation(
            "split_pr_is_head",
            format!("PR #{} is already a caravan head", target.number),
        ));
    }
    // A root that has not reached that state yet is a resumable partial split:
    // its base step already landed and only the second provider step remains.

    let mut pull_requests = status.analysis.pull_requests.clone();
    let virtual_target = pull_requests
        .get_mut(&target.number)
        .expect("target is present");
    virtual_target.base = status.analysis.fleet.default_branch.clone();
    virtual_target.auto_merge = if status.head_merge.actor.github() {
        AutoMergeState::squash()
    } else {
        AutoMergeState::disabled()
    };
    Ok((
        virtual_status(status, pull_requests),
        ReshapePlan {
            child: None,
            child_base: None,
            creates_head: true,
            affected_prs: vec![target.number],
        },
    ))
}

fn virtual_status(
    status: &StatusOutput,
    pull_requests: BTreeMap<PrNumber, PullRequestSnapshot>,
) -> StatusOutput {
    let snapshot = RepositorySnapshot {
        merge_candidates: Vec::new(),
        merge_candidates_truncated: 0,
        previous_default_oid: None,
        default_branch_movements: Vec::new(),
        repository: status.repository.clone(),
        default_branch: status.analysis.fleet.default_branch.clone(),
        current_branch: status.current_branch.clone(),
        current_pr: status.current_pr,
        pull_requests: pull_requests.into_values().collect(),
        generation_facts: Vec::new(),
        observed_at: None,
    };
    let analysis = crate::graph::derive_for_actor(&snapshot, status.head_merge.actor);
    let admission = crate::read::resolve_admission_with_generation(
        &analysis,
        &status.admission.priority_labels,
        status.admission.generation_integrity.clone(),
    );
    StatusOutput {
        config_provenance: None,
        head_merge: crate::read::HeadMergeStatus::default(),
        runtime: status.runtime.clone(),
        provider_api: status.provider_api.clone(),
        merge_candidates: Vec::new(),
        merge_candidates_truncated: 0,
        previous_default_oid: None,
        default_branch_movements: Vec::new(),
        timing: None,
        repository: status.repository.clone(),
        rebase_on_join: status.rebase_on_join.clone(),
        auto_admission: status.auto_admission.clone(),
        default_branch: status.default_branch.clone(),
        current_branch: status.current_branch.clone(),
        current_pr: status.current_pr,
        healthy: false,
        initialization: status.initialization.clone(),
        analysis,
        pauses: status.pauses.clone(),
        admission,
        sync_budget: status.sync_budget.clone(),
    }
}

/// Project the exact graph problems a status would report.
fn projected_problems(
    status: &StatusOutput,
    checker: &impl CompatibilityChecker,
) -> Result<Vec<crate::model::GraphProblem>, AppError> {
    let snapshot = RepositorySnapshot {
        merge_candidates: Vec::new(),
        merge_candidates_truncated: 0,
        previous_default_oid: None,
        default_branch_movements: Vec::new(),
        repository: status.repository.clone(),
        default_branch: status.analysis.fleet.default_branch.clone(),
        current_branch: status.current_branch.clone(),
        current_pr: status.current_pr,
        pull_requests: status.analysis.pull_requests.values().cloned().collect(),
        generation_facts: Vec::new(),
        observed_at: None,
    };
    Ok(analyze(&snapshot, checker)?.fleet.problems)
}

/// Refuse a reshape only when it *introduces* a graph problem.
///
/// bd-0dab27: this used to require the whole projected fleet to be healthy, so
/// any pre-existing problem anywhere blocked every eviction — including
/// evicting a tail, which cannot introduce a problem because it has no child
/// and therefore re-links no edge. That made the one tool for dismantling a
/// broken chain unusable precisely when the chain was broken, and forced raw
/// provider mutation instead. Reshapes that reduce or preserve the existing
/// problem set are now allowed, and the refusal names exactly what this
/// operation would break rather than restating what was already broken.
fn preflight_result(
    current: &StatusOutput,
    projected: &StatusOutput,
    checker: &impl CompatibilityChecker,
) -> Result<(), AppError> {
    let before = projected_problems(current, checker)?;
    let after = projected_problems(projected, checker)?;
    let introduced = after
        .iter()
        .filter(|problem| !before.contains(problem))
        .cloned()
        .collect::<Vec<_>>();
    if introduced.is_empty() {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "reshape_would_break_fleet",
        "the proposed reshape would introduce new Caravan graph/compatibility problems",
        Some(json!({
            "introduced_problems": introduced,
            "preexisting_problems": before,
            "safe_next_action": "resolve the introduced problems, or reshape tail-first so no surviving edge has to be re-linked across a removed member",
        })),
    ))
}

fn preflight_new_head(
    provider: &impl MembershipProvider,
    status: &StatusOutput,
) -> Result<(), AppError> {
    // bd-4d725c: native auto-merge is a precondition only when the provider
    // performs the merge. Under `sync.head_merge_actor: caravan` cara merges the
    // promoted head itself, so a repository that deliberately disabled native
    // auto-merge must still be able to promote a new head by eviction.
    if !status.head_merge.actor.caravan()
        && !provider
            .repository_allows_auto_merge(&status.repository)
            .map_err(|error| provider_error(&error, None))?
    {
        return Err(AppError::validation(
            "auto_merge_not_enabled",
            "repository settings do not permit auto-merge for the new head; enable it, or set sync.head_merge_actor=\"caravan\" so cara owns the merge",
        ));
    }
    if !provider
        .branch_is_protected(&status.repository, &status.default_branch)
        .map_err(|error| provider_error(&error, None))?
    {
        return Err(AppError::validation(
            "default_branch_not_protected",
            "the default branch must be protected before creating a new head",
        ));
    }
    Ok(())
}

struct ReshapeState {
    operation_id: OperationId,
    operation: ReshapeOperation,
    steps: Vec<MutationStep>,
    provider_receipts: Vec<GitHubMutationReceipt>,
    pull_requests: BTreeMap<PrNumber, PullRequestSnapshot>,
}

impl ReshapeState {
    fn new(
        operation: ReshapeOperation,
        pull_requests: BTreeMap<PrNumber, PullRequestSnapshot>,
    ) -> Self {
        Self {
            operation_id: OperationId::new(),
            operation,
            steps: Vec::new(),
            provider_receipts: Vec::new(),
            pull_requests,
        }
    }

    fn receipt(&self) -> OperationReceipt {
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

    fn current(&self, number: PrNumber) -> &PullRequestSnapshot {
        self.pull_requests
            .get(&number)
            .expect("reshape target is present")
    }

    fn precondition(&self, number: PrNumber) -> PullRequestPrecondition {
        PullRequestPrecondition::from(self.current(number))
    }

    fn record(&mut self, receipt: GitHubMutationReceipt, summary: &str) {
        let number = receipt.after.number;
        self.pull_requests.insert(number, receipt.after.clone());
        self.steps.push(MutationStep {
            kind: receipt.kind,
            state: MutationStepState::Completed,
            pr: Some(number),
            summary: summary.to_owned(),
        });
        self.provider_receipts.push(receipt);
    }

    fn already(&mut self, kind: MutationKind, number: PrNumber, summary: &str) {
        self.steps.push(MutationStep {
            kind,
            state: MutationStepState::AlreadySatisfied,
            pr: Some(number),
            summary: summary.to_owned(),
        });
    }

    fn ensure_base(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &crate::model::RepositoryId,
        number: PrNumber,
        base: &BranchSnapshot,
    ) -> Result<(), AppError> {
        if self.current(number).base.name == base.name {
            self.already(MutationKind::SetBase, number, "required base already set");
            return Ok(());
        }
        let receipt = provider
            .set_base(repository, &self.precondition(number), &base.name)
            .map_err(|error| provider_error(&error, Some(self)))?;
        self.record(receipt, "changed PR base during reshape");
        Ok(())
    }

    fn ensure_label_present(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &crate::model::RepositoryId,
        number: PrNumber,
        label: &str,
    ) -> Result<(), AppError> {
        if self.current(number).has_label(label) {
            self.already(
                MutationKind::AddLabel,
                number,
                "required label already present",
            );
            return Ok(());
        }
        let receipt = provider
            .add_label(repository, &self.precondition(number), label)
            .map_err(|error| provider_error(&error, Some(self)))?;
        self.record(receipt, &format!("added label `{label}`"));
        Ok(())
    }

    fn ensure_label_absent(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &crate::model::RepositoryId,
        number: PrNumber,
        label: &str,
    ) -> Result<(), AppError> {
        if !self.current(number).has_label(label) {
            self.already(MutationKind::RemoveLabel, number, "label already absent");
            return Ok(());
        }
        let receipt = provider
            .remove_label(repository, &self.precondition(number), label)
            .map_err(|error| provider_error(&error, Some(self)))?;
        self.record(receipt, &format!("removed label `{label}`"));
        Ok(())
    }

    fn ensure_control_label_comment(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &crate::model::RepositoryId,
        number: PrNumber,
        audit: &ControlLabelAudit,
    ) -> Result<(), AppError> {
        let receipt = provider
            .ensure_control_label_comment(repository, &self.precondition(number), audit)
            .map_err(|error| comment_provider_error(&error, self))?;
        let already = receipt
            .provider_output
            .as_deref()
            .is_some_and(|output| output.starts_with("existing GitHub comment"));
        if already {
            self.already(
                MutationKind::Comment,
                number,
                "control-label audit comment already present",
            );
            self.pull_requests.insert(number, receipt.after);
        } else {
            self.record(receipt, "posted durable control-label audit comment");
        }
        Ok(())
    }

    fn ensure_squash_auto_merge(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &crate::model::RepositoryId,
        number: PrNumber,
    ) -> Result<(), AppError> {
        let current = self.current(number);
        if current.auto_merge.enabled
            && current.auto_merge.merge_method == Some(MergeMethod::Squash)
        {
            self.already(
                MutationKind::EnableAutoMerge,
                number,
                "squash auto-merge already enabled",
            );
            return Ok(());
        }
        let receipt = provider
            .enable_squash_auto_merge(repository, &self.precondition(number))
            .map_err(|error| provider_error(&error, Some(self)))?;
        self.record(receipt, "enabled squash auto-merge on new head");
        Ok(())
    }

    fn ensure_auto_merge_disabled(
        &mut self,
        provider: &impl MembershipProvider,
        repository: &crate::model::RepositoryId,
        number: PrNumber,
    ) -> Result<(), AppError> {
        if !self.current(number).auto_merge.enabled {
            self.already(
                MutationKind::DisableAutoMerge,
                number,
                "auto-merge already disabled",
            );
            return Ok(());
        }
        let receipt = provider
            .disable_auto_merge(repository, &self.precondition(number))
            .map_err(|error| provider_error(&error, Some(self)))?;
        self.record(receipt, "disabled auto-merge during reshape");
        Ok(())
    }
}

fn comment_provider_error(error: &MutationError, state: &ReshapeState) -> AppError {
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

fn provider_error(error: &MutationError, state: Option<&ReshapeState>) -> AppError {
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
            "operation_id": state.map(|state| &state.operation_id),
            "completed_steps": state.map(|state| &state.steps),
            "provider_receipts": state.map(|state| &state.provider_receipts),
            "resumable": true,
        })),
    )
}

fn missing_pr(number: PrNumber) -> AppError {
    AppError::structured(
        ErrorCategory::TargetNotFound,
        "reshape_pr_not_found",
        format!("PR #{number} is missing from discovery"),
        None,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    use super::*;
    use crate::graph::CompatibilityChecker;
    use crate::model::{CommitOid, CompatibilityOutcome, CompatibilityReport, RepositoryId};

    struct FakeProvider {
        pull_requests: RefCell<BTreeMap<PrNumber, PullRequestSnapshot>>,
        allows_auto_merge: bool,
    }

    impl FakeProvider {
        fn new(pull_requests: &[PullRequestSnapshot]) -> Self {
            Self {
                allows_auto_merge: true,
                pull_requests: RefCell::new(
                    pull_requests
                        .iter()
                        .cloned()
                        .map(|pull_request| (pull_request.number, pull_request))
                        .collect(),
                ),
            }
        }

        #[allow(clippy::unnecessary_wraps)]
        fn mutate(
            &self,
            expected: &PullRequestPrecondition,
            kind: MutationKind,
            update: impl FnOnce(&mut PullRequestSnapshot),
        ) -> Result<GitHubMutationReceipt, MutationError> {
            let mut pulls = self.pull_requests.borrow_mut();
            let current = pulls.get_mut(&expected.number).expect("fake PR");
            assert_eq!(PullRequestPrecondition::from(&*current), *expected);
            let before = current.clone();
            update(current);
            Ok(GitHubMutationReceipt {
                kind,
                before: Some(before),
                after: current.clone(),
                provider_output: None,
            })
        }
    }

    impl MembershipProvider for FakeProvider {
        fn verify_branch_head(
            &self,
            _repository: &RepositoryId,
            _branch: &str,
            _expected: &CommitOid,
        ) -> Result<(), MutationError> {
            Ok(())
        }

        fn refetch_pull_request(
            &self,
            _repository: &RepositoryId,
            number: PrNumber,
        ) -> Result<PullRequestSnapshot, MutationError> {
            Ok(self.pull_requests.borrow()[&number].clone())
        }

        fn branch_is_protected(
            &self,
            _repository: &RepositoryId,
            _branch: &str,
        ) -> Result<bool, MutationError> {
            Ok(true)
        }

        fn repository_allows_auto_merge(
            &self,
            _repository: &RepositoryId,
        ) -> Result<bool, MutationError> {
            Ok(self.allows_auto_merge)
        }

        fn repository_labels(
            &self,
            _repository: &RepositoryId,
        ) -> Result<BTreeSet<String>, MutationError> {
            Ok([ACTIVE_LABEL, EVICTED_LABEL, FORCE_LABEL]
                .into_iter()
                .map(str::to_owned)
                .collect())
        }

        fn create_pull_request(
            &self,
            _repository: &RepositoryId,
            _input: &crate::github::CreatePullRequestInput,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            unreachable!()
        }

        fn set_base(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            base: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::SetBase, |pull_request| {
                pull_request.base = branch(base, 99);
            })
        }

        fn add_label(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            label: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::AddLabel, |pull_request| {
                pull_request.labels.insert(label.to_owned());
            })
        }

        fn remove_label(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            label: &str,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::RemoveLabel, |pull_request| {
                pull_request.labels.remove(label);
            })
        }

        fn ensure_control_label_comment(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
            _audit: &crate::github::ControlLabelAudit,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::Comment, |_| {})
        }

        fn enable_squash_auto_merge(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::EnableAutoMerge, |pull_request| {
                pull_request.auto_merge = AutoMergeState::squash();
            })
        }

        fn disable_auto_merge(
            &self,
            _repository: &RepositoryId,
            expected: &PullRequestPrecondition,
        ) -> Result<GitHubMutationReceipt, MutationError> {
            self.mutate(expected, MutationKind::DisableAutoMerge, |pull_request| {
                pull_request.auto_merge = AutoMergeState::disabled();
            })
        }
    }

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "caravan".to_owned(),
        }
    }

    fn branch(name: &str, number: u64) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(format!("{number:040x}")),
        }
    }

    fn pull_request(number: u64, base: &str) -> PullRequestSnapshot {
        PullRequestSnapshot {
            merge_state_status: None,
            number: PrNumber(number),
            title: format!("PR {number}"),
            url: format!("https://example.invalid/{number}"),
            state: PullRequestState::Open,
            draft: false,
            head: branch(&format!("pr-{number}"), number),
            base: branch(base, 99),
            cross_repository: false,
            labels: BTreeSet::from([ACTIVE_LABEL.to_owned()]),
            // Absent configuration keeps the historical provider-native actor.
            auto_merge: if base == "main" {
                AutoMergeState::squash()
            } else {
                AutoMergeState::disabled()
            },
            checks: Vec::new(),
            created_at: Some(format!("2026-01-01T00:00:{number:02}Z")),
            merged_at: None,
            updated_at: None,
        }
    }

    fn status(pulls: Vec<PullRequestSnapshot>) -> StatusOutput {
        let snapshot = RepositorySnapshot {
            merge_candidates: Vec::new(),
            merge_candidates_truncated: 0,
            previous_default_oid: None,
            default_branch_movements: Vec::new(),
            repository: repository(),
            default_branch: branch("main", 99),
            current_branch: Some("fixture".to_owned()),
            current_pr: pulls.first().map(|pull_request| pull_request.number),
            pull_requests: pulls,
            generation_facts: Vec::new(),
            observed_at: None,
        };
        let analysis = crate::graph::analyze(&snapshot, &Clean).unwrap();
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
            admission: crate::read::resolve_admission(
                &analysis,
                &crate::config::CaravanConfig::default().agent_priority_labels,
            ),
            analysis,
            pauses: Vec::new(),
            sync_budget: crate::sync::SyncBudgetStatus::default(),
        }
    }

    struct Clean;

    impl CompatibilityChecker for Clean {
        fn check(
            &self,
            candidate: &BranchSnapshot,
            target: &BranchSnapshot,
        ) -> Result<CompatibilityReport, AppError> {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Clean,
                conflicting_paths: Vec::new(),
                diagnostic: None,
            })
        }
    }

    #[test]
    fn eviction_failure_event_carries_selected_pr_fleet_and_reason() {
        let status = status(vec![pull_request(1, "main"), pull_request(2, "pr-1")]);
        let error = AppError::validation("eviction_rejected", "cannot evict");

        let event =
            eviction_failed_event(&status, Some(PrNumber(2)), Some("known breakage"), &error);

        assert_eq!(event.kind, EventKind::EvictionFailed);
        assert_eq!(event.caravan_id, Some(PrNumber(1)));
        assert_eq!(event.prs, vec![PrNumber(2)]);
        assert_eq!(event.fleet, Some(status.analysis.fleet));
        assert_eq!(event.metadata["error_code"], "eviction_rejected");
        assert_eq!(event.metadata["requested_reason"], "known breakage");
    }

    #[test]
    fn evict_middle_retargets_child_and_marks_target() {
        let pulls = vec![
            pull_request(1, "main"),
            pull_request(2, "pr-1"),
            pull_request(3, "pr-2"),
        ];
        let provider = FakeProvider::new(&pulls);
        let output = execute(
            status(pulls),
            &Clean,
            &provider,
            ReshapeOperation::Evict,
            Some(PrNumber(2)),
            Some("broken".to_owned()),
            None,
        )
        .unwrap();
        let state = provider.pull_requests.borrow();
        assert!(state[&PrNumber(2)].has_label(EVICTED_LABEL));
        assert!(!state[&PrNumber(2)].has_label(ACTIVE_LABEL));
        assert_eq!(state[&PrNumber(3)].base.name, "pr-1");
        assert!(!state[&PrNumber(3)].auto_merge.enabled);
        assert_eq!(output.affected_prs, vec![PrNumber(2), PrNumber(3)]);
    }

    #[test]
    fn evict_head_promotes_child_with_squash_auto_merge() {
        let pulls = vec![pull_request(1, "main"), pull_request(2, "pr-1")];
        let provider = FakeProvider::new(&pulls);
        execute(
            status(pulls),
            &Clean,
            &provider,
            ReshapeOperation::Evict,
            Some(PrNumber(1)),
            Some("head failed".to_owned()),
            None,
        )
        .unwrap();
        let state = provider.pull_requests.borrow();
        assert_eq!(state[&PrNumber(2)].base.name, "main");
        assert_eq!(state[&PrNumber(2)].auto_merge, AutoMergeState::squash());
    }

    #[test]
    fn evict_tail_needs_no_child_retarget() {
        let pulls = vec![pull_request(1, "main"), pull_request(2, "pr-1")];
        let provider = FakeProvider::new(&pulls);
        let output = execute(
            status(pulls),
            &Clean,
            &provider,
            ReshapeOperation::Evict,
            Some(PrNumber(2)),
            Some("tail failed".to_owned()),
            None,
        )
        .unwrap();
        assert_eq!(output.affected_prs, vec![PrNumber(2)]);
    }

    /// Every pair conflicts, so the fleet is unhealthy before and after.
    struct AlwaysConflicts;
    impl CompatibilityChecker for AlwaysConflicts {
        fn check(
            &self,
            candidate: &BranchSnapshot,
            target: &BranchSnapshot,
        ) -> Result<CompatibilityReport, AppError> {
            Ok(CompatibilityReport {
                candidate: candidate.clone(),
                target: target.clone(),
                outcome: CompatibilityOutcome::Conflict,
                conflicting_paths: vec!["src/lib.rs".to_owned()],
                diagnostic: None,
            })
        }
    }

    /// bd-4d725c: promoting a new head by eviction must not demand a
    /// provider-native setting the repository will never use.
    #[test]
    fn a_caravan_merge_actor_promotes_a_head_without_native_auto_merge() {
        let pulls = vec![pull_request(1, "main"), pull_request(2, "pr-1")];
        let mut provider = FakeProvider::new(&pulls);
        provider.allows_auto_merge = false;
        let mut status = status(pulls);
        status.head_merge.actor = crate::model::HeadMergeActor::Caravan;

        let result = execute(
            status,
            &Clean,
            &provider,
            ReshapeOperation::Evict,
            Some(PrNumber(1)),
            Some("head failed".to_owned()),
            None,
        );

        let refused = result
            .as_ref()
            .err()
            .is_some_and(|error| mcp_cli::StructuredError::code(error) == "auto_merge_not_enabled");
        assert!(
            !refused,
            "cara-owned merges must not require native auto-merge: {result:?}"
        );
    }

    /// bd-cef612: retargeting a child does not strip the evicted member's
    /// commits from members that were physically rebased onto it.
    #[test]
    fn eviction_reports_descendants_that_still_carry_the_evicted_patch() {
        let pulls = vec![
            pull_request(1, "main"),
            pull_request(2, "pr-1"),
            pull_request(3, "pr-2"),
        ];
        let provider = FakeProvider::new(&pulls);

        let output = execute(
            status(pulls),
            &Clean,
            &provider,
            ReshapeOperation::Evict,
            Some(PrNumber(1)),
            Some("head failed".to_owned()),
            None,
        )
        .expect("head eviction succeeds");

        assert_eq!(
            output.descendants_inheriting_evicted_patch,
            vec![PrNumber(2), PrNumber(3)]
        );
    }

    /// bd-0dab27 live case: a caravan whose members already conflict must still
    /// be dismantleable. Evicting the tail removes a node and re-links no edge,
    /// so it cannot introduce a problem and must not be refused because the
    /// remaining chain is still broken.
    #[test]
    fn evict_tail_succeeds_while_the_rest_of_the_fleet_stays_broken() {
        let pulls = vec![
            pull_request(1, "main"),
            pull_request(2, "pr-1"),
            pull_request(3, "pr-2"),
        ];
        let provider = FakeProvider::new(&pulls);

        let output = execute(
            status(pulls),
            &AlwaysConflicts,
            &provider,
            ReshapeOperation::Evict,
            Some(PrNumber(3)),
            Some("dismantling a broken chain".to_owned()),
            None,
        )
        .expect("evicting the tail introduces no new problem");

        assert_eq!(output.affected_prs, vec![PrNumber(3)]);
    }

    /// The middle case still fails closed, because closing the gap creates a
    /// genuinely new edge, and the refusal names what it would introduce.
    #[test]
    fn evict_middle_still_refuses_when_the_new_edge_conflicts() {
        let pulls = vec![
            pull_request(1, "main"),
            pull_request(2, "pr-1"),
            pull_request(3, "pr-2"),
        ];
        let provider = FakeProvider::new(&pulls);

        let error = execute(
            status(pulls),
            &AlwaysConflicts,
            &provider,
            ReshapeOperation::Evict,
            Some(PrNumber(2)),
            Some("middle".to_owned()),
            None,
        )
        .unwrap_err();

        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "reshape_would_break_fleet"
        );
        let details = mcp_cli::StructuredError::details(&error).expect("details");
        assert!(
            !details["introduced_problems"]
                .as_array()
                .expect("introduced problems")
                .is_empty(),
            "the refusal must name the problem this reshape introduces"
        );
    }

    #[test]
    fn split_existing_healthy_head_is_rejected() {
        let pulls = vec![pull_request(1, "main"), pull_request(2, "pr-1")];
        let provider = FakeProvider::new(&pulls);
        let error = execute(
            status(pulls),
            &Clean,
            &provider,
            ReshapeOperation::Split,
            Some(PrNumber(1)),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(mcp_cli::StructuredError::code(&error), "split_pr_is_head");
    }

    #[test]
    fn split_non_head_creates_second_healthy_caravan() {
        let pulls = vec![
            pull_request(1, "main"),
            pull_request(2, "pr-1"),
            pull_request(3, "pr-2"),
        ];
        let provider = FakeProvider::new(&pulls);
        let output = execute(
            status(pulls),
            &Clean,
            &provider,
            ReshapeOperation::Split,
            Some(PrNumber(2)),
            None,
            None,
        )
        .unwrap();
        let state = provider.pull_requests.borrow();
        assert_eq!(state[&PrNumber(2)].base.name, "main");
        assert_eq!(state[&PrNumber(2)].auto_merge, AutoMergeState::squash());
        assert_eq!(output.resulting_fleet.caravans.len(), 2);
    }

    #[test]
    fn incompatible_final_fleet_fails_before_provider_mutation() {
        struct Conflict;
        impl CompatibilityChecker for Conflict {
            fn check(
                &self,
                candidate: &BranchSnapshot,
                target: &BranchSnapshot,
            ) -> Result<CompatibilityReport, AppError> {
                Ok(CompatibilityReport {
                    candidate: candidate.clone(),
                    target: target.clone(),
                    outcome: CompatibilityOutcome::Conflict,
                    conflicting_paths: vec!["src/lib.rs".to_owned()],
                    diagnostic: None,
                })
            }
        }
        let pulls = vec![pull_request(1, "main"), pull_request(2, "pr-1")];
        let provider = FakeProvider::new(&pulls);
        let error = execute(
            status(pulls.clone()),
            &Conflict,
            &provider,
            ReshapeOperation::Split,
            Some(PrNumber(2)),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "reshape_would_break_fleet"
        );
        let state = provider.pull_requests.borrow();
        assert_eq!(state[&PrNumber(2)].base.name, "pr-1");
    }
}
