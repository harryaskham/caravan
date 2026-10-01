//! Crate-private close primitives; the public entry point is the fenced transaction.

#[cfg(test)]
mod tests;

use super::{
    CommitDetailJson, GitHubMutationAdapter, MutationError, PullRequestJson, commit_command,
};
use crate::command::{CommandRunner, CommandSpec};
use crate::model::{CommitOid, GitCommitIdentity, PrNumber, PullRequestSnapshot, RepositoryId};

const CLOSE_SUBJECT_QUERY: &str = r"query($owner:String!,$name:String!,$pr:Int!){repository(owner:$owner,name:$name){pullRequest(number:$pr){number title state isDraft headRefName headRefOid headRepository{name nameWithOwner} headRepositoryOwner{login} isCrossRepository baseRefName baseRefOid labels(first:100){nodes{name} pageInfo{hasNextPage}} autoMergeRequest{mergeMethod enabledBy{login}} createdAt mergedAt url updatedAt}}}";
const CLOSE_SUBJECT_JQ: &str = ".data.repository.pullRequest | .labels as $labels | . + {labels:$labels.nodes,labelsTruncated:$labels.pageInfo.hasNextPage}";

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloseSubjectJson {
    labels_truncated: bool,
    #[serde(flatten)]
    pull: PullRequestJson,
}

impl<R: CommandRunner> GitHubMutationAdapter<R> {
    pub(crate) fn refetch_close_subject(
        &self,
        repository: &RepositoryId,
        pr: PrNumber,
    ) -> Result<PullRequestSnapshot, MutationError> {
        let subject: CloseSubjectJson = self.json(CommandSpec::new("gh").args([
            "api",
            "graphql",
            "-f",
            &format!("query={CLOSE_SUBJECT_QUERY}"),
            "-F",
            &format!("owner={}", repository.owner),
            "-F",
            &format!("name={}", repository.name),
            "-F",
            &format!("pr={pr}"),
            "--jq",
            CLOSE_SUBJECT_JQ,
        ]))?;
        if subject.labels_truncated
            || subject.pull.head_repository.is_none()
            || subject.pull.head_repository_owner.is_none()
        {
            return Err(MutationError::MissingProviderResource {
                resource: "complete close-subject labels and source-repository identity".to_owned(),
            });
        }
        subject.pull.into_snapshot(repository).map_err(Into::into)
    }

    /// Closing cannot use the legacy missing-SHA compatibility fallback.
    pub(crate) fn close_commit_identity(
        &self,
        repository: &RepositoryId,
        expected: &CommitOid,
    ) -> Result<GitCommitIdentity, MutationError> {
        let detail: CommitDetailJson = self.json(commit_command(repository, &expected.0))?;
        if detail.sha.is_empty() || detail.sha != expected.0 {
            return Err(MutationError::MissingProviderResource {
                resource: "exact close commit identity".to_owned(),
            });
        }
        let tree = detail
            .commit
            .tree
            .ok_or_else(|| MutationError::MissingProviderResource {
                resource: "close commit tree identity".to_owned(),
            })?;
        Ok(GitCommitIdentity {
            oid: CommitOid(detail.sha),
            tree_oid: CommitOid(tree.sha),
            parents: detail
                .parents
                .into_iter()
                .map(|parent| CommitOid(parent.sha))
                .collect(),
        })
    }

    pub(crate) fn close_nonmember_request(
        &self,
        repository: &RepositoryId,
        pr: PrNumber,
    ) -> Result<(), MutationError> {
        self.checked(
            CommandSpec::new("gh")
                .args([
                    "api",
                    &format!("repos/{repository}/pulls/{pr}"),
                    "--method",
                    "PATCH",
                    "--raw-field",
                    "state=closed",
                ])
                .provider_write(),
        )?;
        Ok(())
    }
}
