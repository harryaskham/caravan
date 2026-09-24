//! Opt-in, single-owner source continuation. This never performs queue recovery.

use super::*;
use crate::model::AutoMergeState;

/// Exact provider incarnation and mutation-sensitive generation, excluding CI progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NonForceGeneration {
    pub pr: PrNumber,
    pub head: BranchSnapshot,
    pub base: BranchSnapshot,
    pub created_at: String,
    pub updated_at: String,
    pub labels: BTreeSet<String>,
    pub auto_merge: AutoMergeState,
}

impl NonForceGeneration {
    pub(super) fn capture(pull: &PullRequestSnapshot) -> Result<Self, AppError> {
        if pull.state != crate::model::PullRequestState::Open
            || pull.draft
            || pull.cross_repository
            || pull.merged_at.is_some()
        {
            return Err(refusal(
                "repair_non_force_generation_unsupported",
                "non-force continuation requires an open, non-draft same-repository generation",
            ));
        }
        let stamp = |value: &Option<String>| {
            value
                .as_ref()
                .filter(|value| value.len() <= 64 && crate::required_runs::rfc3339_to_unix_secs(value).is_some())
                .cloned()
                .ok_or_else(|| {
                    refusal(
                        "repair_non_force_generation_unknown",
                        "provider creation/update evidence is missing; no source publication is authorized",
                    )
                })
        };
        let created_at = stamp(&pull.created_at)?;
        let updated_at = stamp(&pull.updated_at)?;
        if crate::required_runs::rfc3339_to_unix_secs(&created_at)
            > crate::required_runs::rfc3339_to_unix_secs(&updated_at)
        {
            return Err(refusal(
                "repair_non_force_generation_unknown",
                "provider update generation predates PR creation",
            ));
        }
        Ok(Self {
            pr: pull.number,
            head: pull.head.clone(),
            base: pull.base.clone(),
            created_at,
            updated_at,
            labels: pull.labels.clone(),
            auto_merge: pull.auto_merge.clone(),
        })
    }

    pub(super) fn verify(&self, pull: &PullRequestSnapshot) -> Result<(), AppError> {
        if Self::capture(pull)? != *self {
            return Err(refusal(
                "repair_non_force_generation_changed",
                "PR incarnation, state, source/base, or control metadata changed; preserve the workspace and rediscover",
            ));
        }
        Ok(())
    }
}

/// Durable custody and publication policy. Actor binding is audit/custody, not
/// a transfer of another source owner's authority or permission to bypass a hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NonForceRepair {
    pub actor: String,
    pub reason: String,
    pub default_ref: String,
    pub source: NonForceGeneration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<NonForceGeneration>,
    /// Written before the first push; missing evidence is never a false default.
    pub publication_attempted: bool,
}

impl NonForceRepair {
    pub(super) fn new(
        input: &RepairStartInput,
        repository: &RepositoryId,
        candidate: &PullRequestSnapshot,
        target: Option<&PullRequestSnapshot>,
        default_ref: &str,
    ) -> Result<Option<Self>, AppError> {
        if !input.non_force {
            if input.actor.is_some() || input.reason.is_some() {
                return Err(refusal(
                    "repair_non_force_input_invalid",
                    "start actor/reason must accompany --non-force",
                ));
            }
            return Ok(None);
        }
        let text = |value: &Option<String>, limit| {
            value
                .as_ref()
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= limit
                        && value.trim() == value.as_str()
                        && !value.chars().any(char::is_control)
                })
                .cloned()
                .ok_or_else(|| {
                    refusal(
                        "repair_non_force_input_invalid",
                        "--non-force requires bounded, trimmed --actor and --reason",
                    )
                })
        };
        if default_ref.is_empty()
            || candidate.number.0 != input.pr
            || target.map(|pull| pull.number.0) != input.target_pr
            || candidate.head.name == default_ref
            || target.is_some_and(|target| {
                target.number == candidate.number
                    || target.head.name == candidate.head.name
                    || target.head.repository != *repository
                    || target.base.repository != *repository
            })
        {
            return Err(refusal(
                "repair_non_force_scope_invalid",
                "non-force repair cannot publish the default ref or merge a foreign/self target",
            ));
        }
        require_owned_repair(repository, candidate, &candidate.base)?;
        Ok(Some(Self {
            actor: text(&input.actor, MAX_GRANT_ACTOR_BYTES)?,
            reason: text(&input.reason, MAX_GRANT_REASON_BYTES)?,
            default_ref: default_ref.to_owned(),
            source: NonForceGeneration::capture(candidate)?,
            target: target.map(NonForceGeneration::capture).transpose()?,
            publication_attempted: false,
        }))
    }

    pub(super) fn matches_session(&self, repair: &RepairSession) -> bool {
        self.source.pr == repair.pr
            && self.source.head == repair.head
            && self.source.base == repair.old_base
            && self.source.head.repository == repair.repository
            && self.source.base.repository == repair.repository
            && self.source.head.name != self.default_ref
            && !self.default_ref.is_empty()
            && !self.actor.trim().is_empty()
            && !self.reason.trim().is_empty()
            && self.target.as_ref().map(|target| target.pr) == repair.target_pr
            && self.target.as_ref().map_or_else(
                || {
                    repair.target.name == self.default_ref
                        && repair.target.repository == repair.repository
                },
                |target| {
                    target.head == repair.target
                        && target.head.repository == repair.repository
                        && target.base.repository == repair.repository
                        && target.pr != repair.pr
                },
            )
            && (!self.publication_attempted
                || matches!(
                    repair.state,
                    RepairState::Committed | RepairState::Published
                ))
            && (repair.state != RepairState::Published || self.publication_attempted)
    }

    pub(super) fn require_continue(&self, input: &RepairContinueInput) -> Result<(), AppError> {
        if input.actor.as_deref() != Some(self.actor.as_str()) || !input.no_sync {
            return Err(refusal(
                "repair_non_force_custody_required",
                "continue this non-force session with the exact --actor and --no-sync; sync may perform force-with-lease native recovery",
            ));
        }
        Ok(())
    }
}

pub(super) fn refusal(code: &str, message: &str) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        code,
        message,
        Some(json!({"provider_mutated_by_this_refusal": false, "workspace_preserved": true})),
    )
}

struct BorrowedRunner<'a>(&'a dyn CommandRunner);
impl CommandRunner for BorrowedRunner<'_> {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
        self.0.run(command)
    }
}

pub(super) fn verify_provider(
    repair: &RepairSession,
    runner: &dyn CommandRunner,
) -> Result<(), AppError> {
    let Some(policy) = &repair.non_force else {
        return Ok(());
    };
    let default = runner
        .run(&CommandSpec::new("gh").args([
            "repo",
            "view",
            &format!("{}/{}", repair.repository.owner, repair.repository.name),
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ]))
        .map_err(|_| {
            refusal(
                "repair_non_force_generation_unavailable",
                "could not revalidate the repository default ref",
            )
        })?;
    if !default.is_success()
        || default.stdout.trim() != policy.default_ref
        || default.stdout.trim() == repair.head.name
    {
        return Err(refusal(
            "repair_non_force_default_changed",
            "repository default ref changed or is the source branch; do not publish",
        ));
    }
    let provider = crate::github::GitHubMutationAdapter::new(BorrowedRunner(runner));
    for expected in std::iter::once(&policy.source).chain(policy.target.iter()) {
        let observed = provider
            .refetch_pull_request(&repair.repository, expected.pr)
            .map_err(|_| {
                refusal(
                    "repair_non_force_generation_unavailable",
                    "fresh provider generation could not be proved; preserve the session without publishing",
                )
            })?;
        expected.verify(&observed)?;
    }
    Ok(())
}

pub(super) fn verify_workspace(
    repair: &RepairSession,
    runner: &impl CommandRunner,
    head: &CommitOid,
) -> Result<(), AppError> {
    if repair.non_force.is_none() {
        return Ok(());
    }
    let changes = require_success(
        runner,
        CommandSpec::new("git").args(["status", "--porcelain", "--untracked-files=no"]),
        "repair_non_force_workspace_unavailable",
        "could not verify the validated source workspace",
    )?;
    if rev_parse(runner, "HEAD")? != *head || !changes.stdout.trim().is_empty() {
        return Err(refusal(
            "repair_non_force_workspace_changed",
            "validation changed the prepared source; preserve it rather than publishing unvalidated bytes",
        ));
    }
    require_exact_parents(
        repair,
        head,
        &commit_parents(runner, head)?,
        &[repair.head.oid.clone(), repair.target.oid.clone()],
    )
}

pub(super) fn publication_command(repair: &RepairSession, head: &CommitOid) -> CommandSpec {
    let mut args = vec![
        "push".to_owned(),
        "--no-follow-tags".to_owned(),
        "--recurse-submodules=no".to_owned(),
    ];
    if repair.non_force.is_some() {
        args.push("--no-force".to_owned());
    } else {
        args.push(format!(
            "--force-with-lease=refs/heads/{}:{}",
            repair.head.name, repair.head.oid
        ));
    }
    args.push(repair.provider_git_url.clone());
    // Publish the verified object, never a HEAD a validation command could move.
    args.push(format!("{}:refs/heads/{}", head.0, repair.head.name));
    CommandSpec::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .git_write()
}
