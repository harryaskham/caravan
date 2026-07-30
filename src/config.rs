//! Strict repository-local `.caravan/config.yaml` parsing and validation.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{EventKind, HeadMergeActor};
use crate::root_merge::ExternalAutoMergePolicy;

/// Supported config schema version.
pub const CONFIG_VERSION: u32 = 1;
/// Version of this Cara reader.
pub const CARA_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Default repository-relative config path.
pub const DEFAULT_CONFIG_PATH: &str = ".caravan/config.yaml";
const MAX_INTERVAL_SECS: u64 = 86_400;
const MAX_COMMAND_TIMEOUT_SECS: u64 = 3_600;
const MAX_HOOK_TIMEOUT_SECS: u64 = 86_400;
const MAX_SYNC_CANDIDATES_PER_TICK: u32 = 100;
const MAX_SYNC_MUTATIONS_PER_TICK: u32 = 1_000;
const MAX_SYNC_GITHUB_REQUESTS_PER_TICK: u32 = 10_000;
const MAX_SYNC_DURATION_SECS: u64 = 3_600;

fn config_version() -> u32 {
    CONFIG_VERSION
}

fn legacy_min_cara_version() -> String {
    "0.0.0".to_owned()
}

fn default_loop_interval_secs() -> u64 {
    60
}

fn default_command_timeout_secs() -> u64 {
    30
}

fn default_repair_materialization_timeout_secs() -> u64 {
    180
}

fn default_hook_timeout_secs() -> u64 {
    30
}

fn default_journal_max_bytes() -> u64 {
    8 * 1024 * 1024
}

fn default_journal_max_archives() -> u32 {
    3
}

const fn default_sync_reserve_secs_per_command() -> u64 {
    15
}

fn default_sync_max_candidates_per_tick() -> u32 {
    8
}

fn default_sync_max_mutations_per_tick() -> u32 {
    64
}

fn default_sync_max_github_requests_per_tick() -> u32 {
    256
}

/// Historical behaviour: leave the operator on the PR that needs repairing.
const fn default_checkout_on_decision() -> bool {
    true
}

fn default_sync_max_duration_secs() -> u64 {
    120
}

fn default_missing_required_runs_grace_secs() -> u64 {
    300
}

fn default_sync_max_root_merges_per_tick() -> u32 {
    8
}

fn default_agent_priority_labels() -> Vec<String> {
    vec![
        "caravan-priority:high".to_owned(),
        "caravan-priority:normal".to_owned(),
        "caravan-priority:low".to_owned(),
    ]
}

fn compare_release_versions(left: &str, right: &str) -> Result<std::cmp::Ordering, ConfigError> {
    fn parse(value: &str) -> Result<[u64; 3], ConfigError> {
        let components = value
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                ConfigError::Validation(format!(
                    "Cara version `{value}` must be an X.Y.Z release version"
                ))
            })?;
        let [major, minor, patch] = components.as_slice() else {
            return Err(ConfigError::Validation(format!(
                "Cara version `{value}` must be an X.Y.Z release version"
            )));
        };
        Ok([*major, *minor, *patch])
    }

    Ok(parse(left)?.cmp(&parse(right)?))
}

/// Bounded repository event-journal retention policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct JournalConfig {
    /// Rotate the active JSONL file before it grows beyond this size.
    pub max_bytes: u64,
    /// Number of rotated JSONL files retained alongside the active file.
    pub max_archives: u32,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            max_bytes: default_journal_max_bytes(),
            max_archives: default_journal_max_archives(),
        }
    }
}

/// Foreground `cara loop` policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LoopConfig {
    pub interval_secs: u64,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_loop_interval_secs(),
        }
    }
}

/// Dedicated bounds for network-heavy isolated repair materialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RepairConfig {
    /// Whole-command bound for clone/fetch/checkout materialization phases.
    pub materialization_timeout_secs: u64,
}

impl Default for RepairConfig {
    fn default() -> Self {
        Self {
            materialization_timeout_secs: default_repair_materialization_timeout_secs(),
        }
    }
}

/// Explicit sync-owned provider actions. Every action is disabled by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SyncActionsConfig {
    /// Greedily admit eligible unlabelled PRs after existing caravans converge.
    pub join_unlabelled_prs: bool,
}

/// Whole-tick safety bounds for reconciliation and optional automatic admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    pub actions: SyncActionsConfig,
    /// Leave the working tree checked out on a decision's PR so it can be
    /// repaired in place.
    ///
    /// True is the historical behaviour and is right for an interactive
    /// checkout. It is wrong for an unattended sync worktree: cara checks out a
    /// PR to evaluate a decision and never returns, so the worktree silently
    /// becomes whatever PR was last inspected. One was found parked on a dead
    /// agent's branch 95 commits behind main, and every value read from it came
    /// from that old commit (bd-6f234e).
    #[serde(default = "default_checkout_on_decision")]
    pub checkout_on_decision: bool,
    /// Maximum fresh candidate generations considered by one `sync --all` tick.
    pub max_candidates_per_tick: u32,
    /// Maximum completed provider/branch mutations retained by one tick.
    pub max_mutations_per_tick: u32,
    /// Maximum authenticated `gh` subprocesses issued by one tick.
    pub max_github_requests_per_tick: u32,
    /// Absolute wall-clock ceiling for one complete sync tick.
    pub max_duration_secs: u64,
    /// Bounded wait, measured from the latest provider timestamp that could
    /// have triggered CI for the exact head, before a required context with
    /// zero reporting lineage is declared `missing_required_runs` instead of
    /// pending. Nothing is claimed missing inside this window.
    pub missing_required_runs_grace_secs: u64,
    /// Allow exactly one auditable check-suite rerequest against the unchanged
    /// head when required contexts have no reporting lineage. Disabling it only
    /// changes recovery to a typed operator-action problem; detection and the
    /// visible scheduler degradation always happen.
    pub retrigger_missing_required_runs: bool,
    /// Seconds reserved per planned provider command before the physical apply
    /// phase (bd-5528e6). Reserving the full `command_timeout_secs` for every
    /// slot is a worst case, not a plan, and makes larger caravans permanently
    /// unconvergeable. Capped by `command_timeout_secs`.
    ///
    /// The admission bound shares this price (bd-b1c7b7): the largest
    /// admissible chain and the reserve required to drain it are computed from
    /// one model, so raising a proven-safe `command_timeout_secs` never closes
    /// admission.
    pub reserve_secs_per_command: u64,
    /// Which actor merges the caravan root into the default branch.
    ///
    /// `caravan` means Cara promotes the root to the exact default branch and
    /// performs the squash merges itself, so no caravan member ever carries a
    /// provider `autoMergeRequest`. `github` (the default) keeps the historical
    /// delegation where the scheduler arms native squash auto-merge on the root.
    /// The name is deliberately self-describing: `github` never means "do not
    /// merge the head".
    ///
    /// Optional and backward compatible in both directions. Cara 0.0.7-0.0.10
    /// reject unknown config keys, so a repository must only add this field once
    /// every consumer of its `.caravan/config.yaml` has upgraded; and an absent
    /// field keeps the historical merge actor, so deploying a newer runtime
    /// against an existing config never silently changes who merges. Adopting
    /// caravan-owned merging is an explicit, ordered operator decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_merge_actor: Option<HeadMergeActor>,
    /// Historical boolean spelling of [`Self::head_merge_actor`]. `true` selects
    /// [`HeadMergeActor::Github`]; `false` selects [`HeadMergeActor::Caravan`].
    /// Ignored when `head_merge_actor` is set explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_merge_head: Option<bool>,
    /// What a caravan-owned tick does about a foreign `autoMergeRequest`.
    ///
    /// There must be exactly one merge actor: either Cara converges the foreign
    /// request away (`disable`, default) or it refuses to race it (`refuse`).
    pub external_auto_merge_policy: ExternalAutoMergePolicy,
    /// Maximum caravan roots one tick may promote and merge before deferring to
    /// the next bounded tick. A whole green caravan can drain in one tick, but
    /// every iteration re-reads exact provider facts and re-proves landing.
    pub max_root_merges_per_tick: u32,
}

impl SyncConfig {
    /// Resolve the configured merge actor from either spelling.
    ///
    /// An explicit `head_merge_actor` always wins; the historical
    /// `auto_merge_head` boolean is honoured for mixed-version rollouts
    /// (`true` = provider, `false` = caravan); absent configuration preserves
    /// the historical provider-native actor so an upgrade alone never changes
    /// who merges.
    #[must_use]
    pub fn resolved_head_merge_actor(&self) -> HeadMergeActor {
        self.head_merge_actor.unwrap_or_else(|| {
            self.auto_merge_head
                .map_or_else(HeadMergeActor::default, |native| {
                    if native {
                        HeadMergeActor::Github
                    } else {
                        HeadMergeActor::Caravan
                    }
                })
        })
    }
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            actions: SyncActionsConfig::default(),
            reserve_secs_per_command: default_sync_reserve_secs_per_command(),
            max_candidates_per_tick: default_sync_max_candidates_per_tick(),
            max_mutations_per_tick: default_sync_max_mutations_per_tick(),
            max_github_requests_per_tick: default_sync_max_github_requests_per_tick(),
            checkout_on_decision: default_checkout_on_decision(),
            max_duration_secs: default_sync_max_duration_secs(),
            missing_required_runs_grace_secs: default_missing_required_runs_grace_secs(),
            retrigger_missing_required_runs: true,
            head_merge_actor: None,
            auto_merge_head: None,
            external_auto_merge_policy: ExternalAutoMergePolicy::default(),
            max_root_merges_per_tick: default_sync_max_root_merges_per_tick(),
        }
    }
}

/// One shell hook policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HookConfig {
    /// Command executed by the platform shell with event JSON on stdin.
    pub command: String,
    /// Hard execution timeout.
    pub timeout_secs: u64,
    /// Whether hook failure makes the invoking Caravan operation fail.
    pub blocking: bool,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            timeout_secs: default_hook_timeout_secs(),
            blocking: false,
        }
    }
}

/// Strict repository policy. Unknown fields are rejected at every level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CaravanConfig {
    pub version: u32,
    /// Oldest Cara release authorized to read this policy. Repositories must
    /// advance their pinned reader before or atomically with this declaration.
    #[serde(default = "legacy_min_cara_version")]
    pub min_cara_version: String,
    pub force_merge: bool,
    /// Explicitly authorize lease-protected history rewriting so PR ancestry
    /// physically follows the Caravan chain. Safe default is disabled.
    pub rebase_on_join: bool,
    /// GitHub labels in highest-to-lowest automatic-admission priority order.
    /// Candidates without one of these labels follow all explicitly prioritized
    /// candidates. Labels in the `caravan-priority:` namespace which are not
    /// listed here are invalid and fail closed.
    pub agent_priority_labels: Vec<String>,
    /// Hard deadline for each lightweight Git or GitHub CLI subprocess.
    pub command_timeout_secs: u64,
    /// Exact `owner/name` this policy governs.
    ///
    /// `gh repo view` infers identity from git remotes only, takes the
    /// repository positionally, and does not honour `GH_REPO`. A managed agent
    /// checkout whose origin is a local daemon mirror therefore cannot name its
    /// own repository, and Cara failed before reaching the queue. Declaring it
    /// here removes the guess entirely (bd-ff639b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    pub repair: RepairConfig,
    pub sync: SyncConfig,
    #[serde(rename = "loop")]
    pub loop_config: LoopConfig,
    pub journal: JournalConfig,
    pub hooks: BTreeMap<EventKind, HookConfig>,
}

impl Default for CaravanConfig {
    fn default() -> Self {
        Self {
            version: config_version(),
            min_cara_version: CARA_VERSION.to_owned(),
            force_merge: false,
            rebase_on_join: false,
            agent_priority_labels: default_agent_priority_labels(),
            command_timeout_secs: default_command_timeout_secs(),
            repository: None,
            repair: RepairConfig::default(),
            sync: SyncConfig::default(),
            loop_config: LoopConfig::default(),
            journal: JournalConfig::default(),
            hooks: BTreeMap::new(),
        }
    }
}

impl CaravanConfig {
    /// Parse YAML and validate the complete policy.
    pub fn parse(yaml: &str) -> Result<Self, ConfigError> {
        Self::check_reader_compatibility(yaml, CARA_VERSION)?;
        let config: Self = serde_yaml::from_str(yaml).map_err(|error| ConfigError::Parse {
            path: None,
            message: error.to_string(),
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Check a repository's declared reader floor before strict schema parsing.
    ///
    /// Pin-validation CI can call this with the pinned Cara version. Performing
    /// this preflight first ensures a future additive section produces a typed
    /// upgrade error rather than an unrelated unknown-field diagnostic.
    pub fn check_reader_compatibility(yaml: &str, reader_version: &str) -> Result<(), ConfigError> {
        let document: serde_yaml::Value =
            serde_yaml::from_str(yaml).map_err(|error| ConfigError::Parse {
                path: None,
                message: error.to_string(),
            })?;
        let Some(mapping) = document.as_mapping() else {
            return Ok(());
        };
        let key = serde_yaml::Value::String("min_cara_version".to_owned());
        let Some(required) = mapping.get(&key) else {
            return Ok(());
        };
        let Some(required) = required.as_str() else {
            return Err(ConfigError::Validation(
                "min_cara_version must be a quoted X.Y.Z release version".to_owned(),
            ));
        };
        if compare_release_versions(reader_version, required)? == std::cmp::Ordering::Less {
            return Err(ConfigError::UpgradeRequired {
                required: required.to_owned(),
                running: reader_version.to_owned(),
            });
        }
        Ok(())
    }

    /// Read an explicit config file. Missing files are errors.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let metadata = fs::symlink_metadata(path).map_err(|error| ConfigError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        if !metadata.file_type().is_file() {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                message: "config must be a regular file, not a symlink or special file".to_owned(),
            });
        }
        let content = fs::read_to_string(path).map_err(|error| ConfigError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let config = Self::parse(&content).map_err(|error| error.with_path(path))?;
        config.validate()?;
        Ok(config)
    }

    /// Resolve an override or the repository default. An absent default file
    /// means default policy; an explicitly supplied missing path is an error.
    pub fn load_or_default(path: Option<&Path>) -> Result<LoadedConfig, ConfigError> {
        let resolved = path.map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), Path::to_path_buf);
        if !resolved.exists() {
            if path.is_some() {
                return Err(ConfigError::Read {
                    path: resolved,
                    message: "configured path does not exist".to_owned(),
                });
            }
            return Ok(LoadedConfig {
                path: resolved,
                existed: false,
                config: Self::default(),
            });
        }
        Ok(LoadedConfig {
            config: Self::load(&resolved)?,
            path: resolved,
            existed: true,
        })
    }

    /// Validate cross-field bounds after deserialization.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion {
                found: self.version,
                supported: CONFIG_VERSION,
            });
        }
        if compare_release_versions(CARA_VERSION, &self.min_cara_version)?
            == std::cmp::Ordering::Less
        {
            return Err(ConfigError::UpgradeRequired {
                required: self.min_cara_version.clone(),
                running: CARA_VERSION.to_owned(),
            });
        }
        let mut priority_labels = std::collections::BTreeSet::new();
        for label in &self.agent_priority_labels {
            if !label.starts_with("caravan-priority:")
                || label.trim() == "caravan-priority:"
                || label.trim() != label
            {
                return Err(ConfigError::Validation(format!(
                    "agent_priority_labels entry `{label}` must be an exact non-empty `caravan-priority:*` label"
                )));
            }
            if !priority_labels.insert(label) {
                return Err(ConfigError::Validation(format!(
                    "agent_priority_labels contains duplicate `{label}`"
                )));
            }
        }
        if !(1..=MAX_COMMAND_TIMEOUT_SECS).contains(&self.command_timeout_secs) {
            return Err(ConfigError::Validation(format!(
                "command_timeout_secs must be between 1 and {MAX_COMMAND_TIMEOUT_SECS}"
            )));
        }
        if !(1..=MAX_COMMAND_TIMEOUT_SECS).contains(&self.repair.materialization_timeout_secs) {
            return Err(ConfigError::Validation(format!(
                "repair.materialization_timeout_secs must be between 1 and {MAX_COMMAND_TIMEOUT_SECS}"
            )));
        }
        if self.sync.actions.join_unlabelled_prs && !self.rebase_on_join {
            return Err(ConfigError::Validation(
                "sync.actions.join_unlabelled_prs requires rebase_on_join: true".to_owned(),
            ));
        }
        if !(1..=MAX_SYNC_CANDIDATES_PER_TICK).contains(&self.sync.max_candidates_per_tick) {
            return Err(ConfigError::Validation(format!(
                "sync.max_candidates_per_tick must be between 1 and {MAX_SYNC_CANDIDATES_PER_TICK}"
            )));
        }
        if !(1..=MAX_SYNC_MUTATIONS_PER_TICK).contains(&self.sync.max_mutations_per_tick) {
            return Err(ConfigError::Validation(format!(
                "sync.max_mutations_per_tick must be between 1 and {MAX_SYNC_MUTATIONS_PER_TICK}"
            )));
        }
        if !(1..=MAX_SYNC_GITHUB_REQUESTS_PER_TICK)
            .contains(&self.sync.max_github_requests_per_tick)
        {
            return Err(ConfigError::Validation(format!(
                "sync.max_github_requests_per_tick must be between 1 and {MAX_SYNC_GITHUB_REQUESTS_PER_TICK}"
            )));
        }
        if !(1..=MAX_SYNC_DURATION_SECS).contains(&self.sync.max_duration_secs) {
            return Err(ConfigError::Validation(format!(
                "sync.max_duration_secs must be between 1 and {MAX_SYNC_DURATION_SECS}"
            )));
        }
        if !(1..=MAX_INTERVAL_SECS).contains(&self.loop_config.interval_secs) {
            return Err(ConfigError::Validation(format!(
                "loop.interval_secs must be between 1 and {MAX_INTERVAL_SECS}"
            )));
        }
        if !(1024..=1024 * 1024 * 1024).contains(&self.journal.max_bytes) {
            return Err(ConfigError::Validation(
                "journal.max_bytes must be between 1024 and 1073741824".to_owned(),
            ));
        }
        if self.journal.max_archives > 100 {
            return Err(ConfigError::Validation(
                "journal.max_archives must be between 0 and 100".to_owned(),
            ));
        }
        for (event, hook) in &self.hooks {
            if hook.command.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "hooks.{event:?}.command must not be empty"
                )));
            }
            if !(1..=MAX_HOOK_TIMEOUT_SECS).contains(&hook.timeout_secs) {
                return Err(ConfigError::Validation(format!(
                    "hooks.{event:?}.timeout_secs must be between 1 and {MAX_HOOK_TIMEOUT_SECS}"
                )));
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn hook(&self, event: EventKind) -> Option<&HookConfig> {
        self.hooks.get(&event)
    }
}

/// Resolved config plus source metadata useful in status output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub existed: bool,
    pub config: CaravanConfig,
}

/// Config discovery, parsing, and validation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    RepositoryNotFound {
        path: PathBuf,
        message: String,
    },
    /// The repository probe did not FAIL, it did not FINISH.
    ///
    /// Distinct from [`Self::RepositoryNotFound`] because the remedy is
    /// opposite: "not found" is terminal and sends a reader to check paths,
    /// symlinks, and permissions, while a timed-out `git rev-parse` means the
    /// repository is probably fine and the filesystem was slow. Live: a 5s probe
    /// deadline expired twice under load on a valid checkout whose path resolved
    /// instantly when run by hand (bd-f42a5e).
    RepositoryProbeTimeout {
        path: PathBuf,
        message: String,
    },
    Read {
        path: PathBuf,
        message: String,
    },
    Parse {
        path: Option<PathBuf>,
        message: String,
    },
    UnsupportedVersion {
        found: u32,
        supported: u32,
    },
    UpgradeRequired {
        required: String,
        running: String,
    },
    Validation(String),
}

impl ConfigError {
    fn with_path(self, path: &Path) -> Self {
        match self {
            Self::Parse { message, .. } => Self::Parse {
                path: Some(path.to_path_buf()),
                message,
            },
            other => other,
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepositoryNotFound { path, message } => {
                write!(formatter, "repository {}: {message}", path.display())
            }
            Self::RepositoryProbeTimeout { path, message } => {
                write!(
                    formatter,
                    "repository probe for {} did not finish: {message}",
                    path.display()
                )
            }
            Self::Read { path, message } => {
                write!(formatter, "read {}: {message}", path.display())
            }
            Self::Parse { path, message } => match path {
                Some(path) => write!(formatter, "parse {}: {message}", path.display()),
                None => write!(formatter, "parse caravan config: {message}"),
            },
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported caravan config version {found}; supported version is {supported}"
            ),
            Self::UpgradeRequired { required, running } => write!(
                formatter,
                "Cara {running} cannot read this repository policy; upgrade the pinned Cara runtime to {required} or newer before using this config"
            ),
            Self::Validation(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

impl StructuredError for ConfigError {
    fn category(&self) -> ErrorCategory {
        ErrorCategory::ConfigError
    }

    fn code(&self) -> String {
        match self {
            Self::RepositoryNotFound { .. } => "repository_not_found",
            Self::RepositoryProbeTimeout { .. } => "repository_probe_timeout",
            Self::Read { .. } => "config_read_failed",
            Self::Parse { .. } => "config_parse_failed",
            Self::UnsupportedVersion { .. } => "unsupported_config_version",
            Self::UpgradeRequired { .. } => "cara_upgrade_required",
            Self::Validation(_) => "invalid_config",
        }
        .to_owned()
    }

    fn message(&self) -> String {
        self.to_string()
    }

    fn details(&self) -> Option<Value> {
        match self {
            Self::RepositoryNotFound { path, .. } => Some(json!({
                "path": path,
                "mutated": false,
                "safe_next_action": "run Cara from inside a non-bare Git worktree"
            })),
            Self::RepositoryProbeTimeout { path, .. } => Some(json!({
                "path": path,
                "mutated": false,
                "retryable": true,
                "resumable": true,
                "safe_next_action": "retry; the repository resolved but `git rev-parse --show-toplevel` exceeded its deadline, which is filesystem or load pressure rather than a missing worktree"
            })),
            Self::Read { path, .. } => Some(json!({ "path": path })),
            Self::Parse { path, .. } => path.as_ref().map(|path| json!({ "path": path })),
            Self::UnsupportedVersion { found, supported } => {
                Some(json!({ "found": found, "supported": supported }))
            }
            Self::UpgradeRequired { required, running } => Some(json!({
                "required": required,
                "running": running,
                "mutated": false,
                "safe_next_action": format!("upgrade the pinned Cara runtime to {required} or newer")
            })),
            Self::Validation(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_merge_actor_is_optional_backward_compatible_and_self_describing() {
        // Two independent compatibility directions must hold.
        //
        // Forward: Cara 0.0.7-0.0.10 reject unknown config keys, so a
        // repository can only add the field once every consumer upgraded.
        //
        // Backward (bd-f8cf99, backcompat-default-github): deploying a newer
        // runtime against an *existing* config must never silently change who
        // merges that repository's pull requests. An absent field therefore
        // resolves to the historical provider-native actor, and caravan-owned
        // merging is an explicit, ordered operator decision.
        let legacy = CaravanConfig::parse("version: 1\n").expect("old documents still parse");
        assert_eq!(legacy.sync.head_merge_actor, None);
        assert_eq!(legacy.sync.auto_merge_head, None);
        assert_eq!(
            legacy.sync.resolved_head_merge_actor(),
            HeadMergeActor::Github,
            "an old config on a new runtime keeps the native merge actor"
        );
        assert_eq!(
            CaravanConfig::default().sync.resolved_head_merge_actor(),
            HeadMergeActor::Github
        );
        assert_eq!(HeadMergeActor::default(), HeadMergeActor::Github);
        assert_eq!(
            legacy.sync.external_auto_merge_policy,
            ExternalAutoMergePolicy::Disable
        );

        // Caravan-owned merging is opted into explicitly.
        let opted_in = CaravanConfig::parse(
            "version: 1\nsync:\n  head_merge_actor: caravan\n  external_auto_merge_policy: refuse\n",
        )
        .expect("the typed field parses");
        assert_eq!(
            opted_in.sync.resolved_head_merge_actor(),
            HeadMergeActor::Caravan
        );
        assert_eq!(
            opted_in.sync.external_auto_merge_policy,
            ExternalAutoMergePolicy::Refuse
        );
        let explicit_native =
            CaravanConfig::parse("version: 1\nsync:\n  head_merge_actor: github\n")
                .expect("the typed field parses");
        assert_eq!(
            explicit_native.sync.resolved_head_merge_actor(),
            HeadMergeActor::Github
        );

        // The historical boolean spelling stays accepted for mixed-version
        // rollouts: `true` means the provider is the merge actor.
        let boolean = CaravanConfig::parse("version: 1\nsync:\n  auto_merge_head: true\n")
            .expect("alias parses");
        assert_eq!(
            boolean.sync.resolved_head_merge_actor(),
            HeadMergeActor::Github
        );
        let boolean_off = CaravanConfig::parse("version: 1\nsync:\n  auto_merge_head: false\n")
            .expect("alias parses");
        assert_eq!(
            boolean_off.sync.resolved_head_merge_actor(),
            HeadMergeActor::Caravan
        );

        // An explicit typed field always wins over the historical alias.
        let both = CaravanConfig::parse(
            "version: 1\nsync:\n  head_merge_actor: caravan\n  auto_merge_head: true\n",
        )
        .expect("both spellings parse");
        assert_eq!(
            both.sync.resolved_head_merge_actor(),
            HeadMergeActor::Caravan
        );

        // A serialized default document never emits the new keys, so a config
        // written by this runtime is still readable by an older one.
        let rendered = serde_yaml::to_string(&CaravanConfig::default()).expect("config serializes");
        assert!(!rendered.contains("head_merge_actor"), "{rendered}");
        assert!(!rendered.contains("auto_merge_head"), "{rendered}");
    }

    #[test]
    fn the_merge_actor_key_must_be_nested_under_sync_and_fails_closed_otherwise() {
        // The migration record depends on this: a misplaced top-level key must
        // never be silently ignored, because "ignored" reads exactly like
        // "applied" to an operator and would leave a fleet believing it had
        // opted in. Strict parsing rejects it at every level, so the whole
        // policy fails closed and the mistake is visible before any mutation.
        let error = CaravanConfig::parse("version: 1\nhead_merge_actor: caravan\n")
            .expect_err("a top-level key is not a merge-actor opt-in");
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "config_parse_failed",
            "a misplaced key fails closed rather than resolving to a default"
        );
        let error = CaravanConfig::parse("version: 1\nauto_merge_head: true\n")
            .expect_err("the historical spelling is also sync-scoped");
        assert_eq!(
            mcp_cli::StructuredError::code(&error),
            "config_parse_failed"
        );

        // Correctly nested is the only form that opts in.
        let nested = CaravanConfig::parse("version: 1\nsync:\n  head_merge_actor: caravan\n")
            .expect("the nested key parses");
        assert_eq!(
            nested.sync.resolved_head_merge_actor(),
            HeadMergeActor::Caravan
        );
    }

    #[test]
    fn empty_document_uses_safe_defaults() {
        let config = CaravanConfig::parse("{}\n").expect("defaults parse");
        assert_eq!(config.version, 1);
        assert!(!config.force_merge);
        assert!(!config.rebase_on_join);
        assert_eq!(
            config.agent_priority_labels,
            vec![
                "caravan-priority:high".to_owned(),
                "caravan-priority:normal".to_owned(),
                "caravan-priority:low".to_owned(),
            ]
        );
        assert_eq!(config.command_timeout_secs, 30);
        assert_eq!(config.repair.materialization_timeout_secs, 180);
        assert!(!config.sync.actions.join_unlabelled_prs);
        assert_eq!(config.sync.max_candidates_per_tick, 8);
        assert_eq!(config.sync.max_mutations_per_tick, 64);
        assert_eq!(config.sync.max_github_requests_per_tick, 256);
        assert_eq!(config.sync.max_duration_secs, 120);
        assert_eq!(config.loop_config.interval_secs, 60);
        assert!(config.hooks.is_empty());
    }

    #[test]
    fn repository_example_parses_strictly() {
        let config = CaravanConfig::parse(
            r"
version: 1
force_merge: true
rebase_on_join: true
command_timeout_secs: 45
repair:
  materialization_timeout_secs: 240
sync:
  actions:
    join_unlabelled_prs: true
  max_candidates_per_tick: 5
  max_mutations_per_tick: 40
  max_github_requests_per_tick: 200
  max_duration_secs: 90
loop:
  interval_secs: 10
hooks:
  sync_failed:
    command: ./scripts/on-sync-failed
    timeout_secs: 45
    blocking: false
",
        )
        .expect("example config");
        assert!(config.force_merge);
        assert!(config.rebase_on_join);
        assert_eq!(config.command_timeout_secs, 45);
        assert_eq!(config.repair.materialization_timeout_secs, 240);
        assert!(config.sync.actions.join_unlabelled_prs);
        assert_eq!(config.sync.max_candidates_per_tick, 5);
        assert_eq!(config.sync.max_mutations_per_tick, 40);
        assert_eq!(config.sync.max_github_requests_per_tick, 200);
        assert_eq!(config.sync.max_duration_secs, 90);
        let hook = config.hook(EventKind::SyncFailed).expect("hook");
        assert_eq!(hook.command, "./scripts/on-sync-failed");
        assert_eq!(hook.timeout_secs, 45);
        assert!(!hook.blocking);
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_level() {
        let top = CaravanConfig::parse("version: 1\nsurprise: true\n").unwrap_err();
        assert_eq!(top.code(), "config_parse_failed");
        let nested =
            CaravanConfig::parse("version: 1\nloop:\n  interval_secs: 10\n  surprise: true\n")
                .unwrap_err();
        assert_eq!(nested.code(), "config_parse_failed");
        let repair = CaravanConfig::parse(
            "version: 1\nrepair:\n  materialization_timeout_secs: 180\n  surprise: true\n",
        )
        .unwrap_err();
        assert_eq!(repair.code(), "config_parse_failed");
        let sync = CaravanConfig::parse(
            "version: 1\nsync:\n  actions:\n    join_unlabelled_prs: true\n    surprise: true\n",
        )
        .unwrap_err();
        assert_eq!(sync.code(), "config_parse_failed");
        let hook = CaravanConfig::parse(
            "version: 1\nhooks:\n  sync_failed:\n    command: echo hi\n    surprise: true\n",
        )
        .unwrap_err();
        assert_eq!(hook.code(), "config_parse_failed");
    }

    #[test]
    fn rolling_config_fixtures_gate_old_readers_and_accept_old_configs() {
        let old_config = include_str!("../tests/fixtures/config-v0.0.6.yaml");
        let current_config = include_str!("../tests/fixtures/config-v0.0.7.yaml");
        let future_config = include_str!("../tests/fixtures/config-future.yaml");

        let parsed_old = CaravanConfig::parse(old_config).expect("new reader accepts old config");
        assert_eq!(parsed_old.min_cara_version, "0.0.0");
        assert!(!parsed_old.sync.actions.join_unlabelled_prs);
        assert_eq!(
            CaravanConfig::check_reader_compatibility(current_config, "0.0.6")
                .unwrap_err()
                .code(),
            "cara_upgrade_required"
        );
        CaravanConfig::parse(current_config).expect("released reader accepts sync policy");

        // Compatibility is checked before strict parsing. A pinned old reader
        // receives the upgrade gate even when a newer schema has unknown keys.
        assert_eq!(
            CaravanConfig::check_reader_compatibility(future_config, "0.0.7")
                .unwrap_err()
                .code(),
            "cara_upgrade_required"
        );
        assert_eq!(
            CaravanConfig::parse("version: 1\nmin_cara_version: \"0.0.7\"\nmisspelled_sync: {}\n")
                .unwrap_err()
                .code(),
            "config_parse_failed"
        );
    }

    #[test]
    fn required_run_policy_defaults_without_breaking_existing_sync_policy() {
        // A deployed config that predates missing-run detection must keep
        // parsing, and must arrive with detection armed rather than disabled.
        let parsed = CaravanConfig::parse(
            "version: 1\nsync:\n  max_duration_secs: 3600\n  max_mutations_per_tick: 64\n",
        )
        .expect("an existing sync policy still parses");
        assert_eq!(parsed.sync.missing_required_runs_grace_secs, 300);
        assert!(parsed.sync.retrigger_missing_required_runs);

        let explicit = CaravanConfig::parse(
            "version: 1\nsync:\n  missing_required_runs_grace_secs: 900\n  retrigger_missing_required_runs: false\n",
        )
        .expect("the policy is explicitly configurable");
        assert_eq!(explicit.sync.missing_required_runs_grace_secs, 900);
        assert!(!explicit.sync.retrigger_missing_required_runs);

        assert_eq!(
            CaravanConfig::parse("version: 1\nsync:\n  missing_required_runs_grace_sec: 900\n")
                .unwrap_err()
                .code(),
            "config_parse_failed",
            "a misspelled bound must never silently disable detection"
        );
    }

    #[test]
    fn upgrade_error_is_typed_and_non_mutating() {
        let error = CaravanConfig::check_reader_compatibility(
            "version: 1\nmin_cara_version: \"1.2.3\"\nunknown_future_policy: {}\n",
            "1.2.2",
        )
        .unwrap_err();
        assert_eq!(error.code(), "cara_upgrade_required");
        let details = error.details().expect("typed details");
        assert_eq!(details["required"], "1.2.3");
        assert_eq!(details["running"], "1.2.2");
        assert_eq!(details["mutated"], false);
    }

    #[test]
    fn priority_labels_are_ordered_and_strictly_validated() {
        let config = CaravanConfig::parse(
            "version: 1\nagent_priority_labels: [caravan-priority:urgent, caravan-priority:later]\n",
        )
        .unwrap();
        assert_eq!(config.agent_priority_labels[0], "caravan-priority:urgent");
        for yaml in [
            "version: 1\nagent_priority_labels: [urgent]\n",
            "version: 1\nagent_priority_labels: [caravan-priority:high, caravan-priority:high]\n",
            "version: 1\nagent_priority_labels: ['caravan-priority: ']\n",
        ] {
            assert_eq!(
                CaravanConfig::parse(yaml).unwrap_err().code(),
                "invalid_config"
            );
        }
    }

    #[test]
    fn automatic_admission_requires_physical_atomic_join() {
        let error =
            CaravanConfig::parse("version: 1\nsync:\n  actions:\n    join_unlabelled_prs: true\n")
                .unwrap_err();
        assert_eq!(error.code(), "invalid_config");
        assert!(error.to_string().contains("requires rebase_on_join"));
    }

    #[test]
    fn versions_intervals_and_hooks_are_validated() {
        assert_eq!(
            CaravanConfig::parse("version: 2\n").unwrap_err().code(),
            "unsupported_config_version"
        );
        assert_eq!(
            CaravanConfig::parse("version: 1\ncommand_timeout_secs: 0\n")
                .unwrap_err()
                .code(),
            "invalid_config"
        );
        assert_eq!(
            CaravanConfig::parse("version: 1\nrepair:\n  materialization_timeout_secs: 0\n",)
                .unwrap_err()
                .code(),
            "invalid_config"
        );
        assert_eq!(
            CaravanConfig::parse("version: 1\nsync:\n  max_candidates_per_tick: 0\n")
                .unwrap_err()
                .code(),
            "invalid_config"
        );
        assert_eq!(
            CaravanConfig::parse("version: 1\nsync:\n  max_mutations_per_tick: 0\n")
                .unwrap_err()
                .code(),
            "invalid_config"
        );
        assert_eq!(
            CaravanConfig::parse("version: 1\nsync:\n  max_github_requests_per_tick: 0\n")
                .unwrap_err()
                .code(),
            "invalid_config"
        );
        assert_eq!(
            CaravanConfig::parse("version: 1\nsync:\n  max_duration_secs: 0\n")
                .unwrap_err()
                .code(),
            "invalid_config"
        );
        assert_eq!(
            CaravanConfig::parse("version: 1\nloop:\n  interval_secs: 0\n")
                .unwrap_err()
                .code(),
            "invalid_config"
        );
        assert_eq!(
            CaravanConfig::parse("version: 1\nhooks:\n  sync_failed:\n    command: '  '\n")
                .unwrap_err()
                .code(),
            "invalid_config"
        );
    }

    #[test]
    fn absent_default_is_safe_but_missing_override_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();
        let loaded = CaravanConfig::load_or_default(None).expect("missing default is safe");
        std::env::set_current_dir(previous).unwrap();
        assert!(!loaded.existed);
        assert_eq!(loaded.path, PathBuf::from(DEFAULT_CONFIG_PATH));

        let error =
            CaravanConfig::load_or_default(Some(&temp.path().join("missing.yaml"))).unwrap_err();
        assert_eq!(error.code(), "config_read_failed");
    }

    #[test]
    fn explicit_file_load_records_parse_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        fs::write(&path, "unknown: true\n").unwrap();
        let error = CaravanConfig::load(&path).unwrap_err();
        assert_eq!(error.code(), "config_parse_failed");
        assert_eq!(error.details().unwrap()["path"], json!(path));
    }
}

#[cfg(test)]
mod decision_checkout_policy_tests {
    use super::*;

    /// bd-6f234e: cara checks out a PR to evaluate a decision and never returns
    /// the worktree to its base, so an unattended sync worktree silently becomes
    /// whatever PR was last inspected. Interactive use still wants that, so the
    /// historical behaviour stays the default and unattended callers opt out.
    #[test]
    fn decision_checkout_defaults_to_the_historical_interactive_behaviour() {
        assert!(CaravanConfig::default().sync.checkout_on_decision);
    }

    #[test]
    fn an_unattended_worktree_can_opt_out_without_touching_other_policy() {
        let config: CaravanConfig = serde_yaml::from_str(
            r"
version: 1
force_merge: false
rebase_on_join: false
command_timeout_secs: 30
sync:
  checkout_on_decision: false
",
        )
        .expect("partial config uses defaults for every unset field");

        assert!(!config.sync.checkout_on_decision);
        assert_eq!(
            config.sync.max_duration_secs,
            default_sync_max_duration_secs(),
            "opting out must not silently change any other bound"
        );
    }
}
