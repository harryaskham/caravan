//! Explicit, idempotent first-use initialization for a Caravan repository.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;

use mcp_cli::ErrorCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::github::{DefaultBranchPolicy, GitHubMutationAdapter, MutationError, RepositoryLabel};
use crate::model::RepositoryId;
use crate::{AppContext, AppError};

/// Canonical repository label managed by Caravan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredLabel {
    pub name: String,
    pub color: String,
    pub description: String,
}

fn required_label(name: &str, color: &str, description: impl Into<String>) -> RequiredLabel {
    RequiredLabel {
        name: name.to_owned(),
        color: color.to_owned(),
        description: description.into(),
    }
}

/// Complete deterministic label policy: fixed membership labels followed by
/// configured highest-to-lowest admission-priority labels.
#[must_use]
pub fn required_labels(priority_labels: &[String]) -> Vec<RequiredLabel> {
    const PRIORITY_COLORS: [&str; 6] = ["B60205", "D93F0B", "FBCA04", "0E8A16", "1D76DB", "5319E7"];
    let mut labels = vec![
        required_label("caravan", "5319E7", "Active member of a Caravan PR chain"),
        required_label(
            "caravan-evicted",
            "B60205",
            "Removed from a Caravan chain pending renew or rejoin",
        ),
        required_label(
            "caravan-force",
            "D93F0B",
            "Allow configured force handling for known CI failures",
        ),
    ];
    labels.extend(priority_labels.iter().enumerate().map(|(rank, name)| {
        required_label(
            name,
            PRIORITY_COLORS[rank % PRIORITY_COLORS.len()],
            format!(
                "Caravan automatic admission priority rank {} (1 highest)",
                rank + 1
            ),
        )
    }));
    labels
}

const LEGACY_ACTIVE_COLOR: &str = "1D76DB";
const LEGACY_ACTIVE_DESCRIPTION: &str = "Active member of a Caravan merge chain";

const DEFAULT_CONFIG: &str = "version: 1\nforce_merge: false\nagent_priority_labels:\n  - caravan-priority:high\n  - caravan-priority:normal\n  - caravan-priority:low\ncommand_timeout_secs: 30\nloop:\n  interval_secs: 60\njournal:\n  max_bytes: 8388608\n  max_archives: 3\nhooks: {}\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourceState {
    Created,
    AlreadyPresent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigReceipt {
    pub path: String,
    pub state: ResourceState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LabelReceipt {
    pub name: String,
    pub color: String,
    pub description: String,
    pub state: ResourceState,
}

/// Read-only label readiness included in `status` and `check` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InitializationStatus {
    pub ready: bool,
    #[serde(default)]
    pub missing_labels: Vec<String>,
    #[serde(default)]
    pub mismatched_labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

impl Default for InitializationStatus {
    fn default() -> Self {
        Self {
            ready: true,
            missing_labels: Vec::new(),
            mismatched_labels: Vec::new(),
            next: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepositoryPreflightReceipt {
    pub permission: String,
    pub default_branch: String,
    pub default_branch_protected: bool,
    pub default_branch_policy: DefaultBranchPolicy,
    pub default_branch_policy_ready: bool,
    pub squash_auto_merge_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InitOutput {
    pub repository: RepositoryId,
    pub ready: bool,
    pub config: ConfigReceipt,
    pub repository_preflight: RepositoryPreflightReceipt,
    pub labels: Vec<LabelReceipt>,
}

pub trait InitializationProvider {
    /// Lightweight repository/default-branch discovery; must not enumerate PRs
    /// or verify moving PR/default-branch object IDs.
    fn repository_identity(&self) -> Result<(RepositoryId, String), MutationError>;
    fn labels(&self, repository: &RepositoryId) -> Result<Vec<RepositoryLabel>, MutationError>;
    fn permission(&self, repository: &RepositoryId) -> Result<String, MutationError>;
    fn allows_auto_merge(&self, repository: &RepositoryId) -> Result<bool, MutationError>;
    fn branch_is_protected(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<bool, MutationError>;
    fn default_branch_policy(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<DefaultBranchPolicy, MutationError>;
    fn create_label(
        &self,
        repository: &RepositoryId,
        label: &RequiredLabel,
    ) -> Result<(), MutationError>;
}

impl<R: crate::command::CommandRunner> InitializationProvider for GitHubMutationAdapter<R> {
    fn repository_identity(&self) -> Result<(RepositoryId, String), MutationError> {
        self.repository_identity()
    }
    fn labels(&self, repository: &RepositoryId) -> Result<Vec<RepositoryLabel>, MutationError> {
        self.repository_label_definitions(repository)
    }
    fn permission(&self, repository: &RepositoryId) -> Result<String, MutationError> {
        self.repository_permission(repository)
    }
    fn allows_auto_merge(&self, repository: &RepositoryId) -> Result<bool, MutationError> {
        self.repository_allows_auto_merge(repository)
    }
    fn branch_is_protected(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<bool, MutationError> {
        self.branch_is_protected(repository, branch)
    }
    fn default_branch_policy(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> Result<DefaultBranchPolicy, MutationError> {
        self.default_branch_policy(repository, branch)
    }
    fn create_label(
        &self,
        repository: &RepositoryId,
        label: &RequiredLabel,
    ) -> Result<(), MutationError> {
        self.create_repository_label(repository, &label.name, &label.color, &label.description)
    }
}

/// Inspect labels without mutation. Existing metadata is compared exactly
/// (colors are hexadecimal and therefore case-insensitive).
#[must_use]
pub fn inspect_labels(
    labels: &[RepositoryLabel],
    priority_labels: &[String],
) -> InitializationStatus {
    let by_name: BTreeMap<_, _> = labels
        .iter()
        .map(|label| (label.name.as_str(), label))
        .collect();
    let mut missing = Vec::new();
    let mut mismatched = Vec::new();
    for required in required_labels(priority_labels) {
        match by_name.get(required.name.as_str()) {
            None => missing.push(required.name.clone()),
            Some(actual) if !label_matches(actual, &required) => {
                mismatched.push(required.name.clone());
            }
            Some(_) => {}
        }
    }
    let ready = missing.is_empty() && mismatched.is_empty();
    InitializationStatus {
        ready,
        missing_labels: missing,
        mismatched_labels: mismatched,
        next: (!ready).then(|| "run `cara init`; mismatched labels must be reconciled by an operator before retrying".to_owned()),
    }
}

/// Fail a mutating operation before provider mutation when first-use resources
/// are absent or incompatible.
pub fn require_ready(status: &InitializationStatus) -> Result<(), AppError> {
    if status.ready {
        return Ok(());
    }
    Err(AppError::structured(
        ErrorCategory::Validation,
        "repository_not_initialized",
        "Caravan repository initialization must complete before mutation",
        Some(json!({
            "missing_labels": status.missing_labels,
            "mismatched_labels": status.mismatched_labels,
            "next": "run `cara init`, reconcile any bounded mismatch, then rerun the same command",
        })),
    ))
}

/// Run the reviewed first-use operation. No pull request is ever mutated here.
pub fn init(context: &AppContext) -> Result<InitOutput, AppError> {
    let provider = GitHubMutationAdapter::new(
        crate::command::ProcessRunner::in_directory(&context.repository_path).with_timeout(
            std::time::Duration::from_secs(context.config.command_timeout_secs),
        ),
    );
    let (repository, default_branch) = provider.repository_identity().map_err(provider_error)?;
    init_with_provider(context, &repository, &default_branch, &provider)
}

#[allow(clippy::too_many_lines)]
pub fn init_with_provider(
    context: &AppContext,
    repository: &RepositoryId,
    default_branch: &str,
    provider: &impl InitializationProvider,
) -> Result<InitOutput, AppError> {
    let config = ensure_config(context)?;

    let permission = provider.permission(repository).map_err(provider_error)?;
    if !matches!(permission.as_str(), "WRITE" | "MAINTAIN" | "ADMIN") {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "repository_label_write_permission_required",
            "cara init requires repository label-write permission",
            Some(
                json!({"repository": repository, "required": "WRITE", "actual": permission, "next": "grant repository write access, then rerun `cara init`"}),
            ),
        ));
    }
    let squash_auto_merge_ready = provider
        .allows_auto_merge(repository)
        .map_err(provider_error)?;
    if !squash_auto_merge_ready {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "auto_merge_not_enabled",
            "enable squash merge and GitHub auto-merge, then rerun `cara init`",
            Some(
                json!({"repository_preflight": {"permission": permission, "default_branch": default_branch, "squash_auto_merge_ready": false}}),
            ),
        ));
    }
    let default_branch_protected = provider
        .branch_is_protected(repository, default_branch)
        .map_err(provider_error)?;
    if !default_branch_protected {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "default_branch_not_protected",
            "the default branch must be protected before repository initialization",
            Some(
                json!({"default_branch": default_branch, "next": "protect the default branch with a required check or approving review, then rerun `cara init`"}),
            ),
        ));
    }
    let default_branch_policy = provider
        .default_branch_policy(repository, default_branch)
        .map_err(provider_error)?;
    let default_branch_policy_ready = default_branch_policy.ready();
    if !default_branch_policy_ready {
        return Err(AppError::structured(
            ErrorCategory::Validation,
            "default_branch_check_policy_missing",
            "the default branch must be protected by a required check or review policy",
            Some(
                json!({"default_branch": default_branch, "actual_policy": default_branch_policy, "next": "configure default-branch protection with a required check or approving review, then rerun `cara init`"}),
            ),
        ));
    }

    let required = required_labels(&context.config.agent_priority_labels);
    let initial = provider.labels(repository).map_err(provider_error)?;
    reject_mismatches(repository, &initial, &required)?;
    let mut receipts = Vec::new();
    for required in &required {
        let current = provider.labels(repository).map_err(provider_error)?;
        if let Some(actual) = current.iter().find(|label| label.name == required.name) {
            if !label_matches(actual, required) {
                return Err(mismatch_error(repository, actual, required));
            }
            receipts.push(label_receipt_actual(actual, ResourceState::AlreadyPresent));
            continue;
        }
        if let Err(create_error) = provider.create_label(repository, required) {
            // A timeout or duplicate-name race is indeterminate. Re-read and
            // converge only if the exact desired resource now exists.
            let after = provider
                .labels(repository)
                .map_err(|_| provider_error(create_error.clone()))?;
            match after.iter().find(|label| label.name == required.name) {
                Some(actual) if label_matches(actual, required) => {}
                Some(actual) => return Err(mismatch_error(repository, actual, required)),
                None => return Err(provider_error(create_error)),
            }
        }
        let after = provider.labels(repository).map_err(provider_error)?;
        match after.iter().find(|label| label.name == required.name) {
            Some(actual) if label_matches(actual, required) => {
                receipts.push(label_receipt_actual(actual, ResourceState::Created));
            }
            Some(actual) => return Err(mismatch_error(repository, actual, required)),
            None => {
                return Err(AppError::validation(
                    "repository_label_create_indeterminate",
                    format!(
                        "label `{}` was not observable after creation; rerun `cara init`",
                        required.name
                    ),
                ));
            }
        }
    }

    Ok(InitOutput {
        repository: repository.clone(),
        ready: true,
        config,
        repository_preflight: RepositoryPreflightReceipt {
            permission,
            default_branch: default_branch.to_owned(),
            default_branch_protected,
            default_branch_policy,
            default_branch_policy_ready,
            squash_auto_merge_ready,
        },
        labels: receipts,
    })
}

fn ensure_config(context: &AppContext) -> Result<ConfigReceipt, AppError> {
    let receipt_path = context.config_path.display().to_string();
    let resolved_path = if context.config_path.is_absolute() {
        context.config_path.clone()
    } else {
        context.repository_path.join(&context.config_path)
    };
    let path = resolved_path.as_path();
    if path.exists() {
        validate_existing_config(path)?;
        return Ok(ConfigReceipt {
            path: receipt_path,
            state: ResourceState::AlreadyPresent,
        });
    }
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    if is_default_config_path(context) {
        require_default_parent_contained(&context.repository_path, parent, path)?;
    }
    fs::create_dir_all(parent).map_err(|error| io_error(path, error))?;
    if is_default_config_path(context) {
        require_default_parent_contained(&context.repository_path, parent, path)?;
    }

    // Publish a fully written inode with an atomic, no-overwrite hard link.
    // Unlike rename, hard_link fails if a concurrent initializer won the name.
    let temporary = parent.join(format!(".config.yaml.cara-init-{}", uuid::Uuid::now_v7()));
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(DEFAULT_CONFIG.as_bytes())?;
        file.sync_all()?;
        fs::hard_link(&temporary, path)?;
        sync_parent_directory(parent)?;
        Ok::<(), std::io::Error>(())
    })();
    let _ = fs::remove_file(&temporary);
    match write_result {
        Ok(()) => Ok(ConfigReceipt {
            path: receipt_path.clone(),
            state: ResourceState::Created,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_existing_config(path)?;
            Ok(ConfigReceipt {
                path: receipt_path,
                state: ResourceState::AlreadyPresent,
            })
        }
        Err(error) => Err(io_error(path, error)),
    }
}

#[cfg(unix)]
fn sync_parent_directory(parent: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

fn is_default_config_path(context: &AppContext) -> bool {
    let configured = if context.config_path.is_absolute() {
        context.config_path.clone()
    } else {
        context.repository_path.join(&context.config_path)
    };
    configured == context.repository_path.join(crate::config::DEFAULT_CONFIG_PATH)
}

fn require_default_parent_contained(
    repository: &std::path::Path,
    parent: &std::path::Path,
    config_path: &std::path::Path,
) -> Result<(), AppError> {
    let repository = fs::canonicalize(repository).map_err(|error| io_error(config_path, error))?;
    let mut ancestor = parent;
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| {
            AppError::validation(
                "initialization_config_path_escape",
                "default config parent does not resolve inside the repository",
            )
        })?;
    }
    let ancestor = fs::canonicalize(ancestor).map_err(|error| io_error(config_path, error))?;
    if !ancestor.starts_with(&repository) {
        return Err(AppError::structured(
            ErrorCategory::ConfigError,
            "initialization_config_path_escape",
            "refusing to create the default config through a path outside the repository",
            Some(json!({ "path": config_path, "repository": repository })),
        ));
    }
    Ok(())
}

fn validate_existing_config(path: &std::path::Path) -> Result<(), AppError> {
    crate::config::CaravanConfig::load(path).map_err(|error| {
        AppError::structured(
            ErrorCategory::ConfigError,
            "initialization_config_incompatible",
            error.to_string(),
            Some(json!({"path": path, "next": "repair the existing config; cara init never overwrites it"})),
        )
    })?;
    Ok(())
}

fn reject_mismatches(
    repository: &RepositoryId,
    labels: &[RepositoryLabel],
    required_labels: &[RequiredLabel],
) -> Result<(), AppError> {
    for required in required_labels {
        if let Some(actual) = labels.iter().find(|label| label.name == required.name) {
            if !label_matches(actual, required) {
                return Err(mismatch_error(repository, actual, required));
            }
        }
    }
    Ok(())
}

fn label_matches(actual: &RepositoryLabel, required: &RequiredLabel) -> bool {
    let canonical = actual.color.eq_ignore_ascii_case(&required.color)
        && actual.description.as_deref() == Some(required.description.as_str());
    let legacy_active = required.name == "caravan"
        && actual.color.eq_ignore_ascii_case(LEGACY_ACTIVE_COLOR)
        && actual.description.as_deref() == Some(LEGACY_ACTIVE_DESCRIPTION);
    canonical || legacy_active
}

fn label_receipt_actual(label: &RepositoryLabel, state: ResourceState) -> LabelReceipt {
    LabelReceipt {
        name: label.name.clone(),
        color: label.color.clone(),
        description: label.description.clone().unwrap_or_default(),
        state,
    }
}

fn mismatch_error(
    repository: &RepositoryId,
    actual: &RepositoryLabel,
    expected: &RequiredLabel,
) -> AppError {
    AppError::structured(
        ErrorCategory::Validation,
        "repository_label_metadata_mismatch",
        format!(
            "existing label `{}` has operator-owned metadata that cara init will not overwrite",
            expected.name
        ),
        Some(
            json!({"repository": repository, "label": expected.name, "expected": {"color": expected.color, "description": expected.description}, "actual": actual, "next": "review and reconcile this label manually, then rerun `cara init`"}),
        ),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn provider_error(error: MutationError) -> AppError {
    AppError::structured(
        ErrorCategory::ExecutionFailure,
        "repository_initialization_provider_failed",
        error.to_string(),
        Some(
            json!({"next": "inspect provider access/evidence and rerun `cara init`; the operation is idempotent"}),
        ),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn io_error(path: &std::path::Path, error: std::io::Error) -> AppError {
    AppError::structured(
        ErrorCategory::ConfigError,
        "initialization_config_io_failed",
        error.to_string(),
        Some(
            json!({"path": path, "next": "repair filesystem access and rerun `cara init`; existing files are never overwritten"}),
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use mcp_cli::StructuredError;

    use super::*;
    use crate::command::CommandSpec;
    use crate::github::DiscoveryError;

    struct FakeProvider {
        labels: Mutex<Vec<RepositoryLabel>>,
        permission: &'static str,
        auto_merge: bool,
        protected: bool,
        policy_ready: bool,
        fail_create_once: Mutex<bool>,
    }

    impl FakeProvider {
        fn new(labels: Vec<RepositoryLabel>) -> Self {
            Self {
                labels: Mutex::new(labels),
                permission: "WRITE",
                auto_merge: true,
                protected: true,
                policy_ready: true,
                fail_create_once: Mutex::new(false),
            }
        }
    }

    impl InitializationProvider for FakeProvider {
        fn repository_identity(&self) -> Result<(RepositoryId, String), MutationError> {
            Ok((repository(), "main".to_owned()))
        }
        fn labels(&self, _: &RepositoryId) -> Result<Vec<RepositoryLabel>, MutationError> {
            Ok(self.labels.lock().unwrap().clone())
        }
        fn permission(&self, _: &RepositoryId) -> Result<String, MutationError> {
            Ok(self.permission.to_owned())
        }
        fn allows_auto_merge(&self, _: &RepositoryId) -> Result<bool, MutationError> {
            Ok(self.auto_merge)
        }
        fn branch_is_protected(&self, _: &RepositoryId, _: &str) -> Result<bool, MutationError> {
            Ok(self.protected)
        }
        fn default_branch_policy(
            &self,
            _: &RepositoryId,
            _: &str,
        ) -> Result<DefaultBranchPolicy, MutationError> {
            Ok(DefaultBranchPolicy {
                required_status_checks: self
                    .policy_ready
                    .then(|| "Check & Lint".to_owned())
                    .into_iter()
                    .collect(),
                strict_status_checks: false,
                required_approving_review_count: 0,
            })
        }
        fn create_label(
            &self,
            _: &RepositoryId,
            label: &RequiredLabel,
        ) -> Result<(), MutationError> {
            let mut labels = self.labels.lock().unwrap();
            if !labels.iter().any(|actual| actual.name == label.name) {
                labels.push(canonical(label));
            }
            if std::mem::take(&mut *self.fail_create_once.lock().unwrap()) {
                return Err(MutationError::Provider(DiscoveryError::CommandFailed {
                    command: CommandSpec::new("gh"),
                    code: None,
                    stderr: "indeterminate timeout".to_owned(),
                }));
            }
            Ok(())
        }
    }

    fn canonical(label: &RequiredLabel) -> RepositoryLabel {
        RepositoryLabel {
            name: label.name.clone(),
            color: label.color.clone(),
            description: Some(label.description.clone()),
        }
    }

    fn required() -> Vec<RequiredLabel> {
        required_labels(&crate::config::CaravanConfig::default().agent_priority_labels)
    }

    fn repository() -> RepositoryId {
        RepositoryId {
            owner: "acme".to_owned(),
            name: "widgets".to_owned(),
        }
    }

    fn context(directory: &tempfile::TempDir) -> AppContext {
        AppContext {
            repository_path: directory.path().to_path_buf(),
            config_path: directory.path().join(".caravan/config.yaml"),
            config_existed: false,
            config: crate::config::CaravanConfig::default(),
        }
    }

    #[test]
    fn empty_repository_is_initialized_and_replay_is_a_noop() {
        let directory = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new(Vec::new());
        let first =
            init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap();
        assert_eq!(first.config.state, ResourceState::Created);
        assert_eq!(first.repository_preflight.permission, "WRITE");
        assert_eq!(
            first
                .repository_preflight
                .default_branch_policy
                .required_status_checks,
            ["Check & Lint"]
        );
        assert_eq!(first.labels.len(), 6);
        assert_eq!(
            first
                .labels
                .iter()
                .map(|receipt| receipt.name.as_str())
                .collect::<Vec<_>>(),
            [
                "caravan",
                "caravan-evicted",
                "caravan-force",
                "caravan-priority:high",
                "caravan-priority:normal",
                "caravan-priority:low",
            ]
        );
        assert!(
            first
                .labels
                .iter()
                .all(|receipt| receipt.state == ResourceState::Created)
        );
        assert_eq!(first.labels[3].color, "B60205");
        assert_eq!(first.labels[4].color, "D93F0B");
        assert_eq!(first.labels[5].color, "FBCA04");
        assert_eq!(
            first.labels[3].description,
            "Caravan automatic admission priority rank 1 (1 highest)"
        );
        let generated =
            crate::config::CaravanConfig::load(&directory.path().join(".caravan/config.yaml"))
                .unwrap();
        assert_eq!(
            generated.agent_priority_labels,
            crate::config::CaravanConfig::default().agent_priority_labels
        );

        let mut replay_context = context(&directory);
        replay_context.config_existed = true;
        let second = init_with_provider(&replay_context, &repository(), "main", &provider).unwrap();
        assert_eq!(second.config.state, ResourceState::AlreadyPresent);
        assert!(
            second
                .labels
                .iter()
                .all(|receipt| receipt.state == ResourceState::AlreadyPresent)
        );
    }

    #[test]
    fn relative_default_config_resolves_from_repository_when_parent_is_absent() {
        let directory = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new(Vec::new());
        let mut relative = context(&directory);
        relative.config_path = std::path::PathBuf::from(crate::config::DEFAULT_CONFIG_PATH);
        assert!(!directory.path().join(".caravan").exists());

        let output = init_with_provider(&relative, &repository(), "main", &provider).unwrap();

        assert_eq!(output.config.state, ResourceState::Created);
        assert_eq!(output.config.path, crate::config::DEFAULT_CONFIG_PATH);
        crate::config::CaravanConfig::load(
            &directory.path().join(crate::config::DEFAULT_CONFIG_PATH),
        )
        .unwrap();
    }

    #[test]
    fn concurrent_config_initializers_create_once_without_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new(Vec::new());
        let outputs = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap()
            });
            let second = scope.spawn(|| {
                init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap()
            });
            [first.join().unwrap(), second.join().unwrap()]
        });
        let created = outputs
            .iter()
            .filter(|output| output.config.state == ResourceState::Created)
            .count();
        assert_eq!(created, 1);
        crate::config::CaravanConfig::load(&directory.path().join(".caravan/config.yaml")).unwrap();
    }

    #[test]
    fn incompatible_existing_config_is_preserved_byte_for_byte() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join(".caravan/config.yaml");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(&config_path, "version: 999\noperator: owned\n").unwrap();
        let before = fs::read(&config_path).unwrap();
        let provider = FakeProvider::new(Vec::new());
        let error =
            init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("initialization_config_incompatible")
        );
        assert_eq!(fs::read(&config_path).unwrap(), before);
        assert!(provider.labels.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn default_config_rejects_symlink_and_parent_escape_before_provider_mutation() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new(Vec::new());
        let caravan_dir = directory.path().join(".caravan");
        symlink(external.path(), &caravan_dir).unwrap();
        let error = init_with_provider(&context(&directory), &repository(), "main", &provider)
            .unwrap_err();
        assert_eq!(error.code(), "initialization_config_path_escape");
        assert!(!external.path().join("config.yaml").exists());
        assert!(provider.labels.lock().unwrap().is_empty());

        fs::remove_file(caravan_dir).unwrap();
        fs::create_dir_all(directory.path().join(".caravan")).unwrap();
        let target = external.path().join("existing.yaml");
        fs::write(&target, "{}\n").unwrap();
        symlink(&target, directory.path().join(".caravan/config.yaml")).unwrap();
        let error = init_with_provider(&context(&directory), &repository(), "main", &provider)
            .unwrap_err();
        assert_eq!(error.code(), "initialization_config_incompatible");
        assert!(provider.labels.lock().unwrap().is_empty());
    }

    #[test]
    fn explicit_config_outside_worktree_is_supported() {
        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let config_path = external.path().join("config.yaml");
        let mut explicit = context(&directory);
        explicit.config_path = config_path.clone();
        let provider = FakeProvider::new(Vec::new());

        let output = init_with_provider(&explicit, &repository(), "main", &provider).unwrap();
        assert_eq!(output.config.state, ResourceState::Created);
        crate::config::CaravanConfig::load(&config_path).unwrap();
    }

    #[test]
    fn partial_repository_creates_only_missing_labels() {
        let directory = tempfile::tempdir().unwrap();
        let required = required();
        let provider = FakeProvider::new(vec![canonical(&required[0])]);
        let output =
            init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap();
        assert_eq!(output.labels[0].state, ResourceState::AlreadyPresent);
        assert_eq!(output.labels[1].state, ResourceState::Created);
        assert_eq!(output.labels[2].state, ResourceState::Created);
    }

    #[test]
    fn missing_priority_labels_are_created_then_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let required = required();
        let fixed = required[..3].iter().map(canonical).collect();
        let provider = FakeProvider::new(fixed);

        let first =
            init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap();
        assert!(
            first.labels[..3]
                .iter()
                .all(|receipt| receipt.state == ResourceState::AlreadyPresent)
        );
        assert!(
            first.labels[3..]
                .iter()
                .all(|receipt| receipt.state == ResourceState::Created)
        );

        let mut replay_context = context(&directory);
        replay_context.config_existed = true;
        let replay = init_with_provider(&replay_context, &repository(), "main", &provider).unwrap();
        assert!(
            replay
                .labels
                .iter()
                .all(|receipt| receipt.state == ResourceState::AlreadyPresent)
        );
    }

    #[test]
    fn legacy_active_label_is_ready_and_receipt_preserves_actual_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let mut labels: Vec<_> = required().iter().map(canonical).collect();
        labels[0].color = LEGACY_ACTIVE_COLOR.to_owned();
        labels[0].description = Some(LEGACY_ACTIVE_DESCRIPTION.to_owned());
        assert!(
            inspect_labels(
                &labels,
                &crate::config::CaravanConfig::default().agent_priority_labels,
            )
            .ready
        );
        let provider = FakeProvider::new(labels);
        let output =
            init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap();
        assert_eq!(output.labels[0].state, ResourceState::AlreadyPresent);
        assert_eq!(output.labels[0].color, LEGACY_ACTIVE_COLOR);
        assert_eq!(output.labels[0].description, LEGACY_ACTIVE_DESCRIPTION);
    }

    #[test]
    fn mismatch_fails_without_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let wrong = RepositoryLabel {
            name: "caravan".to_owned(),
            color: "ffffff".to_owned(),
            description: Some("operator label".to_owned()),
        };
        let provider = FakeProvider::new(vec![wrong.clone()]);
        let error =
            init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("repository_label_metadata_mismatch")
        );
        assert_eq!(provider.labels.lock().unwrap()[0], wrong);
    }

    #[test]
    fn indeterminate_create_converges_after_exact_reread() {
        let directory = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new(Vec::new());
        *provider.fail_create_once.lock().unwrap() = true;
        let output =
            init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap();
        assert!(output.ready);
        assert_eq!(provider.labels.lock().unwrap().len(), required().len());
    }

    #[test]
    fn disabled_auto_merge_fails_before_label_creation() {
        let directory = tempfile::tempdir().unwrap();
        let mut provider = FakeProvider::new(Vec::new());
        provider.auto_merge = false;
        let error =
            init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap_err();
        assert!(error.to_string().contains("auto_merge_not_enabled"));
        assert!(provider.labels.lock().unwrap().is_empty());
    }

    #[test]
    fn unprotected_default_branch_fails_before_label_creation() {
        let directory = tempfile::tempdir().unwrap();
        let mut provider = FakeProvider::new(Vec::new());
        provider.protected = false;
        let error =
            init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap_err();
        assert!(error.to_string().contains("default_branch_not_protected"));
        assert!(provider.labels.lock().unwrap().is_empty());
    }

    #[test]
    fn protected_branch_without_check_or_review_fails_before_label_creation() {
        let directory = tempfile::tempdir().unwrap();
        let mut provider = FakeProvider::new(Vec::new());
        provider.policy_ready = false;
        let error =
            init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("default_branch_check_policy_missing")
        );
        assert!(provider.labels.lock().unwrap().is_empty());
    }

    #[test]
    fn permission_denial_creates_no_labels() {
        let directory = tempfile::tempdir().unwrap();
        let mut provider = FakeProvider::new(Vec::new());
        provider.permission = "READ";
        let error =
            init_with_provider(&context(&directory), &repository(), "main", &provider).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("repository_label_write_permission_required")
        );
        assert!(provider.labels.lock().unwrap().is_empty());
    }

    #[test]
    fn read_only_inventory_reports_missing_and_mismatch() {
        let required = required();
        let labels = vec![
            canonical(&required[0]),
            RepositoryLabel {
                name: required[1].name.clone(),
                color: "000000".to_owned(),
                description: None,
            },
        ];
        let priorities = crate::config::CaravanConfig::default().agent_priority_labels;
        let status = inspect_labels(&labels, &priorities);
        assert!(!status.ready);
        assert_eq!(
            status.missing_labels,
            [
                "caravan-force",
                "caravan-priority:high",
                "caravan-priority:normal",
                "caravan-priority:low",
            ]
        );
        assert_eq!(status.mismatched_labels, ["caravan-evicted"]);
        assert!(status.next.unwrap().contains("cara init"));
    }
}
