//! Authoritative, secret-free GitHub App repository-tenancy projection.
//!
//! Webhook deliveries are wake hints only.  The projection changes only after
//! a complete `/installation/repositories` read, and deployment allowlists are
//! always the upper bound.  Provider uncertainty quarantines every configured
//! repository before a hosted writer can run.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::command::{CommandRunner, CommandSpec, GithubRequestBudget, ProcessRunner};
use crate::{AppContext, AppError};

const TENANCY_SCHEMA_VERSION: u32 = 1;
const PROVIDER_PAGE_SIZE: usize = 100;
const MAX_PROVIDER_REPOSITORIES: usize = 10_000;

/// Whether one deployment-allowlisted repository may reach hosted mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostedTenancyState {
    Active,
    Quarantined,
}

/// Sanitized durable evidence for one allowlisted repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HostedTenancyRecord {
    pub repository: String,
    pub installation_id: u64,
    pub state: HostedTenancyState,
    pub reason: String,
    pub provider_generation: String,
    pub observed_unix_ms: u64,
}

/// Complete bounded installation projection persisted atomically between runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct HostedTenancyProjection {
    pub(crate) schema_version: u32,
    pub(crate) installation_id: u64,
    pub(crate) provider_generation: String,
    pub(crate) provider_total_count: usize,
    pub(crate) observed_unix_ms: u64,
    pub(crate) complete: bool,
    pub(crate) reason: String,
    pub(crate) repositories: BTreeMap<String, HostedTenancyRecord>,
}

impl HostedTenancyProjection {
    pub(crate) fn active(&self, repository: &str) -> bool {
        self.complete
            && self.repositories.get(repository).is_some_and(|record| {
                record.state == HostedTenancyState::Active
                    && record.installation_id == self.installation_id
                    && record.provider_generation == self.provider_generation
            })
    }

    pub(crate) fn quarantine(
        installation_id: u64,
        allowlist: &BTreeSet<String>,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        let observed_unix_ms = unix_ms();
        let repositories = allowlist
            .iter()
            .map(|repository| {
                (
                    repository.clone(),
                    HostedTenancyRecord {
                        repository: repository.clone(),
                        installation_id,
                        state: HostedTenancyState::Quarantined,
                        reason: reason.clone(),
                        provider_generation: "unavailable".to_owned(),
                        observed_unix_ms,
                    },
                )
            })
            .collect();
        Self {
            schema_version: TENANCY_SCHEMA_VERSION,
            installation_id,
            provider_generation: "unavailable".to_owned(),
            provider_total_count: 0,
            observed_unix_ms,
            complete: false,
            reason,
            repositories,
        }
    }
}

/// Complete provider observation before deployment-policy intersection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostedTenancyObservation {
    pub(crate) installation_id: u64,
    pub(crate) provider_generation: String,
    pub(crate) provider_total_count: usize,
    pub(crate) observed_unix_ms: u64,
    pub(crate) repositories: BTreeSet<String>,
}

pub(crate) trait HostedTenancyProvider {
    fn observe(
        &self,
        installation_id: u64,
    ) -> Result<HostedTenancyObservation, HostedTenancyProviderError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostedTenancyProviderError {
    reason: &'static str,
}

impl HostedTenancyProviderError {
    pub(crate) fn new(reason: &'static str) -> Self {
        Self { reason }
    }
}

/// Installation-token provider backed by Cara's repository-scoped credential
/// broker.  Every page is a separate command and therefore consumes the normal
/// GitHub request/deadline budgets.
pub(crate) struct CommandHostedTenancyProvider {
    context: AppContext,
}

impl CommandHostedTenancyProvider {
    pub(crate) fn new(context: &AppContext) -> Self {
        Self {
            context: context.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct InstallationRepositoriesPage {
    total_count: usize,
    repositories: Vec<InstallationRepository>,
}

#[derive(Debug, Deserialize)]
struct InstallationRepository {
    id: u64,
    full_name: String,
}

impl HostedTenancyProvider for CommandHostedTenancyProvider {
    fn observe(
        &self,
        installation_id: u64,
    ) -> Result<HostedTenancyObservation, HostedTenancyProviderError> {
        let budget = Duration::from_secs(self.context.config.command_timeout_secs);
        let runner = ProcessRunner::in_directory(&self.context.repository_path)
            .with_timeout(budget)
            .with_operation_deadline(Instant::now() + budget)
            .with_github_request_budget(GithubRequestBudget::new(100));
        observe_with_runner(&runner, installation_id)
    }
}

fn observe_with_runner(
    runner: &impl CommandRunner,
    installation_id: u64,
) -> Result<HostedTenancyObservation, HostedTenancyProviderError> {
    let mut page_number = 1usize;
    let mut expected_total = None;
    let mut repositories = BTreeMap::new();
    loop {
        let endpoint =
            format!("/installation/repositories?per_page={PROVIDER_PAGE_SIZE}&page={page_number}");
        let command = CommandSpec::new("gh").args([
            "api".to_owned(),
            "--method".to_owned(),
            "GET".to_owned(),
            endpoint,
        ]);
        let output = runner
            .run(&command)
            .map_err(|_| HostedTenancyProviderError::new("provider_command_unavailable"))?;
        if !output.is_success() {
            return Err(HostedTenancyProviderError::new(
                "installation_repository_read_failed",
            ));
        }
        let page: InstallationRepositoriesPage = serde_json::from_str(&output.stdout)
            .map_err(|_| HostedTenancyProviderError::new("provider_response_invalid"))?;
        if page.total_count > MAX_PROVIDER_REPOSITORIES {
            return Err(HostedTenancyProviderError::new(
                "provider_repository_limit_exceeded",
            ));
        }
        if expected_total
            .replace(page.total_count)
            .is_some_and(|total| total != page.total_count)
        {
            return Err(HostedTenancyProviderError::new(
                "provider_repository_total_changed",
            ));
        }
        let page_len = page.repositories.len();
        for repository in page.repositories {
            if repository.full_name.split_once('/').is_none()
                || repositories
                    .insert(repository.full_name, repository.id)
                    .is_some()
            {
                return Err(HostedTenancyProviderError::new(
                    "provider_repository_identity_invalid",
                ));
            }
        }
        let total = expected_total.unwrap_or_default();
        if repositories.len() == total {
            break;
        }
        if page_len == 0 || repositories.len() > total {
            return Err(HostedTenancyProviderError::new(
                "provider_repository_projection_incomplete",
            ));
        }
        page_number = page_number.saturating_add(1);
    }
    let mut digest = Sha256::new();
    digest.update(installation_id.to_be_bytes());
    for (repository, id) in &repositories {
        digest.update(repository.as_bytes());
        digest.update([0]);
        digest.update(id.to_be_bytes());
    }
    Ok(HostedTenancyObservation {
        installation_id,
        provider_generation: format!("sha256:{:x}", digest.finalize()),
        provider_total_count: repositories.len(),
        observed_unix_ms: unix_ms(),
        repositories: repositories.into_keys().collect(),
    })
}

/// Intersect one complete provider observation with the explicit deployment
/// allowlist.  Provider repositories outside the allowlist never become rows.
pub(crate) fn reconcile_tenancy(
    installation_id: u64,
    allowlist: &BTreeSet<String>,
    observation: Result<HostedTenancyObservation, HostedTenancyProviderError>,
) -> HostedTenancyProjection {
    let observation = match observation {
        Ok(observation) if observation.installation_id == installation_id => observation,
        Ok(_) => {
            return HostedTenancyProjection::quarantine(
                installation_id,
                allowlist,
                "installation_identity_mismatch",
            );
        }
        Err(error) => {
            return HostedTenancyProjection::quarantine(installation_id, allowlist, error.reason);
        }
    };
    let repositories = allowlist
        .iter()
        .map(|repository| {
            let active = observation.repositories.contains(repository);
            (
                repository.clone(),
                HostedTenancyRecord {
                    repository: repository.clone(),
                    installation_id,
                    state: if active {
                        HostedTenancyState::Active
                    } else {
                        HostedTenancyState::Quarantined
                    },
                    reason: if active {
                        "authoritative_installation_access".to_owned()
                    } else {
                        "repository_absent_from_installation".to_owned()
                    },
                    provider_generation: observation.provider_generation.clone(),
                    observed_unix_ms: observation.observed_unix_ms,
                },
            )
        })
        .collect();
    HostedTenancyProjection {
        schema_version: TENANCY_SCHEMA_VERSION,
        installation_id,
        provider_generation: observation.provider_generation,
        provider_total_count: observation.provider_total_count,
        observed_unix_ms: observation.observed_unix_ms,
        complete: true,
        reason: "authoritative_installation_reconciliation".to_owned(),
        repositories,
    }
}

pub(crate) fn classify_tenancy_transitions(
    previous: Option<&HostedTenancyProjection>,
    projection: &mut HostedTenancyProjection,
) {
    for (repository, record) in &mut projection.repositories {
        let was_active = previous.is_some_and(|previous| previous.active(repository));
        match (projection.complete, record.state, was_active) {
            (true, HostedTenancyState::Active, true) => "retained_authoritative_access",
            (true, HostedTenancyState::Active, false) => "added_authoritative_access",
            (true, HostedTenancyState::Quarantined, true) => {
                "removed_from_authoritative_installation"
            }
            (true, HostedTenancyState::Quarantined, false) => {
                "quarantined_not_in_authoritative_installation"
            }
            (false, _, _) => "quarantined_provider_unknown",
        }
        .clone_into(&mut record.reason);
    }
}

pub(crate) fn load_tenancy(
    path: &Path,
    installation_id: u64,
    allowlist: &BTreeSet<String>,
) -> HostedTenancyProjection {
    let Ok(bytes) = fs::read(path) else {
        return HostedTenancyProjection::quarantine(
            installation_id,
            allowlist,
            "tenancy_state_missing",
        );
    };
    let Ok(projection) = serde_json::from_slice::<HostedTenancyProjection>(&bytes) else {
        return HostedTenancyProjection::quarantine(
            installation_id,
            allowlist,
            "tenancy_state_invalid",
        );
    };
    let exact_rows = projection.repositories.keys().eq(allowlist.iter());
    let exact_generations = projection.repositories.iter().all(|(repository, record)| {
        record.repository == *repository
            && record.installation_id == installation_id
            && record.provider_generation == projection.provider_generation
            && record.observed_unix_ms == projection.observed_unix_ms
    });
    if projection.schema_version != TENANCY_SCHEMA_VERSION
        || projection.installation_id != installation_id
        || !projection.complete
        || !exact_rows
        || !exact_generations
    {
        return HostedTenancyProjection::quarantine(
            installation_id,
            allowlist,
            "tenancy_state_generation_mismatch",
        );
    }
    projection
}

pub(crate) fn persist_tenancy(
    path: &Path,
    projection: &HostedTenancyProjection,
) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::validation("hosted_tenancy_state_invalid", "state path has no parent")
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        AppError::validation("hosted_tenancy_state_unavailable", error.to_string())
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|error| {
            AppError::validation("hosted_tenancy_state_unavailable", error.to_string())
        })?;
    }
    let temporary = temporary_path(path);
    let bytes = serde_json::to_vec_pretty(projection)
        .map_err(|error| AppError::validation("hosted_tenancy_state_invalid", error.to_string()))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|error| {
        AppError::validation("hosted_tenancy_state_unavailable", error.to_string())
    })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            let _ = fs::remove_file(&temporary);
            AppError::validation("hosted_tenancy_state_unavailable", error.to_string())
        })?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        AppError::validation("hosted_tenancy_state_unavailable", error.to_string())
    })?;
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "tenancy".into(), std::ffi::OsStr::to_os_string);
    name.push(format!(".{}.tmp", uuid::Uuid::now_v7()));
    path.with_file_name(name)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;
    use crate::command::{CommandOutput, CommandRunError};

    struct FakeRunner {
        outputs: RefCell<VecDeque<(CommandSpec, CommandOutput)>>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            let (expected, output) = self.outputs.borrow_mut().pop_front().unwrap();
            assert_eq!(&expected, command);
            Ok(output)
        }
    }

    fn provider_command(page: usize) -> CommandSpec {
        CommandSpec::new("gh").args([
            "api".to_owned(),
            "--method".to_owned(),
            "GET".to_owned(),
            format!("/installation/repositories?per_page=100&page={page}"),
        ])
    }

    fn allowlist() -> BTreeSet<String> {
        ["acme/one".to_owned(), "acme/two".to_owned()]
            .into_iter()
            .collect()
    }

    fn observation(repositories: &[&str]) -> HostedTenancyObservation {
        HostedTenancyObservation {
            installation_id: 42,
            provider_generation: "sha256:generation".to_owned(),
            provider_total_count: repositories.len(),
            observed_unix_ms: 123,
            repositories: repositories
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    #[test]
    fn provider_inventory_is_complete_bounded_and_page_accounted() {
        let runner = FakeRunner {
            outputs: RefCell::new(VecDeque::from([
                (
                    provider_command(1),
                    CommandOutput::success(
                        r#"{"total_count":3,"repositories":[{"id":1,"full_name":"acme/one"},{"id":2,"full_name":"acme/two"}],"token":"ghs_secret_sentinel"}"#,
                    ),
                ),
                (
                    provider_command(2),
                    CommandOutput::success(
                        r#"{"total_count":3,"repositories":[{"id":3,"full_name":"acme/three"}]}"#,
                    ),
                ),
            ])),
        };
        let observed = observe_with_runner(&runner, 42).unwrap();
        assert_eq!(observed.provider_total_count, 3);
        assert!(!observed.provider_generation.contains("secret_sentinel"));
        assert_eq!(
            observed.repositories,
            ["acme/one", "acme/three", "acme/two"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        assert!(runner.outputs.borrow().is_empty());
    }

    #[test]
    fn provider_inventory_refuses_truncation_and_duplicate_identity() {
        let truncated = FakeRunner {
            outputs: RefCell::new(VecDeque::from([
                (
                    provider_command(1),
                    CommandOutput::success(
                        r#"{"total_count":2,"repositories":[{"id":1,"full_name":"acme/one"}]}"#,
                    ),
                ),
                (
                    provider_command(2),
                    CommandOutput::success(r#"{"total_count":2,"repositories":[]}"#),
                ),
            ])),
        };
        assert_eq!(
            observe_with_runner(&truncated, 42).unwrap_err().reason,
            "provider_repository_projection_incomplete"
        );

        let duplicate = FakeRunner {
            outputs: RefCell::new(VecDeque::from([(
                provider_command(1),
                CommandOutput::success(
                    r#"{"total_count":2,"repositories":[{"id":1,"full_name":"acme/one"},{"id":1,"full_name":"acme/one"}]}"#,
                ),
            )])),
        };
        assert_eq!(
            observe_with_runner(&duplicate, 42).unwrap_err().reason,
            "provider_repository_identity_invalid"
        );
    }

    #[test]
    fn reconciliation_intersects_provider_with_deployment_allowlist() {
        let projection = reconcile_tenancy(
            42,
            &allowlist(),
            Ok(observation(&["acme/one", "other/not-allowed"])),
        );
        assert_eq!(
            projection.repositories["acme/one"].state,
            HostedTenancyState::Active
        );
        assert_eq!(
            projection.repositories["acme/two"].state,
            HostedTenancyState::Quarantined
        );
        assert!(!projection.repositories.contains_key("other/not-allowed"));
    }

    #[test]
    fn uncertainty_and_installation_mismatch_quarantine_every_row() {
        let error = reconcile_tenancy(
            42,
            &allowlist(),
            Err(HostedTenancyProviderError::new("provider_truncated")),
        );
        assert!(!error.complete);
        assert!(
            error
                .repositories
                .values()
                .all(|row| row.state == HostedTenancyState::Quarantined)
        );

        let mut wrong = observation(&["acme/one", "acme/two"]);
        wrong.installation_id = 99;
        let mismatch = reconcile_tenancy(42, &allowlist(), Ok(wrong));
        assert!(!mismatch.complete);
        assert_eq!(mismatch.reason, "installation_identity_mismatch");
    }

    #[test]
    fn transition_receipts_distinguish_added_retained_removed_and_unknown() {
        let mut first = reconcile_tenancy(42, &allowlist(), Ok(observation(&["acme/one"])));
        classify_tenancy_transitions(None, &mut first);
        assert_eq!(
            first.repositories["acme/one"].reason,
            "added_authoritative_access"
        );

        let mut second =
            reconcile_tenancy(42, &allowlist(), Ok(observation(&["acme/one", "acme/two"])));
        classify_tenancy_transitions(Some(&first), &mut second);
        assert_eq!(
            second.repositories["acme/one"].reason,
            "retained_authoritative_access"
        );
        assert_eq!(
            second.repositories["acme/two"].reason,
            "added_authoritative_access"
        );

        let mut removed = reconcile_tenancy(42, &allowlist(), Ok(observation(&[])));
        classify_tenancy_transitions(Some(&second), &mut removed);
        assert_eq!(
            removed.repositories["acme/one"].reason,
            "removed_from_authoritative_installation"
        );

        let mut unknown = reconcile_tenancy(
            42,
            &allowlist(),
            Err(HostedTenancyProviderError::new("provider_unavailable")),
        );
        classify_tenancy_transitions(Some(&second), &mut unknown);
        assert_eq!(
            unknown.repositories["acme/one"].reason,
            "quarantined_provider_unknown"
        );
    }

    #[test]
    fn projection_persists_atomically_without_secret_material() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tenancy.json");
        let projection =
            reconcile_tenancy(42, &allowlist(), Ok(observation(&["acme/one", "acme/two"])));
        persist_tenancy(&path, &projection).unwrap();
        let bytes = fs::read_to_string(&path).unwrap();
        assert!(!bytes.contains("ghs_secret_sentinel"));
        assert_eq!(load_tenancy(&path, 42, &allowlist()), projection);
    }
}
