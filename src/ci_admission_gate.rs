//! Trusted default-policy CI admission gate (bd-efc8ba).
//!
//! Unknown evidence always runs expensive CI. Only one complete exact provider
//! state may emit `deferred_unjoined`; the consuming workflow deliberately turns
//! that receipt into its required failing sentinel.

use std::fs;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::model::{CommitOid, PrNumber, RepositoryId};
use crate::{
    AppContext, CiAdmissionGateDecision, CiAdmissionGateInput, CiAdmissionGateOutput,
    CiAdmissionGatePolicyEvidence,
};

const EVENT_MAX_BYTES: u64 = 128 * 1024;
const SCHEMA_VERSION: u32 = 1;
const DEFERRED_WORKFLOW_EXIT_CODE: i32 = 78;

#[derive(Debug)]
struct EventFacts {
    repository: RepositoryId,
    action: String,
    wake_pr: PrNumber,
    head: CommitOid,
    base: CommitOid,
}

/// Evaluate one GitHub `pull_request` event against freshly materialized
/// default-branch policy and live provider membership. This surface performs
/// provider reads and temporary local materialization but no provider writes.
pub fn evaluate(context: &AppContext, input: &CiAdmissionGateInput) -> CiAdmissionGateOutput {
    let fingerprint = config_fingerprint(context);
    if context.config_path.is_absolute() || !context.config.sync.allow_fetch {
        return output(
            context,
            None,
            input.selected_pr.map(PrNumber),
            None,
            None,
            CiAdmissionGateDecision::RunUnproven,
            false,
            false,
            "admission deferral requires freshly fetched default-branch policy; explicit/local-only config runs expensive CI"
                .to_owned(),
            fingerprint,
            None,
        );
    }
    let prepared = match crate::sync_authority::prepare_for_ci_admission(context) {
        Ok(prepared) => prepared,
        Err(error) => {
            return output(
                context,
                None,
                input.selected_pr.map(PrNumber),
                None,
                None,
                CiAdmissionGateDecision::RunUnproven,
                false,
                false,
                format!(
                    "authoritative default policy is unproven ({}); expensive CI is required",
                    error.code
                ),
                fingerprint,
                None,
            );
        }
    };
    // `prepare` has already fetched and sealed the provider default generation.
    // Its materialized config path is necessarily absolute, so downstream
    // provenance will describe that path as `Explicit`; carry the authority
    // proof instead of asking path syntax to rediscover it.
    evaluate_trusted(prepared.context(), input, true)
}

#[allow(clippy::too_many_lines)] // Linear fail-safe validation keeps every safe default explicit.
fn evaluate_trusted(
    context: &AppContext,
    input: &CiAdmissionGateInput,
    authority_materialized: bool,
) -> CiAdmissionGateOutput {
    let config_fingerprint = config_fingerprint(context);
    let selected_pr = input.selected_pr.map(PrNumber);
    let event = match read_event(&input.event) {
        Ok(event) => event,
        Err(reason) => {
            return output(
                context,
                None,
                selected_pr,
                None,
                None,
                CiAdmissionGateDecision::RunUnproven,
                false,
                false,
                reason,
                config_fingerprint,
                None,
            );
        }
    };
    let Some(policy) = context.config.ci.admission_gate.clone() else {
        return output(
            context,
            Some(&event),
            selected_pr.or(Some(event.wake_pr)),
            None,
            None,
            CiAdmissionGateDecision::RunUnproven,
            false,
            false,
            "ci.admission_gate is disabled in trusted default policy; expensive CI is required"
                .to_owned(),
            config_fingerprint,
            None,
        );
    };
    let policy_evidence = CiAdmissionGatePolicyEvidence {
        mode: policy.mode,
        context: policy.context,
        member_label: policy.member_label,
    };
    if !is_code_generation_action(&event.action) {
        return output(
            context,
            Some(&event),
            selected_pr.or(Some(event.wake_pr)),
            Some(policy_evidence),
            None,
            CiAdmissionGateDecision::RunUnproven,
            false,
            false,
            format!(
                "unsupported pull_request action `{}` cannot produce a deferral; expensive CI is required",
                event.action
            ),
            config_fingerprint,
            None,
        );
    }
    let selected_pr = selected_pr.unwrap_or(event.wake_pr);
    if selected_pr != event.wake_pr {
        return output(
            context,
            Some(&event),
            Some(selected_pr),
            Some(policy_evidence),
            None,
            CiAdmissionGateDecision::RunUnproven,
            false,
            false,
            "selected PR differs from workflow wake PR; this suite cannot run or defer another generation"
                .to_owned(),
            config_fingerprint,
            None,
        );
    }
    if context
        .config
        .repository
        .as_deref()
        .is_some_and(|repository| repository != event.repository.slug())
    {
        return output(
            context,
            Some(&event),
            Some(selected_pr),
            Some(policy_evidence),
            None,
            CiAdmissionGateDecision::RunUnproven,
            false,
            false,
            "event repository differs from trusted configured repository; expensive CI is required"
                .to_owned(),
            config_fingerprint,
            None,
        );
    }

    evaluate_live_provider(
        context,
        &event,
        selected_pr,
        policy_evidence,
        config_fingerprint,
        authority_materialized,
    )
}

fn evaluate_live_provider(
    context: &AppContext,
    event: &EventFacts,
    selected_pr: PrNumber,
    policy: CiAdmissionGatePolicyEvidence,
    config_fingerprint: String,
    authority_materialized: bool,
) -> CiAdmissionGateOutput {
    let membership = match crate::read::admission_membership(context, selected_pr) {
        Ok(membership) => membership,
        Err(error) => {
            return output(
                context,
                Some(event),
                Some(selected_pr),
                Some(policy),
                None,
                CiAdmissionGateDecision::RunUnproven,
                false,
                false,
                format!(
                    "live provider membership is unproven ({}); expensive CI is required",
                    error.code
                ),
                config_fingerprint,
                None,
            );
        }
    };
    let telemetry = Some(membership.provider_api.clone());
    if !effective_policy_trusted(authority_materialized, None) {
        return output(
            context,
            Some(event),
            Some(selected_pr),
            Some(policy),
            None,
            CiAdmissionGateDecision::RunUnproven,
            false,
            false,
            "effective config is not proven byte-identical to default-branch policy; expensive CI is required"
                .to_owned(),
            config_fingerprint,
            telemetry,
        );
    }
    let candidate = &membership.candidate;
    if membership.repository != event.repository
        || candidate.head.oid != event.head
        || candidate.base.oid != event.base
        || candidate.cross_repository
        || candidate.state != crate::model::PullRequestState::Open
        || candidate.draft
    {
        return output(
            context,
            Some(event),
            Some(selected_pr),
            Some(policy),
            None,
            CiAdmissionGateDecision::RunUnproven,
            false,
            false,
            "event/provider repository, head, base, state, draft, or fork generation is unsupported/drifted; expensive CI is required"
                .to_owned(),
            config_fingerprint,
            telemetry,
        );
    }
    let enrolled = membership.enrolled;
    let member_label_present = candidate.has_label(&policy.member_label);
    let (decision, reason) = membership_decision(enrolled, member_label_present);
    let mut receipt = output(
        context,
        Some(event),
        Some(selected_pr),
        Some(policy),
        None,
        decision,
        enrolled,
        member_label_present,
        reason.to_owned(),
        config_fingerprint,
        telemetry,
    );
    receipt.default_head = Some(membership.default_head);
    receipt
}

#[allow(clippy::too_many_arguments)]
fn output(
    _context: &AppContext,
    event: Option<&EventFacts>,
    selected_pr: Option<PrNumber>,
    policy: Option<CiAdmissionGatePolicyEvidence>,
    status: Option<&crate::read::StatusOutput>,
    decision: CiAdmissionGateDecision,
    enrolled: bool,
    member_label_present: bool,
    reason: String,
    config_fingerprint: String,
    provider_api: Option<crate::model::GitHubApiTelemetry>,
) -> CiAdmissionGateOutput {
    let repository = event.map(|event| event.repository.clone());
    let wake_pr = event.map(|event| event.wake_pr);
    let event_action = event.map(|event| event.action.clone());
    let head = event.map(|event| event.head.clone());
    let base = event.map(|event| event.base.clone());
    let default_head = status.map(|status| status.analysis.fleet.default_branch.oid.clone());
    let fingerprint_body = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "decision": decision,
        "repository": repository,
        "wake_pr": wake_pr,
        "selected_pr": selected_pr,
        "event_action": event_action,
        "head": head,
        "base": base,
        "default_head": default_head,
        "enrolled": enrolled,
        "member_label_present": member_label_present,
        "policy": policy,
        "config_fingerprint": config_fingerprint,
    });
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&fingerprint_body).expect("receipt facts serialize"));
    let receipt_fingerprint = format!("sha256:{:x}", digest.finalize());
    CiAdmissionGateOutput {
        schema_version: SCHEMA_VERSION,
        decision,
        decision_code: decision.code().to_owned(),
        run_ci: decision.runs_ci(),
        deferred_unjoined: decision == CiAdmissionGateDecision::DeferredUnjoined,
        workflow_exit_code: if decision == CiAdmissionGateDecision::DeferredUnjoined {
            DEFERRED_WORKFLOW_EXIT_CODE
        } else {
            0
        },
        repository,
        wake_pr,
        selected_pr,
        event_action,
        head,
        base,
        default_head,
        enrolled,
        member_label_present,
        policy,
        config_fingerprint,
        reason,
        receipt_fingerprint,
        provider_api,
    }
}

fn effective_policy_trusted(
    authority_materialized: bool,
    provenance: Option<&crate::config_provenance::ConfigProvenance>,
) -> bool {
    authority_materialized || trusted_default_policy(provenance)
}

fn trusted_default_policy(provenance: Option<&crate::config_provenance::ConfigProvenance>) -> bool {
    provenance.is_some_and(|provenance| {
        matches!(
            provenance.relation,
            crate::config_provenance::ConfigRelation::DefaultBranch
                | crate::config_provenance::ConfigRelation::MatchesDefaultBranch
        )
    })
}

fn is_code_generation_action(action: &str) -> bool {
    matches!(action, "opened" | "synchronize" | "reopened")
}

const fn membership_decision(
    enrolled: bool,
    member_label_present: bool,
) -> (CiAdmissionGateDecision, &'static str) {
    match (enrolled, member_label_present) {
        (true, true) => (
            CiAdmissionGateDecision::RunMember,
            "exact live provider generation is an active Caravan member; run expensive CI",
        ),
        (false, false) => (
            CiAdmissionGateDecision::DeferredUnjoined,
            "exact live provider generation is unjoined; emit the required deferred sentinel",
        ),
        _ => (
            CiAdmissionGateDecision::RunUnproven,
            "membership graph and member label disagree; expensive CI is required",
        ),
    }
}

fn config_fingerprint(context: &AppContext) -> String {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&context.config).expect("validated config serializes"));
    format!("sha256:{:x}", digest.finalize())
}

fn read_event(path: &std::path::Path) -> Result<EventFacts, String> {
    let metadata = fs::metadata(path)
        .map_err(|_| "event file is unavailable; expensive CI is required".to_owned())?;
    if metadata.len() > EVENT_MAX_BYTES {
        return Err(
            "event file exceeds the bounded 128 KiB input; expensive CI is required".to_owned(),
        );
    }
    let bytes = fs::read(path)
        .map_err(|_| "event file could not be read; expensive CI is required".to_owned())?;
    let event: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "event JSON is malformed; expensive CI is required".to_owned())?;
    let repository = parse_repository(
        event
            .pointer("/repository/full_name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "event repository identity is missing; expensive CI is required".to_owned()
            })?,
    )?;
    let wake_pr = event
        .pointer("/pull_request/number")
        .and_then(Value::as_u64)
        .or_else(|| event.get("number").and_then(Value::as_u64))
        .map(PrNumber)
        .ok_or_else(|| "event PR number is missing; expensive CI is required".to_owned())?;
    let action = event
        .get("action")
        .and_then(Value::as_str)
        .filter(|action| !action.is_empty())
        .ok_or_else(|| "event action is missing; expensive CI is required".to_owned())?
        .to_owned();
    let head = parse_oid(
        event
            .pointer("/pull_request/head/sha")
            .and_then(Value::as_str)
            .ok_or_else(|| "event head is missing; expensive CI is required".to_owned())?,
        "head",
    )?;
    let base = parse_oid(
        event
            .pointer("/pull_request/base/sha")
            .and_then(Value::as_str)
            .ok_or_else(|| "event base is missing; expensive CI is required".to_owned())?,
        "base",
    )?;
    Ok(EventFacts {
        repository,
        action,
        wake_pr,
        head,
        base,
    })
}

fn parse_repository(value: &str) -> Result<RepositoryId, String> {
    let Some((owner, name)) = value.split_once('/') else {
        return Err("event repository is malformed; expensive CI is required".to_owned());
    };
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return Err("event repository is malformed; expensive CI is required".to_owned());
    }
    Ok(RepositoryId {
        owner: owner.to_owned(),
        name: name.to_owned(),
    })
}

fn parse_oid(value: &str, field: &str) -> Result<CommitOid, String> {
    if !(40..=64).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "event {field} is not one full Git object ID; expensive CI is required"
        ));
    }
    Ok(CommitOid(value.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_context() -> AppContext {
        let mut context = AppContext::default();
        context.config.repository = Some("owner/example".to_owned());
        context.config.ci.admission_gate = Some(crate::config::CiAdmissionGateConfig {
            mode: crate::config::CiAdmissionGateMode::CaravanLabel,
            context: "Caravan admission".to_owned(),
            member_label: "caravan".to_owned(),
        });
        context
    }

    fn write_event(action: &str, secret: Option<&str>) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("event.json");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "action": action,
                "number": 41,
                "repository": {"full_name": "owner/example"},
                "pull_request": {
                    "number": 41,
                    "head": {"sha": "1111111111111111111111111111111111111111"},
                    "base": {"sha": "2222222222222222222222222222222222222222"},
                },
                "untrusted_secret_sentinel": secret,
            }))
            .unwrap(),
        )
        .unwrap();
        (directory, path)
    }

    #[test]
    fn malformed_event_is_safe_run_not_false_deferral() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("event.json");
        fs::write(&path, b"not-json").unwrap();
        let output = evaluate_trusted(
            &AppContext::default(),
            &CiAdmissionGateInput {
                event: path,
                selected_pr: None,
                github_output: None,
            },
            false,
        );
        assert_eq!(output.decision, CiAdmissionGateDecision::RunUnproven);
        assert!(output.run_ci);
        assert!(!output.deferred_unjoined);
        assert_eq!(output.workflow_exit_code, 0);
    }

    #[test]
    fn only_default_or_byte_identical_policy_can_defer() {
        let provenance = |relation| crate::config_provenance::ConfigProvenance {
            schema_version: 1,
            relation,
            current_branch: None,
            default_branch_ref: Some("origin/main".to_owned()),
            reason: "fixture".to_owned(),
            behind_default_branch: Some(0),
        };
        assert!(trusted_default_policy(Some(&provenance(
            crate::config_provenance::ConfigRelation::DefaultBranch
        ))));
        assert!(trusted_default_policy(Some(&provenance(
            crate::config_provenance::ConfigRelation::MatchesDefaultBranch
        ))));
        for relation in [
            crate::config_provenance::ConfigRelation::Explicit,
            crate::config_provenance::ConfigRelation::DiffersFromDefaultBranch,
            crate::config_provenance::ConfigRelation::Unknown,
        ] {
            assert!(!trusted_default_policy(Some(&provenance(relation))));
        }
        assert!(!trusted_default_policy(None));

        let explicit = provenance(crate::config_provenance::ConfigRelation::Explicit);
        assert!(!effective_policy_trusted(false, Some(&explicit)));
        assert!(
            effective_policy_trusted(true, Some(&explicit)),
            "a successful authority materialization must survive its absolute config path"
        );
    }

    #[test]
    fn only_code_generation_actions_can_reach_live_provider_evaluation() {
        for action in ["opened", "synchronize", "reopened"] {
            assert!(is_code_generation_action(action));
        }
        for action in ["edited", "labeled", "unlabeled", "closed"] {
            assert!(!is_code_generation_action(action));
        }
    }

    #[test]
    fn membership_decision_defers_only_exact_unjoined_unlabelled_state() {
        assert_eq!(
            membership_decision(false, false).0,
            CiAdmissionGateDecision::DeferredUnjoined
        );
        assert_eq!(
            membership_decision(true, true).0,
            CiAdmissionGateDecision::RunMember
        );
        assert_eq!(
            membership_decision(true, false).0,
            CiAdmissionGateDecision::RunUnproven
        );
        assert_eq!(
            membership_decision(false, true).0,
            CiAdmissionGateDecision::RunUnproven
        );
    }

    #[test]
    fn exact_deferred_receipt_is_stable_and_delegates_required_failure_to_workflow() {
        let context = configured_context();
        let event = EventFacts {
            repository: RepositoryId {
                owner: "owner".to_owned(),
                name: "example".to_owned(),
            },
            action: "opened".to_owned(),
            wake_pr: PrNumber(41),
            head: CommitOid("1".repeat(40)),
            base: CommitOid("2".repeat(40)),
        };
        let policy = CiAdmissionGatePolicyEvidence {
            mode: crate::config::CiAdmissionGateMode::CaravanLabel,
            context: "Caravan admission".to_owned(),
            member_label: "caravan".to_owned(),
        };
        let first = output(
            &context,
            Some(&event),
            Some(PrNumber(41)),
            Some(policy.clone()),
            None,
            CiAdmissionGateDecision::DeferredUnjoined,
            false,
            false,
            "exact deferred fixture".to_owned(),
            config_fingerprint(&context),
            None,
        );
        let second = output(
            &context,
            Some(&event),
            Some(PrNumber(41)),
            Some(policy),
            None,
            CiAdmissionGateDecision::DeferredUnjoined,
            false,
            false,
            "rendered prose may differ".to_owned(),
            config_fingerprint(&context),
            None,
        );

        assert_eq!(first.decision, CiAdmissionGateDecision::DeferredUnjoined);
        assert!(!first.run_ci);
        assert!(first.deferred_unjoined);
        assert_eq!(first.workflow_exit_code, DEFERRED_WORKFLOW_EXIT_CODE);
        assert_eq!(first.receipt_fingerprint, second.receipt_fingerprint);
    }

    #[test]
    fn selected_wake_mismatch_and_label_events_run_safely_without_secret_echo() {
        let context = configured_context();
        let (_directory, path) = write_event("opened", Some("ghs_secret_sentinel"));
        let input = CiAdmissionGateInput {
            event: path,
            selected_pr: Some(99),
            github_output: None,
        };
        let first = evaluate_trusted(&context, &input, false);
        let second = evaluate_trusted(&context, &input, false);
        assert_eq!(first.decision, CiAdmissionGateDecision::RunUnproven);
        assert!(first.run_ci);
        assert_eq!(first.wake_pr, Some(PrNumber(41)));
        assert_eq!(first.selected_pr, Some(PrNumber(99)));
        assert_eq!(first.receipt_fingerprint, second.receipt_fingerprint);
        assert!(!serde_json::to_string(&first)
            .unwrap()
            .contains("ghs_secret_sentinel"));

        let (_directory, path) = write_event("labeled", None);
        let labeled = evaluate_trusted(
            &context,
            &CiAdmissionGateInput {
                event: path,
                selected_pr: None,
                github_output: None,
            },
            false,
        );
        assert_eq!(labeled.decision, CiAdmissionGateDecision::RunUnproven);
        assert!(labeled.run_ci);
        assert!(labeled.reason.contains("unsupported pull_request action"));
    }
}
