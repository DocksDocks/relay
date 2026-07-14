//! Durable managed-worker lifecycle state and session-binding serialization.
//!
//! The legacy registry remains backward compatible and contains discovery
//! projection only. Lifecycle maps live in the separate versioned
//! `lifecycle-v1.json` authority so an older registry writer cannot erase
//! them. Session binding locks are separate kernel locks; the global store lock
//! is never held while waiting for one.

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
pub const SUPERVISOR_CONTROL_BIND_DEADLINE_MS: u64 = 5_000;
pub const WATCHDOG_HEARTBEAT_INTERVAL_MS: u64 = 250;
pub const WATCHDOG_STALE_AFTER_MS: i64 = 1_000;

const WORKERS_KEY: &str = "managed_workers";
const PENDING_KEY: &str = "pending_managed";
const BINDINGS_KEY: &str = "session_bindings";
const TOMBSTONES_KEY: &str = "managed_tombstones";
const AUDIT_KEY: &str = "lifecycle_audit";
const GC_MANIFESTS_KEY: &str = "managed_gc_manifests";
const ACTIVE_OPERATIONS_KEY: &str = "active_operations";
const OPERATION_TOMBSTONES_KEY: &str = "operation_tombstones";
const SUPERVISORS_KEY: &str = "lifecycle_supervisors";
const WATCHDOGS_KEY: &str = "lifecycle_watchdogs";
const LIFECYCLE_AUTHORITY_KEYS: &[&str] = &[
    WORKERS_KEY,
    PENDING_KEY,
    BINDINGS_KEY,
    TOMBSTONES_KEY,
    AUDIT_KEY,
    GC_MANIFESTS_KEY,
    ACTIVE_OPERATIONS_KEY,
    OPERATION_TOMBSTONES_KEY,
    SUPERVISORS_KEY,
    WATCHDOGS_KEY,
    "fence_intents",
    "lifecycle_proofs",
];
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalAction {
    Release,
    Abandon,
}

impl TerminalAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Abandon => "abandon",
        }
    }
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
    UnmanagedCanceling {
        operation_id: String,
        operation_version: String,
        binding_epoch: String,
    },
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

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "SessionStartDrain" => Self::SessionStartDrain,
            "UserPromptDrain" => Self::UserPromptDrain,
            "CliInboxDrain" => Self::CliInboxDrain,
            "McpInboxDrain" => Self::McpInboxDrain,
            "ChannelDeliver" => Self::ChannelDeliver,
            "WatchInject" => Self::WatchInject,
            "WatchAutoTurn" => Self::WatchAutoTurn,
            "WatchAck" => Self::WatchAck,
            "WatchWakeFallback" => Self::WatchWakeFallback,
            "WakeAppServer" => Self::WakeAppServer,
            "WakeCli" => Self::WakeCli,
            "AttachResume" => Self::AttachResume,
            "InitialTurn" => Self::InitialTurn,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartGeneration {
    LinuxProcStartTicks(u64),
    DarwinBsdStartTime { sec: i64, usec: i64 },
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessObservation {
    pub pid: u32,
    pub pgid: Option<i32>,
    pub start: StartGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalCustody {
    None,
    ChildStarting {
        supervisor_instance_id: String,
    },
    ChildOwned {
        supervisor_instance_id: String,
        process: ProcessObservation,
    },
    ChildCancelRequested {
        supervisor_instance_id: String,
        process: ProcessObservation,
        request_id: String,
    },
    ChildReaped {
        supervisor_instance_id: String,
        process: ProcessObservation,
        exit_status: i32,
    },
    LostAuthority {
        last_observation: Option<ProcessObservation>,
        reason: LostAuthorityReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LostAuthorityReason {
    CancelDeadline,
    HandoffRebindFailed,
    SupervisorLost,
    CustodyLost,
    ProxyStartupFailed,
    AppServerClaimFailed,
}

impl LostAuthorityReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::CancelDeadline => "CancelDeadline",
            Self::HandoffRebindFailed => "HandoffRebindFailed",
            Self::SupervisorLost => "SupervisorLost",
            Self::CustodyLost => "CustodyLost",
            Self::ProxyStartupFailed => "ProxyStartupFailed",
            Self::AppServerClaimFailed => "AppServerClaimFailed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "CancelDeadline" => Self::CancelDeadline,
            "HandoffRebindFailed" => Self::HandoffRebindFailed,
            "SupervisorLost" => Self::SupervisorLost,
            "CustodyLost" => Self::CustodyLost,
            "ProxyStartupFailed" => Self::ProxyStartupFailed,
            "AppServerClaimFailed" => Self::AppServerClaimFailed,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LostAuthorityEvidence {
    pub operation_id: String,
    pub operation_version: String,
    pub reason: LostAuthorityReason,
    pub observed_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StdioEndpointMode {
    Closed,
    Pipe,
    Pty {
        terminal_group: String,
        rows: u16,
        cols: u16,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StdioProfile {
    pub stdin: StdioEndpointMode,
    pub stdout: StdioEndpointMode,
    pub stderr: StdioEndpointMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveOperationRecord {
    pub operation_id: String,
    pub runtime_session_id: String,
    pub worker_id: Option<String>,
    pub generation: Option<String>,
    pub binding_epoch: String,
    pub lifecycle_version: Option<String>,
    pub operation_version: String,
    pub kind: OperationKind,
    pub custody: ExternalCustody,
    pub terminal: bool,
    pub cancelled: bool,
    reap_proof: Option<OwnedChildReapProof>,
    launch_spec: Option<ChildLaunchSpec>,
    stdio: Option<StdioProfile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedChildReapProof {
    worker_id: String,
    generation: String,
    operation_id: String,
    supervisor_instance_id: String,
    pid: u32,
    exit_status: i32,
    operation_version: String,
    _sealed: Sealed,
}

pub struct ChildCancellationPermit {
    supervisor_instance_id: String,
    operation_id: String,
    operation_version: String,
    child_slot: u64,
    _sealed: Sealed,
}

impl ActiveOperationRecord {
    pub fn reap_proof(&self) -> Option<&OwnedChildReapProof> {
        self.reap_proof.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisorRecord {
    pub supervisor_instance_id: String,
    pub control_epoch: String,
    pub control_nonce_sha256: String,
    pub operation_id: String,
    pub process: ProcessObservation,
    pub socket_path: PathBuf,
    pub socket_dev: u64,
    pub socket_ino: u64,
    pub version: String,
    pub state: SupervisorState,
    pub heartbeat_at: String,
    pub heartbeat_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorState {
    Starting,
    Ready,
    LostAuthority,
    Terminal,
}

impl SupervisorState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Ready => "Ready",
            Self::LostAuthority => "LostAuthority",
            Self::Terminal => "Terminal",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "Starting" => Self::Starting,
            "Ready" => Self::Ready,
            "LostAuthority" => Self::LostAuthority,
            "Terminal" => Self::Terminal,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisorWatchdogRecord {
    pub watchdog_instance_id: String,
    pub supervisor_instance_id: String,
    pub operation_id: String,
    pub control_epoch: String,
    pub control_nonce_sha256: String,
    pub process: ProcessObservation,
    pub heartbeat_at: String,
    pub heartbeat_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedSupervisorLaunch {
    pub operation_id: String,
    pub operation_version: String,
    pub supervisor_instance_id: String,
    pub root: PathBuf,
    pub stdio: StdioProfile,
    pub control_epoch: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedSupervisorLaunch {
    pub operation: ActiveOperationRecord,
    pub tool: String,
    pub canonical_cwd: String,
    pub server: Option<String>,
    pub spec: ChildLaunchSpec,
    pub stdio: StdioProfile,
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

    pub(crate) fn confirm_turn_terminal(
        self,
        expected_thread_id: &str,
        expected_turn_id: &str,
        observed_thread_id: &str,
        observed_turn_id: &str,
    ) -> Result<ManagedWorker, String> {
        if expected_thread_id.is_empty()
            || expected_turn_id.is_empty()
            || expected_thread_id != observed_thread_id
            || expected_turn_id != observed_turn_id
        {
            self.finish_turn_fence(
                ManagedState::FencingUnconfirmed,
                Some("turn terminal evidence did not match the fenced turn".to_string()),
            )?;
            return Err("turn terminal evidence does not match the fenced turn".to_string());
        }
        self.finish_turn_fence(ManagedState::Fenced, None)
    }

    pub(crate) fn mark_turn_unconfirmed(self, reason: &str) -> Result<ManagedWorker, String> {
        if reason.is_empty() || reason.len() > 4096 {
            return Err("turn uncertainty reason must be 1..=4096 bytes".to_string());
        }
        self.finish_turn_fence(ManagedState::FencingUnconfirmed, Some(reason.to_string()))
    }

    fn finish_turn_fence(
        self,
        next: ManagedState,
        proof_gap: Option<String>,
    ) -> Result<ManagedWorker, String> {
        let store = LifecycleStore::new(self.intent.root.clone());
        store.transaction(|registry| {
            validate_fence_intent(registry, &self.intent)?;
            let mut workers = object_map(registry, WORKERS_KEY)?;
            let mut worker = workers
                .get(&self.intent.worker_id)
                .and_then(ManagedWorker::from_json)
                .ok_or_else(|| "fence worker is missing or malformed".to_string())?;
            worker.state = next;
            worker.version = next_version(&worker.version)?;
            worker.proof_gap = proof_gap;
            workers.insert(self.intent.worker_id.clone(), worker.to_json());
            set_object_map(registry, WORKERS_KEY, workers);
            Ok(worker)
        })
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

impl ChildLaunchSpec {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        match self {
            Self::AttachResume(options) => {
                object.insert("kind".into(), JsonValue::from("AttachResume".to_string()));
                object.insert(
                    "model".into(),
                    options
                        .model()
                        .map(|value| JsonValue::from(value.to_string()))
                        .unwrap_or(JsonValue::from(())),
                );
                object.insert(
                    "effort".into(),
                    options
                        .effort()
                        .map(|value| JsonValue::from(value.to_string()))
                        .unwrap_or(JsonValue::from(())),
                );
            }
            Self::WakeDoorbell(message) | Self::WatchWakeFallback(message) => {
                object.insert(
                    "kind".into(),
                    JsonValue::from(
                        if matches!(self, Self::WakeDoorbell(_)) {
                            "WakeDoorbell"
                        } else {
                            "WatchWakeFallback"
                        }
                        .to_string(),
                    ),
                );
                object.insert(
                    "message".into(),
                    JsonValue::from(message.as_str().to_string()),
                );
                object.insert(
                    "model".into(),
                    message
                        .model()
                        .map(|value| JsonValue::from(value.to_string()))
                        .unwrap_or(JsonValue::from(())),
                );
                object.insert(
                    "effort".into(),
                    message
                        .effort()
                        .map(|value| JsonValue::from(value.to_string()))
                        .unwrap_or(JsonValue::from(())),
                );
            }
        }
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        let model = optional_string_from(object, "model")
            .map(|value| ValidatedModel::parse(&value))
            .transpose()
            .ok()?;
        let effort = optional_string_from(object, "effort")
            .map(|value| ValidatedEffort::parse(&value))
            .transpose()
            .ok()?;
        match string(object, "kind")?.as_str() {
            "AttachResume" => Some(Self::AttachResume(AttachOptions::new(model, effort))),
            "WakeDoorbell" | "WatchWakeFallback" => {
                let message = DoorbellMessage::parse(&string(object, "message")?)
                    .ok()?
                    .with_runtime_options(model, effort);
                if string(object, "kind")? == "WakeDoorbell" {
                    Some(Self::WakeDoorbell(message))
                } else {
                    Some(Self::WatchWakeFallback(message))
                }
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
        self.claim_managed_attach_with_discovery(request, None)
    }

    pub fn claim_managed_appserver_attach(
        &self,
        request: ClaimManagedAttach,
        name: Option<&str>,
        server: &str,
    ) -> Result<ClaimOutcome, String> {
        if server.is_empty() || server.len() > 4096 {
            return Err("app-server address must be 1..=4096 bytes".to_string());
        }
        self.claim_managed_attach_with_discovery(
            request,
            Some(ClaimDiscovery {
                name: name.map(str::to_string),
                server: server.to_string(),
            }),
        )
    }

    fn claim_managed_attach_with_discovery(
        &self,
        request: ClaimManagedAttach,
        discovery: Option<ClaimDiscovery>,
    ) -> Result<ClaimOutcome, String> {
        validate_claim(&request)?;
        self.ensure_target_supervisor_live(&request.runtime_session_id)?;
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
            ClaimStart::WaitUnmanaged {
                binding_epoch,
                claim_version,
                operation_id,
            } => self.wait_unmanaged_cancel(
                &request,
                &token_sha256,
                &binding_epoch,
                &claim_version,
                &operation_id,
                ClaimPublication {
                    canonical_cwd: &canonical_cwd,
                    discovery: discovery.as_ref(),
                },
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
                discovery.as_ref(),
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

    pub fn terminalize_worker(
        &self,
        worker_id: &str,
        generation: &str,
        expected_version: &str,
        action: TerminalAction,
        reason: &str,
    ) -> Result<ManagedWorker, String> {
        validate_uuid("worker_id", worker_id)?;
        validate_uuid("generation", generation)?;
        canonical_u64("expected_version", expected_version)?;
        if reason.is_empty() || reason.len() > 4096 {
            return Err("terminal reason must be 1..=4096 bytes".to_string());
        }
        self.transaction(|registry| {
            let mut workers = object_map(registry, WORKERS_KEY)?;
            let mut worker = workers
                .get(worker_id)
                .and_then(ManagedWorker::from_json)
                .ok_or_else(|| "managed worker not found or malformed".to_string())?;
            if worker.generation != generation {
                return Err("managed worker generation changed".to_string());
            }
            if matches!(
                worker.state,
                ManagedState::TerminalRetained | ManagedState::TerminalReleasable
            ) {
                let repeated = worker.release_receipt.as_ref().is_some_and(|receipt| {
                    receipt.worker_id == worker_id
                        && receipt.generation == generation
                        && receipt.mode == action.as_str()
                        && receipt.reason == reason
                });
                return if repeated {
                    Ok(worker)
                } else {
                    Err(
                        "managed worker is already terminal with a different disposition"
                            .to_string(),
                    )
                };
            }
            if worker.version != expected_version {
                return Err("managed worker version changed".to_string());
            }
            let evidence_sha256 = match action {
                TerminalAction::Release => {
                    if worker.state != ManagedState::Fenced {
                        return Err("release requires a Fenced worker".to_string());
                    }
                    owned_process_release_evidence(registry, &worker)?.ok_or_else(|| {
                        "capacity release requires an exact supervisor-owned process reap proof"
                            .to_string()
                    })?
                }
                TerminalAction::Abandon => {
                    if !matches!(
                        worker.state,
                        ManagedState::Fenced | ManagedState::FencingUnconfirmed
                    ) {
                        return Err(
                            "abandon requires a Fenced or FencingUnconfirmed worker".to_string()
                        );
                    }
                    sha256_hex(format!("abandon-v1:{worker_id}:{generation}:{reason}").as_bytes())
                }
            };
            worker.version = next_version(&worker.version)?;
            worker.state = match action {
                TerminalAction::Release => ManagedState::TerminalReleasable,
                TerminalAction::Abandon => ManagedState::TerminalRetained,
            };
            let receipt = ReleaseReceipt {
                worker_id: worker_id.to_string(),
                generation: generation.to_string(),
                released_at: store::iso_now(),
                mode: action.as_str().to_string(),
                reason: reason.to_string(),
                evidence_sha256,
            };
            worker.release_receipt = Some(receipt.clone());
            workers.insert(worker_id.to_string(), worker.to_json());

            let mut pending = object_map(registry, PENDING_KEY)?;
            pending.retain(|_, value| {
                PendingRecord::from_json(value).is_none_or(|record| {
                    record.attach.worker_id != worker_id || record.attach.generation != generation
                })
            });
            let mut tombstones = object_map(registry, TOMBSTONES_KEY)?;
            tombstones.insert(
                format!("{worker_id}:{generation}"),
                ManagedTombstone {
                    worker_id: worker_id.to_string(),
                    generation: generation.to_string(),
                    final_version: worker.version.clone(),
                    retained_at: store::iso_now(),
                    receipt: Some(receipt),
                }
                .to_json(),
            );
            set_object_map(registry, WORKERS_KEY, workers);
            set_object_map(registry, PENDING_KEY, pending);
            set_object_map(registry, TOMBSTONES_KEY, tombstones);
            Ok(worker)
        })
    }

    pub fn admit_operation(
        &self,
        session_or_worker: &str,
        kind: OperationKind,
    ) -> Result<Admission, String> {
        self.ensure_target_supervisor_live(session_or_worker)?;
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
                BindingState::UnmanagedCanceling { .. } => {
                    return Err("session has an unmanaged cancellation in progress".to_string());
                }
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
                worker_or_session_epoch: binding_epoch.clone(),
            },
            operation_id: operation_id.clone(),
            _sealed: Sealed(()),
        };
        self.transaction(|registry| {
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            operations.insert(
                operation_id.clone(),
                ActiveOperationRecord {
                    operation_id: operation_id.clone(),
                    runtime_session_id: session_id.clone(),
                    worker_id: target_worker_id(&guard.target).map(str::to_string),
                    generation: target_generation(&guard.target).map(str::to_string),
                    binding_epoch: binding_epoch.clone(),
                    lifecycle_version: guard.lifecycle_version.clone(),
                    operation_version: "1".to_string(),
                    kind,
                    custody: ExternalCustody::None,
                    terminal: false,
                    cancelled: false,
                    reap_proof: None,
                    launch_spec: None,
                    stdio: None,
                }
                .to_json(),
            );
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
        self.ensure_target_supervisor_live(worker_id)?;
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
        discovery: Option<&ClaimDiscovery>,
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
            upsert_claimed_entry(registry, request, canonical_cwd, discovery);
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

    fn wait_unmanaged_cancel(
        &self,
        request: &ClaimManagedAttach,
        token_sha256: &str,
        binding_epoch: &str,
        claim_version: &str,
        operation_id: &str,
        publication: ClaimPublication<'_>,
    ) -> Result<ClaimOutcome, String> {
        let deadline = Instant::now() + self.attach_deadline;
        let next_epoch = next_version(binding_epoch)?;
        loop {
            let binding = self.read_binding(&request.runtime_session_id)?;
            match binding {
                Some(SessionBinding {
                    binding_epoch: current_epoch,
                    state:
                        BindingState::Claiming {
                            worker_id,
                            generation,
                            claim_version: current_claim,
                        },
                    ..
                }) if current_epoch == next_epoch
                    && worker_id == request.worker_id
                    && generation == request.generation
                    && current_claim == claim_version =>
                {
                    return self.finish_claim(
                        request,
                        publication.canonical_cwd,
                        token_sha256,
                        &current_epoch,
                        claim_version,
                        publication.discovery,
                    );
                }
                Some(SessionBinding {
                    binding_epoch: current_epoch,
                    state:
                        BindingState::UnmanagedCanceling {
                            operation_id: current_operation,
                            ..
                        },
                    ..
                }) if current_epoch == binding_epoch && current_operation == operation_id => {}
                Some(SessionBinding {
                    state:
                        BindingState::Managed {
                            worker_id,
                            generation,
                        },
                    ..
                }) if worker_id == request.worker_id && generation == request.generation => {
                    return self.join_claim(
                        request,
                        publication.canonical_cwd,
                        token_sha256,
                        &next_epoch,
                        claim_version,
                    );
                }
                _ => return Err("unmanaged cancellation claim state changed".to_string()),
            }
            if Instant::now() >= deadline {
                return self.expire_unmanaged_claim(
                    request,
                    token_sha256,
                    binding_epoch,
                    claim_version,
                    operation_id,
                    publication,
                );
            }
            thread::sleep(Duration::from_millis(MANAGED_CANCEL_POLL_MS.min(10)));
        }
    }

    fn expire_unmanaged_claim(
        &self,
        request: &ClaimManagedAttach,
        token_sha256: &str,
        binding_epoch: &str,
        claim_version: &str,
        operation_id: &str,
        publication: ClaimPublication<'_>,
    ) -> Result<ClaimOutcome, String> {
        self.transaction(|registry| {
            let mut workers = object_map(registry, WORKERS_KEY)?;
            let mut pending = object_map(registry, PENDING_KEY)?;
            let mut bindings = object_map(registry, BINDINGS_KEY)?;
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let binding = bindings
                .get(&request.runtime_session_id)
                .and_then(SessionBinding::from_json)
                .ok_or_else(|| "cancel-deadline binding disappeared".to_string())?;
            if binding.binding_epoch != binding_epoch
                || !matches!(
                    binding.state,
                    BindingState::UnmanagedCanceling { operation_id: ref current, .. }
                        if current == operation_id
                )
            {
                return Err("cancel-deadline binding changed".to_string());
            }
            let record = pending
                .get(token_sha256)
                .and_then(PendingRecord::from_json)
                .ok_or_else(|| "cancel-deadline pending attach disappeared".to_string())?;
            if record.claim_version.as_deref() != Some(claim_version)
                || record.waiting_runtime_session_id.as_deref()
                    != Some(request.runtime_session_id.as_str())
            {
                return Err("cancel-deadline claim version changed".to_string());
            }
            let mut operation = operations
                .get(operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .ok_or_else(|| "cancel-deadline operation disappeared".to_string())?;
            if operation.terminal {
                return Err("cancel-deadline raced terminal reap".to_string());
            }
            let mut worker = workers
                .get(&request.worker_id)
                .and_then(ManagedWorker::from_json)
                .ok_or_else(|| "cancel-deadline worker disappeared".to_string())?;
            if worker.generation != request.generation || worker.state != ManagedState::Attaching {
                return Err("cancel-deadline worker changed".to_string());
            }
            worker.runtime_session_id = Some(request.runtime_session_id.clone());
            worker.claimed_token_sha256 = Some(token_sha256.to_string());
            worker.state = ManagedState::FencingUnconfirmed;
            worker.version = next_version(&worker.version)?;
            worker.fence_epoch = Some(store::uuid_v4());
            worker.fence_reason = Some("unmanaged cancellation claim deadline expired".to_string());
            worker.proof_gap = Some(format!(
                "LostAuthority/CancelDeadline operation={operation_id}"
            ));
            operation.worker_id = Some(request.worker_id.clone());
            operation.generation = Some(request.generation.clone());
            operation.lifecycle_version = Some(worker.version.clone());
            operation.operation_version = next_version(&operation.operation_version)?;
            bindings.insert(
                request.runtime_session_id.clone(),
                SessionBinding {
                    runtime_session_id: request.runtime_session_id.clone(),
                    binding_epoch: next_version(binding_epoch)?,
                    state: BindingState::Managed {
                        worker_id: request.worker_id.clone(),
                        generation: request.generation.clone(),
                    },
                }
                .to_json(),
            );
            workers.insert(request.worker_id.clone(), worker.to_json());
            operations.insert(operation_id.to_string(), operation.to_json());
            pending.remove(token_sha256);
            upsert_claimed_entry(
                registry,
                request,
                publication.canonical_cwd,
                publication.discovery,
            );
            set_object_map(registry, WORKERS_KEY, workers);
            set_object_map(registry, PENDING_KEY, pending);
            set_object_map(registry, BINDINGS_KEY, bindings);
            set_object_map(registry, ACTIVE_OPERATIONS_KEY, operations);
            Ok(refused(
                &worker,
                "unmanaged cancellation claim deadline expired",
            ))
        })
    }

    pub fn read_operations_for_session(
        &self,
        runtime_session_id: &str,
    ) -> Result<Vec<ActiveOperationRecord>, String> {
        self.read_transaction(|registry| {
            let mut rows = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            rows.extend(object_map(registry, OPERATION_TOMBSTONES_KEY)?);
            let mut operations = rows
                .into_values()
                .filter_map(|value| ActiveOperationRecord::from_json(&value))
                .filter(|operation| operation.runtime_session_id == runtime_session_id)
                .collect::<Vec<_>>();
            operations.sort_by(|left, right| left.operation_id.cmp(&right.operation_id));
            Ok(operations)
        })
    }

    pub fn read_supervisor_for_session(
        &self,
        runtime_session_id: &str,
    ) -> Result<Option<SupervisorRecord>, String> {
        self.read_transaction(|registry| {
            let operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let supervisors = object_map(registry, SUPERVISORS_KEY)?;
            Ok(supervisors
                .values()
                .filter_map(SupervisorRecord::from_json)
                .find(|supervisor| {
                    operations
                        .get(&supervisor.operation_id)
                        .and_then(ActiveOperationRecord::from_json)
                        .is_some_and(|operation| {
                            operation.runtime_session_id == runtime_session_id
                                && !operation.terminal
                        })
                }))
        })
    }

    pub fn read_watchdog_for_session(
        &self,
        runtime_session_id: &str,
    ) -> Result<Option<SupervisorWatchdogRecord>, String> {
        let supervisor = self.read_supervisor_for_session(runtime_session_id)?;
        let Some(supervisor) = supervisor else {
            return Ok(None);
        };
        self.read_transaction(|registry| {
            Ok(object_map(registry, WATCHDOGS_KEY)?
                .into_values()
                .filter_map(|value| SupervisorWatchdogRecord::from_json(&value))
                .find(|watchdog| {
                    watchdog.supervisor_instance_id == supervisor.supervisor_instance_id
                }))
        })
    }

    fn ensure_target_supervisor_live(&self, session_or_worker: &str) -> Result<(), String> {
        let health = self.read_transaction(|registry| {
            let workers = object_map(registry, WORKERS_KEY)?;
            let session = workers
                .get(session_or_worker)
                .and_then(ManagedWorker::from_json)
                .and_then(|worker| worker.runtime_session_id)
                .unwrap_or_else(|| session_or_worker.to_string());
            let operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let supervisor = object_map(registry, SUPERVISORS_KEY)?
                .into_values()
                .filter_map(|value| SupervisorRecord::from_json(&value))
                .find(|supervisor| {
                    operations
                        .get(&supervisor.operation_id)
                        .and_then(ActiveOperationRecord::from_json)
                        .is_some_and(|operation| {
                            operation.runtime_session_id == session && !operation.terminal
                        })
                });
            let Some(supervisor) = supervisor else {
                return Ok(None);
            };
            let watchdog = object_map(registry, WATCHDOGS_KEY)?
                .into_values()
                .filter_map(|value| SupervisorWatchdogRecord::from_json(&value))
                .find(|watchdog| {
                    watchdog.supervisor_instance_id == supervisor.supervisor_instance_id
                });
            Ok(Some((supervisor, watchdog)))
        })?;
        let Some((supervisor, watchdog)) = health else {
            return Ok(());
        };
        let watchdog_live = watchdog.as_ref().is_some_and(|watchdog| {
            process_observation_is_live(&watchdog.process)
                && store::now_ms().saturating_sub(watchdog.heartbeat_at_ms)
                    <= WATCHDOG_STALE_AFTER_MS
                && watchdog.control_epoch == supervisor.control_epoch
                && watchdog.control_nonce_sha256 == supervisor.control_nonce_sha256
        });
        let supervisor_live = process_observation_is_live(&supervisor.process);
        let socket_live = supervisor.state == SupervisorState::Ready
            && store::now_ms().saturating_sub(supervisor.heartbeat_at_ms)
                <= WATCHDOG_STALE_AFTER_MS
            && ping_supervisor(&supervisor).is_ok();
        if watchdog_live && supervisor_live && socket_live {
            return Ok(());
        }
        self.mark_supervisor_lost(
            &supervisor.supervisor_instance_id,
            LostAuthorityReason::SupervisorLost,
        )?;
        Err("lifecycle supervisor/watchdog authority is unavailable".to_string())
    }

    pub(crate) fn read_supervisor_for_session_from_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<SupervisorRecord>, String> {
        self.read_transaction(|registry| {
            Ok(object_map(registry, SUPERVISORS_KEY)?
                .into_values()
                .filter_map(|value| SupervisorRecord::from_json(&value))
                .find(|record| record.operation_id == operation_id))
        })
    }

    pub fn lifecycle_audit_contains(
        &self,
        event: &str,
        runtime_session_id: &str,
    ) -> Result<bool, String> {
        self.read_transaction(|registry| {
            Ok(registry
                .extra
                .get(AUDIT_KEY)
                .and_then(|value| value.get::<Vec<JsonValue>>())
                .is_some_and(|rows| {
                    rows.iter().any(|row| {
                        let Some(object) = row.get::<HashMap<String, JsonValue>>() else {
                            return false;
                        };
                        optional_string_from(object, "event").as_deref() == Some(event)
                            && optional_string_from(object, "runtime_session_id").as_deref()
                                == Some(runtime_session_id)
                    })
                }))
        })
    }

    pub(crate) fn resolve_supervisor_launch(
        &self,
        operation_id: &str,
        operation_version: &str,
        supervisor_instance_id: &str,
    ) -> Result<ResolvedSupervisorLaunch, String> {
        self.read_transaction(|registry| {
            let operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let operation = operations
                .get(operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .ok_or_else(|| "supervisor operation is missing or malformed".to_string())?;
            if operation.operation_version != operation_version
                || !matches!(
                    &operation.custody,
                    ExternalCustody::ChildStarting {
                        supervisor_instance_id: current,
                    } if current == supervisor_instance_id
                )
            {
                return Err("supervisor launch operation/version changed".to_string());
            }
            let entry = registry
                .agents
                .get(&operation.runtime_session_id)
                .and_then(Entry::from_json)
                .ok_or_else(|| "supervisor launch session is missing".to_string())?;
            let spec = operation
                .launch_spec
                .clone()
                .ok_or_else(|| "supervisor launch spec is missing".to_string())?;
            let stdio = operation
                .stdio
                .clone()
                .ok_or_else(|| "supervisor stdio profile is missing".to_string())?;
            Ok(ResolvedSupervisorLaunch {
                operation,
                tool: entry.tool,
                canonical_cwd: entry
                    .dir
                    .map(|dir| canonical_cwd(&dir))
                    .ok_or_else(|| "supervisor launch cwd is missing".to_string())?,
                server: entry.server,
                spec,
                stdio,
            })
        })
    }

    pub(crate) fn record_supervisor_ready(&self, record: SupervisorRecord) -> Result<(), String> {
        self.transaction(|registry| {
            let operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let operation = operations
                .get(&record.operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .ok_or_else(|| "supervisor ready operation is missing".to_string())?;
            if !matches!(
                operation.custody,
                ExternalCustody::ChildStarting { ref supervisor_instance_id }
                    if supervisor_instance_id == &record.supervisor_instance_id
            ) {
                return Err("supervisor ready instance is stale".to_string());
            }
            let mut supervisors = object_map(registry, SUPERVISORS_KEY)?;
            supervisors.insert(record.supervisor_instance_id.clone(), record.to_json());
            set_object_map(registry, SUPERVISORS_KEY, supervisors);
            Ok(())
        })
    }

    pub(crate) fn record_watchdog(
        &self,
        watchdog_instance_id: &str,
        supervisor_instance_id: &str,
        operation_id: &str,
        control_epoch: &str,
        control_nonce_sha256: &str,
        process: &ProcessObservation,
    ) -> Result<(), String> {
        self.transaction(|registry| {
            let mut watchdogs = object_map(registry, WATCHDOGS_KEY)?;
            watchdogs.insert(
                watchdog_instance_id.to_string(),
                SupervisorWatchdogRecord {
                    watchdog_instance_id: watchdog_instance_id.to_string(),
                    supervisor_instance_id: supervisor_instance_id.to_string(),
                    operation_id: operation_id.to_string(),
                    control_epoch: control_epoch.to_string(),
                    control_nonce_sha256: control_nonce_sha256.to_string(),
                    process: process.clone(),
                    heartbeat_at: store::iso_now(),
                    heartbeat_at_ms: store::now_ms(),
                }
                .to_json(),
            );
            set_object_map(registry, WATCHDOGS_KEY, watchdogs);
            Ok(())
        })
    }

    pub(crate) fn validate_watchdog_bootstrap(
        &self,
        supervisor_instance_id: &str,
        control_epoch: &str,
        control_nonce_sha256: &str,
    ) -> Result<(), String> {
        self.read_transaction(|registry| {
            let valid = object_map(registry, WATCHDOGS_KEY)?
                .into_values()
                .filter_map(|value| SupervisorWatchdogRecord::from_json(&value))
                .any(|watchdog| {
                    watchdog.supervisor_instance_id == supervisor_instance_id
                        && watchdog.control_epoch == control_epoch
                        && watchdog.control_nonce_sha256 == control_nonce_sha256
                        && process_observation_is_live(&watchdog.process)
                });
            if valid {
                Ok(())
            } else {
                Err("watchdog bootstrap authority changed".to_string())
            }
        })
    }

    pub(crate) fn heartbeat_watchdog(
        &self,
        watchdog_instance_id: &str,
        supervisor_instance_id: &str,
        control_epoch: &str,
    ) -> Result<(), String> {
        self.transaction(|registry| {
            let mut watchdogs = object_map(registry, WATCHDOGS_KEY)?;
            let mut watchdog = watchdogs
                .get(watchdog_instance_id)
                .and_then(SupervisorWatchdogRecord::from_json)
                .ok_or_else(|| "watchdog heartbeat record is missing".to_string())?;
            if watchdog.supervisor_instance_id != supervisor_instance_id
                || watchdog.control_epoch != control_epoch
            {
                return Err("watchdog heartbeat authority changed".to_string());
            }
            watchdog.heartbeat_at = store::iso_now();
            watchdog.heartbeat_at_ms = store::now_ms();
            watchdogs.insert(watchdog_instance_id.to_string(), watchdog.to_json());
            set_object_map(registry, WATCHDOGS_KEY, watchdogs);
            Ok(())
        })
    }

    pub(crate) fn heartbeat_supervisor(
        &self,
        supervisor_instance_id: &str,
        operation_id: &str,
    ) -> Result<(), String> {
        self.transaction(|registry| {
            let mut supervisors = object_map(registry, SUPERVISORS_KEY)?;
            let mut supervisor = supervisors
                .get(supervisor_instance_id)
                .and_then(SupervisorRecord::from_json)
                .ok_or_else(|| "supervisor heartbeat record is missing".to_string())?;
            if supervisor.operation_id != operation_id || supervisor.state != SupervisorState::Ready
            {
                return Err("supervisor heartbeat authority changed".to_string());
            }
            supervisor.heartbeat_at = store::iso_now();
            supervisor.heartbeat_at_ms = store::now_ms();
            supervisor.version = next_version(&supervisor.version)?;
            supervisors.insert(supervisor_instance_id.to_string(), supervisor.to_json());
            set_object_map(registry, SUPERVISORS_KEY, supervisors);
            Ok(())
        })
    }

    pub(crate) fn finish_watchdog_service(&self, watchdog_instance_id: &str) -> Result<(), String> {
        self.transaction(|registry| {
            let mut watchdogs = object_map(registry, WATCHDOGS_KEY)?;
            watchdogs.remove(watchdog_instance_id);
            set_object_map(registry, WATCHDOGS_KEY, watchdogs);
            Ok(())
        })
    }

    pub(crate) fn finish_supervisor_service(
        &self,
        supervisor_instance_id: &str,
        watchdog_instance_id: &str,
    ) -> Result<(), String> {
        self.transaction(|registry| {
            let mut supervisors = object_map(registry, SUPERVISORS_KEY)?;
            supervisors.remove(supervisor_instance_id);
            let mut watchdogs = object_map(registry, WATCHDOGS_KEY)?;
            watchdogs.remove(watchdog_instance_id);
            set_object_map(registry, SUPERVISORS_KEY, supervisors);
            set_object_map(registry, WATCHDOGS_KEY, watchdogs);
            Ok(())
        })
    }

    pub(crate) fn record_child_owned(
        &self,
        operation_id: &str,
        expected_version: &str,
        supervisor_instance_id: &str,
        process: ProcessObservation,
    ) -> Result<String, String> {
        self.cas_operation(operation_id, expected_version, |operation| {
            if !matches!(
                &operation.custody,
                ExternalCustody::ChildStarting {
                    supervisor_instance_id: current,
                } if current == supervisor_instance_id
            ) {
                return Err("child-owned transition has stale custody".to_string());
            }
            operation.custody = ExternalCustody::ChildOwned {
                supervisor_instance_id: supervisor_instance_id.to_string(),
                process: process.clone(),
            };
            Ok(())
        })
    }

    pub(crate) fn record_child_start_abandoned(
        &self,
        operation_id: &str,
        expected_version: &str,
        supervisor_instance_id: &str,
        _reason: &str,
    ) -> Result<(), String> {
        self.transaction(|registry| {
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let mut operation = operations
                .get(operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .ok_or_else(|| "abandoned child-start operation disappeared".to_string())?;
            if operation.operation_version != expected_version
                || !matches!(
                    operation.custody,
                    ExternalCustody::ChildStarting { supervisor_instance_id: ref current }
                        if current == supervisor_instance_id
                )
            {
                return Err("abandoned child-start authority is stale".to_string());
            }
            operation.operation_version = next_version(&operation.operation_version)?;
            operation.cancelled = true;
            operation.terminal = true;
            operation.custody = ExternalCustody::LostAuthority {
                last_observation: None,
                reason: LostAuthorityReason::ProxyStartupFailed,
            };
            let mut bindings = object_map(registry, BINDINGS_KEY)?;
            if let Some(binding) = bindings
                .get(&operation.runtime_session_id)
                .and_then(SessionBinding::from_json)
            {
                if matches!(binding.state, BindingState::Unmanaged)
                    && binding.binding_epoch == operation.binding_epoch
                {
                    bindings.insert(
                        operation.runtime_session_id.clone(),
                        SessionBinding {
                            runtime_session_id: operation.runtime_session_id.clone(),
                            binding_epoch: next_version(&binding.binding_epoch)?,
                            state: BindingState::Unmanaged,
                        }
                        .to_json(),
                    );
                }
            }
            operations.remove(operation_id);
            let mut tombstones = object_map(registry, OPERATION_TOMBSTONES_KEY)?;
            tombstones.insert(operation_id.to_string(), operation.to_json());
            set_object_map(registry, BINDINGS_KEY, bindings);
            set_object_map(registry, ACTIVE_OPERATIONS_KEY, operations);
            set_object_map(registry, OPERATION_TOMBSTONES_KEY, tombstones);
            Ok(())
        })
    }

    pub(crate) fn publish_disconnect_cancel(
        &self,
        operation_id: &str,
        expected_version: &str,
        supervisor_instance_id: &str,
    ) -> Result<String, String> {
        self.transaction(|registry| {
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let mut operation = operations
                .get(operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .ok_or_else(|| "disconnect operation is missing".to_string())?;
            if operation.operation_version != expected_version || operation.terminal {
                return Err("disconnect operation version is stale".to_string());
            }
            let process = match &operation.custody {
                ExternalCustody::ChildOwned {
                    supervisor_instance_id: current,
                    process,
                } if current == supervisor_instance_id => process.clone(),
                ExternalCustody::ChildCancelRequested {
                    supervisor_instance_id: current,
                    ..
                } if current == supervisor_instance_id => {
                    return Ok(operation.operation_version.clone());
                }
                _ => return Err("disconnect child custody is stale".to_string()),
            };
            let mut bindings = object_map(registry, BINDINGS_KEY)?;
            let binding = bindings
                .get(&operation.runtime_session_id)
                .and_then(SessionBinding::from_json)
                .ok_or_else(|| "disconnect binding is missing".to_string())?;
            let next_operation_version = next_version(&operation.operation_version)?;
            match binding.state {
                BindingState::Unmanaged if binding.binding_epoch == operation.binding_epoch => {
                    bindings.insert(
                        operation.runtime_session_id.clone(),
                        SessionBinding {
                            runtime_session_id: operation.runtime_session_id.clone(),
                            binding_epoch: binding.binding_epoch.clone(),
                            state: BindingState::UnmanagedCanceling {
                                operation_id: operation.operation_id.clone(),
                                operation_version: next_operation_version.clone(),
                                binding_epoch: binding.binding_epoch,
                            },
                        }
                        .to_json(),
                    );
                    append_lifecycle_event(
                        registry,
                        "unmanaged-cancel-published",
                        &operation.runtime_session_id,
                        &operation.operation_id,
                    );
                }
                BindingState::Claiming { .. } => {
                    append_lifecycle_event(
                        registry,
                        "claim-cancel-published",
                        &operation.runtime_session_id,
                        &operation.operation_id,
                    );
                }
                BindingState::Managed {
                    ref worker_id,
                    ref generation,
                } => {
                    let mut workers = object_map(registry, WORKERS_KEY)?;
                    let mut worker = workers
                        .get(worker_id)
                        .and_then(ManagedWorker::from_json)
                        .ok_or_else(|| "disconnect worker is missing".to_string())?;
                    if worker.generation != *generation || worker.state != ManagedState::Active {
                        return Err("disconnect managed authority is stale".to_string());
                    }
                    worker.state = ManagedState::Fencing;
                    worker.version = next_version(&worker.version)?;
                    worker.fence_epoch = Some(store::uuid_v4());
                    worker.fence_reason = Some("supervisor caller disconnected".to_string());
                    workers.insert(worker_id.clone(), worker.to_json());
                    set_object_map(registry, WORKERS_KEY, workers);
                }
                BindingState::UnmanagedCanceling { .. } => {
                    return Ok(operation.operation_version.clone());
                }
                BindingState::GcDeleting { .. } => {
                    return Err("disconnect binding is being deleted".to_string());
                }
                BindingState::Unmanaged => {
                    return Err("disconnect binding epoch changed".to_string());
                }
            }
            let request_id = store::uuid_v4();
            operation.operation_version = next_operation_version.clone();
            operation.cancelled = true;
            operation.custody = ExternalCustody::ChildCancelRequested {
                supervisor_instance_id: supervisor_instance_id.to_string(),
                process,
                request_id,
            };
            operations.insert(operation.operation_id.clone(), operation.to_json());
            set_object_map(registry, BINDINGS_KEY, bindings);
            set_object_map(registry, ACTIVE_OPERATIONS_KEY, operations);
            Ok(next_operation_version)
        })
    }

    pub(crate) fn record_child_reaped(
        &self,
        operation_id: &str,
        expected_version: &str,
        supervisor_instance_id: &str,
        exit_status: i32,
    ) -> Result<String, String> {
        self.transaction(|registry| {
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let mut operation = operations
                .get(operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .ok_or_else(|| "reaped operation is missing".to_string())?;
            if operation.operation_version != expected_version || operation.terminal {
                return Err("reaped operation version is stale".to_string());
            }
            let process = match &operation.custody {
                ExternalCustody::ChildOwned {
                    supervisor_instance_id: current,
                    process,
                }
                | ExternalCustody::ChildCancelRequested {
                    supervisor_instance_id: current,
                    process,
                    ..
                } if current == supervisor_instance_id => process.clone(),
                _ => return Err("reaped child custody is stale".to_string()),
            };
            let next = next_version(&operation.operation_version)?;
            let reaped_pid = process.pid;
            operation.operation_version = next.clone();
            operation.terminal = true;
            operation.custody = ExternalCustody::ChildReaped {
                supervisor_instance_id: supervisor_instance_id.to_string(),
                process,
                exit_status,
            };
            if let (Some(worker_id), Some(generation)) =
                (operation.worker_id.clone(), operation.generation.clone())
            {
                operation.reap_proof = Some(OwnedChildReapProof {
                    worker_id,
                    generation,
                    operation_id: operation.operation_id.clone(),
                    supervisor_instance_id: supervisor_instance_id.to_string(),
                    pid: reaped_pid,
                    exit_status,
                    operation_version: next.clone(),
                    _sealed: Sealed(()),
                });
            }
            let mut bindings = object_map(registry, BINDINGS_KEY)?;
            if let Some(binding) = bindings
                .get(&operation.runtime_session_id)
                .and_then(SessionBinding::from_json)
            {
                if matches!(
                    binding.state,
                    BindingState::UnmanagedCanceling { ref operation_id, .. }
                        if operation_id == &operation.operation_id
                ) {
                    let next_epoch = next_version(&binding.binding_epoch)?;
                    let waiting = object_map(registry, PENDING_KEY)?
                        .into_values()
                        .filter_map(|value| PendingRecord::from_json(&value))
                        .find(|record| {
                            record.waiting_runtime_session_id.as_deref()
                                == Some(operation.runtime_session_id.as_str())
                                && record.claim_version.is_some()
                        });
                    let state = if let Some(waiting) = waiting {
                        BindingState::Claiming {
                            worker_id: waiting.attach.worker_id,
                            generation: waiting.attach.generation,
                            claim_version: waiting
                                .claim_version
                                .expect("waiting claim has a version"),
                        }
                    } else {
                        BindingState::Unmanaged
                    };
                    bindings.insert(
                        operation.runtime_session_id.clone(),
                        SessionBinding {
                            runtime_session_id: operation.runtime_session_id.clone(),
                            binding_epoch: next_epoch,
                            state,
                        }
                        .to_json(),
                    );
                }
            }
            operations.remove(&operation.operation_id);
            let mut tombstones = object_map(registry, OPERATION_TOMBSTONES_KEY)?;
            tombstones.insert(operation.operation_id.clone(), operation.to_json());
            set_object_map(registry, BINDINGS_KEY, bindings);
            set_object_map(registry, ACTIVE_OPERATIONS_KEY, operations);
            set_object_map(registry, OPERATION_TOMBSTONES_KEY, tombstones);
            Ok(next)
        })
    }

    pub(crate) fn mint_child_cancellation_permit(
        &self,
        operation_id: &str,
        operation_version: &str,
        supervisor_instance_id: &str,
        child_slot: u64,
    ) -> Result<ChildCancellationPermit, String> {
        self.read_transaction(|registry| {
            let operation = object_map(registry, ACTIVE_OPERATIONS_KEY)?
                .get(operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .ok_or_else(|| "child cancellation operation disappeared".to_string())?;
            if operation.operation_version != operation_version
                || !matches!(
                    operation.custody,
                    ExternalCustody::ChildCancelRequested { supervisor_instance_id: ref current, .. }
                        if current == supervisor_instance_id
                )
            {
                return Err("child cancellation authority is stale".to_string());
            }
            Ok(ChildCancellationPermit {
                supervisor_instance_id: supervisor_instance_id.to_string(),
                operation_id: operation_id.to_string(),
                operation_version: operation_version.to_string(),
                child_slot,
                _sealed: Sealed(()),
            })
        })
    }

    pub(crate) fn authorize_child_cancellation(
        &self,
        permit: &ChildCancellationPermit,
    ) -> Result<(), String> {
        self.read_transaction(|registry| {
            let operation = object_map(registry, ACTIVE_OPERATIONS_KEY)?
                .get(&permit.operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .ok_or_else(|| "child cancellation operation disappeared".to_string())?;
            if operation.operation_version != permit.operation_version
                || !matches!(
                    operation.custody,
                    ExternalCustody::ChildCancelRequested { ref supervisor_instance_id, ref process, .. }
                        if supervisor_instance_id == &permit.supervisor_instance_id
                            && u64::from(process.pid) == permit.child_slot
                )
            {
                return Err("child cancellation permit is stale".to_string());
            }
            Ok(())
        })
    }

    pub(crate) fn refresh_supervisor_operation_version(
        &self,
        operation_id: &str,
        supervisor_instance_id: &str,
    ) -> Result<String, String> {
        self.read_transaction(|registry| {
            let operation = object_map(registry, ACTIVE_OPERATIONS_KEY)?
                .get(operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .ok_or_else(|| "supervisor operation disappeared".to_string())?;
            if operation.terminal
                || !matches!(
                    operation.custody,
                    ExternalCustody::ChildOwned { supervisor_instance_id: ref current, .. }
                        | ExternalCustody::ChildCancelRequested { supervisor_instance_id: ref current, .. }
                        if current == supervisor_instance_id
                )
            {
                return Err("supervisor no longer owns this operation".to_string());
            }
            Ok(operation.operation_version)
        })
    }

    pub(crate) fn mark_supervisor_lost(
        &self,
        supervisor_instance_id: &str,
        reason: LostAuthorityReason,
    ) -> Result<(), String> {
        self.transaction(|registry| {
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let mut bindings = object_map(registry, BINDINGS_KEY)?;
            let mut workers = object_map(registry, WORKERS_KEY)?;
            let mut changed = Vec::new();
            let operation_ids = operations.keys().cloned().collect::<Vec<_>>();
            for operation_id in operation_ids {
                let Some(mut operation) = operations
                    .get(&operation_id)
                    .and_then(ActiveOperationRecord::from_json)
                else {
                    continue;
                };
                if operation.terminal {
                    continue;
                }
                let observation = match &operation.custody {
                    ExternalCustody::ChildOwned {
                        supervisor_instance_id: current,
                        process,
                    }
                    | ExternalCustody::ChildCancelRequested {
                        supervisor_instance_id: current,
                        process,
                        ..
                    } if current == supervisor_instance_id => Some(process.clone()),
                    ExternalCustody::ChildStarting {
                        supervisor_instance_id: current,
                    } if current == supervisor_instance_id => None,
                    ExternalCustody::LostAuthority { .. } => continue,
                    _ => continue,
                };
                operation.operation_version = next_version(&operation.operation_version)?;
                operation.custody = ExternalCustody::LostAuthority {
                    last_observation: observation,
                    reason,
                };
                changed.push((
                    operation.runtime_session_id.clone(),
                    operation.binding_epoch.clone(),
                    operation.operation_id.clone(),
                    operation.operation_version.clone(),
                    operation.worker_id.clone(),
                    operation.generation.clone(),
                ));
                operations.insert(operation_id, operation.to_json());
            }

            for (session, binding_epoch, operation_id, operation_version, worker_id, _generation) in
                &changed
            {
                if worker_id.is_some() {
                    continue;
                }
                if let Some(binding) = bindings.get(session).and_then(SessionBinding::from_json) {
                    if matches!(binding.state, BindingState::Unmanaged)
                        && binding.binding_epoch == *binding_epoch
                    {
                        bindings.insert(
                            session.clone(),
                            SessionBinding {
                                runtime_session_id: session.clone(),
                                binding_epoch: binding.binding_epoch.clone(),
                                state: BindingState::UnmanagedCanceling {
                                    operation_id: operation_id.clone(),
                                    operation_version: operation_version.clone(),
                                    binding_epoch: binding.binding_epoch,
                                },
                            }
                            .to_json(),
                        );
                    }
                }
            }

            for (_, _, _, _, worker_id, generation) in &changed {
                let (Some(worker_id), Some(generation)) =
                    (worker_id.as_deref(), generation.as_deref())
                else {
                    continue;
                };
                if let Some(mut worker) = workers
                    .get(worker_id)
                    .and_then(ManagedWorker::from_json)
                    .filter(|worker| worker.generation == generation)
                {
                    if worker.state == ManagedState::Active {
                        worker.state = ManagedState::Fencing;
                        worker.version = next_version(&worker.version)?;
                        worker.fence_epoch = Some(store::uuid_v4());
                    }
                    if worker.state == ManagedState::Fencing {
                        worker.state = ManagedState::FencingUnconfirmed;
                        worker.version = next_version(&worker.version)?;
                        worker.proof_gap = Some(reason.as_str().to_string());
                        workers.insert(worker_id.to_string(), worker.to_json());
                    }
                }
            }

            let mut supervisors = object_map(registry, SUPERVISORS_KEY)?;
            if let Some(mut supervisor) = supervisors
                .get(supervisor_instance_id)
                .and_then(SupervisorRecord::from_json)
            {
                supervisor.state = SupervisorState::LostAuthority;
                supervisor.version = next_version(&supervisor.version)?;
                supervisor.heartbeat_at = store::iso_now();
                supervisor.heartbeat_at_ms = store::now_ms();
                supervisors.insert(supervisor_instance_id.to_string(), supervisor.to_json());
            }
            let mut audit = registry
                .extra
                .get(AUDIT_KEY)
                .and_then(|value| value.get::<Vec<JsonValue>>())
                .cloned()
                .unwrap_or_default();
            let mut row = HashMap::new();
            row.insert(
                "event".into(),
                JsonValue::from("supervisor-loss-batch".to_string()),
            );
            row.insert(
                "supervisor_instance_id".into(),
                JsonValue::from(supervisor_instance_id.to_string()),
            );
            row.insert(
                "reason".into(),
                JsonValue::from(reason.as_str().to_string()),
            );
            row.insert(
                "operation_count".into(),
                JsonValue::from(changed.len().to_string()),
            );
            row.insert("observed_at".into(), JsonValue::from(store::iso_now()));
            audit.push(JsonValue::from(row));
            registry
                .extra
                .insert(AUDIT_KEY.to_string(), JsonValue::from(audit));
            set_object_map(registry, ACTIVE_OPERATIONS_KEY, operations);
            set_object_map(registry, BINDINGS_KEY, bindings);
            set_object_map(registry, WORKERS_KEY, workers);
            set_object_map(registry, SUPERVISORS_KEY, supervisors);
            Ok(())
        })
    }

    fn cas_operation(
        &self,
        operation_id: &str,
        expected_version: &str,
        update: impl FnOnce(&mut ActiveOperationRecord) -> Result<(), String>,
    ) -> Result<String, String> {
        self.transaction(|registry| {
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let mut operation = operations
                .get(operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .ok_or_else(|| "operation is missing or malformed".to_string())?;
            if operation.operation_version != expected_version {
                return Err("operation version is stale".to_string());
            }
            update(&mut operation)?;
            operation.operation_version = next_version(&operation.operation_version)?;
            let version = operation.operation_version.clone();
            operations.insert(operation.operation_id.clone(), operation.to_json());
            set_object_map(registry, ACTIVE_OPERATIONS_KEY, operations);
            Ok(version)
        })
    }

    fn transaction<T>(
        &self,
        f: impl FnOnce(&mut Registry) -> Result<T, String>,
    ) -> Result<T, String> {
        store::with_lock_at(&self.root, || {
            let mut registry = store::read_registry_at(&self.root);
            hydrate_lifecycle_authority(&self.root, &mut registry)?;
            let output = f(&mut registry)?;
            let authority = take_lifecycle_authority(&mut registry);
            store::write_lifecycle_authority_at(&self.root, authority)?;
            store::write_registry_at(&self.root, registry)?;
            Ok(output)
        })
    }

    fn read_transaction<T>(
        &self,
        f: impl FnOnce(&Registry) -> Result<T, String>,
    ) -> Result<T, String> {
        store::with_lock_at(&self.root, || {
            let mut registry = store::read_registry_at(&self.root);
            hydrate_lifecycle_authority(&self.root, &mut registry)?;
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

    pub(crate) fn into_fence_intent(self, reason: &str) -> Result<FenceIntent, String> {
        if reason.is_empty() || reason.len() > 4096 {
            return Err("fence reason must be 1..=4096 bytes".to_string());
        }
        let worker_id = target_worker_id(&self.target)
            .ok_or_else(|| "unmanaged guard cannot publish a managed fence".to_string())?
            .to_string();
        let generation = target_generation(&self.target)
            .ok_or_else(|| "managed guard is missing its generation".to_string())?
            .to_string();
        let lifecycle_version = self
            .lifecycle_version
            .clone()
            .ok_or_else(|| "managed guard is missing its lifecycle version".to_string())?;
        self.store.transaction(|registry| {
            self.authorized_from(registry)?;
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let operation = operations
                .get(&self.operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .ok_or_else(|| "managed guard operation is missing or malformed".to_string())?;
            if operation.worker_id.as_deref() != Some(worker_id.as_str())
                || operation.generation.as_deref() != Some(generation.as_str())
                || operation.binding_epoch != target_binding_epoch(&self.target)
                || operation.lifecycle_version.as_deref() != Some(lifecycle_version.as_str())
                || operation.kind != self.allowed
                || operation.terminal
                || !matches!(operation.custody, ExternalCustody::None)
            {
                return Err("managed guard operation authority changed".to_string());
            }
            let mut workers = object_map(registry, WORKERS_KEY)?;
            let mut worker = workers
                .get(&worker_id)
                .and_then(ManagedWorker::from_json)
                .ok_or_else(|| "managed worker not found or malformed".to_string())?;
            if worker.generation != generation
                || worker.version != lifecycle_version
                || worker.state != ManagedState::Active
            {
                return Err("managed guard lifecycle authority changed".to_string());
            }
            let fence_epoch = store::uuid_v4();
            worker.state = ManagedState::Fencing;
            worker.version = next_version(&worker.version)?;
            worker.fence_reason = Some(reason.to_string());
            worker.fence_epoch = Some(fence_epoch.clone());
            workers.insert(worker_id.clone(), worker.to_json());
            operations.remove(&self.operation_id);
            set_object_map(registry, WORKERS_KEY, workers);
            set_object_map(registry, ACTIVE_OPERATIONS_KEY, operations);
            Ok(FenceIntent {
                worker_id,
                generation,
                fence_epoch,
                fencing_version: worker.version,
                root: self.store.root.clone(),
            })
        })
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
        self.store.read_transaction(|registry| {
            let mut target = self.authorized_from(registry)?;
            target.root = self.store.root.clone();
            f(&target)
        })
    }
    pub(crate) fn cancelled(&mut self) -> bool {
        let _ = (&self.cancel.path, &self.cancel.worker_or_session_epoch);
        self.authorize_use(self.allowed).is_err()
    }

    pub(crate) fn allowed(&self) -> OperationKind {
        self.allowed
    }

    pub(crate) fn prepare_supervisor_launch(
        &mut self,
        spec: ChildLaunchSpec,
        stdio: StdioProfile,
    ) -> Result<PreparedSupervisorLaunch, String> {
        self.authorize_use(self.allowed)?;
        let operation_id = self.operation_id.clone();
        let allowed = self.allowed;
        let root = self.store.root.clone();
        let supervisor_instance_id = store::uuid_v4();
        let supervisor_for_cas = supervisor_instance_id.clone();
        let stdio_for_cas = stdio.clone();
        let version = self
            .store
            .cas_operation(&operation_id, "1", move |operation| {
                if operation.kind != allowed || !matches!(operation.custody, ExternalCustody::None)
                {
                    return Err("supervisor launch operation is not pristine".to_string());
                }
                operation.launch_spec = Some(spec);
                operation.stdio = Some(stdio_for_cas);
                operation.custody = ExternalCustody::ChildStarting {
                    supervisor_instance_id: supervisor_for_cas,
                };
                Ok(())
            })?;
        Ok(PreparedSupervisorLaunch {
            operation_id,
            operation_version: version,
            supervisor_instance_id,
            root,
            stdio,
            control_epoch: String::new(),
        })
    }
}

impl Drop for ReentryGuard {
    fn drop(&mut self) {
        let operation_id = self.operation_id.clone();
        let _ = self.store.transaction(|registry| {
            let mut operations = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
            let removable = operations
                .get(&operation_id)
                .and_then(ActiveOperationRecord::from_json)
                .is_some_and(|operation| matches!(operation.custody, ExternalCustody::None));
            if removable {
                operations.remove(&operation_id);
            }
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
            tool,
            canonical_cwd,
            server,
            server_fingerprint,
            ..
        } => AuthorizedTarget {
            root: PathBuf::new(),
            runtime_session_id: runtime_session_id.clone(),
            tool: tool.clone(),
            canonical_cwd: canonical_cwd.clone(),
            server: server.clone(),
            server_fingerprint: server_fingerprint.clone(),
            thread_id: None,
        },
        GuardTarget::AppServerThread {
            runtime_session_id,
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
    waiting_runtime_session_id: Option<String>,
}

#[derive(Clone)]
struct ClaimDiscovery {
    name: Option<String>,
    server: String,
}

#[derive(Clone, Copy)]
struct ClaimPublication<'a> {
    canonical_cwd: &'a str,
    discovery: Option<&'a ClaimDiscovery>,
}

impl PendingRecord {
    fn new(attach: PendingAttach) -> Self {
        Self {
            attach,
            claim_version: None,
            waiting_runtime_session_id: None,
        }
    }

    fn to_json(&self) -> JsonValue {
        let mut object = self.attach.to_object();
        object.insert("claim_version".into(), optional_string(&self.claim_version));
        object.insert(
            "waiting_runtime_session_id".into(),
            optional_string(&self.waiting_runtime_session_id),
        );
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        Some(Self {
            attach: PendingAttach::from_object(object)?,
            claim_version: optional_string_from(object, "claim_version"),
            waiting_runtime_session_id: optional_string_from(object, "waiting_runtime_session_id"),
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
    WaitUnmanaged {
        binding_epoch: String,
        claim_version: String,
        operation_id: String,
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
            BindingState::UnmanagedCanceling {
                operation_id,
                operation_version: _,
                binding_epoch: cancelling_epoch,
            } => {
                if cancelling_epoch != binding.binding_epoch {
                    return Err("unmanaged cancellation binding epoch is malformed".to_string());
                }
                let claim_version = record.claim_version.clone().unwrap_or_else(store::uuid_v4);
                record.claim_version = Some(claim_version.clone());
                record.waiting_runtime_session_id = Some(request.runtime_session_id.clone());
                pending.insert(token_sha256.to_string(), record.to_json());
                set_object_map(registry, PENDING_KEY, pending);
                return Ok(ClaimStart::WaitUnmanaged {
                    binding_epoch: binding.binding_epoch,
                    claim_version,
                    operation_id,
                });
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
    )
}

fn owned_process_release_evidence(
    registry: &Registry,
    worker: &ManagedWorker,
) -> Result<Option<String>, String> {
    if worker.required_scope != RequiredScope::ProcessOnly
        || worker.execution != ExecutionBackend::SupervisorOwnedProcess
    {
        return Ok(None);
    }
    let active = object_map(registry, ACTIVE_OPERATIONS_KEY)?;
    for value in active.values() {
        let operation = ActiveOperationRecord::from_json(value)
            .ok_or_else(|| "malformed active operation while proving release".to_string())?;
        if operation.worker_id.as_deref() == Some(worker.worker_id.as_str())
            && operation.generation.as_deref() == Some(worker.generation.as_str())
        {
            return Ok(None);
        }
    }
    let tombstones = object_map(registry, OPERATION_TOMBSTONES_KEY)?;
    let mut evidence = Vec::new();
    for value in tombstones.values() {
        let Some(raw) = value.get::<HashMap<String, JsonValue>>() else {
            return Ok(None);
        };
        let matches_worker = optional_string_from(raw, "worker_id").as_deref()
            == Some(worker.worker_id.as_str())
            && optional_string_from(raw, "generation").as_deref()
                == Some(worker.generation.as_str());
        if !matches_worker {
            continue;
        }
        let Some(operation) = ActiveOperationRecord::from_json(value) else {
            return Ok(None);
        };
        let Some(proof) = operation.reap_proof.as_ref() else {
            return Ok(None);
        };
        let custody_matches = matches!(
            &operation.custody,
            ExternalCustody::ChildReaped {
                supervisor_instance_id,
                process,
                exit_status,
            } if supervisor_instance_id == &proof.supervisor_instance_id
                && process.pid == proof.pid
                && exit_status == &proof.exit_status
        );
        if !operation.terminal
            || !custody_matches
            || proof.worker_id != worker.worker_id
            || proof.generation != worker.generation
            || proof.operation_id != operation.operation_id
            || proof.operation_version != operation.operation_version
        {
            return Ok(None);
        }
        evidence.push(sha256_hex(
            format!(
                "owned-child-reap-v1:{}:{}:{}:{}:{}:{}:{}",
                proof.worker_id,
                proof.generation,
                proof.operation_id,
                proof.supervisor_instance_id,
                proof.pid,
                proof.exit_status,
                proof.operation_version,
            )
            .as_bytes(),
        ));
    }
    evidence.sort();
    if evidence.is_empty() {
        return Ok(None);
    }
    Ok(Some(sha256_hex(
        format!("owned-child-reap-set-v1:{}", evidence.join(":"),).as_bytes(),
    )))
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
    discovery: Option<&ClaimDiscovery>,
) {
    let previous = registry
        .agents
        .get(&request.runtime_session_id)
        .and_then(Entry::from_json);
    let entry = Entry {
        id: request.runtime_session_id.clone(),
        dir: Some(canonical_cwd.to_string()),
        name: discovery
            .and_then(|value| value.name.clone())
            .or_else(|| previous.as_ref().and_then(|entry| entry.name.clone())),
        tool: request.tool.clone(),
        last_seen: store::iso_now(),
        server: discovery
            .map(|value| value.server.clone())
            .or_else(|| previous.as_ref().and_then(|entry| entry.server.clone())),
        spawned_via: discovery
            .map(|_| "app-server".to_string())
            .or_else(|| previous.and_then(|entry| entry.spawned_via)),
    };
    registry
        .agents
        .insert(request.runtime_session_id.clone(), entry.to_json());
    if let Some(name) = &entry.name {
        registry.names.retain(|key, value| {
            !(value
                .get::<String>()
                .is_some_and(|id| id == &request.runtime_session_id)
                && key != name)
        });
        registry.names.insert(
            name.clone(),
            JsonValue::from(request.runtime_session_id.clone()),
        );
    }
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

fn append_lifecycle_event(
    registry: &mut Registry,
    event: &str,
    runtime_session_id: &str,
    operation_id: &str,
) {
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
        "runtime_session_id".into(),
        JsonValue::from(runtime_session_id.to_string()),
    );
    row.insert(
        "operation_id".into(),
        JsonValue::from(operation_id.to_string()),
    );
    audit.push(JsonValue::from(row));
    registry
        .extra
        .insert(AUDIT_KEY.into(), JsonValue::from(audit));
}

fn hydrate_lifecycle_authority(root: &Path, registry: &mut Registry) -> Result<(), String> {
    for key in LIFECYCLE_AUTHORITY_KEYS {
        registry.extra.remove(*key);
    }
    let Some(authority) = store::read_lifecycle_authority_at(root)? else {
        return Ok(());
    };
    if let Some(key) = authority
        .keys()
        .find(|key| !LIFECYCLE_AUTHORITY_KEYS.contains(&key.as_str()))
    {
        return Err(format!(
            "malformed lifecycle authority: unknown state key {key}"
        ));
    }
    for (key, value) in &authority {
        let well_formed = if key == AUDIT_KEY {
            value.get::<Vec<JsonValue>>().is_some()
        } else {
            value.get::<HashMap<String, JsonValue>>().is_some()
        };
        if !well_formed {
            return Err(format!(
                "malformed lifecycle authority: invalid state field {key}"
            ));
        }
    }
    registry.extra.extend(authority);
    Ok(())
}

fn take_lifecycle_authority(registry: &mut Registry) -> HashMap<String, JsonValue> {
    let mut authority = HashMap::new();
    for key in LIFECYCLE_AUTHORITY_KEYS {
        if let Some(value) = registry.extra.remove(*key) {
            authority.insert((*key).to_string(), value);
        }
    }
    authority
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

impl ProcessObservation {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert("pid".into(), JsonValue::from(self.pid.to_string()));
        object.insert(
            "pgid".into(),
            self.pgid
                .map(|value| JsonValue::from(value.to_string()))
                .unwrap_or(JsonValue::from(())),
        );
        let mut start = HashMap::new();
        match self.start {
            StartGeneration::LinuxProcStartTicks(ticks) => {
                start.insert(
                    "kind".into(),
                    JsonValue::from("LinuxProcStartTicks".to_string()),
                );
                start.insert("ticks".into(), JsonValue::from(ticks.to_string()));
            }
            StartGeneration::DarwinBsdStartTime { sec, usec } => {
                start.insert(
                    "kind".into(),
                    JsonValue::from("DarwinBsdStartTime".to_string()),
                );
                start.insert("sec".into(), JsonValue::from(sec.to_string()));
                start.insert("usec".into(), JsonValue::from(usec.to_string()));
            }
            StartGeneration::Unavailable => {
                start.insert("kind".into(), JsonValue::from("Unavailable".to_string()));
            }
        }
        object.insert("start".into(), JsonValue::from(start));
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        let start = object.get("start")?.get::<HashMap<String, JsonValue>>()?;
        let start = match string(start, "kind")?.as_str() {
            "LinuxProcStartTicks" => {
                StartGeneration::LinuxProcStartTicks(string(start, "ticks")?.parse().ok()?)
            }
            "DarwinBsdStartTime" => StartGeneration::DarwinBsdStartTime {
                sec: string(start, "sec")?.parse().ok()?,
                usec: string(start, "usec")?.parse().ok()?,
            },
            "Unavailable" => StartGeneration::Unavailable,
            _ => return None,
        };
        Some(Self {
            pid: string(object, "pid")?.parse().ok()?,
            pgid: optional_string_from(object, "pgid").and_then(|value| value.parse().ok()),
            start,
        })
    }
}

impl ExternalCustody {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        let (kind, supervisor, process) = match self {
            Self::None => ("None", None, None),
            Self::ChildStarting {
                supervisor_instance_id,
            } => ("ChildStarting", Some(supervisor_instance_id), None),
            Self::ChildOwned {
                supervisor_instance_id,
                process,
            } => ("ChildOwned", Some(supervisor_instance_id), Some(process)),
            Self::ChildCancelRequested {
                supervisor_instance_id,
                process,
                request_id,
            } => {
                object.insert("request_id".into(), JsonValue::from(request_id.clone()));
                (
                    "ChildCancelRequested",
                    Some(supervisor_instance_id),
                    Some(process),
                )
            }
            Self::ChildReaped {
                supervisor_instance_id,
                process,
                exit_status,
            } => {
                object.insert(
                    "exit_status".into(),
                    JsonValue::from(exit_status.to_string()),
                );
                ("ChildReaped", Some(supervisor_instance_id), Some(process))
            }
            Self::LostAuthority {
                last_observation,
                reason,
            } => {
                object.insert(
                    "reason".into(),
                    JsonValue::from(reason.as_str().to_string()),
                );
                object.insert(
                    "last_observation".into(),
                    last_observation
                        .as_ref()
                        .map(ProcessObservation::to_json)
                        .unwrap_or(JsonValue::from(())),
                );
                ("LostAuthority", None, None)
            }
        };
        object.insert("kind".into(), JsonValue::from(kind.to_string()));
        if let Some(supervisor) = supervisor {
            object.insert(
                "supervisor_instance_id".into(),
                JsonValue::from(supervisor.clone()),
            );
        }
        if let Some(process) = process {
            object.insert("process".into(), process.to_json());
        }
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        let supervisor = || string(object, "supervisor_instance_id");
        let process = || {
            object
                .get("process")
                .and_then(ProcessObservation::from_json)
        };
        match string(object, "kind")?.as_str() {
            "None" => Some(Self::None),
            "ChildStarting" => Some(Self::ChildStarting {
                supervisor_instance_id: supervisor()?,
            }),
            "ChildOwned" => Some(Self::ChildOwned {
                supervisor_instance_id: supervisor()?,
                process: process()?,
            }),
            "ChildCancelRequested" => Some(Self::ChildCancelRequested {
                supervisor_instance_id: supervisor()?,
                process: process()?,
                request_id: string(object, "request_id")?,
            }),
            "ChildReaped" => Some(Self::ChildReaped {
                supervisor_instance_id: supervisor()?,
                process: process()?,
                exit_status: string(object, "exit_status")?.parse().ok()?,
            }),
            "LostAuthority" => Some(Self::LostAuthority {
                last_observation: object
                    .get("last_observation")
                    .and_then(ProcessObservation::from_json),
                reason: LostAuthorityReason::parse(&string(object, "reason")?)?,
            }),
            _ => None,
        }
    }
}

fn stdio_endpoint_to_json(endpoint: &StdioEndpointMode) -> JsonValue {
    let mut object = HashMap::new();
    match endpoint {
        StdioEndpointMode::Closed => {
            object.insert("kind".into(), JsonValue::from("Closed".to_string()));
        }
        StdioEndpointMode::Pipe => {
            object.insert("kind".into(), JsonValue::from("Pipe".to_string()));
        }
        StdioEndpointMode::Pty {
            terminal_group,
            rows,
            cols,
        } => {
            object.insert("kind".into(), JsonValue::from("Pty".to_string()));
            object.insert(
                "terminal_group".into(),
                JsonValue::from(terminal_group.clone()),
            );
            object.insert("rows".into(), JsonValue::from(rows.to_string()));
            object.insert("cols".into(), JsonValue::from(cols.to_string()));
        }
    }
    JsonValue::from(object)
}

fn stdio_endpoint_from_json(value: &JsonValue) -> Option<StdioEndpointMode> {
    let object = value.get::<HashMap<String, JsonValue>>()?;
    match string(object, "kind")?.as_str() {
        "Closed" => Some(StdioEndpointMode::Closed),
        "Pipe" => Some(StdioEndpointMode::Pipe),
        "Pty" => Some(StdioEndpointMode::Pty {
            terminal_group: string(object, "terminal_group")?,
            rows: string(object, "rows")?.parse().ok()?,
            cols: string(object, "cols")?.parse().ok()?,
        }),
        _ => None,
    }
}

impl StdioProfile {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert("stdin".into(), stdio_endpoint_to_json(&self.stdin));
        object.insert("stdout".into(), stdio_endpoint_to_json(&self.stdout));
        object.insert("stderr".into(), stdio_endpoint_to_json(&self.stderr));
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        Some(Self {
            stdin: stdio_endpoint_from_json(object.get("stdin")?)?,
            stdout: stdio_endpoint_from_json(object.get("stdout")?)?,
            stderr: stdio_endpoint_from_json(object.get("stderr")?)?,
        })
    }
}

impl ActiveOperationRecord {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert(
            "operation_id".into(),
            JsonValue::from(self.operation_id.clone()),
        );
        object.insert(
            "runtime_session_id".into(),
            JsonValue::from(self.runtime_session_id.clone()),
        );
        object.insert("worker_id".into(), optional_string(&self.worker_id));
        object.insert("generation".into(), optional_string(&self.generation));
        object.insert(
            "binding_epoch".into(),
            JsonValue::from(self.binding_epoch.clone()),
        );
        object.insert(
            "lifecycle_version".into(),
            optional_string(&self.lifecycle_version),
        );
        object.insert(
            "operation_version".into(),
            JsonValue::from(self.operation_version.clone()),
        );
        object.insert(
            "kind".into(),
            JsonValue::from(self.kind.as_str().to_string()),
        );
        object.insert("custody".into(), self.custody.to_json());
        object.insert("terminal".into(), JsonValue::from(self.terminal));
        object.insert("cancelled".into(), JsonValue::from(self.cancelled));
        object.insert(
            "reap_proof".into(),
            self.reap_proof
                .as_ref()
                .map(OwnedChildReapProof::to_json)
                .unwrap_or(JsonValue::from(())),
        );
        object.insert(
            "launch_spec".into(),
            self.launch_spec
                .as_ref()
                .map(ChildLaunchSpec::to_json)
                .unwrap_or(JsonValue::from(())),
        );
        object.insert(
            "stdio".into(),
            self.stdio
                .as_ref()
                .map(StdioProfile::to_json)
                .unwrap_or(JsonValue::from(())),
        );
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        let operation_version = string(object, "operation_version")?;
        canonical_u64("operation_version", &operation_version).ok()?;
        Some(Self {
            operation_id: string(object, "operation_id")?,
            runtime_session_id: string(object, "runtime_session_id")?,
            worker_id: optional_string_from(object, "worker_id"),
            generation: optional_string_from(object, "generation"),
            binding_epoch: string(object, "binding_epoch")?,
            lifecycle_version: optional_string_from(object, "lifecycle_version"),
            operation_version,
            kind: OperationKind::parse(&string(object, "kind")?)?,
            custody: object.get("custody").and_then(ExternalCustody::from_json)?,
            terminal: object.get("terminal")?.get::<bool>().copied()?,
            cancelled: object.get("cancelled")?.get::<bool>().copied()?,
            reap_proof: object
                .get("reap_proof")
                .and_then(OwnedChildReapProof::from_json),
            launch_spec: object
                .get("launch_spec")
                .and_then(ChildLaunchSpec::from_json),
            stdio: object.get("stdio").and_then(StdioProfile::from_json),
        })
    }
}

impl OwnedChildReapProof {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert("worker_id".into(), JsonValue::from(self.worker_id.clone()));
        object.insert(
            "generation".into(),
            JsonValue::from(self.generation.clone()),
        );
        object.insert(
            "operation_id".into(),
            JsonValue::from(self.operation_id.clone()),
        );
        object.insert(
            "supervisor_instance_id".into(),
            JsonValue::from(self.supervisor_instance_id.clone()),
        );
        object.insert("pid".into(), JsonValue::from(self.pid.to_string()));
        object.insert(
            "exit_status".into(),
            JsonValue::from(self.exit_status.to_string()),
        );
        object.insert(
            "operation_version".into(),
            JsonValue::from(self.operation_version.clone()),
        );
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        Some(Self {
            worker_id: string(object, "worker_id")?,
            generation: string(object, "generation")?,
            operation_id: string(object, "operation_id")?,
            supervisor_instance_id: string(object, "supervisor_instance_id")?,
            pid: string(object, "pid")?.parse().ok()?,
            exit_status: string(object, "exit_status")?.parse().ok()?,
            operation_version: string(object, "operation_version")?,
            _sealed: Sealed(()),
        })
    }
}

impl SupervisorRecord {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert(
            "supervisor_instance_id".into(),
            JsonValue::from(self.supervisor_instance_id.clone()),
        );
        object.insert(
            "control_epoch".into(),
            JsonValue::from(self.control_epoch.clone()),
        );
        object.insert(
            "control_nonce_sha256".into(),
            JsonValue::from(self.control_nonce_sha256.clone()),
        );
        object.insert(
            "operation_id".into(),
            JsonValue::from(self.operation_id.clone()),
        );
        object.insert("process".into(), self.process.to_json());
        object.insert(
            "socket_path".into(),
            JsonValue::from(self.socket_path.to_string_lossy().into_owned()),
        );
        object.insert(
            "socket_dev".into(),
            JsonValue::from(self.socket_dev.to_string()),
        );
        object.insert(
            "socket_ino".into(),
            JsonValue::from(self.socket_ino.to_string()),
        );
        object.insert("version".into(), JsonValue::from(self.version.clone()));
        object.insert(
            "state".into(),
            JsonValue::from(self.state.as_str().to_string()),
        );
        object.insert(
            "heartbeat_at".into(),
            JsonValue::from(self.heartbeat_at.clone()),
        );
        object.insert(
            "heartbeat_at_ms".into(),
            JsonValue::from(self.heartbeat_at_ms.to_string()),
        );
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        Some(Self {
            supervisor_instance_id: string(object, "supervisor_instance_id")?,
            control_epoch: string(object, "control_epoch")?,
            control_nonce_sha256: string(object, "control_nonce_sha256")?,
            operation_id: string(object, "operation_id")?,
            process: object
                .get("process")
                .and_then(ProcessObservation::from_json)?,
            socket_path: PathBuf::from(string(object, "socket_path")?),
            socket_dev: string(object, "socket_dev")?.parse().ok()?,
            socket_ino: string(object, "socket_ino")?.parse().ok()?,
            version: string(object, "version")?,
            state: SupervisorState::parse(&string(object, "state")?)?,
            heartbeat_at: string(object, "heartbeat_at")?,
            heartbeat_at_ms: string(object, "heartbeat_at_ms")?.parse().ok()?,
        })
    }
}

impl SupervisorWatchdogRecord {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert(
            "watchdog_instance_id".into(),
            JsonValue::from(self.watchdog_instance_id.clone()),
        );
        object.insert(
            "supervisor_instance_id".into(),
            JsonValue::from(self.supervisor_instance_id.clone()),
        );
        object.insert(
            "operation_id".into(),
            JsonValue::from(self.operation_id.clone()),
        );
        object.insert(
            "control_epoch".into(),
            JsonValue::from(self.control_epoch.clone()),
        );
        object.insert(
            "control_nonce_sha256".into(),
            JsonValue::from(self.control_nonce_sha256.clone()),
        );
        object.insert("process".into(), self.process.to_json());
        object.insert(
            "heartbeat_at".into(),
            JsonValue::from(self.heartbeat_at.clone()),
        );
        object.insert(
            "heartbeat_at_ms".into(),
            JsonValue::from(self.heartbeat_at_ms.to_string()),
        );
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        Some(Self {
            watchdog_instance_id: string(object, "watchdog_instance_id")?,
            supervisor_instance_id: string(object, "supervisor_instance_id")?,
            operation_id: string(object, "operation_id")?,
            control_epoch: string(object, "control_epoch")?,
            control_nonce_sha256: string(object, "control_nonce_sha256")?,
            process: object
                .get("process")
                .and_then(ProcessObservation::from_json)?,
            heartbeat_at: string(object, "heartbeat_at")?,
            heartbeat_at_ms: string(object, "heartbeat_at_ms")?.parse().ok()?,
        })
    }
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
            BindingState::UnmanagedCanceling {
                operation_id,
                operation_version,
                binding_epoch,
            } => {
                state.insert(
                    "kind".into(),
                    JsonValue::from("UnmanagedCanceling".to_string()),
                );
                state.insert("operation_id".into(), JsonValue::from(operation_id.clone()));
                state.insert(
                    "operation_version".into(),
                    JsonValue::from(operation_version.clone()),
                );
                state.insert(
                    "binding_epoch".into(),
                    JsonValue::from(binding_epoch.clone()),
                );
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
            "UnmanagedCanceling" => BindingState::UnmanagedCanceling {
                operation_id: string(state, "operation_id")?,
                operation_version: string(state, "operation_version")?,
                binding_epoch: string(state, "binding_epoch")?,
            },
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
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert("worker_id".into(), JsonValue::from(self.worker_id.clone()));
        object.insert(
            "generation".into(),
            JsonValue::from(self.generation.clone()),
        );
        object.insert(
            "final_version".into(),
            JsonValue::from(self.final_version.clone()),
        );
        object.insert(
            "retained_at".into(),
            JsonValue::from(self.retained_at.clone()),
        );
        object.insert(
            "receipt".into(),
            self.receipt
                .as_ref()
                .map(ReleaseReceipt::to_json)
                .unwrap_or(JsonValue::from(())),
        );
        JsonValue::from(object)
    }

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
        ACTIVE_OPERATIONS_KEY,
        OPERATION_TOMBSTONES_KEY,
        SUPERVISORS_KEY,
        WATCHDOGS_KEY,
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

fn process_observation_is_live(observation: &ProcessObservation) -> bool {
    #[cfg(target_os = "linux")]
    {
        let current = fs::read_to_string(format!("/proc/{}/stat", observation.pid))
            .ok()
            .and_then(|stat| {
                let end = stat.rfind(") ")?;
                let fields = stat[end + 2..].split_whitespace().collect::<Vec<_>>();
                let state = *fields.first()?;
                let start = fields.get(19)?.parse::<u64>().ok()?;
                Some((state.to_string(), start))
            });
        match observation.start {
            StartGeneration::LinuxProcStartTicks(expected) => {
                current.is_some_and(|(state, start)| state != "Z" && start == expected)
            }
            _ => current.is_some_and(|(state, _)| state != "Z"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: signal 0 is an observation-only existence/permission probe.
        unsafe { libc::kill(observation.pid as i32, 0) == 0 }
    }
}

fn ping_supervisor(supervisor: &SupervisorRecord) -> Result<(), String> {
    let metadata = fs::metadata(&supervisor.socket_path)
        .map_err(|error| format!("stat supervisor health socket: {error}"))?;
    if metadata.dev() != supervisor.socket_dev || metadata.ino() != supervisor.socket_ino {
        return Err("supervisor health socket identity changed".to_string());
    }
    let mut stream = std::os::unix::net::UnixStream::connect(&supervisor.socket_path)
        .map_err(|error| format!("connect supervisor health socket: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(MANAGED_CANCEL_POLL_MS)))
        .map_err(|error| format!("configure supervisor health timeout: {error}"))?;
    let challenge_nonce = store::uuid_v4();
    let request = format!(
        "{{\"challenge_nonce\":\"{challenge_nonce}\",\"kind\":\"ping\",\"role\":\"health\"}}\n"
    );
    std::io::Write::write_all(&mut stream, request.as_bytes())
        .map_err(|error| format!("write supervisor health ping: {error}"))?;
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(stream), &mut line)
        .map_err(|error| format!("read supervisor health pong: {error}"))?;
    let value = line
        .trim_end()
        .parse::<JsonValue>()
        .map_err(|error| format!("parse supervisor health pong: {error}"))?;
    let object = value
        .get::<HashMap<String, JsonValue>>()
        .ok_or_else(|| "supervisor health pong is not an object".to_string())?;
    if optional_string_from(object, "kind").as_deref() == Some("pong")
        && optional_string_from(object, "role").as_deref() == Some("health")
        && optional_string_from(object, "supervisor_instance_id").as_deref()
            == Some(supervisor.supervisor_instance_id.as_str())
        && optional_string_from(object, "challenge_nonce").as_deref()
            == Some(challenge_nonce.as_str())
    {
        Ok(())
    } else {
        Err("supervisor health pong identity mismatch".to_string())
    }
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
