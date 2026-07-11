//! Durable managed-worker lifecycle state and session-binding serialization.
//!
//! The registry remains backward compatible: legacy `agents`/`names` entries
//! are unchanged, while lifecycle maps live beside them in the same atomic
//! document. Session binding locks are separate kernel locks; the global store
//! lock is never held while waiting for one.

use crate::sha256::Sha256;
use crate::store::{self, Entry, Registry};
use rustix::fs::{FlockOperation, flock};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tinyjson::JsonValue;

pub const MANAGED_ATTACH_DEADLINE_MS: u64 = 4_360;
pub const MANAGED_CANCEL_POLL_MS: u64 = 100;
pub const MANAGED_CANCEL_GRACE_MS: u64 = 5_000;

const WORKERS_KEY: &str = "managed_workers";
const PENDING_KEY: &str = "pending_managed";
const BINDINGS_KEY: &str = "session_bindings";
const TOMBSTONES_KEY: &str = "managed_tombstones";
const AUDIT_KEY: &str = "lifecycle_audit";
const GC_MANIFESTS_KEY: &str = "managed_gc_manifests";
const ACTIVE_OPERATIONS_KEY: &str = "active_operations";
const GC_DAYS: u64 = 14;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedState {
    Attaching,
    Active,
    Fencing,
    FencingUnconfirmed,
    Fenced,
    TerminalRetained,
    TerminalReleasable,
}

impl ManagedState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Attaching => "Attaching",
            Self::Active => "Active",
            Self::Fencing => "Fencing",
            Self::FencingUnconfirmed => "FencingUnconfirmed",
            Self::Fenced => "Fenced",
            Self::TerminalRetained => "TerminalRetained",
            Self::TerminalReleasable => "TerminalReleasable",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "Attaching" => Some(Self::Attaching),
            "Active" => Some(Self::Active),
            "Fencing" => Some(Self::Fencing),
            "FencingUnconfirmed" => Some(Self::FencingUnconfirmed),
            "Fenced" => Some(Self::Fenced),
            "TerminalRetained" => Some(Self::TerminalRetained),
            "TerminalReleasable" => Some(Self::TerminalReleasable),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredScope {
    ProcessOnly,
    ProtocolTree,
    WorkerTree,
}

impl RequiredScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::ProcessOnly => "ProcessOnly",
            Self::ProtocolTree => "ProtocolTree",
            Self::WorkerTree => "WorkerTree",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "ProcessOnly" => Some(Self::ProcessOnly),
            "ProtocolTree" => Some(Self::ProtocolTree),
            "WorkerTree" => Some(Self::WorkerTree),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionBackend {
    LinuxPidFdProcess,
    SupervisorOwnedProcess,
    SupervisorOwnedGroup,
    ConfinedCgroup,
    TrackedTree,
    SharedAppServer,
    DedicatedConfinedAppServer,
    ObservationOnly,
}

impl ExecutionBackend {
    fn as_str(self) -> &'static str {
        match self {
            Self::LinuxPidFdProcess => "LinuxPidFdProcess",
            Self::SupervisorOwnedProcess => "SupervisorOwnedProcess",
            Self::SupervisorOwnedGroup => "SupervisorOwnedGroup",
            Self::ConfinedCgroup => "ConfinedCgroup",
            Self::TrackedTree => "TrackedTree",
            Self::SharedAppServer => "SharedAppServer",
            Self::DedicatedConfinedAppServer => "DedicatedConfinedAppServer",
            Self::ObservationOnly => "ObservationOnly",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "LinuxPidFdProcess" => Some(Self::LinuxPidFdProcess),
            "SupervisorOwnedProcess" => Some(Self::SupervisorOwnedProcess),
            "SupervisorOwnedGroup" => Some(Self::SupervisorOwnedGroup),
            "ConfinedCgroup" => Some(Self::ConfinedCgroup),
            "TrackedTree" => Some(Self::TrackedTree),
            "SharedAppServer" => Some(Self::SharedAppServer),
            "DedicatedConfinedAppServer" => Some(Self::DedicatedConfinedAppServer),
            "ObservationOnly" => Some(Self::ObservationOnly),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseReceipt {
    pub worker_id: String,
    pub generation: String,
    pub released_at: String,
    pub mode: String,
    pub reason: String,
    pub evidence_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingAttach {
    pub worker_id: String,
    pub generation: String,
    pub token_sha256: String,
    pub expected_runtime_session_id: Option<String>,
    pub expected_tool: String,
    pub expected_cwd: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWorker {
    pub worker_id: String,
    pub generation: String,
    pub runtime_session_id: Option<String>,
    pub claimed_token_sha256: Option<String>,
    pub tool: String,
    pub cwd: String,
    pub state: ManagedState,
    pub version: String,
    pub required_scope: RequiredScope,
    pub execution: ExecutionBackend,
    pub fence_reason: Option<String>,
    pub fence_epoch: Option<String>,
    pub proof_gap: Option<String>,
    pub release_receipt: Option<ReleaseReceipt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionBinding {
    pub runtime_session_id: String,
    pub binding_epoch: String,
    pub state: BindingState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingState {
    Unmanaged,
    GcDeleting {
        gc_epoch: String,
        binding_epoch: String,
        entry_version: String,
    },
    Claiming {
        worker_id: String,
        generation: String,
        claim_version: String,
    },
    Managed {
        worker_id: String,
        generation: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedTombstone {
    pub worker_id: String,
    pub generation: String,
    pub final_version: String,
    pub retained_at: String,
    pub receipt: Option<ReleaseReceipt>,
}

#[derive(Clone, Debug)]
pub struct PendingAttachSpec {
    pub worker_id: String,
    pub generation: String,
    pub expected_runtime_session_id: Option<String>,
    pub expected_tool: String,
    pub expected_cwd: String,
    pub expires_at_ms: i64,
    pub required_scope: RequiredScope,
    pub execution: ExecutionBackend,
}

#[derive(Clone, Debug)]
pub struct ClaimManagedAttach {
    pub raw_token: String,
    pub worker_id: String,
    pub generation: String,
    pub runtime_session_id: String,
    pub tool: String,
    pub cwd: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimOutcome {
    Active {
        worker: Box<ManagedWorker>,
        duplicate: bool,
    },
    Refused {
        worker_id: String,
        state: ManagedState,
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OperationKind {
    SessionStartDrain,
    UserPromptDrain,
    CliInboxDrain,
    McpInboxDrain,
    ChannelDeliver,
    WatchInject,
    WatchAutoTurn,
    WatchAck,
    WatchWakeFallback,
    WakeAppServer,
    WakeCli,
    AttachResume,
    InitialTurn,
}

impl OperationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStartDrain => "SessionStartDrain",
            Self::UserPromptDrain => "UserPromptDrain",
            Self::CliInboxDrain => "CliInboxDrain",
            Self::McpInboxDrain => "McpInboxDrain",
            Self::ChannelDeliver => "ChannelDeliver",
            Self::WatchInject => "WatchInject",
            Self::WatchAutoTurn => "WatchAutoTurn",
            Self::WatchAck => "WatchAck",
            Self::WatchWakeFallback => "WatchWakeFallback",
            Self::WakeAppServer => "WakeAppServer",
            Self::WakeCli => "WakeCli",
            Self::AttachResume => "AttachResume",
            Self::InitialTurn => "InitialTurn",
        }
    }
}

#[derive(Debug)]
enum GuardTarget {
    Session {
        runtime_session_id: String,
        worker_id: Option<String>,
        generation: Option<String>,
        binding_epoch: String,
        tool: String,
        canonical_cwd: String,
        server: Option<String>,
        server_fingerprint: Option<String>,
    },
    AppServerThread {
        runtime_session_id: String,
        worker_id: Option<String>,
        generation: Option<String>,
        binding_epoch: String,
        tool: String,
        canonical_cwd: String,
        server: String,
        server_fingerprint: String,
        thread_id: String,
    },
}

pub struct SharedFileLock {
    _file: fs::File,
    _sealed: Sealed,
}

pub struct ExclusiveFileLock {
    _file: fs::File,
    _sealed: Sealed,
}

struct CancelToken {
    path: PathBuf,
    worker_or_session_epoch: String,
}

pub struct ReentryGuard {
    store: LifecycleStore,
    target: GuardTarget,
    allowed: OperationKind,
    lifecycle_version: Option<String>,
    _binding_guard: SharedFileLock,
    _activity_guard: Option<SharedFileLock>,
    _deadline: Instant,
    cancel: CancelToken,
    operation_id: String,
    _sealed: Sealed,
}

pub enum Admission {
    Unmanaged(ReentryGuard),
    Managed(ReentryGuard),
    Refused {
        worker_id: String,
        state: ManagedState,
        reason: String,
    },
}

impl Admission {
    pub fn into_guard(self) -> Result<ReentryGuard, String> {
        match self {
            Self::Unmanaged(guard) | Self::Managed(guard) => Ok(guard),
            Self::Refused {
                worker_id,
                state,
                reason,
            } => Err(format!(
                "operation refused for managed worker {worker_id} in {}: {reason}",
                state.as_str()
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FenceIntent {
    worker_id: String,
    generation: String,
    fence_epoch: String,
    fencing_version: String,
    root: PathBuf,
}

pub struct FencePermit {
    intent: FenceIntent,
    _binding_guards: Vec<ExclusiveFileLock>,
    _activity_guard: ExclusiveFileLock,
    _sealed: Sealed,
}

impl FencePermit {
    pub fn worker_id(&self) -> &str {
        &self.intent.worker_id
    }

    pub fn generation(&self) -> &str {
        &self.intent.generation
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum DrainError {
    TimedOut { prior_operations: Vec<String> },
    StaleGeneration,
    StateChanged,
    Store(String),
}

impl From<String> for DrainError {
    fn from(value: String) -> Self {
        Self::Store(value)
    }
}

pub(crate) struct AuthorizedTarget {
    pub(crate) root: PathBuf,
    pub(crate) runtime_session_id: String,
    pub(crate) worker_id: Option<String>,
    pub(crate) generation: Option<String>,
    pub(crate) tool: String,
    pub(crate) canonical_cwd: String,
    pub(crate) server: Option<String>,
    pub(crate) server_fingerprint: Option<String>,
    pub(crate) thread_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedModel(String);

impl ValidatedModel {
    pub fn parse(value: &str) -> Result<Self, String> {
        validate_bounded_token("model", value, 128).map(|()| Self(value.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedEffort(String);

impl ValidatedEffort {
    pub fn parse(value: &str) -> Result<Self, String> {
        if matches!(
            value,
            "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            Ok(Self(value.to_string()))
        } else {
            Err("effort is not an allowed tier".to_string())
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoorbellMessage {
    text: String,
    model: Option<ValidatedModel>,
    effort: Option<ValidatedEffort>,
}

impl DoorbellMessage {
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.is_empty() || value.len() > 16_384 || value.contains('\0') {
            Err("doorbell message must be 1..=16384 bytes without NUL".to_string())
        } else {
            Ok(Self {
                text: value.to_string(),
                model: None,
                effort: None,
            })
        }
    }

    pub fn with_runtime_options(
        mut self,
        model: Option<ValidatedModel>,
        effort: Option<ValidatedEffort>,
    ) -> Self {
        self.model = model;
        self.effort = effort;
        self
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn model(&self) -> Option<&str> {
        self.model.as_ref().map(ValidatedModel::as_str)
    }

    pub(crate) fn effort(&self) -> Option<&str> {
        self.effort.as_ref().map(ValidatedEffort::as_str)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachOptions {
    model: Option<ValidatedModel>,
    effort: Option<ValidatedEffort>,
}

impl AttachOptions {
    pub fn new(model: Option<ValidatedModel>, effort: Option<ValidatedEffort>) -> Self {
        Self { model, effort }
    }

    pub(crate) fn model(&self) -> Option<&str> {
        self.model.as_ref().map(ValidatedModel::as_str)
    }

    pub(crate) fn effort(&self) -> Option<&str> {
        self.effort.as_ref().map(ValidatedEffort::as_str)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChildLaunchSpec {
    AttachResume(AttachOptions),
    WakeDoorbell(DoorbellMessage),
    WatchWakeFallback(DoorbellMessage),
}

pub(crate) struct Sealed(());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcCheckpoint {
    Enumerated,
    GcDeletingPublished,
    FirstSurfaceDeleted,
    BindingLockUnlinked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcControl {
    RunToCompletion,
    StopAfter(GcCheckpoint),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcRunResult {
    pub removed_candidates: usize,
    pub stopped_at: Option<GcCheckpoint>,
    pub gc_epoch: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LifecycleStore {
    root: PathBuf,
    attach_deadline: Duration,
    cancel_grace: Duration,
}

impl Default for LifecycleStore {
    fn default() -> Self {
        Self::new(store::home_dir())
    }
}

impl LifecycleStore {
    pub fn new(root: PathBuf) -> Self {
        Self::with_timeouts(
            root,
            Duration::from_millis(MANAGED_ATTACH_DEADLINE_MS),
            Duration::from_millis(MANAGED_CANCEL_GRACE_MS),
        )
    }

    pub fn with_timeouts(root: PathBuf, attach_deadline: Duration, cancel_grace: Duration) -> Self {
        Self {
            root,
            attach_deadline,
            cancel_grace,
        }
    }

    pub fn create_pending(
        &self,
        spec: PendingAttachSpec,
        raw_token: &str,
    ) -> Result<PendingAttach, String> {
        validate_uuid("worker_id", &spec.worker_id)?;
        validate_uuid("generation", &spec.generation)?;
        if let Some(id) = spec.expected_runtime_session_id.as_deref() {
            validate_uuid("expected_runtime_session_id", id)?;
        }
        validate_tool(&spec.expected_tool)?;
        if raw_token.is_empty() {
            return Err("managed attach token is empty".to_string());
        }
        let token_sha256 = sha256_hex(raw_token.as_bytes());
        let expected_cwd = canonical_cwd(&spec.expected_cwd);
        let pending = PendingAttach {
            worker_id: spec.worker_id.clone(),
            generation: spec.generation.clone(),
            token_sha256: token_sha256.clone(),
            expected_runtime_session_id: spec.expected_runtime_session_id.clone(),
            expected_tool: spec.expected_tool.clone(),
            expected_cwd: expected_cwd.clone(),
            expires_at: store::iso_from_unix_ms(spec.expires_at_ms),
        };
        self.transaction(|registry| {
            let mut workers = object_map(registry, WORKERS_KEY)?;
            let mut pending_map = object_map(registry, PENDING_KEY)?;
            let bindings = object_map(registry, BINDINGS_KEY)?;
            let tombstones = object_map(registry, TOMBSTONES_KEY)?;
            if workers.contains_key(&spec.worker_id) {
                return Err(format!("managed worker {} already exists", spec.worker_id));
            }
            if pending_map.contains_key(&token_sha256) {
                return Err("managed attach token hash already exists".to_string());
            }
            if tombstones.values().any(|value| {
                ManagedTombstone::from_json(value).is_some_and(|tombstone| {
                    tombstone.worker_id == spec.worker_id && tombstone.generation == spec.generation
                })
            }) {
                return Err("managed generation is tombstoned".to_string());
            }
            if let Some(session_id) = spec.expected_runtime_session_id.as_deref() {
                if bindings.get(session_id).is_some_and(|value| {
                    SessionBinding::from_json(value).is_some_and(|binding| {
                        matches!(binding.state, BindingState::GcDeleting { .. })
                    })
                }) {
                    return Err("session binding is being deleted".to_string());
                }
            }
            let worker = ManagedWorker {
                worker_id: spec.worker_id.clone(),
                generation: spec.generation.clone(),
                runtime_session_id: None,
                claimed_token_sha256: None,
                tool: spec.expected_tool,
                cwd: expected_cwd,
                state: ManagedState::Attaching,
                version: "1".to_string(),
                required_scope: spec.required_scope,
                execution: spec.execution,
                fence_reason: None,
                fence_epoch: None,
                proof_gap: None,
                release_receipt: None,
            };
            workers.insert(worker.worker_id.clone(), worker.to_json());
            pending_map.insert(token_sha256, PendingRecord::new(pending.clone()).to_json());
            set_object_map(registry, WORKERS_KEY, workers);
            set_object_map(registry, PENDING_KEY, pending_map);
            Ok(pending.clone())
        })
    }

    pub fn claim_managed_attach(
        &self,
        request: ClaimManagedAttach,
    ) -> Result<ClaimOutcome, String> {
        validate_claim(&request)?;
        let canonical_cwd = canonical_cwd(&request.cwd);
        let token_sha256 = sha256_hex(request.raw_token.as_bytes());
        let start = self.transaction(|registry| {
            begin_claim(registry, &request, &canonical_cwd, &token_sha256)
        })?;
        match start {
            ClaimStart::Immediate(outcome) => Ok(*outcome),
            ClaimStart::Join {
                binding_epoch,
                claim_version,
            } => self.join_claim(
                &request,
                &canonical_cwd,
                &token_sha256,
                &binding_epoch,
                &claim_version,
            ),
            ClaimStart::Initial {
                binding_epoch,
                claim_version,
            } => self.finish_claim(
                &request,
                &canonical_cwd,
                &token_sha256,
                &binding_epoch,
                &claim_version,
            ),
        }
    }

    pub fn resume_managed_attach(
        &self,
        runtime_session_id: &str,
        tool: &str,
        cwd: &str,
    ) -> Result<ClaimOutcome, String> {
        validate_uuid("runtime_session_id", runtime_session_id)?;
        validate_tool(tool)?;
        let cwd = canonical_cwd(cwd);
        self.read_transaction(|registry| {
            let bindings = object_map(registry, BINDINGS_KEY)?;
            let workers = object_map(registry, WORKERS_KEY)?;
            let Some(binding) = bindings
                .get(runtime_session_id)
                .and_then(SessionBinding::from_json)
            else {
                return Err("session is not managed".to_string());
            };
            let BindingState::Managed {
                worker_id,
                generation,
            } = binding.state
            else {
                return Err("session binding is not managed".to_string());
            };
            let worker = workers
                .get(&worker_id)
                .and_then(ManagedWorker::from_json)
                .ok_or_else(|| "managed binding references a missing worker".to_string())?;
            if worker.generation != generation
                || worker.runtime_session_id.as_deref() != Some(runtime_session_id)
                || worker.tool != tool
                || worker.cwd != cwd
            {
                return Ok(refused(&worker, "managed resume tuple mismatch"));
            }
            if worker.state == ManagedState::Active {
                Ok(ClaimOutcome::Active {
                    worker: Box::new(worker),
                    duplicate: true,
                })
            } else {
                Ok(refused(&worker, "managed worker is not Active"))
            }
        })
    }

    pub fn hold_unmanaged_epoch(
        &self,
        runtime_session_id: &str,
    ) -> Result<BindingEpochGuard, String> {
        validate_uuid("runtime_session_id", runtime_session_id)?;
        let epoch = self.transaction(|registry| {
            let mut bindings = object_map(registry, BINDINGS_KEY)?;
            let epoch = match bindings.get(runtime_session_id) {
                Some(value) => {
                    let binding = SessionBinding::from_json(value)
                        .ok_or_else(|| "malformed session binding".to_string())?;
                    if !matches!(binding.state, BindingState::Unmanaged) {
                        return Err("session binding does not admit unmanaged work".to_string());
                    }
                    canonical_u64("binding_epoch", &binding.binding_epoch)?;
                    binding.binding_epoch
                }
                None => {
                    let binding = SessionBinding {
                        runtime_session_id: runtime_session_id.to_string(),
                        binding_epoch: "0".to_string(),
                        state: BindingState::Unmanaged,
                    };
                    bindings.insert(runtime_session_id.to_string(), binding.to_json());
                    set_object_map(registry, BINDINGS_KEY, bindings);
                    "0".to_string()
                }
            };
            Ok(epoch)
        })?;
        let file = acquire_binding_lock(
            &self.root,
            runtime_session_id,
            FlockOperation::NonBlockingLockShared,
            self.attach_deadline,
        )?
        .ok_or_else(|| "timed out acquiring shared session binding lock".to_string())?;
        let still_current = self
            .read_binding(runtime_session_id)?
            .is_some_and(|binding| {
                binding.binding_epoch == epoch && matches!(binding.state, BindingState::Unmanaged)
            });
        if !still_current {
            drop(file);
            return Err("session binding changed during unmanaged admission".to_string());
        }
        let operation_id = store::uuid_v4();
        self.transaction(|registry| {
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let mut row = HashMap::new();
            row.insert("operation_id".into(), JsonValue::from(operation_id.clone()));
            row.insert(
                "kind".into(),
                JsonValue::from("LegacyUnmanagedEpoch".to_string()),
            );
            row.insert(
                "runtime_session_id".into(),
                JsonValue::from(runtime_session_id.to_string()),
            );
            row.insert("worker_id".into(), JsonValue::from(()));
            row.insert("generation".into(), JsonValue::from(()));
            operations.insert(operation_id.clone(), JsonValue::from(row));
            set_object_map(registry, ACTIVE_OPERATIONS_KEY, operations);
            Ok(())
        })?;
        Ok(BindingEpochGuard {
            _file: file,
            binding_epoch: epoch,
            store: self.clone(),
            operation_id,
        })
    }

    pub fn read_binding(&self, runtime_session_id: &str) -> Result<Option<SessionBinding>, String> {
        self.read_transaction(|registry| {
            let bindings = object_map(registry, BINDINGS_KEY)?;
            bindings
                .get(runtime_session_id)
                .map(|value| {
                    SessionBinding::from_json(value)
                        .ok_or_else(|| "malformed session binding".to_string())
                })
                .transpose()
        })
    }

    pub fn read_worker(&self, worker_id: &str) -> Result<Option<ManagedWorker>, String> {
        self.read_transaction(|registry| {
            let workers = object_map(registry, WORKERS_KEY)?;
            workers
                .get(worker_id)
                .map(|value| {
                    ManagedWorker::from_json(value)
                        .ok_or_else(|| "malformed managed worker".to_string())
                })
                .transpose()
        })
    }

    pub fn read_pending_by_token(&self, raw_token: &str) -> Result<Option<PendingAttach>, String> {
        let token_sha256 = sha256_hex(raw_token.as_bytes());
        self.read_transaction(|registry| {
            let pending = object_map(registry, PENDING_KEY)?;
            pending
                .get(&token_sha256)
                .map(|value| {
                    PendingRecord::from_json(value)
                        .map(|record| record.attach)
                        .ok_or_else(|| "malformed pending managed attach".to_string())
                })
                .transpose()
        })
    }

    pub fn transition_worker(
        &self,
        worker_id: &str,
        generation: &str,
        expected_version: &str,
        next: ManagedState,
    ) -> Result<ManagedWorker, String> {
        canonical_u64("expected_version", expected_version)?;
        self.transaction(|registry| {
            let mut workers = object_map(registry, WORKERS_KEY)?;
            let mut worker = workers
                .get(worker_id)
                .and_then(ManagedWorker::from_json)
                .ok_or_else(|| "managed worker not found or malformed".to_string())?;
            if worker.generation != generation || worker.version != expected_version {
                return Err("managed worker generation/version changed".to_string());
            }
            if !transition_allowed(worker.state, next) {
                return Err(format!(
                    "invalid managed transition {} -> {}",
                    worker.state.as_str(),
                    next.as_str()
                ));
            }
            worker.state = next;
            worker.version = next_version(&worker.version)?;
            workers.insert(worker_id.to_string(), worker.to_json());
            set_object_map(registry, WORKERS_KEY, workers);
            Ok(worker)
        })
    }

    pub fn admit_operation(
        &self,
        session_or_worker: &str,
        kind: OperationKind,
    ) -> Result<Admission, String> {
        let plan = self.transaction(|registry| {
            let workers = object_map(registry, WORKERS_KEY)?;
            let mut bindings = object_map(registry, BINDINGS_KEY)?;
            let (runtime_session_id, selected_worker) = if let Some(worker) = workers
                .get(session_or_worker)
                .and_then(ManagedWorker::from_json)
            {
                let session = worker
                    .runtime_session_id
                    .clone()
                    .ok_or_else(|| "managed worker has no bound runtime session".to_string())?;
                (session, Some(worker))
            } else {
                (session_or_worker.to_string(), None)
            };
            validate_uuid("runtime_session_id", &runtime_session_id)?;
            let entry = registry
                .agents
                .get(&runtime_session_id)
                .and_then(Entry::from_json)
                .ok_or_else(|| "operation target is not a registered session".to_string())?;
            let binding = match bindings.get(&runtime_session_id) {
                Some(value) => SessionBinding::from_json(value)
                    .ok_or_else(|| "malformed session binding".to_string())?,
                None => {
                    let binding = SessionBinding {
                        runtime_session_id: runtime_session_id.clone(),
                        binding_epoch: "0".to_string(),
                        state: BindingState::Unmanaged,
                    };
                    bindings.insert(runtime_session_id.clone(), binding.to_json());
                    set_object_map(registry, BINDINGS_KEY, bindings.clone());
                    binding
                }
            };
            let (worker_id, generation, lifecycle_version, managed) = match &binding.state {
                BindingState::Unmanaged => (None, None, None, false),
                BindingState::GcDeleting { .. } => {
                    return Err("session binding is being deleted".to_string());
                }
                BindingState::Claiming {
                    worker_id,
                    generation,
                    ..
                } => {
                    let state = workers
                        .get(worker_id)
                        .and_then(ManagedWorker::from_json)
                        .map(|worker| worker.state)
                        .unwrap_or(ManagedState::Attaching);
                    return Ok(AdmissionPlan::Refused {
                        worker_id: worker_id.clone(),
                        state,
                        reason: format!("binding generation {generation} is still Claiming"),
                    });
                }
                BindingState::Managed {
                    worker_id,
                    generation,
                } => {
                    let worker = workers
                        .get(worker_id)
                        .and_then(ManagedWorker::from_json)
                        .ok_or_else(|| "managed binding references a missing worker".to_string())?;
                    if worker.generation != *generation {
                        return Err("managed binding generation mismatch".to_string());
                    }
                    if worker.state != ManagedState::Active {
                        return Ok(AdmissionPlan::Refused {
                            worker_id: worker_id.clone(),
                            state: worker.state,
                            reason: "managed worker is not Active".to_string(),
                        });
                    }
                    (
                        Some(worker_id.clone()),
                        Some(generation.clone()),
                        Some(worker.version),
                        true,
                    )
                }
            };
            if let Some(worker) = selected_worker {
                if worker_id.as_deref() != Some(worker.worker_id.as_str())
                    || generation.as_deref() != Some(worker.generation.as_str())
                {
                    return Err("worker selector does not match its session binding".to_string());
                }
            }
            let selected_server = entry.server.clone();
            let target = if operation_uses_appserver(kind) {
                if let Some(server) = selected_server {
                    GuardTarget::AppServerThread {
                        runtime_session_id: runtime_session_id.clone(),
                        worker_id: worker_id.clone(),
                        generation: generation.clone(),
                        binding_epoch: binding.binding_epoch.clone(),
                        tool: entry.tool,
                        canonical_cwd: entry
                            .dir
                            .map(|dir| canonical_cwd(&dir))
                            .ok_or_else(|| "operation target has no cwd".to_string())?,
                        server_fingerprint: sha256_hex(server.as_bytes()),
                        server,
                        thread_id: runtime_session_id,
                    }
                } else {
                    return Err("operation target has no app-server authority".to_string());
                }
            } else {
                GuardTarget::Session {
                    runtime_session_id,
                    worker_id: worker_id.clone(),
                    generation: generation.clone(),
                    binding_epoch: binding.binding_epoch,
                    tool: entry.tool,
                    canonical_cwd: entry
                        .dir
                        .map(|dir| canonical_cwd(&dir))
                        .ok_or_else(|| "operation target has no cwd".to_string())?,
                    server_fingerprint: selected_server
                        .as_ref()
                        .map(|server| sha256_hex(server.as_bytes())),
                    server: selected_server,
                }
            };
            Ok(AdmissionPlan::Guard {
                target,
                lifecycle_version,
                managed,
            })
        })?;
        let AdmissionPlan::Guard {
            target,
            lifecycle_version,
            managed,
        } = plan
        else {
            let AdmissionPlan::Refused {
                worker_id,
                state,
                reason,
            } = plan
            else {
                unreachable!()
            };
            return Ok(Admission::Refused {
                worker_id,
                state,
                reason,
            });
        };
        let session_id = target_runtime_session_id(&target).to_string();
        let binding_epoch = target_binding_epoch(&target).to_string();
        let binding_file = acquire_binding_lock(
            &self.root,
            &session_id,
            FlockOperation::NonBlockingLockShared,
            self.attach_deadline,
        )?
        .ok_or_else(|| "timed out acquiring shared binding guard".to_string())?;
        let activity_guard = if let (Some(worker_id), Some(generation)) =
            (target_worker_id(&target), target_generation(&target))
        {
            let file = acquire_path_lock(
                &activity_lock_path(&self.root, worker_id, generation),
                FlockOperation::NonBlockingLockShared,
                self.attach_deadline,
            )?
            .ok_or_else(|| "timed out acquiring shared activity guard".to_string())?;
            Some(SharedFileLock {
                _file: file,
                _sealed: Sealed(()),
            })
        } else {
            None
        };
        let operation_id = store::uuid_v4();
        let mut guard = ReentryGuard {
            store: self.clone(),
            target,
            allowed: kind,
            lifecycle_version,
            _binding_guard: SharedFileLock {
                _file: binding_file,
                _sealed: Sealed(()),
            },
            _activity_guard: activity_guard,
            _deadline: Instant::now() + self.attach_deadline,
            cancel: CancelToken {
                path: self.root.join("locks").join(format!(
                    "cancel-{}-{}.token",
                    store::sanitize(&session_id),
                    binding_epoch
                )),
                worker_or_session_epoch: binding_epoch,
            },
            operation_id: operation_id.clone(),
            _sealed: Sealed(()),
        };
        self.transaction(|registry| {
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let mut row = HashMap::new();
            row.insert("operation_id".into(), JsonValue::from(operation_id.clone()));
            row.insert("kind".into(), JsonValue::from(kind.as_str().to_string()));
            row.insert(
                "runtime_session_id".into(),
                JsonValue::from(session_id.clone()),
            );
            row.insert(
                "worker_id".into(),
                target_worker_id(&guard.target)
                    .map(|value| JsonValue::from(value.to_string()))
                    .unwrap_or(JsonValue::from(())),
            );
            row.insert(
                "generation".into(),
                target_generation(&guard.target)
                    .map(|value| JsonValue::from(value.to_string()))
                    .unwrap_or(JsonValue::from(())),
            );
            operations.insert(operation_id.clone(), JsonValue::from(row));
            set_object_map(registry, ACTIVE_OPERATIONS_KEY, operations);
            Ok(())
        })?;
        guard.authorize_use(kind)?;
        if managed {
            Ok(Admission::Managed(guard))
        } else {
            Ok(Admission::Unmanaged(guard))
        }
    }

    pub fn publish_fence(
        &self,
        worker_id: &str,
        generation: &str,
        reason: &str,
    ) -> Result<FenceIntent, String> {
        if reason.is_empty() || reason.len() > 4096 {
            return Err("fence reason must be 1..=4096 bytes".to_string());
        }
        self.transaction(|registry| {
            let mut workers = object_map(registry, WORKERS_KEY)?;
            let mut worker = workers
                .get(worker_id)
                .and_then(ManagedWorker::from_json)
                .ok_or_else(|| "managed worker not found or malformed".to_string())?;
            if worker.generation != generation || worker.state != ManagedState::Active {
                return Err("worker generation is stale or not Active".to_string());
            }
            let fence_epoch = store::uuid_v4();
            worker.state = ManagedState::Fencing;
            worker.version = next_version(&worker.version)?;
            worker.fence_reason = Some(reason.to_string());
            worker.fence_epoch = Some(fence_epoch.clone());
            workers.insert(worker_id.to_string(), worker.to_json());
            set_object_map(registry, WORKERS_KEY, workers);
            Ok(FenceIntent {
                worker_id: worker_id.to_string(),
                generation: generation.to_string(),
                fence_epoch,
                fencing_version: worker.version,
                root: self.root.clone(),
            })
        })
    }

    pub fn drain_prior_operations(&self, intent: FenceIntent) -> Result<FencePermit, DrainError> {
        if intent.root != self.root {
            return Err(DrainError::StateChanged);
        }
        let mut sessions = self.read_transaction(|registry| {
            validate_fence_intent(registry, &intent)?;
            let bindings = object_map(registry, BINDINGS_KEY)?;
            Ok(bindings
                .into_iter()
                .filter_map(|(session_id, value)| {
                    let binding = SessionBinding::from_json(&value)?;
                    matches!(
                        binding.state,
                        BindingState::Managed { ref worker_id, ref generation }
                            if worker_id == &intent.worker_id
                                && generation == &intent.generation
                    )
                    .then_some(session_id)
                })
                .collect::<Vec<_>>())
        })?;
        sessions.sort();
        let drain_deadline = Instant::now() + self.cancel_grace;
        let mut binding_guards = Vec::with_capacity(sessions.len());
        for session_id in &sessions {
            let Some(file) = acquire_binding_lock(
                &self.root,
                session_id,
                FlockOperation::NonBlockingLockExclusive,
                drain_deadline.saturating_duration_since(Instant::now()),
            )?
            else {
                self.mark_fence_unconfirmed(
                    &intent,
                    &sessions,
                    "binding drain exceeded cancellation grace",
                )?;
                return Err(DrainError::TimedOut {
                    prior_operations: sessions,
                });
            };
            binding_guards.push(ExclusiveFileLock {
                _file: file,
                _sealed: Sealed(()),
            });
        }
        let Some(activity_file) = acquire_path_lock(
            &activity_lock_path(&self.root, &intent.worker_id, &intent.generation),
            FlockOperation::NonBlockingLockExclusive,
            drain_deadline.saturating_duration_since(Instant::now()),
        )?
        else {
            self.mark_fence_unconfirmed(
                &intent,
                &sessions,
                "activity drain exceeded cancellation grace",
            )?;
            return Err(DrainError::TimedOut {
                prior_operations: sessions,
            });
        };
        self.read_transaction(|registry| validate_fence_intent(registry, &intent))
            .map_err(|_| DrainError::StateChanged)?;
        Ok(FencePermit {
            intent,
            _binding_guards: binding_guards,
            _activity_guard: ExclusiveFileLock {
                _file: activity_file,
                _sealed: Sealed(()),
            },
            _sealed: Sealed(()),
        })
    }

    fn mark_fence_unconfirmed(
        &self,
        intent: &FenceIntent,
        _prior_operations: &[String],
        reason: &str,
    ) -> Result<(), DrainError> {
        self.transaction(|registry| {
            validate_fence_intent(registry, intent)?;
            let active_operations = active_operation_ids(
                registry,
                Some(&intent.worker_id),
                Some(&intent.generation),
                &[],
            )?;
            let mut workers = object_map(registry, WORKERS_KEY)?;
            let mut worker = workers
                .get(&intent.worker_id)
                .and_then(ManagedWorker::from_json)
                .ok_or_else(|| "fence worker is missing or malformed".to_string())?;
            worker.state = ManagedState::FencingUnconfirmed;
            worker.version = next_version(&worker.version)?;
            worker.proof_gap = Some(format!(
                "{reason}; active_operations={}",
                active_operations.join(",")
            ));
            workers.insert(intent.worker_id.clone(), worker.to_json());
            set_object_map(registry, WORKERS_KEY, workers);
            Ok(())
        })
        .map_err(|error| {
            if error.contains("fence epoch/version/state changed") {
                DrainError::StateChanged
            } else {
                DrainError::Store(error)
            }
        })
    }

    pub fn gc_unmanaged(&self, now: SystemTime, control: GcControl) -> Result<GcRunResult, String> {
        self.gc_unmanaged_excluding(now, control, None)
    }

    pub(crate) fn gc_unmanaged_excluding(
        &self,
        now: SystemTime,
        control: GcControl,
        self_id: Option<&str>,
    ) -> Result<GcRunResult, String> {
        let days = match std::env::var("AGENT_RELAY_GC_DAYS") {
            Ok(value) if !value.is_empty() => value
                .parse::<u64>()
                .map_err(|_| "AGENT_RELAY_GC_DAYS must be a non-negative integer".to_string())?,
            Ok(_) | Err(std::env::VarError::NotPresent) => GC_DAYS,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err("AGENT_RELAY_GC_DAYS must be valid UTF-8".to_string());
            }
        };
        if days == 0 {
            return Ok(GcRunResult {
                removed_candidates: 0,
                stopped_at: None,
                gc_epoch: None,
            });
        }
        let cutoff = now
            .checked_sub(Duration::from_secs(days * 24 * 60 * 60))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut removed_candidates = 0;

        let resumable = self.read_transaction(gc_manifests)?;
        for manifest in resumable.into_values() {
            let result = self.resume_gc_manifest(manifest, control)?;
            removed_candidates += result.removed_candidates;
            if result.stopped_at.is_some() {
                return Ok(GcRunResult {
                    removed_candidates,
                    ..result
                });
            }
        }

        let candidates = self.enumerate_gc_candidates(cutoff, self_id)?;
        if !candidates.is_empty() && control == GcControl::StopAfter(GcCheckpoint::Enumerated) {
            return Ok(GcRunResult {
                removed_candidates,
                stopped_at: Some(GcCheckpoint::Enumerated),
                gc_epoch: None,
            });
        }
        for candidate in candidates {
            let Some(manifest) = self.publish_gc_deleting(candidate, cutoff)? else {
                continue;
            };
            if control == GcControl::StopAfter(GcCheckpoint::GcDeletingPublished) {
                return Ok(GcRunResult {
                    removed_candidates,
                    stopped_at: Some(GcCheckpoint::GcDeletingPublished),
                    gc_epoch: Some(manifest.gc_epoch),
                });
            }
            let result = self.resume_gc_manifest(manifest, control)?;
            removed_candidates += result.removed_candidates;
            if result.stopped_at.is_some() {
                return Ok(GcRunResult {
                    removed_candidates,
                    ..result
                });
            }
        }
        Ok(GcRunResult {
            removed_candidates,
            stopped_at: None,
            gc_epoch: None,
        })
    }

    fn enumerate_gc_candidates(
        &self,
        cutoff: SystemTime,
        self_id: Option<&str>,
    ) -> Result<Vec<GcManifest>, String> {
        let registry = self.read_transaction(|registry| Ok(registry.clone()))?;
        let bindings = object_map(&registry, BINDINGS_KEY)?;
        if bindings
            .values()
            .any(|value| SessionBinding::from_json(value).is_none())
        {
            return Ok(Vec::new());
        }
        let mut ids: Vec<String> = registry
            .agents
            .keys()
            .filter(|id| store::is_uuid(id))
            .cloned()
            .collect();
        for id in bindings.keys().filter(|id| store::is_uuid(id)) {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids.sort();
        let cutoff_iso = system_time_iso(cutoff);
        let mut candidates = Vec::new();
        for id in ids {
            if self_id == Some(id.as_str()) {
                continue;
            }
            let entry = registry.agents.get(&id).cloned();
            if entry.as_ref().is_some_and(|value| {
                Entry::from_json(value)
                    .is_none_or(|entry| entry.last_seen.is_empty() || entry.last_seen > cutoff_iso)
            }) {
                continue;
            }
            let binding_epoch = match bindings.get(&id) {
                Some(value) => {
                    let Some(binding) = SessionBinding::from_json(value) else {
                        continue;
                    };
                    if !matches!(binding.state, BindingState::Unmanaged) {
                        continue;
                    }
                    binding.binding_epoch
                }
                None => "0".to_string(),
            };
            if lifecycle_references_session(&registry, &id)? {
                continue;
            }
            if !self.runtime_locks_are_quiescent(&id)? {
                continue;
            }
            let binding_path = self.binding_lock_path(&id);
            let Some(lock) = try_exclusive_file(&binding_path)? else {
                continue;
            };
            drop(lock);
            let surfaces = self.snapshot_candidate_surfaces(&id, entry.as_ref(), cutoff)?;
            let Some(surfaces) = surfaces else {
                continue;
            };
            candidates.push(GcManifest {
                runtime_session_id: id,
                gc_epoch: store::uuid_v4(),
                binding_epoch,
                entry_version: entry
                    .as_ref()
                    .map(entry_version)
                    .unwrap_or_else(|| sha256_hex(b"absent-entry")),
                entry,
                surfaces,
                deletion_started: false,
                binding_lock_unlinked: false,
            });
        }
        Ok(candidates)
    }

    fn snapshot_candidate_surfaces(
        &self,
        id: &str,
        entry: Option<&JsonValue>,
        cutoff: SystemTime,
    ) -> Result<Option<Vec<GcSurfaceSnapshot>>, String> {
        let mut relative_paths = vec![
            format!("mailbox/{id}.jsonl"),
            format!("watchers/{id}.lock"),
            format!("watchers/{id}.progress"),
            format!("locks/resume-{id}.lock"),
            format!("spawn-logs/{id}.stderr"),
            format!("locks/binding-{id}.lock"),
        ];
        if let Some(dir) = entry.and_then(Entry::from_json).and_then(|entry| entry.dir) {
            relative_paths.push(format!("markers/{}", store::encode_dir(&dir)));
        }
        let mut surfaces = Vec::new();
        for relative in relative_paths {
            let path = self.root.join(&relative);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("stat GC surface {}: {error}", path.display())),
            };
            if !metadata.file_type().is_file() {
                return Ok(None);
            }
            let snapshot = GcSurfaceSnapshot::from_metadata(relative, &metadata);
            if !snapshot.relative_path.starts_with("locks/binding-")
                && metadata.modified().unwrap_or(SystemTime::now()) > cutoff
            {
                return Ok(None);
            }
            surfaces.push(snapshot);
        }
        Ok(Some(surfaces))
    }

    fn publish_gc_deleting(
        &self,
        manifest: GcManifest,
        cutoff: SystemTime,
    ) -> Result<Option<GcManifest>, String> {
        if !manifest.surfaces_match(&self.root, true)? {
            return Ok(None);
        }
        let cutoff_iso = system_time_iso(cutoff);
        self.transaction(|registry| {
            let current_entry = registry.agents.get(&manifest.runtime_session_id).cloned();
            if current_entry != manifest.entry {
                return Ok(None);
            }
            if current_entry.as_ref().is_some_and(|value| {
                Entry::from_json(value)
                    .is_none_or(|entry| entry.last_seen.is_empty() || entry.last_seen > cutoff_iso)
            }) || lifecycle_references_session(registry, &manifest.runtime_session_id)?
            {
                return Ok(None);
            }
            let mut bindings = object_map(registry, BINDINGS_KEY)?;
            match bindings.get(&manifest.runtime_session_id) {
                Some(value) => {
                    let Some(binding) = SessionBinding::from_json(value) else {
                        return Ok(None);
                    };
                    if binding.binding_epoch != manifest.binding_epoch
                        || !matches!(binding.state, BindingState::Unmanaged)
                    {
                        return Ok(None);
                    }
                }
                None if manifest.binding_epoch == "0" => {}
                None => return Ok(None),
            }
            bindings.insert(
                manifest.runtime_session_id.clone(),
                SessionBinding {
                    runtime_session_id: manifest.runtime_session_id.clone(),
                    binding_epoch: manifest.binding_epoch.clone(),
                    state: BindingState::GcDeleting {
                        gc_epoch: manifest.gc_epoch.clone(),
                        binding_epoch: manifest.binding_epoch.clone(),
                        entry_version: manifest.entry_version.clone(),
                    },
                }
                .to_json(),
            );
            let mut manifests = gc_manifests(registry)?;
            manifests.insert(manifest.runtime_session_id.clone(), manifest.clone());
            set_object_map(registry, BINDINGS_KEY, bindings);
            set_gc_manifests(registry, manifests);
            Ok(Some(manifest.clone()))
        })
    }

    fn resume_gc_manifest(
        &self,
        mut manifest: GcManifest,
        control: GcControl,
    ) -> Result<GcRunResult, String> {
        self.validate_gc_epoch(&manifest)?;
        let binding_path = self.binding_lock_path(&manifest.runtime_session_id);
        let binding_lock = if manifest.binding_lock_unlinked || !binding_path.exists() {
            None
        } else {
            acquire_binding_lock(
                &self.root,
                &manifest.runtime_session_id,
                FlockOperation::NonBlockingLockExclusive,
                self.cancel_grace,
            )?
        };
        if !manifest.binding_lock_unlinked && binding_lock.is_none() {
            if !manifest.deletion_started {
                self.rollback_gc_deleting(&manifest)?;
                return Ok(GcRunResult {
                    removed_candidates: 0,
                    stopped_at: None,
                    gc_epoch: None,
                });
            }
            return Err("cannot reacquire binding lock for started GC epoch".to_string());
        }
        if !manifest.deletion_started {
            manifest.deletion_started = true;
            self.update_gc_manifest(&manifest)?;
        }

        let mut deleted_one = false;
        for surface in manifest
            .surfaces
            .iter()
            .filter(|surface| !surface.is_binding_lock())
        {
            let path = self.root.join(&surface.relative_path);
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    if !surface.matches(&metadata) {
                        return Err(format!(
                            "refusing GC: surface changed after GcDeleting: {}",
                            path.display()
                        ));
                    }
                    fs::remove_file(&path).map_err(|error| {
                        format!("remove GC surface {}: {error}", path.display())
                    })?;
                    deleted_one = true;
                    if control == GcControl::StopAfter(GcCheckpoint::FirstSurfaceDeleted) {
                        return Ok(GcRunResult {
                            removed_candidates: 0,
                            stopped_at: Some(GcCheckpoint::FirstSurfaceDeleted),
                            gc_epoch: Some(manifest.gc_epoch),
                        });
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("stat GC surface {}: {error}", path.display()));
                }
            }
        }
        let _ = deleted_one;

        if !manifest.binding_lock_unlinked {
            if let Some(surface) = manifest
                .surfaces
                .iter()
                .find(|surface| surface.is_binding_lock())
            {
                match fs::symlink_metadata(&binding_path) {
                    Ok(metadata) if surface.matches(&metadata) => {}
                    Ok(_) => return Err("binding lock identity changed during GC".to_string()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(format!("stat binding lock: {error}")),
                }
            }
            match fs::remove_file(&binding_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("unlink binding lock: {error}")),
            }
            manifest.binding_lock_unlinked = true;
            self.update_gc_manifest(&manifest)?;
        }
        if control == GcControl::StopAfter(GcCheckpoint::BindingLockUnlinked) {
            return Ok(GcRunResult {
                removed_candidates: 0,
                stopped_at: Some(GcCheckpoint::BindingLockUnlinked),
                gc_epoch: Some(manifest.gc_epoch),
            });
        }

        self.finish_gc_manifest(&manifest)?;
        drop(binding_lock);
        Ok(GcRunResult {
            removed_candidates: 1,
            stopped_at: None,
            gc_epoch: None,
        })
    }

    fn validate_gc_epoch(&self, manifest: &GcManifest) -> Result<(), String> {
        self.read_transaction(|registry| {
            let bindings = object_map(registry, BINDINGS_KEY)?;
            let binding = bindings
                .get(&manifest.runtime_session_id)
                .and_then(SessionBinding::from_json)
                .ok_or_else(|| "GC binding is missing or malformed".to_string())?;
            match binding.state {
                BindingState::GcDeleting {
                    gc_epoch,
                    binding_epoch,
                    entry_version,
                } if gc_epoch == manifest.gc_epoch
                    && binding_epoch == manifest.binding_epoch
                    && entry_version == manifest.entry_version =>
                {
                    Ok(())
                }
                _ => Err("GC epoch no longer matches binding".to_string()),
            }
        })
    }

    fn update_gc_manifest(&self, manifest: &GcManifest) -> Result<(), String> {
        self.transaction(|registry| {
            validate_manifest_binding(registry, manifest)?;
            let mut manifests = gc_manifests(registry)?;
            let current = manifests
                .get(&manifest.runtime_session_id)
                .ok_or_else(|| "GC manifest disappeared".to_string())?;
            if current.gc_epoch != manifest.gc_epoch {
                return Err("GC manifest epoch changed".to_string());
            }
            manifests.insert(manifest.runtime_session_id.clone(), manifest.clone());
            set_gc_manifests(registry, manifests);
            Ok(())
        })
    }

    fn rollback_gc_deleting(&self, manifest: &GcManifest) -> Result<(), String> {
        self.transaction(|registry| {
            validate_manifest_binding(registry, manifest)?;
            let mut bindings = object_map(registry, BINDINGS_KEY)?;
            bindings.insert(
                manifest.runtime_session_id.clone(),
                SessionBinding {
                    runtime_session_id: manifest.runtime_session_id.clone(),
                    binding_epoch: manifest.binding_epoch.clone(),
                    state: BindingState::Unmanaged,
                }
                .to_json(),
            );
            let mut manifests = gc_manifests(registry)?;
            manifests.remove(&manifest.runtime_session_id);
            set_object_map(registry, BINDINGS_KEY, bindings);
            set_gc_manifests(registry, manifests);
            Ok(())
        })
    }

    fn finish_gc_manifest(&self, manifest: &GcManifest) -> Result<(), String> {
        if manifest
            .surfaces
            .iter()
            .any(|surface| self.root.join(&surface.relative_path).exists())
            || self
                .binding_lock_path(&manifest.runtime_session_id)
                .exists()
        {
            return Err("GC final CAS refused while candidate surfaces remain".to_string());
        }
        self.transaction(|registry| {
            validate_manifest_binding(registry, manifest)?;
            if registry.agents.get(&manifest.runtime_session_id) != manifest.entry.as_ref() {
                return Err("GC entry changed before final CAS".to_string());
            }
            registry.agents.remove(&manifest.runtime_session_id);
            registry.names.retain(|_, value| {
                value
                    .get::<String>()
                    .is_none_or(|id| id != &manifest.runtime_session_id)
            });
            let mut bindings = object_map(registry, BINDINGS_KEY)?;
            bindings.remove(&manifest.runtime_session_id);
            let mut manifests = gc_manifests(registry)?;
            manifests.remove(&manifest.runtime_session_id);
            set_object_map(registry, BINDINGS_KEY, bindings);
            set_gc_manifests(registry, manifests);
            Ok(())
        })
    }

    fn binding_lock_path(&self, id: &str) -> PathBuf {
        self.root
            .join("locks")
            .join(format!("binding-{}.lock", store::sanitize(id)))
    }

    fn runtime_locks_are_quiescent(&self, id: &str) -> Result<bool, String> {
        for path in [
            self.root.join("watchers").join(format!("{id}.lock")),
            self.root.join("locks").join(format!("resume-{id}.lock")),
            self.root.join("spawn-logs").join(format!("{id}.stderr")),
        ] {
            if !existing_file_accepts_exclusive_lock(&path)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn finish_claim(
        &self,
        request: &ClaimManagedAttach,
        canonical_cwd: &str,
        token_sha256: &str,
        binding_epoch: &str,
        claim_version: &str,
    ) -> Result<ClaimOutcome, String> {
        let binding_lock = acquire_binding_lock(
            &self.root,
            &request.runtime_session_id,
            FlockOperation::NonBlockingLockExclusive,
            self.cancel_grace,
        )?;
        let drained = binding_lock.is_some();
        let outcome = self.transaction(|registry| {
            let mut workers = object_map(registry, WORKERS_KEY)?;
            let mut pending = object_map(registry, PENDING_KEY)?;
            let mut bindings = object_map(registry, BINDINGS_KEY)?;
            let binding = bindings
                .get(&request.runtime_session_id)
                .and_then(SessionBinding::from_json)
                .ok_or_else(|| "claim binding disappeared".to_string())?;
            let expected_state = BindingState::Claiming {
                worker_id: request.worker_id.clone(),
                generation: request.generation.clone(),
                claim_version: claim_version.to_string(),
            };
            if binding.binding_epoch != binding_epoch || binding.state != expected_state {
                return Err("claim binding changed before final CAS".to_string());
            }
            let record = pending
                .get(token_sha256)
                .and_then(PendingRecord::from_json)
                .ok_or_else(|| "pending attach disappeared before final CAS".to_string())?;
            if record.claim_version.as_deref() != Some(claim_version) {
                return Err("pending claim version changed before final CAS".to_string());
            }
            let mut worker = workers
                .get(&request.worker_id)
                .and_then(ManagedWorker::from_json)
                .ok_or_else(|| "managed worker disappeared before final CAS".to_string())?;
            if worker.generation != request.generation
                || worker.state != ManagedState::Attaching
                || worker.version != "1"
            {
                return Err("managed worker changed before final CAS".to_string());
            }
            worker.runtime_session_id = Some(request.runtime_session_id.clone());
            worker.claimed_token_sha256 = Some(token_sha256.to_string());
            worker.state = if drained {
                ManagedState::Active
            } else {
                ManagedState::FencingUnconfirmed
            };
            worker.version = next_version(&worker.version)?;
            if !drained {
                worker.fence_reason = Some("older binding epoch did not drain before grace".into());
                let active_operations = active_operation_ids(
                    registry,
                    None,
                    None,
                    std::slice::from_ref(&request.runtime_session_id),
                )?;
                worker.proof_gap = Some(format!(
                    "older binding epoch did not drain before grace; active_operations={}",
                    active_operations.join(",")
                ));
            }
            bindings.insert(
                request.runtime_session_id.clone(),
                SessionBinding {
                    runtime_session_id: request.runtime_session_id.clone(),
                    binding_epoch: binding_epoch.to_string(),
                    state: BindingState::Managed {
                        worker_id: request.worker_id.clone(),
                        generation: request.generation.clone(),
                    },
                }
                .to_json(),
            );
            workers.insert(request.worker_id.clone(), worker.to_json());
            pending.remove(token_sha256);
            upsert_claimed_entry(registry, request, canonical_cwd);
            set_object_map(registry, WORKERS_KEY, workers);
            set_object_map(registry, PENDING_KEY, pending);
            set_object_map(registry, BINDINGS_KEY, bindings);
            if drained {
                Ok(ClaimOutcome::Active {
                    worker: Box::new(worker),
                    duplicate: false,
                })
            } else {
                Ok(refused(
                    &worker,
                    "older binding epoch did not drain before grace",
                ))
            }
        });
        drop(binding_lock);
        outcome
    }

    fn join_claim(
        &self,
        request: &ClaimManagedAttach,
        canonical_cwd: &str,
        token_sha256: &str,
        binding_epoch: &str,
        claim_version: &str,
    ) -> Result<ClaimOutcome, String> {
        let deadline = Instant::now() + self.attach_deadline;
        loop {
            let state = self.read_transaction(|registry| {
                observe_join(
                    registry,
                    request,
                    canonical_cwd,
                    token_sha256,
                    binding_epoch,
                    claim_version,
                )
            })?;
            if let Some(outcome) = state {
                return Ok(outcome);
            }
            if Instant::now() >= deadline {
                return Ok(ClaimOutcome::Refused {
                    worker_id: request.worker_id.clone(),
                    state: ManagedState::FencingUnconfirmed,
                    reason: "timed out joining in-progress managed claim".to_string(),
                });
            }
            thread::sleep(Duration::from_millis(MANAGED_CANCEL_POLL_MS.min(10)));
        }
    }

    fn transaction<T>(
        &self,
        f: impl FnOnce(&mut Registry) -> Result<T, String>,
    ) -> Result<T, String> {
        store::with_lock_at(&self.root, || {
            let mut registry = store::read_registry_at(&self.root);
            let output = f(&mut registry)?;
            store::write_registry_at(&self.root, registry)?;
            Ok(output)
        })
    }

    fn read_transaction<T>(
        &self,
        f: impl FnOnce(&Registry) -> Result<T, String>,
    ) -> Result<T, String> {
        store::with_lock_at(&self.root, || {
            let registry = store::read_registry_at(&self.root);
            f(&registry)
        })
    }
}

enum AdmissionPlan {
    Guard {
        target: GuardTarget,
        lifecycle_version: Option<String>,
        managed: bool,
    },
    Refused {
        worker_id: String,
        state: ManagedState,
        reason: String,
    },
}

impl ReentryGuard {
    pub fn binding_epoch(&self) -> &str {
        target_binding_epoch(&self.target)
    }

    pub fn validate_kind(&self, expected: OperationKind) -> Result<(), String> {
        if self.allowed == expected {
            Ok(())
        } else {
            Err(format!(
                "guard kind {} cannot authorize {}",
                self.allowed.as_str(),
                expected.as_str()
            ))
        }
    }

    pub(crate) fn authorize_use(
        &mut self,
        expected: OperationKind,
    ) -> Result<AuthorizedTarget, String> {
        self.validate_kind(expected)?;
        let mut authorized = self
            .store
            .read_transaction(|registry| self.authorized_from(registry))?;
        authorized.root = self.store.root.clone();
        Ok(authorized)
    }

    fn authorized_from(&self, registry: &Registry) -> Result<AuthorizedTarget, String> {
        let bindings = object_map(registry, BINDINGS_KEY)?;
        let workers = object_map(registry, WORKERS_KEY)?;
        let session_id = target_runtime_session_id(&self.target);
        let binding = bindings
            .get(session_id)
            .and_then(SessionBinding::from_json)
            .ok_or_else(|| "operation binding is missing or malformed".to_string())?;
        if binding.binding_epoch != target_binding_epoch(&self.target) {
            return Err("operation binding epoch changed".to_string());
        }
        let entry = registry
            .agents
            .get(session_id)
            .and_then(Entry::from_json)
            .ok_or_else(|| "operation registry entry is missing or malformed".to_string())?;
        let expected = authorized_target(&self.target);
        if entry.tool != expected.tool
            || entry.dir.as_deref().map(canonical_cwd).as_deref()
                != Some(expected.canonical_cwd.as_str())
            || entry
                .server
                .as_deref()
                .map(|server| sha256_hex(server.as_bytes()))
                != expected.server_fingerprint
        {
            return Err("operation registry authority changed".to_string());
        }
        match (
            target_worker_id(&self.target),
            target_generation(&self.target),
            &binding.state,
        ) {
            (None, None, BindingState::Unmanaged) => {}
            (
                Some(expected_worker),
                Some(expected_generation),
                BindingState::Managed {
                    worker_id,
                    generation,
                },
            ) if worker_id == expected_worker && generation == expected_generation => {
                let worker = workers
                    .get(worker_id)
                    .and_then(ManagedWorker::from_json)
                    .ok_or_else(|| "operation worker is missing or malformed".to_string())?;
                if worker.state != ManagedState::Active
                    || worker.version.as_str()
                        != self.lifecycle_version.as_deref().unwrap_or_default()
                {
                    return Err("operation lifecycle version/state changed".to_string());
                }
            }
            _ => return Err("operation binding state changed".to_string()),
        }
        Ok(authorized_target(&self.target))
    }

    pub(crate) fn with_authorized<T>(
        &mut self,
        expected: OperationKind,
        f: impl FnOnce(&AuthorizedTarget) -> Result<T, String>,
    ) -> Result<T, String> {
        self.validate_kind(expected)?;
        store::with_lock_at(&self.store.root, || {
            let registry = store::read_registry_at(&self.store.root);
            let mut target = self.authorized_from(&registry)?;
            target.root = self.store.root.clone();
            f(&target)
        })
    }

    pub(crate) fn mark_cancellation_unconfirmed(&mut self, reason: &str) -> Result<(), String> {
        let Some(worker_id) = target_worker_id(&self.target).map(str::to_string) else {
            return Ok(());
        };
        let Some(generation) = target_generation(&self.target).map(str::to_string) else {
            return Ok(());
        };
        self.store.transaction(|registry| {
            let mut workers = object_map(registry, WORKERS_KEY)?;
            let mut worker = workers
                .get(&worker_id)
                .and_then(ManagedWorker::from_json)
                .ok_or_else(|| "cancelled operation worker is missing or malformed".to_string())?;
            if worker.generation != generation {
                return Err("cancelled operation generation changed".to_string());
            }
            if worker.state == ManagedState::FencingUnconfirmed {
                return Ok(());
            }
            if worker.state != ManagedState::Fencing || worker.fence_epoch.is_none() {
                return Err("cancelled operation is not in an exact fencing epoch".to_string());
            }
            worker.state = ManagedState::FencingUnconfirmed;
            worker.version = next_version(&worker.version)?;
            worker.proof_gap = Some(format!("{reason}; active_operations={}", self.operation_id));
            workers.insert(worker_id.clone(), worker.to_json());
            set_object_map(registry, WORKERS_KEY, workers);
            Ok(())
        })
    }

    pub(crate) fn cancelled(&mut self) -> bool {
        let _ = (&self.cancel.path, &self.cancel.worker_or_session_epoch);
        self.authorize_use(self.allowed).is_err()
    }

    pub(crate) fn allowed(&self) -> OperationKind {
        self.allowed
    }
}

impl Drop for ReentryGuard {
    fn drop(&mut self) {
        let operation_id = self.operation_id.clone();
        let _ = self.store.transaction(|registry| {
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            operations.remove(&operation_id);
            set_object_map(registry, ACTIVE_OPERATIONS_KEY, operations);
            Ok(())
        });
    }
}

pub fn admit_operation(session_or_worker: &str, kind: OperationKind) -> Result<Admission, String> {
    LifecycleStore::default().admit_operation(session_or_worker, kind)
}

pub fn publish_fence(
    worker_id: &str,
    generation: &str,
    reason: &str,
) -> Result<FenceIntent, String> {
    LifecycleStore::default().publish_fence(worker_id, generation, reason)
}

pub fn drain_prior_operations(intent: FenceIntent) -> Result<FencePermit, DrainError> {
    LifecycleStore::new(intent.root.clone()).drain_prior_operations(intent)
}

fn validate_fence_intent(registry: &Registry, intent: &FenceIntent) -> Result<(), String> {
    let workers = object_map(registry, WORKERS_KEY)?;
    let worker = workers
        .get(&intent.worker_id)
        .and_then(ManagedWorker::from_json)
        .ok_or_else(|| "fence worker is missing or malformed".to_string())?;
    if worker.generation != intent.generation {
        return Err("fence generation changed".to_string());
    }
    if worker.state != ManagedState::Fencing
        || worker.version != intent.fencing_version
        || worker.fence_epoch.as_deref() != Some(intent.fence_epoch.as_str())
    {
        return Err("fence epoch/version/state changed".to_string());
    }
    Ok(())
}

fn operation_uses_appserver(kind: OperationKind) -> bool {
    matches!(
        kind,
        OperationKind::WatchInject
            | OperationKind::WatchAutoTurn
            | OperationKind::WatchAck
            | OperationKind::WakeAppServer
            | OperationKind::InitialTurn
    )
}

fn target_runtime_session_id(target: &GuardTarget) -> &str {
    match target {
        GuardTarget::Session {
            runtime_session_id, ..
        }
        | GuardTarget::AppServerThread {
            runtime_session_id, ..
        } => runtime_session_id,
    }
}

fn target_binding_epoch(target: &GuardTarget) -> &str {
    match target {
        GuardTarget::Session { binding_epoch, .. }
        | GuardTarget::AppServerThread { binding_epoch, .. } => binding_epoch,
    }
}

fn target_worker_id(target: &GuardTarget) -> Option<&str> {
    match target {
        GuardTarget::Session { worker_id, .. } | GuardTarget::AppServerThread { worker_id, .. } => {
            worker_id.as_deref()
        }
    }
}

fn target_generation(target: &GuardTarget) -> Option<&str> {
    match target {
        GuardTarget::Session { generation, .. }
        | GuardTarget::AppServerThread { generation, .. } => generation.as_deref(),
    }
}

fn authorized_target(target: &GuardTarget) -> AuthorizedTarget {
    match target {
        GuardTarget::Session {
            runtime_session_id,
            worker_id,
            generation,
            tool,
            canonical_cwd,
            server,
            server_fingerprint,
            ..
        } => AuthorizedTarget {
            root: PathBuf::new(),
            runtime_session_id: runtime_session_id.clone(),
            worker_id: worker_id.clone(),
            generation: generation.clone(),
            tool: tool.clone(),
            canonical_cwd: canonical_cwd.clone(),
            server: server.clone(),
            server_fingerprint: server_fingerprint.clone(),
            thread_id: None,
        },
        GuardTarget::AppServerThread {
            runtime_session_id,
            worker_id,
            generation,
            tool,
            canonical_cwd,
            server,
            server_fingerprint,
            thread_id,
            ..
        } => {
            let _ = server_fingerprint;
            AuthorizedTarget {
                root: PathBuf::new(),
                runtime_session_id: runtime_session_id.clone(),
                worker_id: worker_id.clone(),
                generation: generation.clone(),
                tool: tool.clone(),
                canonical_cwd: canonical_cwd.clone(),
                server: Some(server.clone()),
                server_fingerprint: Some(server_fingerprint.clone()),
                thread_id: Some(thread_id.clone()),
            }
        }
    }
}

fn activity_lock_path(root: &Path, worker_id: &str, generation: &str) -> PathBuf {
    root.join("locks").join(format!(
        "activity-{}-{}.lock",
        store::sanitize(worker_id),
        store::sanitize(generation)
    ))
}

fn validate_bounded_token(name: &str, value: &str, max: usize) -> Result<(), String> {
    if value.is_empty()
        || value.len() > max
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':')
        })
    {
        Err(format!("{name} is outside the closed token grammar"))
    } else {
        Ok(())
    }
}

pub struct BindingEpochGuard {
    _file: fs::File,
    binding_epoch: String,
    store: LifecycleStore,
    operation_id: String,
}

impl Drop for BindingEpochGuard {
    fn drop(&mut self) {
        let operation_id = self.operation_id.clone();
        let _ = self.store.transaction(|registry| {
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            operations.remove(&operation_id);
            set_object_map(registry, ACTIVE_OPERATIONS_KEY, operations);
            Ok(())
        });
    }
}

impl BindingEpochGuard {
    pub fn binding_epoch(&self) -> &str {
        &self.binding_epoch
    }
}

#[derive(Clone)]
struct PendingRecord {
    attach: PendingAttach,
    claim_version: Option<String>,
}

impl PendingRecord {
    fn new(attach: PendingAttach) -> Self {
        Self {
            attach,
            claim_version: None,
        }
    }

    fn to_json(&self) -> JsonValue {
        let mut object = self.attach.to_object();
        object.insert("claim_version".into(), optional_string(&self.claim_version));
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        Some(Self {
            attach: PendingAttach::from_object(object)?,
            claim_version: optional_string_from(object, "claim_version"),
        })
    }
}

enum ClaimStart {
    Initial {
        binding_epoch: String,
        claim_version: String,
    },
    Join {
        binding_epoch: String,
        claim_version: String,
    },
    Immediate(Box<ClaimOutcome>),
}

fn begin_claim(
    registry: &mut Registry,
    request: &ClaimManagedAttach,
    canonical_cwd: &str,
    token_sha256: &str,
) -> Result<ClaimStart, String> {
    let mut workers = object_map(registry, WORKERS_KEY)?;
    let mut pending = object_map(registry, PENDING_KEY)?;
    let mut bindings = object_map(registry, BINDINGS_KEY)?;

    let Some(mut record) = pending.get(token_sha256).and_then(PendingRecord::from_json) else {
        if let Some(worker) = workers
            .values()
            .filter_map(ManagedWorker::from_json)
            .find(|worker| worker.claimed_token_sha256.as_deref() == Some(token_sha256))
        {
            let exact = worker.worker_id == request.worker_id
                && worker.generation == request.generation
                && worker.runtime_session_id.as_deref() == Some(&request.runtime_session_id)
                && worker.tool == request.tool
                && worker.cwd == canonical_cwd;
            if exact && worker.state == ManagedState::Active {
                return Ok(ClaimStart::Immediate(Box::new(ClaimOutcome::Active {
                    worker: Box::new(worker),
                    duplicate: true,
                })));
            }
            append_audit(
                registry,
                "claim-replay-refused",
                request,
                "claimed token tuple mismatch",
            );
            return Ok(ClaimStart::Immediate(Box::new(refused(
                &worker,
                "claimed token cannot bind another tuple",
            ))));
        }
        return Err("unknown managed attach token".to_string());
    };

    let tuple_matches = record.attach.worker_id == request.worker_id
        && record.attach.generation == request.generation
        && record
            .attach
            .expected_runtime_session_id
            .as_deref()
            .is_none_or(|id| id == request.runtime_session_id)
        && record.attach.expected_tool == request.tool
        && record.attach.expected_cwd == canonical_cwd;
    let worker = workers
        .get(&record.attach.worker_id)
        .and_then(ManagedWorker::from_json)
        .ok_or_else(|| "pending attach references a missing worker".to_string())?;
    if !tuple_matches {
        append_audit(
            registry,
            "claim-tuple-refused",
            request,
            "pending tuple mismatch",
        );
        return Ok(ClaimStart::Immediate(Box::new(refused(
            &worker,
            "pending managed attach tuple mismatch",
        ))));
    }
    if record.attach.expires_at <= store::iso_now() {
        let mut expired = worker;
        if expired.state == ManagedState::Attaching {
            expired.state = ManagedState::Fencing;
            expired.version = next_version(&expired.version)?;
            expired.fence_reason = Some("pending managed attach expired".to_string());
            workers.insert(expired.worker_id.clone(), expired.to_json());
            pending.remove(token_sha256);
            set_object_map(registry, WORKERS_KEY, workers);
            set_object_map(registry, PENDING_KEY, pending);
        }
        append_audit(registry, "claim-expired", request, "pending token expired");
        return Ok(ClaimStart::Immediate(Box::new(refused(
            &expired,
            "pending managed attach expired",
        ))));
    }

    if let Some(binding) = bindings
        .get(&request.runtime_session_id)
        .and_then(SessionBinding::from_json)
    {
        match binding.state {
            BindingState::GcDeleting { .. } => {
                return Ok(ClaimStart::Immediate(Box::new(refused(
                    &worker,
                    "session binding is being deleted",
                ))));
            }
            BindingState::Managed {
                worker_id,
                generation,
            } if worker_id != request.worker_id || generation != request.generation => {
                append_audit(
                    registry,
                    "claim-conflict-refused",
                    request,
                    "session is bound to another worker",
                );
                return Ok(ClaimStart::Immediate(Box::new(refused(
                    &worker,
                    "session is already bound to another worker",
                ))));
            }
            BindingState::Claiming {
                worker_id,
                generation,
                claim_version,
            } if worker_id == request.worker_id
                && generation == request.generation
                && record.claim_version.as_deref() == Some(&claim_version) =>
            {
                return Ok(ClaimStart::Join {
                    binding_epoch: binding.binding_epoch,
                    claim_version,
                });
            }
            BindingState::Claiming { .. } | BindingState::Managed { .. } => {
                return Ok(ClaimStart::Immediate(Box::new(refused(
                    &worker,
                    "session has a conflicting managed claim",
                ))));
            }
            BindingState::Unmanaged => {}
        }
    }

    let prior_epoch = bindings
        .get(&request.runtime_session_id)
        .and_then(SessionBinding::from_json)
        .map(|binding| canonical_u64("binding_epoch", &binding.binding_epoch))
        .transpose()?
        .unwrap_or(0);
    let binding_epoch = prior_epoch
        .checked_add(1)
        .ok_or_else(|| "binding epoch overflow".to_string())?
        .to_string();
    let claim_version = store::uuid_v4();
    record.claim_version = Some(claim_version.clone());
    pending.insert(token_sha256.to_string(), record.to_json());
    bindings.insert(
        request.runtime_session_id.clone(),
        SessionBinding {
            runtime_session_id: request.runtime_session_id.clone(),
            binding_epoch: binding_epoch.clone(),
            state: BindingState::Claiming {
                worker_id: request.worker_id.clone(),
                generation: request.generation.clone(),
                claim_version: claim_version.clone(),
            },
        }
        .to_json(),
    );
    set_object_map(registry, PENDING_KEY, pending);
    set_object_map(registry, BINDINGS_KEY, bindings);
    Ok(ClaimStart::Initial {
        binding_epoch,
        claim_version,
    })
}

fn observe_join(
    registry: &Registry,
    request: &ClaimManagedAttach,
    canonical_cwd: &str,
    token_sha256: &str,
    binding_epoch: &str,
    claim_version: &str,
) -> Result<Option<ClaimOutcome>, String> {
    let bindings = object_map(registry, BINDINGS_KEY)?;
    let workers = object_map(registry, WORKERS_KEY)?;
    let pending = object_map(registry, PENDING_KEY)?;
    let binding = bindings
        .get(&request.runtime_session_id)
        .and_then(SessionBinding::from_json)
        .ok_or_else(|| "joined claim binding disappeared".to_string())?;
    if binding.binding_epoch != binding_epoch {
        return Err("joined claim binding epoch changed".to_string());
    }
    match binding.state {
        BindingState::Claiming {
            worker_id,
            generation,
            claim_version: current,
        } if worker_id == request.worker_id
            && generation == request.generation
            && current == claim_version
            && pending
                .get(token_sha256)
                .and_then(PendingRecord::from_json)
                .is_some_and(|record| record.claim_version.as_deref() == Some(claim_version)) =>
        {
            Ok(None)
        }
        BindingState::Managed {
            worker_id,
            generation,
        } if worker_id == request.worker_id && generation == request.generation => {
            let worker = workers
                .get(&worker_id)
                .and_then(ManagedWorker::from_json)
                .ok_or_else(|| "joined claim worker disappeared".to_string())?;
            if worker.runtime_session_id.as_deref() != Some(&request.runtime_session_id)
                || worker.claimed_token_sha256.as_deref() != Some(token_sha256)
                || worker.tool != request.tool
                || worker.cwd != canonical_cwd
            {
                return Err("joined claim finalized to a different tuple".to_string());
            }
            if worker.state == ManagedState::Active {
                Ok(Some(ClaimOutcome::Active {
                    worker: Box::new(worker),
                    duplicate: true,
                }))
            } else {
                Ok(Some(refused(
                    &worker,
                    "joined managed claim did not become Active",
                )))
            }
        }
        _ => Err("joined claim state changed unexpectedly".to_string()),
    }
}

fn transition_allowed(from: ManagedState, to: ManagedState) -> bool {
    matches!(
        (from, to),
        (
            ManagedState::Attaching,
            ManagedState::Active | ManagedState::Fencing
        ) | (ManagedState::Active, ManagedState::Fencing)
            | (
                ManagedState::Fencing,
                ManagedState::Fenced | ManagedState::FencingUnconfirmed
            )
            | (ManagedState::FencingUnconfirmed, ManagedState::Fencing)
            | (ManagedState::Fenced, ManagedState::TerminalRetained)
            | (
                ManagedState::TerminalRetained,
                ManagedState::TerminalReleasable
            )
            | (
                ManagedState::FencingUnconfirmed,
                ManagedState::TerminalReleasable
            )
    )
}

fn refused(worker: &ManagedWorker, reason: &str) -> ClaimOutcome {
    ClaimOutcome::Refused {
        worker_id: worker.worker_id.clone(),
        state: worker.state,
        reason: reason.to_string(),
    }
}

fn upsert_claimed_entry(
    registry: &mut Registry,
    request: &ClaimManagedAttach,
    canonical_cwd: &str,
) {
    let previous = registry
        .agents
        .get(&request.runtime_session_id)
        .and_then(Entry::from_json);
    let entry = Entry {
        id: request.runtime_session_id.clone(),
        dir: Some(canonical_cwd.to_string()),
        name: previous.as_ref().and_then(|entry| entry.name.clone()),
        tool: request.tool.clone(),
        last_seen: store::iso_now(),
        server: previous.as_ref().and_then(|entry| entry.server.clone()),
        spawned_via: previous.and_then(|entry| entry.spawned_via),
    };
    registry
        .agents
        .insert(request.runtime_session_id.clone(), entry.to_json());
}

fn append_audit(registry: &mut Registry, event: &str, request: &ClaimManagedAttach, reason: &str) {
    let mut audit = registry
        .extra
        .get(AUDIT_KEY)
        .and_then(|value| value.get::<Vec<JsonValue>>())
        .cloned()
        .unwrap_or_default();
    let mut row = HashMap::new();
    row.insert("event".into(), JsonValue::from(event.to_string()));
    row.insert("at".into(), JsonValue::from(store::iso_now()));
    row.insert(
        "worker_id".into(),
        JsonValue::from(request.worker_id.clone()),
    );
    row.insert(
        "generation".into(),
        JsonValue::from(request.generation.clone()),
    );
    row.insert(
        "runtime_session_id".into(),
        JsonValue::from(request.runtime_session_id.clone()),
    );
    row.insert("reason".into(), JsonValue::from(reason.to_string()));
    audit.push(JsonValue::from(row));
    registry
        .extra
        .insert(AUDIT_KEY.into(), JsonValue::from(audit));
}

fn object_map(registry: &Registry, key: &str) -> Result<HashMap<String, JsonValue>, String> {
    match registry.extra.get(key) {
        None => Ok(HashMap::new()),
        Some(value) => value
            .get::<HashMap<String, JsonValue>>()
            .cloned()
            .ok_or_else(|| format!("malformed lifecycle registry field {key}")),
    }
}

fn active_operation_ids(
    registry: &Registry,
    worker_id: Option<&str>,
    generation: Option<&str>,
    sessions: &[String],
) -> Result<Vec<String>, String> {
    let operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
    let mut ids = operations
        .into_iter()
        .filter_map(|(id, value)| {
            let row = value.get::<HashMap<String, JsonValue>>()?;
            let string = |key: &str| row.get(key)?.get::<String>().map(String::as_str);
            let worker_match = worker_id
                .is_some_and(|expected| string("worker_id") == Some(expected))
                && generation.is_some_and(|expected| string("generation") == Some(expected));
            let session_match = string("runtime_session_id")
                .is_some_and(|session| sessions.iter().any(|candidate| candidate == session));
            (worker_match || session_match).then_some(id)
        })
        .collect::<Vec<_>>();
    ids.sort();
    Ok(ids)
}

fn set_object_map(registry: &mut Registry, key: &str, map: HashMap<String, JsonValue>) {
    registry.extra.insert(key.into(), JsonValue::from(map));
}

fn acquire_binding_lock(
    root: &Path,
    runtime_session_id: &str,
    operation: FlockOperation,
    timeout: Duration,
) -> Result<Option<fs::File>, String> {
    let locks = root.join("locks");
    fs::create_dir_all(&locks).map_err(|error| format!("create lifecycle lock dir: {error}"))?;
    let path = locks.join(format!(
        "binding-{}.lock",
        store::sanitize(runtime_session_id)
    ));
    acquire_path_lock(&path, operation, timeout)
}

fn acquire_path_lock(
    path: &Path,
    operation: FlockOperation,
    timeout: Duration,
) -> Result<Option<fs::File>, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create lifecycle lock dir: {error}"))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("open binding lock {}: {error}", path.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match flock(&file, operation) {
            Ok(()) => return Ok(Some(file)),
            Err(error) if error == rustix::io::Errno::AGAIN || error == rustix::io::Errno::INTR => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                thread::sleep(Duration::from_millis(MANAGED_CANCEL_POLL_MS.min(10)));
            }
            Err(error) => {
                return Err(format!("lock lifecycle file {}: {error}", path.display()));
            }
        }
    }
}

fn validate_claim(request: &ClaimManagedAttach) -> Result<(), String> {
    if request.raw_token.is_empty() {
        return Err("managed attach token is empty".to_string());
    }
    validate_uuid("worker_id", &request.worker_id)?;
    validate_uuid("generation", &request.generation)?;
    validate_uuid("runtime_session_id", &request.runtime_session_id)?;
    validate_tool(&request.tool)
}

fn validate_uuid(name: &str, value: &str) -> Result<(), String> {
    if store::is_uuid(value) {
        Ok(())
    } else {
        Err(format!("{name} must be a UUID"))
    }
}

fn validate_tool(tool: &str) -> Result<(), String> {
    if matches!(tool, "claude" | "codex") {
        Ok(())
    } else {
        Err("managed tool must be claude or codex".to_string())
    }
}

fn canonical_cwd(cwd: &str) -> String {
    std::path::absolute(cwd)
        .unwrap_or_else(|_| PathBuf::from(cwd))
        .to_string_lossy()
        .into_owned()
}

fn canonical_u64(name: &str, value: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("{name} is not a checked decimal u64"))?;
    if parsed.to_string() != value {
        return Err(format!("{name} is not canonical decimal"));
    }
    Ok(parsed)
}

fn next_version(value: &str) -> Result<String, String> {
    canonical_u64("version", value)?
        .checked_add(1)
        .map(|version| version.to_string())
        .ok_or_else(|| "version overflow".to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .digest()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to String cannot fail");
            hex
        })
}

fn string(object: &HashMap<String, JsonValue>, key: &str) -> Option<String> {
    object.get(key)?.get::<String>().cloned()
}

fn optional_string_from(object: &HashMap<String, JsonValue>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(|value| value.get::<String>())
        .cloned()
}

fn optional_string(value: &Option<String>) -> JsonValue {
    value
        .clone()
        .map(JsonValue::from)
        .unwrap_or(JsonValue::from(()))
}

impl PendingAttach {
    fn to_object(&self) -> HashMap<String, JsonValue> {
        let mut object = HashMap::new();
        object.insert("worker_id".into(), JsonValue::from(self.worker_id.clone()));
        object.insert(
            "generation".into(),
            JsonValue::from(self.generation.clone()),
        );
        object.insert(
            "token_sha256".into(),
            JsonValue::from(self.token_sha256.clone()),
        );
        object.insert(
            "expected_runtime_session_id".into(),
            optional_string(&self.expected_runtime_session_id),
        );
        object.insert(
            "expected_tool".into(),
            JsonValue::from(self.expected_tool.clone()),
        );
        object.insert(
            "expected_cwd".into(),
            JsonValue::from(self.expected_cwd.clone()),
        );
        object.insert(
            "expires_at".into(),
            JsonValue::from(self.expires_at.clone()),
        );
        object
    }

    fn from_object(object: &HashMap<String, JsonValue>) -> Option<Self> {
        Some(Self {
            worker_id: string(object, "worker_id")?,
            generation: string(object, "generation")?,
            token_sha256: string(object, "token_sha256")?,
            expected_runtime_session_id: optional_string_from(
                object,
                "expected_runtime_session_id",
            ),
            expected_tool: string(object, "expected_tool")?,
            expected_cwd: string(object, "expected_cwd")?,
            expires_at: string(object, "expires_at")?,
        })
    }
}

impl ManagedWorker {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert("worker_id".into(), JsonValue::from(self.worker_id.clone()));
        object.insert(
            "generation".into(),
            JsonValue::from(self.generation.clone()),
        );
        object.insert(
            "runtime_session_id".into(),
            optional_string(&self.runtime_session_id),
        );
        object.insert(
            "claimed_token_sha256".into(),
            optional_string(&self.claimed_token_sha256),
        );
        object.insert("tool".into(), JsonValue::from(self.tool.clone()));
        object.insert("cwd".into(), JsonValue::from(self.cwd.clone()));
        object.insert(
            "state".into(),
            JsonValue::from(self.state.as_str().to_string()),
        );
        object.insert("version".into(), JsonValue::from(self.version.clone()));
        object.insert(
            "required_scope".into(),
            JsonValue::from(self.required_scope.as_str().to_string()),
        );
        object.insert(
            "execution".into(),
            JsonValue::from(self.execution.as_str().to_string()),
        );
        object.insert("process_identity".into(), JsonValue::from(()));
        object.insert("appserver_lineage".into(), JsonValue::from(()));
        object.insert("fence_reason".into(), optional_string(&self.fence_reason));
        object.insert("fence_epoch".into(), optional_string(&self.fence_epoch));
        object.insert("proof_gap".into(), optional_string(&self.proof_gap));
        object.insert(
            "release_receipt".into(),
            self.release_receipt
                .as_ref()
                .map(ReleaseReceipt::to_json)
                .unwrap_or(JsonValue::from(())),
        );
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        let version = string(object, "version")?;
        canonical_u64("version", &version).ok()?;
        Some(Self {
            worker_id: string(object, "worker_id")?,
            generation: string(object, "generation")?,
            runtime_session_id: optional_string_from(object, "runtime_session_id"),
            claimed_token_sha256: optional_string_from(object, "claimed_token_sha256"),
            tool: string(object, "tool")?,
            cwd: string(object, "cwd")?,
            state: ManagedState::parse(&string(object, "state")?)?,
            version,
            required_scope: RequiredScope::parse(&string(object, "required_scope")?)?,
            execution: ExecutionBackend::parse(&string(object, "execution")?)?,
            fence_reason: optional_string_from(object, "fence_reason"),
            fence_epoch: optional_string_from(object, "fence_epoch"),
            proof_gap: optional_string_from(object, "proof_gap"),
            release_receipt: object
                .get("release_receipt")
                .and_then(ReleaseReceipt::from_json),
        })
    }
}

impl SessionBinding {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert(
            "runtime_session_id".into(),
            JsonValue::from(self.runtime_session_id.clone()),
        );
        object.insert(
            "binding_epoch".into(),
            JsonValue::from(self.binding_epoch.clone()),
        );
        let mut state = HashMap::new();
        match &self.state {
            BindingState::Unmanaged => {
                state.insert("kind".into(), JsonValue::from("Unmanaged".to_string()));
            }
            BindingState::GcDeleting {
                gc_epoch,
                binding_epoch,
                entry_version,
            } => {
                state.insert("kind".into(), JsonValue::from("GcDeleting".to_string()));
                state.insert("gc_epoch".into(), JsonValue::from(gc_epoch.clone()));
                state.insert(
                    "binding_epoch".into(),
                    JsonValue::from(binding_epoch.clone()),
                );
                state.insert(
                    "entry_version".into(),
                    JsonValue::from(entry_version.clone()),
                );
            }
            BindingState::Claiming {
                worker_id,
                generation,
                claim_version,
            } => {
                state.insert("kind".into(), JsonValue::from("Claiming".to_string()));
                state.insert("worker_id".into(), JsonValue::from(worker_id.clone()));
                state.insert("generation".into(), JsonValue::from(generation.clone()));
                state.insert(
                    "claim_version".into(),
                    JsonValue::from(claim_version.clone()),
                );
            }
            BindingState::Managed {
                worker_id,
                generation,
            } => {
                state.insert("kind".into(), JsonValue::from("Managed".to_string()));
                state.insert("worker_id".into(), JsonValue::from(worker_id.clone()));
                state.insert("generation".into(), JsonValue::from(generation.clone()));
            }
        }
        object.insert("state".into(), JsonValue::from(state));
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        let binding_epoch = string(object, "binding_epoch")?;
        canonical_u64("binding_epoch", &binding_epoch).ok()?;
        let state = object.get("state")?.get::<HashMap<String, JsonValue>>()?;
        let state = match string(state, "kind")?.as_str() {
            "Unmanaged" => BindingState::Unmanaged,
            "GcDeleting" => BindingState::GcDeleting {
                gc_epoch: string(state, "gc_epoch")?,
                binding_epoch: string(state, "binding_epoch")?,
                entry_version: string(state, "entry_version")?,
            },
            "Claiming" => BindingState::Claiming {
                worker_id: string(state, "worker_id")?,
                generation: string(state, "generation")?,
                claim_version: string(state, "claim_version")?,
            },
            "Managed" => BindingState::Managed {
                worker_id: string(state, "worker_id")?,
                generation: string(state, "generation")?,
            },
            _ => return None,
        };
        Some(Self {
            runtime_session_id: string(object, "runtime_session_id")?,
            binding_epoch,
            state,
        })
    }
}

impl ReleaseReceipt {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert("worker_id".into(), JsonValue::from(self.worker_id.clone()));
        object.insert(
            "generation".into(),
            JsonValue::from(self.generation.clone()),
        );
        object.insert(
            "released_at".into(),
            JsonValue::from(self.released_at.clone()),
        );
        object.insert("mode".into(), JsonValue::from(self.mode.clone()));
        object.insert("reason".into(), JsonValue::from(self.reason.clone()));
        object.insert(
            "evidence_sha256".into(),
            JsonValue::from(self.evidence_sha256.clone()),
        );
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        Some(Self {
            worker_id: string(object, "worker_id")?,
            generation: string(object, "generation")?,
            released_at: string(object, "released_at")?,
            mode: string(object, "mode")?,
            reason: string(object, "reason")?,
            evidence_sha256: string(object, "evidence_sha256")?,
        })
    }
}

impl ManagedTombstone {
    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        Some(Self {
            worker_id: string(object, "worker_id")?,
            generation: string(object, "generation")?,
            final_version: string(object, "final_version")?,
            retained_at: string(object, "retained_at")?,
            receipt: object.get("receipt").and_then(ReleaseReceipt::from_json),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GcSurfaceSnapshot {
    relative_path: String,
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl GcSurfaceSnapshot {
    fn from_metadata(relative_path: String, metadata: &fs::Metadata) -> Self {
        Self {
            relative_path,
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.size(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
        }
    }

    fn matches(&self, metadata: &fs::Metadata) -> bool {
        metadata.file_type().is_file()
            && metadata.dev() == self.dev
            && metadata.ino() == self.ino
            && metadata.size() == self.size
            && metadata.mtime() == self.mtime
            && metadata.mtime_nsec() == self.mtime_nsec
    }

    fn is_binding_lock(&self) -> bool {
        self.relative_path.starts_with("locks/binding-")
    }

    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert(
            "relative_path".into(),
            JsonValue::from(self.relative_path.clone()),
        );
        object.insert("dev".into(), JsonValue::from(self.dev.to_string()));
        object.insert("ino".into(), JsonValue::from(self.ino.to_string()));
        object.insert("size".into(), JsonValue::from(self.size.to_string()));
        object.insert("mtime".into(), JsonValue::from(self.mtime.to_string()));
        object.insert(
            "mtime_nsec".into(),
            JsonValue::from(self.mtime_nsec.to_string()),
        );
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        Some(Self {
            relative_path: string(object, "relative_path")?,
            dev: string(object, "dev")?.parse().ok()?,
            ino: string(object, "ino")?.parse().ok()?,
            size: string(object, "size")?.parse().ok()?,
            mtime: string(object, "mtime")?.parse().ok()?,
            mtime_nsec: string(object, "mtime_nsec")?.parse().ok()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct GcManifest {
    runtime_session_id: String,
    gc_epoch: String,
    binding_epoch: String,
    entry_version: String,
    entry: Option<JsonValue>,
    surfaces: Vec<GcSurfaceSnapshot>,
    deletion_started: bool,
    binding_lock_unlinked: bool,
}

impl GcManifest {
    fn surfaces_match(&self, root: &Path, require_present: bool) -> Result<bool, String> {
        for surface in &self.surfaces {
            let path = root.join(&surface.relative_path);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if surface.matches(&metadata) => {}
                Ok(_) => return Ok(false),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if require_present {
                        return Ok(false);
                    }
                }
                Err(error) => {
                    return Err(format!("stat GC surface {}: {error}", path.display()));
                }
            }
        }
        Ok(true)
    }

    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert(
            "runtime_session_id".into(),
            JsonValue::from(self.runtime_session_id.clone()),
        );
        object.insert("gc_epoch".into(), JsonValue::from(self.gc_epoch.clone()));
        object.insert(
            "binding_epoch".into(),
            JsonValue::from(self.binding_epoch.clone()),
        );
        object.insert(
            "entry_version".into(),
            JsonValue::from(self.entry_version.clone()),
        );
        object.insert(
            "entry".into(),
            self.entry.clone().unwrap_or(JsonValue::from(())),
        );
        object.insert(
            "surfaces".into(),
            JsonValue::from(
                self.surfaces
                    .iter()
                    .map(GcSurfaceSnapshot::to_json)
                    .collect::<Vec<_>>(),
            ),
        );
        object.insert(
            "deletion_started".into(),
            JsonValue::from(self.deletion_started),
        );
        object.insert(
            "binding_lock_unlinked".into(),
            JsonValue::from(self.binding_lock_unlinked),
        );
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        let surfaces = object
            .get("surfaces")?
            .get::<Vec<JsonValue>>()?
            .iter()
            .map(GcSurfaceSnapshot::from_json)
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            runtime_session_id: string(object, "runtime_session_id")?,
            gc_epoch: string(object, "gc_epoch")?,
            binding_epoch: string(object, "binding_epoch")?,
            entry_version: string(object, "entry_version")?,
            entry: object
                .get("entry")
                .filter(|value| value.get::<()>().is_none())
                .cloned(),
            surfaces,
            deletion_started: object.get("deletion_started")?.get::<bool>().copied()?,
            binding_lock_unlinked: object
                .get("binding_lock_unlinked")?
                .get::<bool>()
                .copied()?,
        })
    }
}

fn gc_manifests(registry: &Registry) -> Result<HashMap<String, GcManifest>, String> {
    object_map(registry, GC_MANIFESTS_KEY)?
        .into_iter()
        .map(|(id, value)| {
            GcManifest::from_json(&value)
                .filter(|manifest| manifest.runtime_session_id == id)
                .map(|manifest| (id, manifest))
                .ok_or_else(|| "malformed managed GC manifest".to_string())
        })
        .collect()
}

fn set_gc_manifests(registry: &mut Registry, manifests: HashMap<String, GcManifest>) {
    set_object_map(
        registry,
        GC_MANIFESTS_KEY,
        manifests
            .into_iter()
            .map(|(id, manifest)| (id, manifest.to_json()))
            .collect(),
    );
}

fn validate_manifest_binding(registry: &Registry, manifest: &GcManifest) -> Result<(), String> {
    let bindings = object_map(registry, BINDINGS_KEY)?;
    let binding = bindings
        .get(&manifest.runtime_session_id)
        .and_then(SessionBinding::from_json)
        .ok_or_else(|| "GC binding is missing or malformed".to_string())?;
    match binding.state {
        BindingState::GcDeleting {
            gc_epoch,
            binding_epoch,
            entry_version,
        } if gc_epoch == manifest.gc_epoch
            && binding_epoch == manifest.binding_epoch
            && entry_version == manifest.entry_version =>
        {
            Ok(())
        }
        _ => Err("GC binding epoch/version changed".to_string()),
    }
}

fn lifecycle_references_session(registry: &Registry, id: &str) -> Result<bool, String> {
    let workers = object_map(registry, WORKERS_KEY)?;
    if workers.values().any(|value| {
        ManagedWorker::from_json(value)
            .is_none_or(|worker| worker.runtime_session_id.as_deref() == Some(id))
    }) {
        return Ok(!workers.is_empty());
    }
    let pending = object_map(registry, PENDING_KEY)?;
    if pending.values().any(|value| {
        PendingRecord::from_json(value)
            .is_none_or(|record| record.attach.expected_runtime_session_id.as_deref() == Some(id))
    }) {
        return Ok(!pending.is_empty());
    }
    for key in [
        TOMBSTONES_KEY,
        AUDIT_KEY,
        "fence_intents",
        "lifecycle_proofs",
    ] {
        if registry
            .extra
            .get(key)
            .is_some_and(|value| json_contains_string(value, id))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn registry_protects_session(registry: &Registry, id: &str) -> Result<bool, String> {
    let bindings = object_map(registry, BINDINGS_KEY)?;
    if bindings.contains_key(id) {
        return Ok(true);
    }
    lifecycle_references_session(registry, id)
}

fn json_contains_string(value: &JsonValue, needle: &str) -> bool {
    if value.get::<String>().is_some_and(|value| value == needle) {
        return true;
    }
    if let Some(object) = value.get::<HashMap<String, JsonValue>>() {
        return object
            .values()
            .any(|value| json_contains_string(value, needle));
    }
    value.get::<Vec<JsonValue>>().is_some_and(|array| {
        array
            .iter()
            .any(|value| json_contains_string(value, needle))
    })
}

fn try_exclusive_file(path: &Path) -> Result<Option<fs::File>, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create binding lock directory: {error}"))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("open binding lock {}: {error}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(error) if error == rustix::io::Errno::AGAIN => Ok(None),
        Err(error) => Err(format!("inspect binding lock {}: {error}", path.display())),
    }
}

fn existing_file_accepts_exclusive_lock(path: &Path) -> Result<bool, String> {
    let file = match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(_) => return Ok(false),
    };
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(error) if error == rustix::io::Errno::AGAIN => Ok(false),
        Err(_) => Ok(false),
    }
}

fn system_time_iso(time: SystemTime) -> String {
    let millis = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(i64::MAX as u128) as i64;
    store::iso_from_unix_ms(millis)
}

fn entry_version(value: &JsonValue) -> String {
    let bytes = value
        .stringify()
        .unwrap_or_else(|_| "malformed".to_string());
    sha256_hex(bytes.as_bytes())
}
