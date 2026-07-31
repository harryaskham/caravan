//! Bounded CI-admission gate (bd-2a29c8).
//!
//! Repositories adopting Caravan should not re-derive Caravan semantics from
//! GitHub event shape. This surface answers exactly one question — *is existing
//! CI evidence still valid for this pull request generation* — so a workflow can
//! make its expensive jobs conditional without guessing.
//!
//! The gate is advisory about **cost**, never about **safety**. It may only
//! assert that exact existing evidence still applies; it may never assert that a
//! head can merge without evidence. Anything unproven runs CI.

use serde_json::json;

use crate::model::PrNumber;
use crate::read::StatusOutput;
use crate::{AppError, CiGateDecision, CiGateInput, CiGateOutput};

/// Evaluate the gate against an exact discovered status.
pub fn evaluate(status: &StatusOutput, input: &CiGateInput) -> Result<CiGateOutput, AppError> {
    let pr = PrNumber(input.pr);
    let Some(candidate) = status.analysis.pull_requests.get(&pr) else {
        return Ok(output(
            CiGateDecision::CiUnknown,
            pr,
            "pull request was not present in discovery, so nothing can be proven".to_owned(),
            json!({ "head_evidence": input.head_evidence }),
        ));
    };
    let enrolled = status.analysis.fleet.containing(pr).is_some();
    if !enrolled {
        return Ok(output(
            CiGateDecision::CiNotApplicable,
            pr,
            "pull request is not an active caravan member; Caravan has no opinion".to_owned(),
            json!({ "head": candidate.head.oid, "enrolled": false }),
        ));
    }
    if candidate.has_label("caravan-force") {
        return Ok(output(
            CiGateDecision::CiForceAccepted,
            pr,
            "durable PR-scoped caravan-force intent bypasses CI for the member's current generation".to_owned(),
            json!({ "head": candidate.head.oid, "label": "caravan-force", "scope": "pull_request" }),
        ));
    }
    if !input.head_evidence {
        return Ok(output(
            CiGateDecision::CiRequired,
            pr,
            "no prior successful required-check run is known for this exact head".to_owned(),
            json!({ "head": candidate.head.oid, "head_evidence": false }),
        ));
    }
    // The promotion receipt proves tree equality when a retarget was a content
    // no-op. Without that proof the merge content may differ, so CI must run.
    let proven = status
        .analysis
        .cumulative_trees
        .iter()
        .any(|proof| proof.candidate.oid == candidate.head.oid);
    if proven {
        Ok(output(
            CiGateDecision::CiValid,
            pr,
            "head is unchanged and its proven merge tree is unchanged, so existing checks still apply".to_owned(),
            json!({ "head": candidate.head.oid, "cumulative_tree_proved": true }),
        ))
    } else {
        Ok(output(
            CiGateDecision::CiRequired,
            pr,
            "merge content for this head is not proven unchanged".to_owned(),
            json!({ "head": candidate.head.oid, "cumulative_tree_proved": false }),
        ))
    }
}

fn output(
    decision: CiGateDecision,
    pr: PrNumber,
    reason: String,
    evidence: serde_json::Value,
) -> CiGateOutput {
    CiGateOutput {
        schema_version: 1,
        decision,
        decision_code: decision.code().to_owned(),
        run_ci: decision.runs_ci(),
        pr,
        reason,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_decision_that_is_not_proven_runs_ci() {
        // The safety property this surface exists to guarantee.
        assert!(CiGateDecision::CiUnknown.runs_ci());
        assert!(CiGateDecision::CiRequired.runs_ci());
        assert!(!CiGateDecision::CiValid.runs_ci());
        assert!(!CiGateDecision::CiForceAccepted.runs_ci());
        assert!(!CiGateDecision::CiNotApplicable.runs_ci());
    }
}
