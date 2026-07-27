//! Membership shape, target resolution, repository preflight, and audit policy.

use super::{
    ACTIVE_LABEL, AppError, BTreeSet, Caravan, CheckInput, CheckOutput, CompatibilityChecker,
    ControlLabelAudit, EVICTED_LABEL, ErrorCategory, ExecutionState, FORCE_LABEL, JoinTarget,
    MembershipOperation, MembershipProvider, MembershipRequest, PrNumber, PullRequestSnapshot,
    PullRequestState, REQUIRED_LABELS, RepositoryId, SKIPPED_LABEL, StatusOutput,
    control_label_marker, json, mutation_error, read,
};

pub(super) fn validate_post_rebase_target(
    status: &StatusOutput,
    request: &MembershipRequest,
    target: Option<&JoinTarget>,
    rebase: &crate::physical_rebase::RebaseReceipt,
) -> Result<(), AppError> {
    let live_candidate = status.analysis.pull_requests.get(&rebase.pr);
    let candidate_matches = status.current_pr == Some(rebase.pr)
        && live_candidate.is_some_and(|candidate| candidate.head.oid == rebase.new_head_oid);
    let live_default = &status.analysis.fleet.default_branch;
    let candidate_caravan = status.analysis.fleet.containing(rebase.pr);
    let target_matches = if request.operation.is_join() {
        target.is_some_and(|target| {
            target.tail.head.name == rebase.new_base_branch
                && target.tail.head.oid == rebase.new_base_oid
        })
    } else {
        target.is_none()
            && candidate_caravan.is_none()
            && status.default_branch == rebase.new_base_branch
            && live_default.name == rebase.new_base_branch
            && live_default.oid == rebase.new_base_oid
    };
    if candidate_matches && target_matches {
        return Ok(());
    }

    let join = request.operation.is_join();
    Err(AppError::structured(
        ErrorCategory::Validation,
        if join {
            "join_target_moved_after_rebase"
        } else {
            "new_target_moved_after_rebase"
        },
        if join {
            "candidate or live caravan tail changed after physical preflight; refusing admission"
        } else {
            "candidate, default branch, or membership topology changed after physical preflight; refusing new caravan admission"
        },
        Some(json!({
            "operation": request.operation,
            "rebase_receipt": rebase,
            "current_pr": status.current_pr,
            "live_candidate": live_candidate,
            "live_tail": target.map(|target| &target.tail),
            "live_default": live_default,
            "candidate_caravan": candidate_caravan,
            "mutated_membership": false,
            "resumable": true,
            "safe_next_action": if join {
                "rediscover and retry the same atomic join"
            } else {
                "rediscover and retry the same atomic new/renew operation against the current default branch"
            }
        })),
    ))
}

pub(super) fn resolve_join_target(
    status: &StatusOutput,
    request: &MembershipRequest,
) -> Result<JoinTarget, AppError> {
    let caravan = if let Some(head) = request.head_pr.map(PrNumber) {
        status
            .analysis
            .fleet
            .caravan(head)
            .cloned()
            .ok_or_else(|| {
                AppError::validation(
                    "caravan_head_not_found",
                    format!("PR #{head} is not a current caravan head"),
                )
            })?
    } else if let Some(tail) = request.tail_pr.map(PrNumber) {
        status
            .analysis
            .fleet
            .caravans
            .iter()
            .find(|caravan| caravan.tail() == Some(tail))
            .cloned()
            .ok_or_else(|| {
                AppError::validation(
                    "caravan_tail_not_found",
                    format!("PR #{tail} is not a current caravan tail"),
                )
            })?
    } else {
        match status.analysis.fleet.caravans.as_slice() {
            [caravan] => caravan.clone(),
            [] => {
                return Err(AppError::validation(
                    "caravan_tail_not_found",
                    "there is no caravan to join; use `cara new`",
                ));
            }
            caravans => {
                return Err(AppError::structured(
                    ErrorCategory::Validation,
                    "ambiguous_caravan_tail",
                    "multiple caravan tails exist; pass --tail-pr or --head-pr",
                    Some(json!({
                        "candidate_tails": caravans.iter().filter_map(Caravan::tail).collect::<Vec<_>>(),
                    })),
                ));
            }
        }
    };
    let tail_number = caravan.tail().expect("caravans are non-empty");
    let tail = status
        .analysis
        .pull_requests
        .get(&tail_number)
        .cloned()
        .expect("derived tail has a snapshot");
    Ok(JoinTarget { caravan, tail })
}

pub(super) fn validate_operation_shape(
    candidate: &PullRequestSnapshot,
    request: &MembershipRequest,
    desired_base: &str,
) -> Result<(), AppError> {
    if candidate.state != PullRequestState::Open {
        return Err(AppError::validation(
            "current_pr_not_open",
            format!("PR #{} is not open", candidate.number),
        ));
    }
    if candidate.draft {
        return Err(AppError::validation(
            "current_pr_is_draft",
            format!("PR #{} is a draft", candidate.number),
        ));
    }
    if candidate.cross_repository {
        return Err(AppError::validation(
            "fork_only_head",
            "Caravan v1 requires the PR head branch in the base repository",
        ));
    }

    let active = candidate.has_label(ACTIVE_LABEL);
    let evicted = candidate.has_label(EVICTED_LABEL);
    match request.operation {
        MembershipOperation::New | MembershipOperation::Join if evicted => {
            return Err(AppError::validation(
                "current_pr_is_evicted",
                "the current PR is evicted; use renew or rejoin",
            ));
        }
        MembershipOperation::Renew | MembershipOperation::Rejoin if !evicted && !active => {
            return Err(AppError::validation(
                "current_pr_not_evicted",
                "renew and rejoin require an evicted PR",
            ));
        }
        _ => {}
    }

    if active && candidate.base.name != desired_base {
        return Err(AppError::validation(
            "active_pr_wrong_target",
            format!(
                "active PR #{} targets `{}` instead of `{desired_base}`",
                candidate.number, candidate.base.name
            ),
        ));
    }
    Ok(())
}

/// Typed admission-intent provenance for an already active resumed candidate.
///
/// Membership always operates on the owner's own current PR, so this is
/// checked-out owner selection: canonical position is evidence, never a gate.
/// Automatic priority/FIFO selection stays owned by sync.
fn resume_admission_intent(
    status: &StatusOutput,
    candidate: &PullRequestSnapshot,
    target: Option<&JoinTarget>,
) -> crate::admission::AdmissionIntentDecision {
    let mut decision = crate::admission::evaluate(
        &status.admission,
        &status.analysis,
        candidate,
        target.map(|target| &target.caravan),
        crate::admission::AdmissionSelection::CheckedOut,
    );
    decision.record_preflight(true, true);
    decision
}

pub(super) fn preflight_eligibility(
    status: &StatusOutput,
    candidate: &PullRequestSnapshot,
    request: &MembershipRequest,
    target: Option<&JoinTarget>,
    checker: &impl CompatibilityChecker,
) -> Result<CheckOutput, AppError> {
    crate::generation::require_admissible(
        &status.admission.generation_integrity,
        candidate.number,
    )?;
    if candidate.has_label(ACTIVE_LABEL) {
        let unrelated = status.analysis.fleet.problems.iter().filter(|problem| {
            !(problem.kind == crate::model::GraphProblemKind::AutoMergeInvariant
                && problem.prs == [candidate.number])
        });
        if let Some(problem) = unrelated.into_iter().next() {
            return Err(AppError::structured(
                ErrorCategory::Validation,
                "invalid_graph",
                "cannot resume membership while unrelated graph problems remain",
                Some(json!({ "problem": problem })),
            ));
        }
        return Ok(CheckOutput {
            provider_api: status.provider_api.clone(),
            rebase_on_join: status.rebase_on_join.clone(),
            mode: if request.operation.is_join() {
                read::CheckMode::JoinTail
            } else {
                read::CheckMode::NewCaravan
            },
            current_pr: candidate.number,
            candidate: candidate.clone(),
            head_repository_owner: candidate.head.repository.owner.clone(),
            merge_candidate: status
                .merge_candidates
                .iter()
                .find(|identity| identity.pr == candidate.number)
                .cloned(),
            enrolled: true,
            canonical_candidate: status.admission.next_candidate == Some(candidate.number),
            admission_note: None,
            admission_intent: Some(resume_admission_intent(status, candidate, target)),
            next_action: if request.operation.is_join() {
                read::CandidateNextAction::Join
            } else {
                read::CandidateNextAction::New
            },
            caravan_id: target
                .map(|target| target.caravan.id)
                .or(Some(candidate.number)),
            target_pr: target.and_then(|target| target.caravan.tail()),
            eligible: true,
            compatibility: status.analysis.compatibility.clone(),
            squash_reconciliations: status.analysis.squash_reconciliations.clone(),
            problems: Vec::new(),
            initialization: status.initialization.clone(),
        });
    }

    let mut virtual_status = status.clone();
    if request.operation.is_renewal() {
        let virtual_candidate = virtual_status
            .analysis
            .pull_requests
            .get_mut(&candidate.number)
            .expect("current candidate is present");
        virtual_candidate.labels.remove(EVICTED_LABEL);
        virtual_candidate.labels.remove(FORCE_LABEL);
    }
    let check_input = target.map_or_else(CheckInput::default, |target| CheckInput {
        pr: None,
        tail_pr: target.caravan.tail().map(|number| number.0),
        head_pr: None,
    });
    read::check_analysis(&virtual_status, &check_input, checker)
}

pub(super) fn preflight_repository(
    provider: &impl MembershipProvider,
    repository: &RepositoryId,
    default_branch: &str,
    operation: MembershipOperation,
    priority_labels: &[String],
    require_auto_admission_skip: bool,
    state: &ExecutionState,
) -> Result<(), AppError> {
    let labels = provider
        .repository_labels(repository)
        .map_err(|error| mutation_error(&error, state))?;
    require_labels(repository, &labels, require_auto_admission_skip)?;
    let missing_priorities: Vec<_> = priority_labels
        .iter()
        .filter(|label| !labels.contains(*label))
        .cloned()
        .collect();
    if !missing_priorities.is_empty() {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "required_priority_labels_missing",
            "configured priority labels must exist before mutation",
            Some(json!({ "repository": repository, "missing_labels": missing_priorities })),
        ));
    }
    if !operation.is_join()
        && !provider
            .repository_allows_auto_merge(repository)
            .map_err(|error| mutation_error(&error, state))?
    {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "auto_merge_not_enabled",
            "repository settings must allow squash auto-merge before creating a caravan head",
            Some(json!({
                "repository": repository,
                "next": "enable GitHub repository auto-merge and keep squash merge enabled, then rerun the same command",
            })),
        ));
    }
    if !operation.is_join()
        && !provider
            .branch_is_protected(repository, default_branch)
            .map_err(|error| mutation_error(&error, state))?
    {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "default_branch_not_protected",
            "the default branch must have a protection requirement before enabling auto-merge",
            Some(json!({
                "repository": repository,
                "default_branch": default_branch,
                "next": "configure a required status check or review on the default branch, then rerun the same command",
            })),
        ));
    }
    Ok(())
}

pub(super) fn require_labels(
    repository: &RepositoryId,
    labels: &BTreeSet<String>,
    require_auto_admission_skip: bool,
) -> Result<(), AppError> {
    let missing: Vec<_> = REQUIRED_LABELS
        .iter()
        .copied()
        .chain(require_auto_admission_skip.then_some(SKIPPED_LABEL))
        .filter(|label| !labels.contains(*label))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "required_labels_missing",
        "Caravan's operational labels must exist before the first mutation",
        Some(json!({
            "repository": repository,
            "missing_labels": missing,
            "next": "run `cara init` to create the fixed labels required by the active config, then rerun the same command",
        })),
    ))
}

pub(super) fn membership_audit(
    request: &MembershipRequest,
    before_labels: &BTreeSet<String>,
    eligibility: &CheckOutput,
    after: &PullRequestSnapshot,
    admission_priority_basis: String,
) -> ControlLabelAudit {
    let (reason, source) = request.reason.as_ref().map_or_else(
        || {
            let generated = if request.operation.is_join() {
                if request.tail_pr.is_some() || request.head_pr.is_some() {
                    "admitted after the explicitly selected caravan target"
                } else {
                    "admitted to the only mechanically inferred caravan tail"
                }
            } else if request.operation.is_renewal() {
                "evicted PR passed renewed queue eligibility"
            } else {
                "eligible PR admitted as a new caravan"
            };
            (
                generated.to_owned(),
                "deterministic Caravan policy".to_owned(),
            )
        },
        |reason| {
            (
                reason.trim().to_owned(),
                "explicit --reason input".to_owned(),
            )
        },
    );
    let compatibility = if eligibility.compatibility.is_empty() {
        "no new chain edge; repository and graph preflight passed".to_owned()
    } else {
        eligibility
            .compatibility
            .iter()
            .map(|report| {
                format!(
                    "{}@{} -> {}@{} = {:?}",
                    report.candidate.name,
                    report.candidate.oid.0,
                    report.target.name,
                    report.target.oid.0,
                    report.outcome
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    ControlLabelAudit {
        operation: request.operation.name().to_owned(),
        marker: control_label_marker(
            request.operation.name(),
            after.number,
            &after.head.oid,
            before_labels,
            &after.labels,
        ),
        before_labels: before_labels.clone(),
        after_labels: after.labels.clone(),
        actor: "authenticated GitHub actor invoked through cara CLI/JSON/MCP".to_owned(),
        reason,
        reason_source: source,
        compatibility_evidence: compatibility,
        clean_squash_evidence: if request.operation.is_join() {
            "compatibility check was clean; non-head auto-merge is disabled".to_owned()
        } else {
            "compatibility check was clean; squash auto-merge is enabled on the head".to_owned()
        },
        admission_priority_basis,
    }
}
