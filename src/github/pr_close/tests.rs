use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;
use crate::command::{
    CommandIntent, CommandMutationFence, CommandOutput, CommandRunError, FencedCommandRunner,
};

#[derive(Clone)]
struct Runner {
    calls: Arc<Mutex<Vec<CommandSpec>>>,
    output: String,
}
impl CommandRunner for Runner {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
        self.calls.lock().unwrap().push(command.clone());
        Ok(CommandOutput::success(self.output.clone()))
    }
}
fn repository() -> RepositoryId {
    RepositoryId {
        owner: "acme".to_owned(),
        name: "widgets".to_owned(),
    }
}
fn runner(output: String) -> Runner {
    Runner {
        calls: Arc::default(),
        output,
    }
}
fn pull() -> serde_json::Value {
    json!({
        "number":8,"title":"fixture","state":"OPEN","isDraft":false,
        "headRefName":"feature","headRefOid":"a".repeat(40),
        "headRepository":{"name":"widgets","nameWithOwner":"acme/widgets"},
        "headRepositoryOwner":{"login":"acme"},"isCrossRepository":false,
        "baseRefName":"main","baseRefOid":"b".repeat(40),"labels":[],
        "autoMergeRequest":null,"createdAt":"2026-01-01T00:00:00Z","mergedAt":null,
        "url":"https://github.com/acme/widgets/pull/8","updatedAt":"2026-01-01T00:00:00Z",
        "labelsTruncated":false
    })
}

#[test]
fn close_commit_identity_requires_explicit_matching_provider_sha() {
    let expected = "a".repeat(40);
    for sha in [
        None,
        Some(String::new()),
        Some("f".repeat(40)),
        Some(expected.clone()),
    ] {
        let mut data = json!({"commit":{"tree":{"sha":"e".repeat(40)},"committer":{"date":"2026-01-01T00:00:00Z"}},"parents":[]});
        if let Some(value) = &sha {
            data["sha"] = json!(value);
        }
        let valid = sha.as_ref() == Some(&expected);
        let adapter = GitHubMutationAdapter::new(runner(data.to_string()));
        assert_eq!(
            adapter
                .close_commit_identity(&repository(), &CommitOid(expected.clone()))
                .is_ok(),
            valid
        );
    }
}

#[test]
fn close_adapter_marks_only_the_exact_state_patch_as_a_write() {
    let runner = runner("{}".to_owned());
    GitHubMutationAdapter::new(runner.clone())
        .close_nonmember_request(&repository(), PrNumber(8))
        .unwrap();
    let calls = runner.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].intent(), CommandIntent::ProviderWrite);
    assert_eq!(
        calls[0].args,
        [
            "api",
            "repos/acme/widgets/pulls/8",
            "--method",
            "PATCH",
            "--raw-field",
            "state=closed"
        ]
    );
}

struct LostFence;
impl CommandMutationFence for LostFence {
    fn before_write(&self, _: CommandIntent) -> Result<(), String> {
        Err("fixture lease lost".to_owned())
    }
}

#[test]
fn close_adapter_cannot_execute_past_a_lost_fence() {
    let runner = runner("{}".to_owned());
    let adapter = GitHubMutationAdapter::new(FencedCommandRunner::new(runner.clone(), LostFence));
    assert!(matches!(
        adapter.close_nonmember_request(&repository(), PrNumber(8)),
        Err(MutationError::Provider(
            crate::github::DiscoveryError::Runner(CommandRunError::MutationFenceRefused { .. })
        ))
    ));
    assert!(runner.calls.lock().unwrap().is_empty());
}

#[test]
fn close_subject_requires_explicit_complete_label_evidence_without_check_rollups() {
    let runner = runner(pull().to_string());
    let adapter = GitHubMutationAdapter::new(runner.clone());
    assert_eq!(
        adapter
            .refetch_close_subject(&repository(), PrNumber(8))
            .unwrap()
            .number,
        PrNumber(8)
    );
    let calls = runner.calls.lock().unwrap();
    assert_eq!(calls[0].intent(), CommandIntent::Read);
    let query = calls[0]
        .args
        .iter()
        .find(|argument| argument.starts_with("query="))
        .unwrap();
    assert!(query.contains("hasNextPage"));
    assert!(!query.contains("statusCheckRollup"));
    drop(calls);
    for value in [serde_json::Value::Bool(true), serde_json::Value::Null] {
        let mut data = pull();
        data["labelsTruncated"] = value;
        assert!(
            GitHubMutationAdapter::new(super::tests::runner(data.to_string()))
                .refetch_close_subject(&repository(), PrNumber(8))
                .is_err()
        );
    }
    let mut missing = pull();
    missing.as_object_mut().unwrap().remove("labelsTruncated");
    assert!(
        GitHubMutationAdapter::new(super::tests::runner(missing.to_string()))
            .refetch_close_subject(&repository(), PrNumber(8))
            .is_err()
    );
}
