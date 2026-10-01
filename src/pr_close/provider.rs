//! Bounded live observations and the only provider write used by this operation.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::command::ProcessRunner;
use crate::generation::CommitRelation;
use crate::github::GitHubMutationAdapter;
use crate::model::{BranchSnapshot, CommitOid, PrNumber, PullRequestState, RepositoryId};
use crate::writer_guard::{WriterCommandRunner, WriterOperationGuard};
use crate::{AppContext, AppError};

use super::transaction::CloseProvider;
use super::{CloseFacts, ClosePull, MainRepresentation, PrCloseInput};

pub(super) struct LiveProvider<'a> {
    context: &'a AppContext,
    guard: &'a WriterOperationGuard,
    adapter: GitHubMutationAdapter<WriterCommandRunner>,
}

impl<'a> LiveProvider<'a> {
    pub(super) fn new(
        context: &'a AppContext,
        guard: &'a WriterOperationGuard,
        runner: ProcessRunner,
    ) -> Self {
        Self {
            context,
            guard,
            adapter: GitHubMutationAdapter::new(guard.runner(runner))
                .with_configured_repository(context.config.repository.clone()),
        }
    }

    fn native_memberships(&self, repository: &RepositoryId, pr: u64) -> Result<Vec<u64>, AppError> {
        let inventory = self
            .adapter
            .native_stack_inventory(repository)
            .map_err(|_| unavailable("native_inventory"))?;
        let mut seen = BTreeSet::new();
        if inventory.truncated
            || inventory.stacks.iter().any(|stack| {
                stack.number == 0
                    || stack.id == 0
                    || stack.node_id.is_empty()
                    || (stack.open && stack.pull_requests.is_empty())
                    || !seen.insert(stack.number)
            })
        {
            return Err(unavailable("native_inventory_incomplete"));
        }
        let mut memberships = inventory
            .stacks
            .iter()
            .filter(|stack| {
                stack.open && stack.pull_requests.iter().any(|member| member.number == pr)
            })
            .map(|stack| stack.number)
            .collect::<Vec<_>>();
        memberships.sort_unstable();
        Ok(memberships)
    }

    fn config_unchanged(&self) -> Result<(), AppError> {
        let path = self.context.repository_path.join(&self.context.config_path);
        let config = if self.context.config_existed {
            crate::config::CaravanConfig::load(&path).map_err(|_| unavailable("config"))?
        } else if path.exists() {
            return Err(AppError::validation(
                "pr_close_policy_changed",
                "configuration appeared during the operation",
            ));
        } else {
            crate::config::CaravanConfig::default()
        };
        if config != self.context.config {
            return Err(AppError::validation(
                "pr_close_policy_changed",
                "configuration changed during the operation",
            ));
        }
        Ok(())
    }
}

impl CloseProvider for LiveProvider<'_> {
    fn observe(&self, input: &PrCloseInput) -> Result<CloseFacts, AppError> {
        self.config_unchanged()?;
        let (repository, default_name) = self
            .adapter
            .repository_identity()
            .map_err(|_| unavailable("repository_identity"))?;
        if repository.slug() != input.repository || default_name != input.main_ref {
            return Err(AppError::validation(
                "pr_close_repository_changed",
                "repository or default-branch identity differs from the request",
            ));
        }
        let pull = self
            .adapter
            .refetch_close_subject(&repository, PrNumber(input.pr))
            .map_err(|_| unavailable("pull_request"))?;
        if pull.state == PullRequestState::Open {
            self.adapter
                .verify_branch_head(&repository, &pull.head.name, &pull.head.oid)
                .map_err(|_| unavailable("source_ref"))?;
        }
        let default_oid = self
            .adapter
            .branch_head_oid(&repository, &default_name)
            .map_err(|_| unavailable("default_ref"))?;
        let source = self
            .adapter
            .close_commit_identity(&repository, &pull.head.oid)
            .map_err(|_| unavailable("source_commit"))?;
        let represented_at = self
            .adapter
            .close_commit_identity(&repository, &CommitOid(input.represented_at.clone()))
            .map_err(|_| unavailable("representation_commit"))?;
        let contained = |base: &CommitOid, head: &CommitOid| -> Result<bool, AppError> {
            let relation = self
                .adapter
                .compare_commits(&repository, base, head)
                .map_err(|_| unavailable("commit_relation"))?;
            match relation {
                CommitRelation::Ahead | CommitRelation::Identical => Ok(true),
                CommitRelation::Behind | CommitRelation::Diverged => Ok(false),
                CommitRelation::Unknown { .. } => Err(unavailable("commit_relation")),
            }
        };
        let represented_on_main = contained(&represented_at.oid, &default_oid)?;
        let source_in_representation = match input.representation {
            MainRepresentation::Ancestor => contained(&source.oid, &represented_at.oid)?,
            MainRepresentation::SameTree => source.tree_oid == represented_at.tree_oid,
        };
        let native_stacks = self.native_memberships(&repository, input.pr)?;
        let permission = self
            .adapter
            .repository_permission(&repository)
            .map_err(|_| unavailable("repository_permission"))?;
        if !matches!(permission.as_str(), "ADMIN" | "MAINTAIN" | "WRITE") {
            return Err(AppError::validation(
                "pr_close_permission_required",
                "repository write permission is required",
            ));
        }
        Ok(CloseFacts {
            default_branch: BranchSnapshot {
                repository: repository.clone(),
                name: default_name,
                oid: default_oid,
            },
            repository,
            pull: ClosePull {
                number: pull.number,
                head: pull.head,
                base: pull.base,
                state: pull.state,
                merged_at: pull.merged_at,
                draft: pull.draft,
                cross_repository: pull.cross_repository,
                auto_merge: pull.auto_merge.enabled,
                labels: pull.labels,
            },
            source,
            represented_at,
            represented_on_main,
            source_in_representation,
            native_stacks,
            permission,
        })
    }

    fn revalidate_fence(&self) -> Result<String, AppError> {
        self.config_unchanged()?;
        if let Some(fence) = self.guard.remote_fence() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| unavailable("clock"))?
                .as_millis();
            let now = u64::try_from(now).map_err(|_| unavailable("clock"))?;
            fence.before_write_at(now).map_err(|_| {
                AppError::validation(
                    "pr_close_fence_lost",
                    "remote writer authority could not be revalidated",
                )
            })?;
        }
        Ok(super::fingerprint(&serde_json::json!({
            "local_owner": self.guard.owner(),
            "remote_grant": self.guard.remote_grant(),
        })))
    }

    fn close_once(&self, facts: &CloseFacts) -> Result<(), AppError> {
        // The runner independently fences the marked write, including loss after
        // our explicit check. No label, branch, comment or merge write is exposed.
        self.adapter
            .close_nonmember_request(&facts.repository, facts.pull.number)
            .map_err(|error| {
                if matches!(
                    error,
                    crate::github::MutationError::Provider(crate::github::DiscoveryError::Runner(
                        crate::command::CommandRunError::MutationFenceRefused { .. }
                    ))
                ) {
                    AppError::validation(
                        "pr_close_fence_lost",
                        "writer fence refused the provider close",
                    )
                } else {
                    unavailable("provider_close")
                }
            })
    }
}

fn unavailable(phase: &str) -> AppError {
    // Provider stderr/body/credential-helper output never enters the journal.
    AppError::structured(
        mcp_cli::ErrorCategory::ExecutionFailure,
        format!("pr_close_{phase}_unavailable"),
        "exact close evidence is unavailable; inspect the bounded operation phase before retrying",
        Some(serde_json::json!({"phase": phase})),
    )
}
