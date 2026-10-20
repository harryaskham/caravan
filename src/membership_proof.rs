//! Provider-truth membership proof for one exact PR generation (bd-78d4aa).
//!
//! Receipts are advisory evidence. When a caller needs to know whether a
//! publication, join, admission or merge actually happened, the authority is
//! the provider: the exact PR, its head and base, its membership label, its
//! position in the derived caravan graph, or its merge. This command reads only
//! those facts and returns one typed verdict, so a consumer never refuses real
//! progress because an incidental receipt field was spelled or shaped
//! differently from what it expected.
//!
//! Real safety stays in the verdict: an expected head that differs is a
//! different generation, a fork head is never a member, and durable
//! `caravan-force` intent is reported and never folded into an ordinary
//! success.

use clap::Args;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::{
    BranchSnapshot, CommitOid, GitHubApiTelemetry, PrNumber, PullRequestSnapshot, PullRequestState,
    RepositoryId,
};
use crate::{AppContext, AppError};

/// Current wire version of [`MembershipProofOutput`].
pub const MEMBERSHIP_PROOF_SCHEMA_VERSION: u32 = 1;

const FORCE_LABEL: &str = "caravan-force";

/// Read-only provider-truth membership question for one PR.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Args)]
pub struct MembershipProofInput {
    /// Exact PR number.
    #[arg(long)]
    pub pr: u64,
    /// Exact expected head OID: the published generation, or its post-rebase
    /// head when Cara rewrote it. A different head is a different generation.
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_head: Option<String>,
    /// Expected base ref of an open member: the default branch for a root, the
    /// predecessor branch for a joined member. Not applied to merged PRs.
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_base_ref: Option<String>,
}

/// Provider-truth outcome for one PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MembershipProofOutcome {
    /// Open, labelled, and a member of a derived active caravan.
    Enrolled,
    /// Merged by the provider.
    Merged,
    /// Open with neither the member label nor graph membership.
    Unjoined,
    /// Closed without merge.
    ClosedUnmerged,
    /// Open, but label and graph membership disagree (for example labelled
    /// while its base does not chain to a member, or a draft).
    Inconsistent,
}

impl MembershipProofOutcome {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Enrolled => "enrolled",
            Self::Merged => "merged",
            Self::Unjoined => "unjoined",
            Self::ClosedUnmerged => "closed_unmerged",
            Self::Inconsistent => "inconsistent",
        }
    }
}

/// One exact identity fact that differs from the caller's expectation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MembershipProofMismatch {
    pub field: String,
    pub expected: String,
    pub observed: String,
}

/// Typed provider-truth verdict. `succeeded` is the single answer a caller
/// needs: the exact generation is enrolled (without force intent) or merged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[allow(clippy::struct_excessive_bools)] // Independent provider facts, not state flags.
pub struct MembershipProofOutput {
    pub schema_version: u32,
    pub repository: RepositoryId,
    pub pr: PrNumber,
    pub outcome: MembershipProofOutcome,
    pub outcome_code: String,
    pub succeeded: bool,
    pub state: PullRequestState,
    pub draft: bool,
    pub cross_repository: bool,
    pub head_oid: CommitOid,
    pub base_ref: String,
    pub base_oid: CommitOid,
    pub default_branch: BranchSnapshot,
    pub member_label: String,
    pub member_label_present: bool,
    pub enrolled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caravan_id: Option<PrNumber>,
    /// One-based position in the containing caravan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    pub force_intent_present: bool,
    #[serde(default)]
    pub mismatches: Vec<MembershipProofMismatch>,
    pub reason: String,
    #[serde(default)]
    pub provider_api: GitHubApiTelemetry,
}

/// Provider facts the verdict is derived from.
#[derive(Debug, Clone)]
pub struct MembershipProofFacts {
    pub repository: RepositoryId,
    pub candidate: PullRequestSnapshot,
    /// Containing caravan and one-based position from the derived graph.
    pub caravan: Option<(PrNumber, u32)>,
    pub default_branch: BranchSnapshot,
    pub member_label: String,
    pub provider_api: GitHubApiTelemetry,
}

/// Read the four light provider facts and derive the verdict. Read-only.
///
/// # Errors
///
/// Returns typed discovery errors (including retryable GitHub App refusals)
/// and `membership_proof_invalid_input` for a malformed expected head.
pub fn prove(
    context: &AppContext,
    input: &MembershipProofInput,
) -> Result<MembershipProofOutput, AppError> {
    if let Some(head) = &input.expected_head
        && !is_oid(head)
    {
        return Err(AppError::validation(
            "membership_proof_invalid_input",
            "--expected-head must be an exact 40-character lowercase hexadecimal commit OID",
        ));
    }
    let membership = crate::read::admission_membership(context, PrNumber(input.pr))?;
    Ok(derive(
        MembershipProofFacts {
            repository: membership.repository,
            candidate: membership.candidate,
            caravan: membership.caravan,
            default_branch: membership.default_branch,
            member_label: "caravan".to_owned(),
            provider_api: membership.provider_api,
        },
        input,
    ))
}

fn is_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Pure verdict over already-read provider facts.
#[must_use]
pub fn derive(facts: MembershipProofFacts, input: &MembershipProofInput) -> MembershipProofOutput {
    let candidate = &facts.candidate;
    let member_label_present = candidate.has_label(&facts.member_label);
    let force_intent_present = candidate.has_label(FORCE_LABEL);
    let enrolled = facts.caravan.is_some();
    let mut mismatches = Vec::new();
    let mut mismatch = |field: &str, expected: &str, observed: &str| {
        mismatches.push(MembershipProofMismatch {
            field: field.to_owned(),
            expected: expected.to_owned(),
            observed: observed.to_owned(),
        });
    };
    if let Some(expected) = &input.expected_head
        && *expected != candidate.head.oid.0
    {
        mismatch("head_oid", expected, &candidate.head.oid.0);
    }
    if candidate.cross_repository {
        mismatch("head_repository", "base repository", "fork");
    }
    let open = candidate.state == PullRequestState::Open;
    if open
        && let Some(expected) = &input.expected_base_ref
        && *expected != candidate.base.name
    {
        mismatch("base_ref", expected, &candidate.base.name);
    }
    let outcome = match candidate.state {
        PullRequestState::Merged => MembershipProofOutcome::Merged,
        PullRequestState::Closed => MembershipProofOutcome::ClosedUnmerged,
        PullRequestState::Open if candidate.draft => MembershipProofOutcome::Inconsistent,
        PullRequestState::Open => match (enrolled, member_label_present) {
            (true, true) => MembershipProofOutcome::Enrolled,
            (false, false) => MembershipProofOutcome::Unjoined,
            _ => MembershipProofOutcome::Inconsistent,
        },
    };
    if outcome == MembershipProofOutcome::Enrolled && force_intent_present {
        mismatch("force_intent", "absent", "present");
    }
    let identity_exact = mismatches.is_empty();
    let succeeded = identity_exact
        && matches!(
            outcome,
            MembershipProofOutcome::Enrolled | MembershipProofOutcome::Merged
        );
    let reason = match (outcome, identity_exact) {
        (MembershipProofOutcome::Enrolled, true) => {
            "the exact generation is open, labelled and a member of a derived caravan; the join succeeded regardless of receipt fields"
        }
        (MembershipProofOutcome::Merged, true) => {
            "the exact generation was merged by the provider; the operation succeeded"
        }
        (MembershipProofOutcome::Enrolled | MembershipProofOutcome::Merged, false) => {
            "the provider outcome does not belong to the expected generation or carries force intent; do not treat it as this operation's success"
        }
        (MembershipProofOutcome::Unjoined, _) => {
            "the PR is open but not a member; membership did not happen and admission may be retried for this exact generation"
        }
        (MembershipProofOutcome::ClosedUnmerged, _) => {
            "the PR was closed without merge; it is neither a member nor delivered"
        }
        (MembershipProofOutcome::Inconsistent, _) => {
            "label and graph membership disagree or the PR is a draft; let sync converge before relying on membership"
        }
    };
    MembershipProofOutput {
        schema_version: MEMBERSHIP_PROOF_SCHEMA_VERSION,
        repository: facts.repository,
        pr: candidate.number,
        outcome,
        outcome_code: outcome.code().to_owned(),
        succeeded,
        state: candidate.state,
        draft: candidate.draft,
        cross_repository: candidate.cross_repository,
        head_oid: candidate.head.oid.clone(),
        base_ref: candidate.base.name.clone(),
        base_oid: candidate.base.oid.clone(),
        default_branch: facts.default_branch,
        member_label: facts.member_label,
        member_label_present,
        enrolled,
        caravan_id: facts.caravan.map(|(id, _)| id),
        position: facts.caravan.map(|(_, position)| position),
        force_intent_present,
        mismatches,
        reason: reason.to_owned(),
        provider_api: facts.provider_api,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AutoMergeState;

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const OTHER: &str = "9999999999999999999999999999999999999999";

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "harryaskham".to_owned(),
            name: "cacophony".to_owned(),
        }
    }

    fn branch(name: &str, oid: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.to_owned(),
            oid: CommitOid(oid.to_owned()),
        }
    }

    fn pr(state: PullRequestState, labels: &[&str], base: &str) -> PullRequestSnapshot {
        PullRequestSnapshot {
            number: PrNumber(4575),
            title: "PR".to_owned(),
            url: "https://example.invalid/4575".to_owned(),
            state,
            draft: false,
            head: branch("agent/x", HEAD),
            base: branch(base, &"2".repeat(40)),
            cross_repository: false,
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            auto_merge: AutoMergeState {
                enabled: false,
                merge_method: None,
                actor: None,
            },
            checks: Vec::new(),
            created_at: None,
            merged_at: None,
            updated_at: None,
            merge_state_status: None,
        }
    }

    fn facts(candidate: PullRequestSnapshot, caravan: Option<(u64, u32)>) -> MembershipProofFacts {
        MembershipProofFacts {
            repository: repository(),
            candidate,
            caravan: caravan.map(|(id, position)| (PrNumber(id), position)),
            default_branch: branch("main", &"2".repeat(40)),
            member_label: "caravan".to_owned(),
            provider_api: GitHubApiTelemetry::default(),
        }
    }

    fn expecting(head: &str, base: Option<&str>) -> MembershipProofInput {
        MembershipProofInput {
            pr: 4575,
            expected_head: Some(head.to_owned()),
            expected_base_ref: base.map(str::to_owned),
        }
    }

    #[test]
    fn enrolled_exact_generation_succeeds_without_any_receipt() {
        let verdict = derive(
            facts(
                pr(PullRequestState::Open, &["caravan"], "tail"),
                Some((4565, 2)),
            ),
            &expecting(HEAD, Some("tail")),
        );
        assert_eq!(verdict.outcome, MembershipProofOutcome::Enrolled);
        assert!(verdict.succeeded);
        assert_eq!(verdict.caravan_id, Some(PrNumber(4565)));
        assert_eq!(verdict.position, Some(2));
        assert!(verdict.mismatches.is_empty());
        let wire = serde_json::to_value(&verdict).unwrap();
        assert_eq!(wire["outcome"], "enrolled");
        assert_eq!(wire["outcome_code"], "enrolled");
        assert_eq!(wire["schema_version"], 1);
    }

    #[test]
    fn merged_exact_generation_succeeds_whatever_its_final_base() {
        let verdict = derive(
            facts(pr(PullRequestState::Merged, &["caravan"], "main"), None),
            &expecting(HEAD, Some("tail")),
        );
        assert_eq!(verdict.outcome, MembershipProofOutcome::Merged);
        assert!(verdict.succeeded);
    }

    #[test]
    fn a_different_head_is_a_different_generation_and_never_succeeds() {
        for state in [PullRequestState::Open, PullRequestState::Merged] {
            let verdict = derive(
                facts(pr(state, &["caravan"], "tail"), Some((4565, 2))),
                &expecting(OTHER, None),
            );
            assert!(!verdict.succeeded, "{state:?}");
            assert_eq!(verdict.mismatches[0].field, "head_oid");
            assert_eq!(verdict.mismatches[0].expected, OTHER);
            assert_eq!(verdict.mismatches[0].observed, HEAD);
        }
    }

    #[test]
    fn force_intent_fork_and_base_drift_are_refused() {
        let forced = derive(
            facts(
                pr(PullRequestState::Open, &["caravan", FORCE_LABEL], "tail"),
                Some((4565, 2)),
            ),
            &expecting(HEAD, None),
        );
        assert_eq!(forced.outcome, MembershipProofOutcome::Enrolled);
        assert!(forced.force_intent_present);
        assert!(!forced.succeeded);
        assert_eq!(forced.mismatches[0].field, "force_intent");

        let mut fork = pr(PullRequestState::Open, &["caravan"], "tail");
        fork.cross_repository = true;
        let fork = derive(facts(fork, Some((4565, 2))), &expecting(HEAD, None));
        assert!(!fork.succeeded);
        assert_eq!(fork.mismatches[0].field, "head_repository");

        let drift = derive(
            facts(
                pr(PullRequestState::Open, &["caravan"], "main"),
                Some((4565, 1)),
            ),
            &expecting(HEAD, Some("tail")),
        );
        assert!(!drift.succeeded);
        assert_eq!(drift.mismatches[0].field, "base_ref");
    }

    #[test]
    fn unjoined_closed_and_inconsistent_states_are_distinct_and_unsuccessful() {
        let unjoined = derive(
            facts(pr(PullRequestState::Open, &[], "main"), None),
            &expecting(HEAD, None),
        );
        assert_eq!(unjoined.outcome, MembershipProofOutcome::Unjoined);
        assert!(!unjoined.succeeded);

        let closed = derive(
            facts(pr(PullRequestState::Closed, &["caravan"], "main"), None),
            &expecting(HEAD, None),
        );
        assert_eq!(closed.outcome, MembershipProofOutcome::ClosedUnmerged);
        assert!(!closed.succeeded);

        let labelled_orphan = derive(
            facts(pr(PullRequestState::Open, &["caravan"], "gone"), None),
            &expecting(HEAD, None),
        );
        assert_eq!(
            labelled_orphan.outcome,
            MembershipProofOutcome::Inconsistent
        );
        assert!(!labelled_orphan.succeeded);

        let mut draft = pr(PullRequestState::Open, &["caravan"], "tail");
        draft.draft = true;
        let draft = derive(facts(draft, Some((4565, 2))), &expecting(HEAD, None));
        assert_eq!(draft.outcome, MembershipProofOutcome::Inconsistent);
        assert!(!draft.succeeded);
    }

    #[test]
    fn malformed_expected_head_is_refused_before_any_provider_read() {
        let error = prove(
            &AppContext::default(),
            &MembershipProofInput {
                pr: 1,
                expected_head: Some("HEAD".to_owned()),
                expected_base_ref: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "membership_proof_invalid_input"
        );
    }
}
