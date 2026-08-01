//! Backend-neutral remote writer lease and fencing protocol.
//!
//! This module defines the cross-host CAS contract only. Production mutation
//! paths do not consume it yet, so non-local writer config remains fail-closed.

use std::fmt;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::command::{CommandIntent, CommandMutationFence, CommandRunner, CommandSpec};

const LEASE_SCHEMA_VERSION: u32 = 1;
const MAX_PROTOCOL_BYTES: usize = 16 * 1024;
const MAX_IDENTITY_BYTES: usize = 256;
const MAX_LEASE_TTL_MS: u64 = 60 * 60 * 1_000;

/// Exact provider tenancy fenced by one lease.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteLeaseKey {
    pub host: String,
    pub owner: String,
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<u64>,
}

impl RemoteLeaseKey {
    fn validate(&self) -> Result<(), RemoteLeaseError> {
        for (name, value) in [
            ("host", self.host.as_str()),
            ("owner", self.owner.as_str()),
            ("repository", self.repository.as_str()),
        ] {
            validate_identity(name, value)?;
        }
        if self.installation_id == Some(0) {
            return Err(RemoteLeaseError::Invalid(
                "installation ID must be positive".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Atomic lease acquisition input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteLeaseAcquire {
    pub key: RemoteLeaseKey,
    pub writer_owner: String,
    pub operation_id: String,
    pub now_unix_ms: u64,
    pub ttl_ms: u64,
    pub heartbeat_ms: u64,
}

impl RemoteLeaseAcquire {
    fn validate(&self) -> Result<(), RemoteLeaseError> {
        self.key.validate()?;
        validate_identity("writer owner", &self.writer_owner)?;
        validate_identity("operation ID", &self.operation_id)?;
        if self.ttl_ms == 0
            || self.ttl_ms > MAX_LEASE_TTL_MS
            || self.heartbeat_ms == 0
            || self.heartbeat_ms >= self.ttl_ms
            || self.now_unix_ms.checked_add(self.ttl_ms).is_none()
            || self.now_unix_ms.checked_add(self.heartbeat_ms).is_none()
        {
            return Err(RemoteLeaseError::Invalid(
                "lease TTL/heartbeat bounds are invalid".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Secret-free durable lease and fencing receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteLeaseGrant {
    pub schema_version: u32,
    pub key: RemoteLeaseKey,
    pub writer_owner: String,
    pub operation_id: String,
    pub fencing_token: u64,
    pub heartbeat_due_unix_ms: u64,
    pub expires_unix_ms: u64,
    pub backend_revision: String,
}

impl RemoteLeaseGrant {
    fn validate(&self) -> Result<(), RemoteLeaseError> {
        if self.schema_version != LEASE_SCHEMA_VERSION {
            return Err(RemoteLeaseError::Invalid(
                "unsupported remote lease schema version".to_owned(),
            ));
        }
        self.key.validate()?;
        validate_identity("writer owner", &self.writer_owner)?;
        validate_identity("operation ID", &self.operation_id)?;
        validate_identity("backend revision", &self.backend_revision)?;
        if self.fencing_token == 0 || self.heartbeat_due_unix_ms >= self.expires_unix_ms {
            return Err(RemoteLeaseError::Invalid(
                "remote lease grant has invalid fence or expiry".to_owned(),
            ));
        }
        Ok(())
    }

    fn same_holder(&self, request: &RemoteLeaseAcquire) -> bool {
        self.key == request.key
            && self.writer_owner == request.writer_owner
            && self.operation_id == request.operation_id
    }

    fn exact_fence(&self, other: &Self) -> bool {
        self.key == other.key
            && self.writer_owner == other.writer_owner
            && self.operation_id == other.operation_id
            && self.fencing_token == other.fencing_token
    }

    #[must_use]
    pub fn live_at(&self, now_unix_ms: u64) -> bool {
        now_unix_ms < self.expires_unix_ms
    }
}

/// Typed lease backend failure. Values are deliberately secret-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteLeaseError {
    Invalid(String),
    Contended {
        writer_owner: String,
        operation_id: String,
        fencing_token: u64,
        expires_unix_ms: u64,
    },
    Execution(String),
    Lost(String),
    Indeterminate(String),
}

impl fmt::Display for RemoteLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid remote lease data: {message}"),
            Self::Contended {
                writer_owner,
                operation_id,
                fencing_token,
                expires_unix_ms,
            } => write!(
                formatter,
                "remote lease held by {writer_owner}/{operation_id} at fence {fencing_token} until {expires_unix_ms}"
            ),
            Self::Execution(message) => write!(formatter, "remote lease backend failed: {message}"),
            Self::Lost(message) => write!(formatter, "remote writer fence lost: {message}"),
            Self::Indeterminate(message) => {
                write!(
                    formatter,
                    "remote lease outcome is indeterminate: {message}"
                )
            }
        }
    }
}

impl std::error::Error for RemoteLeaseError {}

/// Atomic backend operations required by a remote writer fence.
pub trait RemoteWriterLease: Send + Sync {
    fn acquire(&self, request: &RemoteLeaseAcquire) -> Result<RemoteLeaseGrant, RemoteLeaseError>;
    fn inspect(&self, key: &RemoteLeaseKey) -> Result<Option<RemoteLeaseGrant>, RemoteLeaseError>;
    fn renew(
        &self,
        grant: &RemoteLeaseGrant,
        now_unix_ms: u64,
        ttl_ms: u64,
        heartbeat_ms: u64,
    ) -> Result<RemoteLeaseGrant, RemoteLeaseError>;
    fn release(&self, grant: &RemoteLeaseGrant) -> Result<bool, RemoteLeaseError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BrokerOperation {
    Acquire,
    Inspect,
    Renew,
    Release,
}

impl BrokerOperation {
    const fn code(self) -> &'static str {
        match self {
            Self::Acquire => "acquire",
            Self::Inspect => "inspect",
            Self::Renew => "renew",
            Self::Release => "release",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum BrokerRequest<'a> {
    Acquire {
        schema_version: u32,
        request: &'a RemoteLeaseAcquire,
    },
    Inspect {
        schema_version: u32,
        key: &'a RemoteLeaseKey,
    },
    Renew {
        schema_version: u32,
        grant: &'a RemoteLeaseGrant,
        now_unix_ms: u64,
        ttl_ms: u64,
        heartbeat_ms: u64,
    },
    Release {
        schema_version: u32,
        grant: &'a RemoteLeaseGrant,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrokerResponse {
    schema_version: u32,
    operation: BrokerOperation,
    #[serde(default)]
    grant: Option<RemoteLeaseGrant>,
    #[serde(default)]
    released: bool,
    #[serde(default)]
    contended: bool,
}

impl BrokerResponse {
    fn validate_shape(&self) -> Result<(), RemoteLeaseError> {
        let valid = match self.operation {
            BrokerOperation::Acquire => self.grant.is_some() && !self.released,
            BrokerOperation::Renew => self.grant.is_some() && !self.released && !self.contended,
            BrokerOperation::Inspect => !self.released && !self.contended,
            BrokerOperation::Release => self.grant.is_none() && !self.contended,
        };
        if valid {
            Ok(())
        } else {
            Err(RemoteLeaseError::Invalid(
                "broker response fields do not match the operation".to_owned(),
            ))
        }
    }
}

/// External CAS broker adapter. The executable receives strict JSON on stdin;
/// backend credentials are inherited through deployment environment, never argv.
pub struct CommandRemoteWriterLease<R> {
    runner: R,
    command: String,
}

impl<R> CommandRemoteWriterLease<R> {
    pub fn new(runner: R, command: impl Into<String>) -> Result<Self, RemoteLeaseError> {
        let command = command.into();
        if command.trim().is_empty() {
            return Err(RemoteLeaseError::Invalid(
                "remote lease broker command is empty".to_owned(),
            ));
        }
        Ok(Self { runner, command })
    }
}

impl<R: CommandRunner + Send + Sync> CommandRemoteWriterLease<R> {
    fn invoke(
        &self,
        operation: BrokerOperation,
        request: &BrokerRequest<'_>,
    ) -> Result<BrokerResponse, RemoteLeaseError> {
        let input = serde_json::to_string(request)
            .map_err(|_| RemoteLeaseError::Invalid("could not encode lease request".to_owned()))?;
        if input.len() > MAX_PROTOCOL_BYTES {
            return Err(RemoteLeaseError::Invalid(
                "remote lease request exceeds protocol bound".to_owned(),
            ));
        }
        let output = self
            .runner
            .run(
                &CommandSpec::new(&self.command)
                    .env("CARA_REMOTE_LEASE_OPERATION", operation.code())
                    .stdin(input),
            )
            .map_err(|_| {
                RemoteLeaseError::Execution("broker command did not complete".to_owned())
            })?;
        if !output.is_success() {
            return Err(RemoteLeaseError::Execution(
                "broker command returned a nonzero status".to_owned(),
            ));
        }
        if output.stdout.len() > MAX_PROTOCOL_BYTES {
            return Err(RemoteLeaseError::Invalid(
                "remote lease response exceeds protocol bound".to_owned(),
            ));
        }
        let response: BrokerResponse = serde_json::from_str(&output.stdout).map_err(|_| {
            RemoteLeaseError::Invalid("broker returned invalid strict JSON".to_owned())
        })?;
        if response.schema_version != LEASE_SCHEMA_VERSION || response.operation != operation {
            return Err(RemoteLeaseError::Invalid(
                "broker response operation/schema mismatch".to_owned(),
            ));
        }
        response.validate_shape()?;
        if let Some(grant) = &response.grant {
            grant.validate()?;
        }
        Ok(response)
    }

    fn inspect_once(
        &self,
        key: &RemoteLeaseKey,
    ) -> Result<Option<RemoteLeaseGrant>, RemoteLeaseError> {
        key.validate()?;
        let response = self.invoke(
            BrokerOperation::Inspect,
            &BrokerRequest::Inspect {
                schema_version: LEASE_SCHEMA_VERSION,
                key,
            },
        )?;
        Ok(response.grant)
    }
}

impl<R: CommandRunner + Send + Sync> RemoteWriterLease for CommandRemoteWriterLease<R> {
    fn acquire(&self, request: &RemoteLeaseAcquire) -> Result<RemoteLeaseGrant, RemoteLeaseError> {
        request.validate()?;
        match self.invoke(
            BrokerOperation::Acquire,
            &BrokerRequest::Acquire {
                schema_version: LEASE_SCHEMA_VERSION,
                request,
            },
        ) {
            Ok(response) => {
                let grant = response.grant.ok_or_else(|| {
                    RemoteLeaseError::Invalid("acquire response omitted grant".to_owned())
                })?;
                if response.contended {
                    if !grant.live_at(request.now_unix_ms) {
                        return Err(RemoteLeaseError::Invalid(
                            "contention response named an expired holder".to_owned(),
                        ));
                    }
                    return Err(RemoteLeaseError::Contended {
                        writer_owner: grant.writer_owner,
                        operation_id: grant.operation_id,
                        fencing_token: grant.fencing_token,
                        expires_unix_ms: grant.expires_unix_ms,
                    });
                }
                if !grant.same_holder(request) || !grant.live_at(request.now_unix_ms) {
                    return Err(RemoteLeaseError::Lost(
                        "acquire returned wrong or expired ownership".to_owned(),
                    ));
                }
                Ok(grant)
            }
            Err(RemoteLeaseError::Execution(_)) => {
                let observed = self.inspect_once(&request.key)?;
                observed
                    .filter(|grant| {
                        grant.same_holder(request) && grant.live_at(request.now_unix_ms)
                    })
                    .ok_or_else(|| {
                        RemoteLeaseError::Indeterminate(
                            "acquire failed and exact ownership was not rediscovered".to_owned(),
                        )
                    })
            }
            Err(error) => Err(error),
        }
    }

    fn inspect(&self, key: &RemoteLeaseKey) -> Result<Option<RemoteLeaseGrant>, RemoteLeaseError> {
        self.inspect_once(key)
    }

    fn renew(
        &self,
        grant: &RemoteLeaseGrant,
        now_unix_ms: u64,
        ttl_ms: u64,
        heartbeat_ms: u64,
    ) -> Result<RemoteLeaseGrant, RemoteLeaseError> {
        grant.validate()?;
        let request = RemoteLeaseAcquire {
            key: grant.key.clone(),
            writer_owner: grant.writer_owner.clone(),
            operation_id: grant.operation_id.clone(),
            now_unix_ms,
            ttl_ms,
            heartbeat_ms,
        };
        request.validate()?;
        match self.invoke(
            BrokerOperation::Renew,
            &BrokerRequest::Renew {
                schema_version: LEASE_SCHEMA_VERSION,
                grant,
                now_unix_ms,
                ttl_ms,
                heartbeat_ms,
            },
        ) {
            Ok(response) => validate_renewed_grant(grant, response.grant),
            Err(RemoteLeaseError::Execution(_)) => {
                let observed = self.inspect_once(&grant.key)?;
                validate_renewed_grant(grant, observed).map_err(|_| {
                    RemoteLeaseError::Indeterminate(
                        "renew failed and exact ownership was not rediscovered".to_owned(),
                    )
                })
            }
            Err(error) => Err(error),
        }
    }

    fn release(&self, grant: &RemoteLeaseGrant) -> Result<bool, RemoteLeaseError> {
        grant.validate()?;
        match self.invoke(
            BrokerOperation::Release,
            &BrokerRequest::Release {
                schema_version: LEASE_SCHEMA_VERSION,
                grant,
            },
        ) {
            Ok(response) => Ok(response.released),
            Err(RemoteLeaseError::Execution(_)) => match self.inspect_once(&grant.key)? {
                None => Ok(true),
                Some(observed) if observed.exact_fence(grant) => {
                    Err(RemoteLeaseError::Indeterminate(
                        "release failed and ownership remains".to_owned(),
                    ))
                }
                Some(_) => Err(RemoteLeaseError::Lost(
                    "release ambiguity observed a replacement fence".to_owned(),
                )),
            },
            Err(error) => Err(error),
        }
    }
}

fn validate_renewed_grant(
    previous: &RemoteLeaseGrant,
    renewed: Option<RemoteLeaseGrant>,
) -> Result<RemoteLeaseGrant, RemoteLeaseError> {
    let renewed = renewed
        .ok_or_else(|| RemoteLeaseError::Invalid("renew response omitted grant".to_owned()))?;
    renewed.validate()?;
    if !renewed.exact_fence(previous) || renewed.expires_unix_ms <= previous.expires_unix_ms {
        return Err(RemoteLeaseError::Lost(
            "renew changed the fence or failed to advance expiry".to_owned(),
        ));
    }
    Ok(renewed)
}

/// Owned lease lifecycle. Drop attempts exact release but never claims success.
pub struct RemoteLeaseGuard {
    backend: Arc<dyn RemoteWriterLease>,
    grant: RemoteLeaseGrant,
    released: bool,
}

impl fmt::Debug for RemoteLeaseGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteLeaseGuard")
            .field("grant", &self.grant)
            .field("released", &self.released)
            .finish_non_exhaustive()
    }
}

impl RemoteLeaseGuard {
    pub fn acquire(
        backend: Arc<dyn RemoteWriterLease>,
        request: &RemoteLeaseAcquire,
    ) -> Result<Self, RemoteLeaseError> {
        let grant = backend.acquire(request)?;
        if !grant.same_holder(request) || !grant.live_at(request.now_unix_ms) {
            return Err(RemoteLeaseError::Lost(
                "acquire returned wrong or expired ownership".to_owned(),
            ));
        }
        Ok(Self {
            backend,
            grant,
            released: false,
        })
    }

    #[must_use]
    pub fn grant(&self) -> &RemoteLeaseGrant {
        &self.grant
    }

    pub fn revalidate(&self, now_unix_ms: u64) -> Result<(), RemoteLeaseError> {
        let observed = self.backend.inspect(&self.grant.key)?;
        match observed {
            Some(observed)
                if observed.exact_fence(&self.grant) && observed.live_at(now_unix_ms) =>
            {
                Ok(())
            }
            _ => Err(RemoteLeaseError::Lost(
                "exact live fencing token was not observed".to_owned(),
            )),
        }
    }

    pub fn renew(
        &mut self,
        now_unix_ms: u64,
        ttl_ms: u64,
        heartbeat_ms: u64,
    ) -> Result<(), RemoteLeaseError> {
        self.grant = self
            .backend
            .renew(&self.grant, now_unix_ms, ttl_ms, heartbeat_ms)?;
        Ok(())
    }

    pub fn release(mut self) -> Result<bool, RemoteLeaseError> {
        let released = self.backend.release(&self.grant)?;
        self.released = released;
        Ok(released)
    }
}

impl Drop for RemoteLeaseGuard {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.backend.release(&self.grant);
        }
    }
}

impl CommandMutationFence for RemoteLeaseGuard {
    fn before_write(&self, _intent: CommandIntent) -> Result<(), String> {
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock is before the Unix epoch".to_owned())?
            .as_millis()
            .try_into()
            .map_err(|_| "system clock exceeds remote lease range".to_owned())?;
        self.revalidate(now_unix_ms)
            .map_err(|error| error.to_string())
    }
}

fn validate_identity(name: &str, value: &str) -> Result<(), RemoteLeaseError> {
    if value.is_empty()
        || value.len() > MAX_IDENTITY_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(RemoteLeaseError::Invalid(format!(
            "{name} must be bounded, non-empty, trimmed, and single-line"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;
    use std::thread;

    use super::*;
    use crate::command::{CommandOutput, CommandRunError, FencedCommandRunner};

    #[derive(Clone, Default)]
    struct ScriptedRunner {
        responses: Arc<Mutex<VecDeque<Result<CommandOutput, CommandRunError>>>>,
        requests: Arc<Mutex<Vec<CommandSpec>>>,
    }

    impl ScriptedRunner {
        fn with(responses: Vec<Result<CommandOutput, CommandRunError>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, request: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
            self.requests.lock().unwrap().push(request.clone());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response")
        }
    }

    #[derive(Default)]
    struct FakeCas {
        state: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        grants: BTreeMap<RemoteLeaseKey, RemoteLeaseGrant>,
        last_fence: BTreeMap<RemoteLeaseKey, u64>,
    }

    impl FakeCas {
        fn replacement(&self, request: &RemoteLeaseAcquire) -> RemoteLeaseGrant {
            let mut state = self.state.lock().unwrap();
            let next = state
                .last_fence
                .get(&request.key)
                .copied()
                .unwrap_or(0)
                .saturating_add(1);
            state.last_fence.insert(request.key.clone(), next);
            let grant = RemoteLeaseGrant {
                schema_version: LEASE_SCHEMA_VERSION,
                key: request.key.clone(),
                writer_owner: request.writer_owner.clone(),
                operation_id: request.operation_id.clone(),
                fencing_token: next,
                heartbeat_due_unix_ms: request.now_unix_ms + request.heartbeat_ms,
                expires_unix_ms: request.now_unix_ms + request.ttl_ms,
                backend_revision: format!("revision-{next}"),
            };
            state.grants.insert(request.key.clone(), grant.clone());
            grant
        }
    }

    impl RemoteWriterLease for FakeCas {
        fn acquire(
            &self,
            request: &RemoteLeaseAcquire,
        ) -> Result<RemoteLeaseGrant, RemoteLeaseError> {
            request.validate()?;
            if let Some(existing) = self
                .state
                .lock()
                .unwrap()
                .grants
                .get(&request.key)
                .cloned()
                .filter(|grant| grant.live_at(request.now_unix_ms))
            {
                return Err(RemoteLeaseError::Contended {
                    writer_owner: existing.writer_owner,
                    operation_id: existing.operation_id,
                    fencing_token: existing.fencing_token,
                    expires_unix_ms: existing.expires_unix_ms,
                });
            }
            Ok(self.replacement(request))
        }

        fn inspect(
            &self,
            key: &RemoteLeaseKey,
        ) -> Result<Option<RemoteLeaseGrant>, RemoteLeaseError> {
            Ok(self.state.lock().unwrap().grants.get(key).cloned())
        }

        fn renew(
            &self,
            grant: &RemoteLeaseGrant,
            now_unix_ms: u64,
            ttl_ms: u64,
            heartbeat_ms: u64,
        ) -> Result<RemoteLeaseGrant, RemoteLeaseError> {
            let mut state = self.state.lock().unwrap();
            let Some(current) = state.grants.get_mut(&grant.key) else {
                return Err(RemoteLeaseError::Lost("lease is absent".to_owned()));
            };
            if !current.exact_fence(grant) || !current.live_at(now_unix_ms) {
                return Err(RemoteLeaseError::Lost(
                    "renew did not match a live exact fence".to_owned(),
                ));
            }
            current.heartbeat_due_unix_ms = now_unix_ms + heartbeat_ms;
            current.expires_unix_ms = now_unix_ms + ttl_ms;
            current.backend_revision = format!("{}-renewed", current.backend_revision);
            Ok(current.clone())
        }

        fn release(&self, grant: &RemoteLeaseGrant) -> Result<bool, RemoteLeaseError> {
            let mut state = self.state.lock().unwrap();
            match state.grants.get(&grant.key) {
                Some(current) if current.exact_fence(grant) => {
                    state.grants.remove(&grant.key);
                    Ok(true)
                }
                Some(_) => Err(RemoteLeaseError::Lost(
                    "release did not match current fence".to_owned(),
                )),
                None => Ok(true),
            }
        }
    }

    fn request(owner: &str, operation: &str, now: u64) -> RemoteLeaseAcquire {
        RemoteLeaseAcquire {
            key: RemoteLeaseKey {
                host: "github.com".to_owned(),
                owner: "owner".to_owned(),
                repository: "repo".to_owned(),
                installation_id: Some(42),
            },
            writer_owner: owner.to_owned(),
            operation_id: operation.to_owned(),
            now_unix_ms: now,
            ttl_ms: 10_000,
            heartbeat_ms: 2_000,
        }
    }

    fn grant_for(request: &RemoteLeaseAcquire, fence: u64) -> RemoteLeaseGrant {
        RemoteLeaseGrant {
            schema_version: LEASE_SCHEMA_VERSION,
            key: request.key.clone(),
            writer_owner: request.writer_owner.clone(),
            operation_id: request.operation_id.clone(),
            fencing_token: fence,
            heartbeat_due_unix_ms: request.now_unix_ms + request.heartbeat_ms,
            expires_unix_ms: request.now_unix_ms + request.ttl_ms,
            backend_revision: format!("revision-{fence}"),
        }
    }

    fn broker_output(
        operation: BrokerOperation,
        grant: Option<RemoteLeaseGrant>,
        released: bool,
    ) -> CommandOutput {
        CommandOutput::success(
            serde_json::to_string(&BrokerResponse {
                schema_version: LEASE_SCHEMA_VERSION,
                operation,
                grant,
                released,
                contended: false,
            })
            .unwrap(),
        )
    }

    #[test]
    fn command_broker_recovers_ambiguous_acquire_by_exact_inspection() {
        let acquisition = request("host-a", "operation", 1_000);
        let grant = grant_for(&acquisition, 7);
        let failed_command = CommandSpec::new("lease-broker");
        let runner = ScriptedRunner::with(vec![
            Err(CommandRunError::Spawn {
                command: failed_command,
                message: "ambiguous transport".to_owned(),
            }),
            Ok(broker_output(
                BrokerOperation::Inspect,
                Some(grant.clone()),
                false,
            )),
        ]);
        let broker = CommandRemoteWriterLease::new(runner.clone(), "lease-broker").unwrap();
        assert_eq!(broker.acquire(&acquisition).unwrap(), grant);
        let requests = runner.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(format!("{:?}", requests[0]).contains("stdin: Some(\"<redacted>\")"));
        assert!(!format!("{:?}", requests[0]).contains("operation"));
    }

    #[test]
    fn command_broker_recovers_ambiguous_renew_and_release_once() {
        let acquisition = request("host-a", "operation", 1_000);
        let previous = grant_for(&acquisition, 7);
        let mut renewed = previous.clone();
        renewed.heartbeat_due_unix_ms = 7_000;
        renewed.expires_unix_ms = 23_000;
        renewed.backend_revision = "revision-7-renewed".to_owned();
        let failed = || {
            Err(CommandRunError::Spawn {
                command: CommandSpec::new("lease-broker"),
                message: "ambiguous transport".to_owned(),
            })
        };
        let renew_runner = ScriptedRunner::with(vec![
            failed(),
            Ok(broker_output(
                BrokerOperation::Inspect,
                Some(renewed.clone()),
                false,
            )),
        ]);
        let broker = CommandRemoteWriterLease::new(renew_runner, "lease-broker").unwrap();
        assert_eq!(
            broker.renew(&previous, 3_000, 20_000, 4_000).unwrap(),
            renewed
        );

        let release_runner = ScriptedRunner::with(vec![
            failed(),
            Ok(broker_output(BrokerOperation::Inspect, None, false)),
        ]);
        let broker = CommandRemoteWriterLease::new(release_runner, "lease-broker").unwrap();
        assert!(broker.release(&previous).unwrap());
    }

    #[test]
    fn command_broker_rejects_malformed_shape_and_wrong_holder() {
        let acquisition = request("host-a", "operation", 1_000);
        let malformed = ScriptedRunner::with(vec![Ok(CommandOutput::success(
            r#"{"schema_version":1,"operation":"acquire","surprise":true}"#,
        ))]);
        let broker = CommandRemoteWriterLease::new(malformed, "lease-broker").unwrap();
        assert!(matches!(
            broker.acquire(&acquisition),
            Err(RemoteLeaseError::Invalid(_))
        ));

        let mut wrong = grant_for(&acquisition, 1);
        wrong.writer_owner = "other-host".to_owned();
        let wrong_holder = ScriptedRunner::with(vec![Ok(broker_output(
            BrokerOperation::Acquire,
            Some(wrong),
            false,
        ))]);
        let broker = CommandRemoteWriterLease::new(wrong_holder, "lease-broker").unwrap();
        assert!(matches!(
            broker.acquire(&acquisition),
            Err(RemoteLeaseError::Lost(_))
        ));
    }

    #[test]
    fn command_broker_reports_a_live_contending_holder() {
        let acquisition = request("host-a", "operation", 1_000);
        let mut holder = grant_for(&acquisition, 8);
        holder.writer_owner = "host-b".to_owned();
        holder.operation_id = "other-operation".to_owned();
        let output = CommandOutput::success(
            serde_json::to_string(&BrokerResponse {
                schema_version: LEASE_SCHEMA_VERSION,
                operation: BrokerOperation::Acquire,
                grant: Some(holder),
                released: false,
                contended: true,
            })
            .unwrap(),
        );
        let broker =
            CommandRemoteWriterLease::new(ScriptedRunner::with(vec![Ok(output)]), "lease-broker")
                .unwrap();
        assert!(matches!(
            broker.acquire(&acquisition),
            Err(RemoteLeaseError::Contended {
                fencing_token: 8,
                ..
            })
        ));
    }

    #[test]
    fn command_broker_rejects_operation_field_contradictions() {
        let acquisition = request("host-a", "operation", 1_000);
        let contradictory = ScriptedRunner::with(vec![Ok(broker_output(
            BrokerOperation::Acquire,
            Some(grant_for(&acquisition, 1)),
            true,
        ))]);
        let broker = CommandRemoteWriterLease::new(contradictory, "lease-broker").unwrap();
        assert!(matches!(
            broker.acquire(&acquisition),
            Err(RemoteLeaseError::Invalid(_))
        ));
    }

    #[test]
    fn cas_excludes_a_second_live_writer_and_takeover_is_monotonic() {
        let backend = FakeCas::default();
        let first = backend.acquire(&request("host-a", "op-a", 1_000)).unwrap();
        assert!(matches!(
            backend.acquire(&request("host-b", "op-b", 2_000)),
            Err(RemoteLeaseError::Contended { .. })
        ));
        let second = backend
            .acquire(&request("host-b", "op-b", first.expires_unix_ms))
            .unwrap();
        assert!(second.fencing_token > first.fencing_token);
        assert!(matches!(
            backend.release(&first),
            Err(RemoteLeaseError::Lost(_))
        ));
    }

    #[test]
    fn two_concurrent_hosts_never_both_acquire() {
        let backend = Arc::new(FakeCas::default());
        let handles = ["host-a", "host-b"].map(|owner| {
            let backend = Arc::clone(&backend);
            thread::spawn(move || backend.acquire(&request(owner, owner, 1_000)).is_ok())
        });
        let successes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|success| *success)
            .count();
        assert_eq!(successes, 1);
    }

    #[test]
    fn guard_revalidates_renews_and_releases_exact_fence() {
        let backend: Arc<dyn RemoteWriterLease> = Arc::new(FakeCas::default());
        let mut guard =
            RemoteLeaseGuard::acquire(Arc::clone(&backend), &request("host-a", "operation", 1_000))
                .unwrap();
        guard.revalidate(2_000).unwrap();
        let old_expiry = guard.grant().expires_unix_ms;
        guard.renew(3_000, 20_000, 4_000).unwrap();
        assert!(guard.grant().expires_unix_ms > old_expiry);
        assert!(guard.release().unwrap());
    }

    #[test]
    fn lost_remote_guard_stops_marked_command_before_inner_runner() {
        let now: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .try_into()
            .unwrap();
        let backend = Arc::new(FakeCas::default());
        let backend_trait: Arc<dyn RemoteWriterLease> = backend.clone();
        let guard =
            RemoteLeaseGuard::acquire(backend_trait, &request("host-a", "operation", now)).unwrap();
        backend.release(guard.grant()).unwrap();
        let inner = ScriptedRunner::with(vec![Ok(CommandOutput::success("unexpected"))]);
        let runner = FencedCommandRunner::new(inner.clone(), guard);
        let write = CommandSpec::new("gh")
            .args(["pr", "edit", "1"])
            .provider_write();
        assert!(matches!(
            runner.run(&write),
            Err(CommandRunError::MutationFenceRefused { .. })
        ));
        assert!(inner.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn expired_guard_and_regressed_renewal_fail_closed() {
        let backend: Arc<dyn RemoteWriterLease> = Arc::new(FakeCas::default());
        let guard =
            RemoteLeaseGuard::acquire(Arc::clone(&backend), &request("host-a", "operation", 1_000))
                .unwrap();
        assert!(matches!(
            guard.revalidate(guard.grant().expires_unix_ms),
            Err(RemoteLeaseError::Lost(_))
        ));
        let previous = guard.grant().clone();
        let mut regressed = previous.clone();
        regressed.expires_unix_ms = previous.expires_unix_ms;
        assert!(matches!(
            validate_renewed_grant(&previous, Some(regressed)),
            Err(RemoteLeaseError::Lost(_))
        ));
    }

    #[test]
    fn identities_and_time_bounds_are_strict() {
        let mut invalid = request("host-a", "operation", 1_000);
        invalid.key.repository = " bad\n".to_owned();
        assert!(matches!(
            invalid.validate(),
            Err(RemoteLeaseError::Invalid(_))
        ));
        let mut invalid = request("host-a", "operation", 1_000);
        invalid.heartbeat_ms = invalid.ttl_ms;
        assert!(matches!(
            invalid.validate(),
            Err(RemoteLeaseError::Invalid(_))
        ));
    }
}
