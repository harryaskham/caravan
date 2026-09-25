//! Caller-reviewed admission lease. This is evidence to enforce, not authority
//! to skip normal admission policy, writer fencing, or required checks.

use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppError;
use crate::membership::MembershipProvider;
use crate::model::{
    CommitOid, PrNumber, PullRequestPrecondition, PullRequestSnapshot, PullRequestState,
    RepositoryId,
};
use crate::read::StatusOutput;

/// Version-one Cacophony caller ABI. Keep the original strings, including OID
/// case, for exact JSON value equality in both check and mutation receipts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExpectedAdmission {
    pub schema_version: u32,
    pub repository: String,
    pub pr: u64,
    pub pr_url: String,
    pub head_ref: String,
    pub head_oid: String,
    pub base_ref: String,
    pub base_oid: String,
    pub default_ref: String,
    pub default_oid: String,
    pub unjoined: bool,
}

impl FromStr for ExpectedAdmission {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let binding: Self = serde_json::from_str(value).map_err(|error| error.to_string())?;
        binding
            .validate(Some(binding.pr))
            .map_err(|error| error.to_string())?;
        Ok(binding)
    }
}

impl ExpectedAdmission {
    pub(crate) fn validate(&self, selected: Option<u64>) -> Result<(), AppError> {
        let text = |s: &str| {
            !s.is_empty() && s.len() <= 2048 && s.trim() == s && !s.chars().any(char::is_control)
        };
        if self.schema_version != 1
            || !self.unjoined
            || self.pr == 0
            || selected != Some(self.pr)
            || ![
                &self.repository,
                &self.pr_url,
                &self.head_ref,
                &self.base_ref,
                &self.default_ref,
            ]
            .into_iter()
            .all(|s| text(s))
            || ![&self.head_oid, &self.base_oid, &self.default_oid]
                .into_iter()
                .all(|s| s.len() == 40 && s.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(refusal(
                "expected_admission_invalid",
                "expected-admission requires schema 1, an exact --pr, bounded identities, full OIDs and unjoined:true",
            ));
        }
        Ok(())
    }

    /// Bind discovery, or only the exact source transition proven by this
    /// operation's physical rewrite receipt. No caller can supply that receipt.
    pub(crate) fn verify_snapshot(
        &self,
        status: &StatusOutput,
        rewrite: Option<&crate::physical_rebase::RebaseReceipt>,
    ) -> Result<(), AppError> {
        self.validate(status.current_pr.map(|pr| pr.0))?;
        let candidate = status
            .analysis
            .pull_requests
            .get(&PrNumber(self.pr))
            .ok_or_else(|| refusal("expected_admission_drift", "reviewed candidate is absent"))?;
        let default = &status.analysis.fleet.default_branch;
        let expected_head = if let Some(receipt) = rewrite {
            if receipt.pr.0 != self.pr
                || receipt.branch != self.head_ref
                || !receipt.old_head_oid.0.eq_ignore_ascii_case(&self.head_oid)
            {
                return Err(refusal(
                    "expected_admission_drift",
                    "rewrite does not preserve the reviewed source identity",
                ));
            }
            &receipt.new_head_oid.0
        } else {
            &self.head_oid
        };
        if status.repository.slug() != self.repository
            || default.repository != status.repository
            || default.name != self.default_ref
            || status.default_branch != self.default_ref
            || !default.oid.0.eq_ignore_ascii_case(&self.default_oid)
            || candidate.number.0 != self.pr
            || candidate.url != self.pr_url
            || candidate.head.repository != status.repository
            || candidate.base.repository != status.repository
            || candidate.head.name != self.head_ref
            || !candidate.head.oid.0.eq_ignore_ascii_case(expected_head)
            || candidate.base.name != self.base_ref
            || !candidate.base.oid.0.eq_ignore_ascii_case(&self.base_oid)
            || candidate.state != PullRequestState::Open
            || candidate.merged_at.is_some()
            || candidate.draft
            || candidate.cross_repository
            || candidate.has_label("caravan")
            || candidate.has_label("caravan-evicted")
            || candidate.has_label("caravan-force")
            || status.analysis.fleet.containing(candidate.number).is_some()
            || status.stack_backend.provider_stacks_truncated
            || status.stack_backend.native_stacks.iter().any(|native| {
                native.stack.open
                    && native
                        .stack
                        .pull_requests
                        .iter()
                        .any(|pr| pr.number == self.pr)
            })
        {
            return Err(refusal(
                "expected_admission_drift",
                "repository, PR, source, base, default, or unjoined/no-force facts differ from the reviewed admission",
            ));
        }
        Ok(())
    }

    /// Re-read provider facts immediately before an effect. `current` is either
    /// the verified initial snapshot or the exact after-state of our own prior
    /// effect. It is never an unconstrained rediscovery/retry of a new source.
    pub(crate) fn verify_provider(
        &self,
        provider: &impl MembershipProvider,
        repository: &RepositoryId,
        current: &PullRequestSnapshot,
        native: bool,
    ) -> Result<(), AppError> {
        let unavailable = |_| {
            refusal(
                "expected_admission_unavailable",
                "fresh caller admission lease could not be proved; no unconstrained fallback is permitted",
            )
        };
        let identity = provider
            .admission_repository_identity()
            .map_err(unavailable)?;
        if identity.as_ref() != Some(&(repository.clone(), self.default_ref.clone()))
            || repository.slug() != self.repository
        {
            return Err(refusal(
                "expected_admission_drift",
                "repository/default identity changed or is unavailable",
            ));
        }
        // Preserve the original base/default lease even after our own retarget.
        // Normal provider mutation preconditions separately fence the evolving PR.
        for (branch, oid) in [
            (
                self.default_ref.as_str(),
                CommitOid(self.default_oid.to_ascii_lowercase()),
            ),
            (
                self.base_ref.as_str(),
                CommitOid(self.base_oid.to_ascii_lowercase()),
            ),
            (current.head.name.as_str(), current.head.oid.clone()),
            (current.base.name.as_str(), current.base.oid.clone()),
        ] {
            provider
                .verify_branch_head(repository, branch, &oid)
                .map_err(|_| {
                    refusal(
                        "expected_admission_drift",
                        "reviewed source/base/default ref moved or could not be verified",
                    )
                })?;
        }
        let observed = provider
            .refetch_pull_request(repository, PrNumber(self.pr))
            .map_err(unavailable)?;
        if observed.url != self.pr_url
            || observed.head.repository != *repository
            || observed.base.repository != *repository
            || observed.head.name != self.head_ref
            || observed.has_label("caravan-evicted")
            || observed.draft
            || observed.cross_repository
            || observed.state != PullRequestState::Open
            || observed.merged_at.is_some()
            || observed.has_label("caravan-force")
            || !PullRequestPrecondition::from(&observed)
                .mutation_identity_eq(&PullRequestPrecondition::from(current))
        {
            return Err(refusal(
                "expected_admission_drift",
                "candidate changed outside this admission transaction",
            ));
        }
        if native
            && !provider
                .admission_native_unjoined(repository, observed.number)
                .map_err(unavailable)?
        {
            return Err(refusal(
                "expected_admission_drift",
                "candidate native membership is joined or unproved",
            ));
        }
        Ok(())
    }
}

pub(crate) fn refusal(code: &str, message: &str) -> AppError {
    AppError::structured(
        mcp_cli::ErrorCategory::Validation,
        code,
        message,
        Some(
            json!({"mutated": false, "provider_mutation": "none", "safe_next_action": "obtain a fresh reviewed admission; do not remove the guard or replay an uncertain operation"}),
        ),
    )
}
