//! Strict repository-local `.caravan/config.yaml` parsing and validation.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use mcp_cli::{ErrorCategory, StructuredError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::EventKind;

/// Supported config schema version.
pub const CONFIG_VERSION: u32 = 1;
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

fn default_sync_max_candidates_per_tick() -> u32 {
    8
}

fn default_sync_max_mutations_per_tick() -> u32 {
    64
}

fn default_sync_max_github_requests_per_tick() -> u32 {
    256
}

fn default_sync_max_duration_secs() -> u64 {
    120
}

fn default_agent_priority_labels() -> Vec<String> {
    vec![
        "caravan-priority:high".to_owned(),
        "caravan-priority:normal".to_owned(),
        "caravan-priority:low".to_owned(),
    ]
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
    /// Maximum fresh candidate generations considered by one `sync --all` tick.
    pub max_candidates_per_tick: u32,
    /// Maximum completed provider/branch mutations retained by one tick.
    pub max_mutations_per_tick: u32,
    /// Maximum authenticated `gh` subprocesses issued by one tick.
    pub max_github_requests_per_tick: u32,
    /// Absolute wall-clock ceiling for one complete sync tick.
    pub max_duration_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            actions: SyncActionsConfig::default(),
            max_candidates_per_tick: default_sync_max_candidates_per_tick(),
            max_mutations_per_tick: default_sync_max_mutations_per_tick(),
            max_github_requests_per_tick: default_sync_max_github_requests_per_tick(),
            max_duration_secs: default_sync_max_duration_secs(),
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
            force_merge: false,
            rebase_on_join: false,
            agent_priority_labels: default_agent_priority_labels(),
            command_timeout_secs: default_command_timeout_secs(),
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
        let config: Self = serde_yaml::from_str(yaml).map_err(|error| ConfigError::Parse {
            path: None,
            message: error.to_string(),
        })?;
        config.validate()?;
        Ok(config)
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
            Self::Read { .. } => "config_read_failed",
            Self::Parse { .. } => "config_parse_failed",
            Self::UnsupportedVersion { .. } => "unsupported_config_version",
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
            Self::Read { path, .. } => Some(json!({ "path": path })),
            Self::Parse { path, .. } => path.as_ref().map(|path| json!({ "path": path })),
            Self::UnsupportedVersion { found, supported } => {
                Some(json!({ "found": found, "supported": supported }))
            }
            Self::Validation(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
