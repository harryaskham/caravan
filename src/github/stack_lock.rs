//! Provider-enforced source-ref lock for one native Stack merge group.
//!
//! GitHub's async merge endpoint leases only the selected top SHA. Cara closes
//! the lower-head race by creating one active, no-bypass repository ruleset
//! over every selected source ref, then re-reading the complete Stack while the
//! ruleset is active. The ruleset remains through terminal merge proof and is
//! released only by exact id/generation. This requires Administration(write)
//! and is never reached by the default Caravan backend.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{DiscoveryError, GitHubMutationAdapter, GitHubStackEntryGeneration, MutationError};
use crate::command::{CommandOutput, CommandRunError, CommandRunner, CommandSpec};
use crate::model::{PrNumber, RepositoryId};

const ACCEPT: &str = "Accept: application/vnd.github+json";
const API_VERSION: &str = "X-GitHub-Api-Version: 2026-03-10";
const SCHEMA_VERSION: u32 = 1;
const RULESET_PAGE_SIZE: usize = 100;
const NAME_PREFIX: &str = "cara-stack-merge-lock-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackBranchLockPlan {
    pub operation_id: String,
    pub actor: String,
    pub stack_number: u64,
    pub selected: Vec<GitHubStackEntryGeneration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackBranchLockGeneration {
    pub id: u64,
    pub node_id: String,
    pub name: String,
    pub repository: RepositoryId,
    pub source: String,
    pub source_type: String,
    pub target: String,
    pub enforcement: String,
    pub selected_pull_requests: Vec<PrNumber>,
    pub selected_refs: Vec<String>,
    pub current_user_can_bypass: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitHubStackBranchLockOperation {
    Acquire,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitHubStackBranchLockDisposition {
    Completed,
    RecoveredAfterAmbiguousResponse,
    AlreadySatisfied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitHubStackBranchLockReceipt {
    pub schema_version: u32,
    pub operation_id: String,
    pub actor: String,
    pub repository: RepositoryId,
    pub operation: GitHubStackBranchLockOperation,
    pub disposition: GitHubStackBranchLockDisposition,
    pub request_method: String,
    pub request_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<GitHubStackBranchLockGeneration>,
    pub evidence_hash: String,
}

impl GitHubStackBranchLockReceipt {
    fn seal(mut self) -> Self {
        self.evidence_hash.clear();
        let material = serde_json::to_vec(&self).expect("Stack branch-lock receipt serializes");
        self.evidence_hash = crate::membership::fnv1a64(&material);
        self
    }

    #[must_use]
    pub fn verify(&self) -> bool {
        let expected = self.evidence_hash.clone();
        let mut material = self.clone();
        material.evidence_hash.clear();
        serde_json::to_vec(&material)
            .ok()
            .is_some_and(|bytes| crate::membership::fnv1a64(&bytes) == expected)
    }
}

#[derive(Debug)]
pub enum GitHubStackBranchLockError {
    InvalidPlan {
        code: String,
        message: String,
    },
    Provider(MutationError),
    InventoryTruncated,
    NameCollision {
        name: String,
    },
    StaleLock {
        expected: Box<GitHubStackBranchLockGeneration>,
        actual: Option<Box<GitHubStackBranchLockGeneration>>,
    },
    AmbiguousResponse {
        operation: GitHubStackBranchLockOperation,
        diagnostic: String,
        rediscovery_diagnostic: Option<String>,
    },
    PostconditionFailed {
        operation: GitHubStackBranchLockOperation,
    },
}

impl std::fmt::Display for GitHubStackBranchLockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPlan { code, message } => write!(formatter, "{code}: {message}"),
            Self::Provider(error) => error.fmt(formatter),
            Self::InventoryTruncated => {
                write!(formatter, "repository ruleset inventory is truncated")
            }
            Self::NameCollision { name } => write!(
                formatter,
                "ruleset `{name}` exists with different lock policy"
            ),
            Self::StaleLock { .. } => write!(formatter, "Stack branch-lock generation changed"),
            Self::AmbiguousResponse {
                operation,
                diagnostic,
                ..
            } => write!(
                formatter,
                "Stack branch-lock {operation:?} is ambiguous: {diagnostic}"
            ),
            Self::PostconditionFailed { operation } => write!(
                formatter,
                "Stack branch-lock {operation:?} postcondition failed"
            ),
        }
    }
}

impl std::error::Error for GitHubStackBranchLockError {}

impl From<MutationError> for GitHubStackBranchLockError {
    fn from(error: MutationError) -> Self {
        Self::Provider(error)
    }
}

#[derive(Debug, Deserialize)]
struct RulesetSummary {
    id: u64,
    name: String,
}

#[derive(Debug, Deserialize)]
struct RulesetResponse {
    id: u64,
    node_id: String,
    name: String,
    target: String,
    source_type: String,
    source: String,
    enforcement: String,
    conditions: RulesetConditions,
    rules: Vec<RulesetRule>,
    #[serde(default)]
    bypass_actors: Vec<serde_json::Value>,
    current_user_can_bypass: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
struct RulesetConditions {
    ref_name: RulesetRefCondition,
}

#[derive(Debug, Deserialize)]
struct RulesetRefCondition {
    include: Vec<String>,
    exclude: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RulesetRule {
    #[serde(rename = "type")]
    kind: String,
}

struct LockReceiptInput<'a> {
    plan: &'a GitHubStackBranchLockPlan,
    repository: &'a RepositoryId,
    operation: GitHubStackBranchLockOperation,
    disposition: GitHubStackBranchLockDisposition,
    request_method: &'a str,
    request_path: String,
    github_request_id: Option<String>,
    lock: Option<GitHubStackBranchLockGeneration>,
}

impl<R: CommandRunner> GitHubMutationAdapter<R> {
    /// Acquire one atomic exact-ref ruleset over every selected source branch.
    /// Exact retries are zero-write; ambiguous writes recover only from an exact
    /// active no-bypass ruleset readback.
    pub fn native_stack_branch_lock_acquire(
        &self,
        repository: &RepositoryId,
        plan: &GitHubStackBranchLockPlan,
    ) -> Result<GitHubStackBranchLockReceipt, GitHubStackBranchLockError> {
        let expected = expected_lock(repository, plan)?;
        if let Some(mut existing) = self.find_stack_branch_lock(repository, &expected.name)? {
            if same_lock_policy(&existing, &expected) {
                bind_selected_pull_requests(&mut existing, plan);
                return Ok(lock_receipt(LockReceiptInput {
                    plan,
                    repository,
                    operation: GitHubStackBranchLockOperation::Acquire,
                    disposition: GitHubStackBranchLockDisposition::AlreadySatisfied,
                    request_method: "POST",
                    request_path: rulesets_path(repository),
                    github_request_id: None,
                    lock: Some(existing),
                }));
            }
            return Err(GitHubStackBranchLockError::NameCollision {
                name: expected.name,
            });
        }

        let command = create_ruleset_command(repository, &expected);
        let response = self.runner.run(&command);
        let request_id = response.as_ref().ok().and_then(github_request_id);
        let diagnostic = response_diagnostic(&response);
        let rediscovered = self.find_stack_branch_lock(repository, &expected.name);
        match (response, rediscovered) {
            (Ok(output), Ok(Some(mut actual)))
                if output.is_success() && same_lock_policy(&actual, &expected) =>
            {
                bind_selected_pull_requests(&mut actual, plan);
                Ok(lock_receipt(LockReceiptInput {
                    plan,
                    repository,
                    operation: GitHubStackBranchLockOperation::Acquire,
                    disposition: GitHubStackBranchLockDisposition::Completed,
                    request_method: "POST",
                    request_path: rulesets_path(repository),
                    github_request_id: request_id,
                    lock: Some(actual),
                }))
            }
            (Ok(output), Ok(Some(mut actual)))
                if !output.is_success() && same_lock_policy(&actual, &expected) =>
            {
                bind_selected_pull_requests(&mut actual, plan);
                Ok(lock_receipt(LockReceiptInput {
                    plan,
                    repository,
                    operation: GitHubStackBranchLockOperation::Acquire,
                    disposition: GitHubStackBranchLockDisposition::RecoveredAfterAmbiguousResponse,
                    request_method: "POST",
                    request_path: rulesets_path(repository),
                    github_request_id: request_id,
                    lock: Some(actual),
                }))
            }
            (Err(_), Ok(Some(mut actual))) if same_lock_policy(&actual, &expected) => {
                bind_selected_pull_requests(&mut actual, plan);
                Ok(lock_receipt(LockReceiptInput {
                    plan,
                    repository,
                    operation: GitHubStackBranchLockOperation::Acquire,
                    disposition: GitHubStackBranchLockDisposition::RecoveredAfterAmbiguousResponse,
                    request_method: "POST",
                    request_path: rulesets_path(repository),
                    github_request_id: None,
                    lock: Some(actual),
                }))
            }
            (Ok(output), Ok(_)) if output.is_success() => {
                Err(GitHubStackBranchLockError::PostconditionFailed {
                    operation: GitHubStackBranchLockOperation::Acquire,
                })
            }
            (_, result) => Err(ambiguous(
                GitHubStackBranchLockOperation::Acquire,
                diagnostic,
                result,
            )),
        }
    }

    /// Re-read and prove the exact active lock immediately before async submit.
    pub fn native_stack_branch_lock_verify(
        &self,
        repository: &RepositoryId,
        expected: &GitHubStackBranchLockGeneration,
    ) -> Result<GitHubStackBranchLockGeneration, GitHubStackBranchLockError> {
        let mut actual = self.read_stack_branch_lock(repository, expected.id)?;
        if let Some(actual) = actual.as_mut() {
            actual
                .selected_pull_requests
                .clone_from(&expected.selected_pull_requests);
        }
        if actual.as_ref() == Some(expected) {
            Ok(actual.expect("checked Some"))
        } else {
            Err(GitHubStackBranchLockError::StaleLock {
                expected: Box::new(expected.clone()),
                actual: actual.map(Box::new),
            })
        }
    }

    /// Release only the exact lock generation. A changed ruleset is never
    /// deleted. Exact retries prove absence without another DELETE.
    pub fn native_stack_branch_lock_release(
        &self,
        repository: &RepositoryId,
        plan: &GitHubStackBranchLockPlan,
        expected: &GitHubStackBranchLockGeneration,
    ) -> Result<GitHubStackBranchLockReceipt, GitHubStackBranchLockError> {
        validate_lock_plan(repository, plan)?;
        let mut before = self.read_stack_branch_lock(repository, expected.id)?;
        if let Some(before) = before.as_mut() {
            before
                .selected_pull_requests
                .clone_from(&expected.selected_pull_requests);
        }
        if before.is_none() {
            if let Some(actual) = self.find_stack_branch_lock(repository, &expected.name)? {
                return Err(GitHubStackBranchLockError::StaleLock {
                    expected: Box::new(expected.clone()),
                    actual: Some(Box::new(actual)),
                });
            }
            return Ok(lock_receipt(LockReceiptInput {
                plan,
                repository,
                operation: GitHubStackBranchLockOperation::Release,
                disposition: GitHubStackBranchLockDisposition::AlreadySatisfied,
                request_method: "DELETE",
                request_path: ruleset_path(repository, expected.id),
                github_request_id: None,
                lock: None,
            }));
        }
        if before.as_ref() != Some(expected) {
            return Err(GitHubStackBranchLockError::StaleLock {
                expected: Box::new(expected.clone()),
                actual: before.map(Box::new),
            });
        }
        let command = delete_ruleset_command(repository, expected.id);
        let response = self.runner.run(&command);
        let request_id = response.as_ref().ok().and_then(github_request_id);
        let diagnostic = response_diagnostic(&response);
        let after = match self.read_stack_branch_lock(repository, expected.id) {
            Ok(None) => self.find_stack_branch_lock(repository, &expected.name),
            other => other,
        };
        match (response, after) {
            (Ok(output), Ok(None)) if output.is_success() => Ok(lock_receipt(LockReceiptInput {
                plan,
                repository,
                operation: GitHubStackBranchLockOperation::Release,
                disposition: GitHubStackBranchLockDisposition::Completed,
                request_method: "DELETE",
                request_path: ruleset_path(repository, expected.id),
                github_request_id: request_id,
                lock: None,
            })),
            (Ok(output), Ok(None)) if !output.is_success() => Ok(lock_receipt(LockReceiptInput {
                plan,
                repository,
                operation: GitHubStackBranchLockOperation::Release,
                disposition: GitHubStackBranchLockDisposition::RecoveredAfterAmbiguousResponse,
                request_method: "DELETE",
                request_path: ruleset_path(repository, expected.id),
                github_request_id: request_id,
                lock: None,
            })),
            (Err(_), Ok(None)) => Ok(lock_receipt(LockReceiptInput {
                plan,
                repository,
                operation: GitHubStackBranchLockOperation::Release,
                disposition: GitHubStackBranchLockDisposition::RecoveredAfterAmbiguousResponse,
                request_method: "DELETE",
                request_path: ruleset_path(repository, expected.id),
                github_request_id: None,
                lock: None,
            })),
            (Ok(output), Ok(Some(_))) if output.is_success() => {
                Err(GitHubStackBranchLockError::PostconditionFailed {
                    operation: GitHubStackBranchLockOperation::Release,
                })
            }
            (_, result) => Err(ambiguous(
                GitHubStackBranchLockOperation::Release,
                diagnostic,
                result,
            )),
        }
    }

    fn find_stack_branch_lock(
        &self,
        repository: &RepositoryId,
        name: &str,
    ) -> Result<Option<GitHubStackBranchLockGeneration>, GitHubStackBranchLockError> {
        let summaries: Vec<RulesetSummary> = self.json(list_rulesets_command(repository))?;
        let matches = summaries
            .iter()
            .filter(|ruleset| ruleset.name == name)
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(GitHubStackBranchLockError::NameCollision {
                name: name.to_owned(),
            });
        }
        if let Some(found) = matches.first() {
            return self.read_stack_branch_lock(repository, found.id);
        }
        if summaries.len() == RULESET_PAGE_SIZE {
            return Err(GitHubStackBranchLockError::InventoryTruncated);
        }
        Ok(None)
    }

    fn read_stack_branch_lock(
        &self,
        repository: &RepositoryId,
        id: u64,
    ) -> Result<Option<GitHubStackBranchLockGeneration>, GitHubStackBranchLockError> {
        let command = read_ruleset_command(repository, id);
        let output = self.runner.run(&command).map_err(|error| {
            GitHubStackBranchLockError::Provider(MutationError::Provider(DiscoveryError::Runner(
                error,
            )))
        })?;
        if output.is_success() {
            let response: RulesetResponse =
                serde_json::from_str(&output.stdout).map_err(|error| {
                    GitHubStackBranchLockError::InvalidPlan {
                        code: "github_stack_lock_provider_json_invalid".to_owned(),
                        message: error.to_string(),
                    }
                })?;
            return Ok(Some(observed_lock(repository, response)?));
        }
        if output.stderr.contains("HTTP 404") {
            return Ok(None);
        }
        Err(GitHubStackBranchLockError::Provider(
            MutationError::Provider(DiscoveryError::CommandFailed {
                command,
                code: output.code,
                stderr: output.stderr,
            }),
        ))
    }
}

fn validate_lock_plan(
    repository: &RepositoryId,
    plan: &GitHubStackBranchLockPlan,
) -> Result<(), GitHubStackBranchLockError> {
    if plan.operation_id.trim().is_empty()
        || plan.actor.trim().is_empty()
        || plan.selected.is_empty()
    {
        return Err(invalid(
            "github_stack_lock_identity_missing",
            "branch lock requires operation, actor, and selected entries",
        ));
    }
    let mut refs = BTreeSet::new();
    for entry in &plan.selected {
        if entry.head.repository != *repository
            || entry.head.name.trim().is_empty()
            || !refs.insert(entry.head.name.clone())
        {
            return Err(invalid(
                "github_stack_lock_ref_invalid",
                "selected branches must be unique same-repository refs",
            ));
        }
    }
    Ok(())
}

fn expected_lock(
    repository: &RepositoryId,
    plan: &GitHubStackBranchLockPlan,
) -> Result<GitHubStackBranchLockGeneration, GitHubStackBranchLockError> {
    validate_lock_plan(repository, plan)?;
    // The operation identity deliberately excludes the mutable selected
    // generation. Reusing one operation ID with different refs must collide
    // with its existing ruleset rather than silently creating a second lock.
    let material = serde_json::to_vec(&(repository, &plan.operation_id, plan.stack_number))
        .expect("lock identity serializes");
    let name = format!("{NAME_PREFIX}{}", crate::membership::fnv1a64(&material));
    let mut refs = plan
        .selected
        .iter()
        .map(|entry| format!("refs/heads/{}", entry.head.name))
        .collect::<Vec<_>>();
    refs.sort();
    Ok(GitHubStackBranchLockGeneration {
        id: 0,
        node_id: String::new(),
        name,
        repository: repository.clone(),
        source: repository.slug(),
        source_type: "Repository".to_owned(),
        target: "branch".to_owned(),
        enforcement: "active".to_owned(),
        selected_pull_requests: plan.selected.iter().map(|entry| entry.pr).collect(),
        selected_refs: refs,
        current_user_can_bypass: "never".to_owned(),
        created_at: String::new(),
        updated_at: String::new(),
    })
}

fn observed_lock(
    repository: &RepositoryId,
    response: RulesetResponse,
) -> Result<GitHubStackBranchLockGeneration, GitHubStackBranchLockError> {
    if !response.conditions.ref_name.exclude.is_empty() || !response.bypass_actors.is_empty() {
        return Err(invalid(
            "github_stack_lock_bypass_detected",
            "Stack lock ruleset must have no exclusions or bypass actors",
        ));
    }
    let kinds = response
        .rules
        .iter()
        .map(|rule| rule.kind.as_str())
        .collect::<BTreeSet<_>>();
    if kinds != BTreeSet::from(["deletion", "update"]) {
        return Err(invalid(
            "github_stack_lock_rules_invalid",
            "Stack lock ruleset requires exactly update and deletion rules",
        ));
    }
    let mut refs = response.conditions.ref_name.include;
    refs.sort();
    Ok(GitHubStackBranchLockGeneration {
        id: response.id,
        node_id: response.node_id,
        name: response.name,
        repository: repository.clone(),
        source: response.source,
        source_type: response.source_type,
        target: response.target,
        enforcement: response.enforcement,
        selected_pull_requests: Vec::new(),
        selected_refs: refs,
        current_user_can_bypass: response.current_user_can_bypass,
        created_at: response.created_at,
        updated_at: response.updated_at,
    })
}

fn bind_selected_pull_requests(
    lock: &mut GitHubStackBranchLockGeneration,
    plan: &GitHubStackBranchLockPlan,
) {
    lock.selected_pull_requests = plan.selected.iter().map(|entry| entry.pr).collect();
}

fn same_lock_policy(
    actual: &GitHubStackBranchLockGeneration,
    expected: &GitHubStackBranchLockGeneration,
) -> bool {
    actual.name == expected.name
        && actual.repository == expected.repository
        && actual.source == expected.source
        && actual.source_type == "Repository"
        && actual.target == "branch"
        && actual.enforcement == "active"
        && actual.selected_refs == expected.selected_refs
        && actual.current_user_can_bypass == "never"
}

fn create_ruleset_command(
    repository: &RepositoryId,
    expected: &GitHubStackBranchLockGeneration,
) -> CommandSpec {
    ruleset_api_command("POST", rulesets_path(repository), true).args(["--input", "-"]).stdin(serde_json::json!({
        "name": expected.name, "target": "branch", "enforcement": "active", "bypass_actors": [],
        "conditions": {"ref_name": {"include": expected.selected_refs, "exclude": []}},
        "rules": [{"type": "update", "parameters": {"update_allows_fetch_and_merge": false}}, {"type": "deletion"}]
    }).to_string())
}

fn list_rulesets_command(repository: &RepositoryId) -> CommandSpec {
    ruleset_api_command(
        "GET",
        format!("{}?per_page={RULESET_PAGE_SIZE}", rulesets_path(repository)),
        false,
    )
}
pub(super) fn read_ruleset_command(repository: &RepositoryId, id: u64) -> CommandSpec {
    ruleset_api_command("GET", ruleset_path(repository, id), false)
}
fn delete_ruleset_command(repository: &RepositoryId, id: u64) -> CommandSpec {
    ruleset_api_command("DELETE", ruleset_path(repository, id), true)
}
fn rulesets_path(repository: &RepositoryId) -> String {
    format!("repos/{}/rulesets", repository.slug())
}
fn ruleset_path(repository: &RepositoryId, id: u64) -> String {
    format!("{}/{id}", rulesets_path(repository))
}

fn ruleset_api_command(method: &str, path: String, include: bool) -> CommandSpec {
    let mut command = CommandSpec::new("gh").args([
        "api".to_owned(),
        "--method".to_owned(),
        method.to_owned(),
        "-H".to_owned(),
        ACCEPT.to_owned(),
        "-H".to_owned(),
        API_VERSION.to_owned(),
    ]);
    if include {
        command = command.arg("--include");
    }
    let command = command.arg(path);
    if method == "GET" {
        command
    } else {
        command.provider_write()
    }
}

fn lock_receipt(input: LockReceiptInput<'_>) -> GitHubStackBranchLockReceipt {
    GitHubStackBranchLockReceipt {
        schema_version: SCHEMA_VERSION,
        operation_id: input.plan.operation_id.clone(),
        actor: input.plan.actor.clone(),
        repository: input.repository.clone(),
        operation: input.operation,
        disposition: input.disposition,
        request_method: input.request_method.to_owned(),
        request_path: input.request_path,
        github_request_id: input.github_request_id,
        lock: input.lock,
        evidence_hash: String::new(),
    }
    .seal()
}

fn invalid(code: &str, message: &str) -> GitHubStackBranchLockError {
    GitHubStackBranchLockError::InvalidPlan {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}
fn response_diagnostic(response: &Result<CommandOutput, CommandRunError>) -> String {
    match response {
        Ok(output) => format!(
            "provider exited {:?}: {}",
            output.code,
            output.stderr.trim()
        ),
        Err(error) => error.to_string(),
    }
}
fn github_request_id(output: &CommandOutput) -> Option<String> {
    output
        .stdout
        .lines()
        .chain(output.stderr.lines())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("x-github-request-id")
                .then(|| value.trim().to_owned())
        })
}
fn ambiguous(
    operation: GitHubStackBranchLockOperation,
    diagnostic: String,
    result: Result<Option<GitHubStackBranchLockGeneration>, GitHubStackBranchLockError>,
) -> GitHubStackBranchLockError {
    match result {
        Ok(_) => GitHubStackBranchLockError::AmbiguousResponse {
            operation,
            diagnostic,
            rediscovery_diagnostic: None,
        },
        Err(error) => GitHubStackBranchLockError::AmbiguousResponse {
            operation,
            diagnostic,
            rediscovery_diagnostic: Some(error.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;
    use crate::command::{CommandIntent, CommandRunError};
    use crate::model::{BranchSnapshot, CommitOid, PullRequestState};

    struct FakeRunner {
        calls: RefCell<VecDeque<(CommandSpec, Result<CommandOutput, CommandRunError>)>>,
    }

    impl FakeRunner {
        fn new(calls: Vec<(CommandSpec, CommandOutput)>) -> Self {
            Self {
                calls: RefCell::new(calls.into_iter().map(|(c, o)| (c, Ok(o))).collect()),
            }
        }
        fn with_results(calls: Vec<(CommandSpec, Result<CommandOutput, CommandRunError>)>) -> Self {
            Self {
                calls: RefCell::new(calls.into()),
            }
        }
        fn assert_exhausted(&self) {
            assert!(self.calls.borrow().is_empty());
        }
    }
    impl CommandRunner for FakeRunner {
        fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            let (expected, output) = self
                .calls
                .borrow_mut()
                .pop_front()
                .expect("unexpected command");
            assert_eq!(*command, expected);
            output
        }
    }

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "acme".into(),
            name: "widgets".into(),
        }
    }
    fn branch(name: &str, oid: &str) -> BranchSnapshot {
        BranchSnapshot {
            repository: repository(),
            name: name.into(),
            oid: CommitOid(oid.into()),
        }
    }
    fn plan() -> GitHubStackBranchLockPlan {
        let base = branch("main", "base");
        let root = GitHubStackEntryGeneration {
            position: 0,
            pr: PrNumber(10),
            stack_state: "open".into(),
            pull_request_state: PullRequestState::Open,
            draft: false,
            merged_at: None,
            base: base.clone(),
            head: branch("root", "aaa"),
        };
        let child = GitHubStackEntryGeneration {
            position: 1,
            pr: PrNumber(11),
            stack_state: "open".into(),
            pull_request_state: PullRequestState::Open,
            draft: false,
            merged_at: None,
            base: root.head.clone(),
            head: branch("child", "bbb"),
        };
        GitHubStackBranchLockPlan {
            operation_id: "op-lock".into(),
            actor: "cara".into(),
            stack_number: 7,
            selected: vec![root, child],
        }
    }
    fn observed(plan: &GitHubStackBranchLockPlan) -> GitHubStackBranchLockGeneration {
        let mut lock = expected_lock(&repository(), plan).unwrap();
        lock.id = 42;
        lock.node_id = "RRS_node".into();
        lock.created_at = "2026-08-01T10:00:00Z".into();
        lock.updated_at = "2026-08-01T10:00:01Z".into();
        bind_selected_pull_requests(&mut lock, plan);
        lock
    }
    fn ruleset_json(lock: &GitHubStackBranchLockGeneration) -> String {
        serde_json::json!({
            "id": lock.id, "node_id": lock.node_id, "name": lock.name,
            "target": "branch", "source_type": "Repository", "source": repository().slug(),
            "enforcement": "active", "conditions": {"ref_name": {"include": lock.selected_refs, "exclude": []}},
            "rules": [{"type":"update"},{"type":"deletion"}], "bypass_actors": [],
            "current_user_can_bypass": "never", "created_at": lock.created_at, "updated_at": lock.updated_at
        }).to_string()
    }
    fn summary_json(lock: &GitHubStackBranchLockGeneration) -> String {
        serde_json::json!([{"id":lock.id,"name":lock.name}]).to_string()
    }
    fn success_headers() -> CommandOutput {
        CommandOutput::success("HTTP/2 201 Created\nx-github-request-id: LOCK-1\n\n{}")
    }
    fn not_found() -> CommandOutput {
        CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "gh: HTTP 404".into(),
        }
    }

    #[test]
    fn acquire_creates_one_atomic_no_bypass_exact_ref_ruleset() {
        let plan = plan();
        let lock = observed(&plan);
        let expected = expected_lock(&repository(), &plan).unwrap();
        let calls = vec![
            (
                list_rulesets_command(&repository()),
                CommandOutput::success("[]"),
            ),
            (
                create_ruleset_command(&repository(), &expected),
                success_headers(),
            ),
            (
                list_rulesets_command(&repository()),
                CommandOutput::success(summary_json(&lock)),
            ),
            (
                read_ruleset_command(&repository(), lock.id),
                CommandOutput::success(ruleset_json(&lock)),
            ),
        ];
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));
        let receipt = adapter
            .native_stack_branch_lock_acquire(&repository(), &plan)
            .unwrap();
        assert_eq!(
            receipt.disposition,
            GitHubStackBranchLockDisposition::Completed
        );
        assert_eq!(receipt.lock, Some(lock));
        assert_eq!(receipt.github_request_id.as_deref(), Some("LOCK-1"));
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn exact_acquire_retry_is_zero_write() {
        let plan = plan();
        let lock = observed(&plan);
        let calls = vec![
            (
                list_rulesets_command(&repository()),
                CommandOutput::success(summary_json(&lock)),
            ),
            (
                read_ruleset_command(&repository(), lock.id),
                CommandOutput::success(ruleset_json(&lock)),
            ),
        ];
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));
        let receipt = adapter
            .native_stack_branch_lock_acquire(&repository(), &plan)
            .unwrap();
        assert_eq!(
            receipt.disposition,
            GitHubStackBranchLockDisposition::AlreadySatisfied
        );
        assert_eq!(receipt.lock, Some(lock));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn same_operation_with_changed_generation_collides_instead_of_relocking() {
        let original = plan();
        let lock = observed(&original);
        let mut changed = original.clone();
        changed.selected[0].head.name = "different-root".to_owned();
        let calls = vec![
            (
                list_rulesets_command(&repository()),
                CommandOutput::success(summary_json(&lock)),
            ),
            (
                read_ruleset_command(&repository(), lock.id),
                CommandOutput::success(ruleset_json(&lock)),
            ),
        ];
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));

        assert!(matches!(
            adapter.native_stack_branch_lock_acquire(&repository(), &changed),
            Err(GitHubStackBranchLockError::NameCollision { .. })
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn timeout_recovers_only_after_exact_ruleset_rediscovery() {
        let plan = plan();
        let lock = observed(&plan);
        let expected = expected_lock(&repository(), &plan).unwrap();
        let create = create_ruleset_command(&repository(), &expected);
        let calls = vec![
            (
                list_rulesets_command(&repository()),
                Ok(CommandOutput::success("[]")),
            ),
            (
                create.clone(),
                Err(CommandRunError::Timeout {
                    command: create,
                    process_group_id: Some(1),
                    timeout_ms: 30_000,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            ),
            (
                list_rulesets_command(&repository()),
                Ok(CommandOutput::success(summary_json(&lock))),
            ),
            (
                read_ruleset_command(&repository(), lock.id),
                Ok(CommandOutput::success(ruleset_json(&lock))),
            ),
        ];
        let adapter = GitHubMutationAdapter::new(FakeRunner::with_results(calls));
        let receipt = adapter
            .native_stack_branch_lock_acquire(&repository(), &plan)
            .unwrap();
        assert_eq!(
            receipt.disposition,
            GitHubStackBranchLockDisposition::RecoveredAfterAmbiguousResponse
        );
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn release_deletes_only_exact_generation_and_proves_absence() {
        let plan = plan();
        let lock = observed(&plan);
        let calls = vec![
            (
                read_ruleset_command(&repository(), lock.id),
                CommandOutput::success(ruleset_json(&lock)),
            ),
            (
                delete_ruleset_command(&repository(), lock.id),
                CommandOutput::success("HTTP/2 204 No Content\nx-github-request-id: LOCK-DEL\n\n"),
            ),
            (read_ruleset_command(&repository(), lock.id), not_found()),
            (
                list_rulesets_command(&repository()),
                CommandOutput::success("[]"),
            ),
        ];
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(calls));
        let receipt = adapter
            .native_stack_branch_lock_release(&repository(), &plan, &lock)
            .unwrap();
        assert_eq!(
            receipt.disposition,
            GitHubStackBranchLockDisposition::Completed
        );
        assert!(receipt.lock.is_none());
        assert!(receipt.verify());
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn changed_ruleset_is_never_deleted() {
        let plan = plan();
        let lock = observed(&plan);
        let mut changed = lock.clone();
        changed.updated_at = "later".into();
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(vec![(
            read_ruleset_command(&repository(), lock.id),
            CommandOutput::success(ruleset_json(&changed)),
        )]));
        assert!(matches!(
            adapter.native_stack_branch_lock_release(&repository(), &plan, &lock),
            Err(GitHubStackBranchLockError::StaleLock { .. })
        ));
        adapter.runner.assert_exhausted();
    }

    #[test]
    fn ruleset_writes_are_fenced_and_reads_remain_read_only() {
        let expected = expected_lock(&repository(), &plan()).unwrap();
        assert_eq!(
            create_ruleset_command(&repository(), &expected).intent(),
            CommandIntent::ProviderWrite
        );
        assert_eq!(
            delete_ruleset_command(&repository(), 42).intent(),
            CommandIntent::ProviderWrite
        );
        assert_eq!(
            list_rulesets_command(&repository()).intent(),
            CommandIntent::Read
        );
        assert_eq!(
            read_ruleset_command(&repository(), 42).intent(),
            CommandIntent::Read
        );
    }

    #[test]
    fn bypass_or_extra_rules_are_rejected() {
        let plan = plan();
        let lock = observed(&plan);
        let json = serde_json::json!({
            "id": lock.id, "node_id":"n", "name":lock.name, "target":"branch", "source_type":"Repository", "source":repository().slug(), "enforcement":"active",
            "conditions":{"ref_name":{"include":lock.selected_refs,"exclude":[]}}, "rules":[{"type":"update"},{"type":"deletion"}],
            "bypass_actors":[{"actor_id":1}], "current_user_can_bypass":"always", "created_at":"a", "updated_at":"b"
        }).to_string();
        let adapter = GitHubMutationAdapter::new(FakeRunner::new(vec![
            (
                list_rulesets_command(&repository()),
                CommandOutput::success(summary_json(&lock)),
            ),
            (
                read_ruleset_command(&repository(), lock.id),
                CommandOutput::success(json),
            ),
        ]));
        assert!(
            matches!(adapter.native_stack_branch_lock_acquire(&repository(), &plan), Err(GitHubStackBranchLockError::InvalidPlan { code, .. }) if code == "github_stack_lock_bypass_detected")
        );
        adapter.runner.assert_exhausted();
    }
}
