pub mod authority;
pub mod capability;
pub mod custody;
pub mod git;
pub mod platform;
pub mod repository_gate;
pub mod resources;
pub mod schema;

use crate::sha256;
use authority::{
    AuthorityRootProvider, AuthorityRoots, BootstrapOutcome, JournalEventV1, LeaseIdentity,
    SystemAuthorityRootProvider, WorkspaceAuthority, WorkspaceJournalLock, WorkspaceLease,
    WorkspaceLeaseProbe,
};
use capability::{WorkerCapabilityV1, mint_worker};
use schema::{
    AbortRequestV1, AbsPath, CleanupReceiptV1, ClosedJcs, FinishRequestV1, HandbackReceiptV1,
    HandbackRequestV1, HashedFileV1, IntegrateRequestV1, IntegrationReceiptV1, JcsValue,
    LowerUuidV4, RecoverRequestV1, RetentionProofV1, Sha256Digest, WipReceiptV1,
    WorkspaceStartRequestV1, WorkspaceStartResultV1, WorkspaceState,
};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const WORKSPACE_HELP: &str = "usage:\n  session-relay workspace preserve --request-file <absolute-file> --request-sha256 <sha256>\n  session-relay workspace start --request-file <absolute-file> --request-sha256 <sha256> [--coordinator-capability-file <absolute-file>]\n  session-relay workspace list --repository <canonical-root> --coordinator-capability-file <absolute-file>\n  session-relay workspace inspect <session-id> --repository <canonical-root> --coordinator-capability-file <absolute-file>\n  session-relay workspace handback --request-file <absolute-file> --request-sha256 <sha256> --worker-capability-file <absolute-file>\n  session-relay workspace integrate|recover|finish|abort --request-file <absolute-file> --request-sha256 <sha256> --coordinator-capability-file <absolute-file>";
pub const WORKSPACE_WORKER_POLICY: &str = "Work only in the assigned Session Relay workspace. Use the generated Git shim for supported Git mutation, stay within admitted path claims, and use only the projected session resources. Do not reenter wake, attach, watch, shared app-server, integration-checkout, or unmanaged writer paths.";
pub const MANAGED_MUTATION_REFUSAL: &str = "mutation is refused for a managed workspace or integration checkout; use session-relay workspace start for a contained writer or continue in read-only mode";
#[cfg(target_os = "linux")]
const RUNTIME_COMMAND_IO_DEADLINE: Duration = Duration::from_millis(200);
#[cfg(target_os = "linux")]
const RUNTIME_EXCHANGE_DEADLINE: Duration = Duration::from_secs(2);
const BROKER_READY_DOMAIN: &[u8] = b"session-relay/broker-ready/v1\0";
const BROKER_READY_TOKEN_LEN: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrokerReadinessState {
    Pending,
    Ready,
    Closed,
}

struct BrokerReadinessReader {
    stream: UnixStream,
    expected: [u8; BROKER_READY_TOKEN_LEN],
    received: Vec<u8>,
}

impl BrokerReadinessReader {
    fn observe(&mut self) -> Result<BrokerReadinessState, String> {
        let mut buffer = [0_u8; BROKER_READY_TOKEN_LEN];
        loop {
            match self
                .stream
                .read(&mut buffer[..BROKER_READY_TOKEN_LEN - self.received.len()])
            {
                Ok(0) => return Ok(BrokerReadinessState::Closed),
                Ok(read) => {
                    self.received.extend_from_slice(&buffer[..read]);
                    if self.received.len() == BROKER_READY_TOKEN_LEN {
                        if !sha256::constant_time_eq(&self.received, &self.expected) {
                            return Err("Git broker readiness authentication failed".into());
                        }
                        return Ok(BrokerReadinessState::Ready);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Ok(BrokerReadinessState::Pending);
                }
                Err(error) => return Err(format!("read Git broker readiness: {error}")),
            }
        }
    }
}

fn broker_readiness_pair(
    capability: &WorkerCapabilityV1,
) -> Result<(BrokerReadinessReader, OwnedFd), String> {
    let (reader, writer) = UnixStream::pair()
        .map_err(|error| format!("create Git broker readiness channel: {error}"))?;
    reader
        .set_nonblocking(true)
        .map_err(|error| format!("set Git broker readiness channel nonblocking: {error}"))?;
    Ok((
        BrokerReadinessReader {
            stream: reader,
            expected: broker_readiness_token(capability)?,
            received: Vec::with_capacity(BROKER_READY_TOKEN_LEN),
        },
        writer.into(),
    ))
}

fn publish_broker_ready(ready_fd: RawFd, capability: &WorkerCapabilityV1) -> Result<(), String> {
    if ready_fd <= libc::STDERR_FILENO {
        return Err("Git broker readiness descriptor collides with stdio".into());
    }
    let mut writer = unsafe { UnixStream::from_raw_fd(ready_fd) };
    writer
        .write_all(&broker_readiness_token(capability)?)
        .map_err(|error| format!("publish authenticated Git broker readiness: {error}"))?;
    writer
        .shutdown(std::net::Shutdown::Write)
        .map_err(|error| format!("close Git broker readiness channel: {error}"))
}

fn broker_readiness_token(
    capability: &WorkerCapabilityV1,
) -> Result<[u8; BROKER_READY_TOKEN_LEN], String> {
    let secret = capability::decode_base64url(&capability.secret_b64url)?;
    let message = [
        BROKER_READY_DOMAIN,
        capability.repository_id.as_bytes(),
        b"\0",
        capability.session_id.as_bytes(),
        b"\0",
        capability.capability_id.as_bytes(),
        b"\0",
        capability.generation.as_bytes(),
    ]
    .concat();
    Ok(sha256::hmac(&secret, &message))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoordinatorMutation {
    Integrate,
    Recover,
    Finish,
    Abort,
}
impl CoordinatorMutation {
    pub fn action(self) -> &'static str {
        match self {
            Self::Integrate => "integrate",
            Self::Recover => "recover",
            Self::Finish => "finish",
            Self::Abort => "abort",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceCommand {
    Preserve {
        request_file: PathBuf,
        request_sha256: String,
    },
    Start {
        request_file: PathBuf,
        request_sha256: String,
        coordinator_capability_file: Option<PathBuf>,
    },
    List {
        repository: PathBuf,
        coordinator_capability_file: PathBuf,
    },
    Inspect {
        session_id: String,
        repository: PathBuf,
        coordinator_capability_file: PathBuf,
    },
    Handback {
        request_file: PathBuf,
        request_sha256: String,
        worker_capability_file: PathBuf,
    },
    Coordinator {
        operation: CoordinatorMutation,
        request_file: PathBuf,
        request_sha256: String,
        coordinator_capability_file: PathBuf,
    },
}

pub fn parse_command(args: &[String]) -> Result<WorkspaceCommand, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(WORKSPACE_HELP.to_string());
    };
    match command {
        "preserve" => {
            let flags = Flags::parse(&args[1..], &["--request-file", "--request-sha256"])?;
            Ok(WorkspaceCommand::Preserve {
                request_file: flags.absolute("--request-file")?,
                request_sha256: flags.sha("--request-sha256")?,
            })
        }
        "start" => {
            let flags = Flags::parse(
                &args[1..],
                &[
                    "--request-file",
                    "--request-sha256",
                    "--coordinator-capability-file",
                ],
            )?;
            Ok(WorkspaceCommand::Start {
                request_file: flags.absolute("--request-file")?,
                request_sha256: flags.sha("--request-sha256")?,
                coordinator_capability_file: flags
                    .optional_absolute("--coordinator-capability-file")?,
            })
        }
        "list" => {
            let flags = Flags::parse(
                &args[1..],
                &["--repository", "--coordinator-capability-file"],
            )?;
            Ok(WorkspaceCommand::List {
                repository: flags.absolute("--repository")?,
                coordinator_capability_file: flags.absolute("--coordinator-capability-file")?,
            })
        }
        "inspect" => {
            let session = args
                .get(1)
                .ok_or_else(|| "workspace inspect requires one session UUID".to_string())?;
            LowerUuidV4::parse(session)?;
            let flags = Flags::parse(
                &args[2..],
                &["--repository", "--coordinator-capability-file"],
            )?;
            Ok(WorkspaceCommand::Inspect {
                session_id: session.clone(),
                repository: flags.absolute("--repository")?,
                coordinator_capability_file: flags.absolute("--coordinator-capability-file")?,
            })
        }
        "handback" => {
            let flags = Flags::parse(
                &args[1..],
                &[
                    "--request-file",
                    "--request-sha256",
                    "--worker-capability-file",
                ],
            )?;
            Ok(WorkspaceCommand::Handback {
                request_file: flags.absolute("--request-file")?,
                request_sha256: flags.sha("--request-sha256")?,
                worker_capability_file: flags.absolute("--worker-capability-file")?,
            })
        }
        "integrate" | "recover" | "finish" | "abort" => {
            let operation = match command {
                "integrate" => CoordinatorMutation::Integrate,
                "recover" => CoordinatorMutation::Recover,
                "finish" => CoordinatorMutation::Finish,
                _ => CoordinatorMutation::Abort,
            };
            let flags = Flags::parse(
                &args[1..],
                &[
                    "--request-file",
                    "--request-sha256",
                    "--coordinator-capability-file",
                ],
            )?;
            Ok(WorkspaceCommand::Coordinator {
                operation,
                request_file: flags.absolute("--request-file")?,
                request_sha256: flags.sha("--request-sha256")?,
                coordinator_capability_file: flags.absolute("--coordinator-capability-file")?,
            })
        }
        _ => Err(format!(
            "unknown workspace command {command}\n{WORKSPACE_HELP}"
        )),
    }
}

struct Flags(std::collections::BTreeMap<String, String>);
impl Flags {
    fn parse(args: &[String], admitted: &[&str]) -> Result<Self, String> {
        if args.len() % 2 != 0 {
            return Err("workspace flags require one value each".to_string());
        }
        let mut flags = std::collections::BTreeMap::new();
        for pair in args.chunks_exact(2) {
            if !admitted.contains(&pair[0].as_str()) {
                return Err(format!("unknown workspace flag {}", pair[0]));
            }
            if pair[1].is_empty() || pair[1].contains('\0') {
                return Err(format!("workspace flag {} has an invalid value", pair[0]));
            }
            if flags.insert(pair[0].clone(), pair[1].clone()).is_some() {
                return Err(format!("duplicate workspace flag {}", pair[0]));
            }
        }
        Ok(Self(flags))
    }
    fn value(&self, key: &str) -> Result<&str, String> {
        self.0
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| format!("missing required workspace flag {key}"))
    }
    fn absolute(&self, key: &str) -> Result<PathBuf, String> {
        let value = self.value(key)?;
        AbsPath::parse(value)?;
        Ok(PathBuf::from(value))
    }
    fn optional_absolute(&self, key: &str) -> Result<Option<PathBuf>, String> {
        self.0
            .get(key)
            .map(|v| {
                AbsPath::parse(v)?;
                Ok(PathBuf::from(v))
            })
            .transpose()
    }
    fn sha(&self, key: &str) -> Result<String, String> {
        let value = self.value(key)?;
        Sha256Digest::parse(value)?;
        Ok(value.to_string())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkspaceManifestRecord(JcsValue);
impl ClosedJcs for WorkspaceManifestRecord {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let object = match &value {
            JcsValue::Object(object) => object,
            _ => return Err("WorkspaceManifestV1 must be an object".into()),
        };
        let keys = [
            "schema",
            "session_id",
            "repository",
            "integration_root",
            "worktree_identity",
            "worktree_root",
            "branch_ref",
            "base_commit",
            "wip_receipt_path",
            "wip_receipt_sha256",
            "applied_wip_commit",
            "worker_base_commit",
            "task_slug",
            "task",
            "tool",
            "owned_paths",
            "coordinator_owned_paths",
            "resources",
            "state",
            "produced_commits",
            "integration_commits",
            "lease_evidence",
            "custody_evidence",
            "worker_capability_file",
            "retention_evidence",
            "last_error",
            "journal_sequence",
            "journal_head_sha256",
            "created_at",
            "updated_at",
        ];
        if object.len() != keys.len() || keys.iter().any(|key| !object.contains_key(*key)) {
            return Err("WorkspaceManifestV1 keys differ from the closed schema".into());
        }
        if object["schema"].as_str() != Ok(schema::SCHEMA_V1) {
            return Err("WorkspaceManifestV1 schema mismatch".into());
        }
        let state = WorkspaceState::parse(object["state"].as_str()?)?;
        let early = matches!(
            state,
            WorkspaceState::Reserved | WorkspaceState::Provisioning
        );
        if early != matches!(object["worktree_identity"], JcsValue::Null) {
            return Err("workspace identity nullability differs from state".into());
        }
        validate_manifest_wip_nullability(
            early,
            &object["applied_wip_commit"],
            &object["worker_base_commit"],
        )?;
        Ok(Self(value))
    }
    fn to_jcs(&self) -> JcsValue {
        self.0.clone()
    }
}

fn validate_manifest_wip_nullability(
    early: bool,
    applied: &JcsValue,
    worker_base: &JcsValue,
) -> Result<(), String> {
    let applied_null = matches!(applied, JcsValue::Null);
    let worker_null = matches!(worker_base, JcsValue::Null);
    if applied_null != worker_null || early != applied_null {
        return Err("applied WIP nullability differs from state".into());
    }
    Ok(())
}

struct CanonicalRecord(JcsValue);
impl ClosedJcs for CanonicalRecord {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        Ok(Self(value))
    }
    fn to_jcs(&self) -> JcsValue {
        self.0.clone()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CleanupIntentV1(JcsValue);

impl ClosedJcs for CleanupIntentV1 {
    fn from_jcs(value: JcsValue) -> Result<Self, String> {
        let object = closed_object(
            &value,
            &[
                "schema",
                "request_id",
                "session_id",
                "repository_id",
                "source_state",
                "source_journal_sequence",
                "source_journal_head_sha256",
                "worktree_identity_sha256",
                "worktree_root",
                "branch_ref",
                "expected_worker_head",
                "resources_sha256",
                "retention_sha256",
                "expected_integration_checkout_head",
                "custody_empty_sha256",
                "broker_close_sha256",
                "created_at",
            ],
            "WorkspaceCleanupIntentV1",
        )?;
        if object["schema"].as_str()? != "WorkspaceCleanupIntentV1" {
            return Err("WorkspaceCleanupIntentV1 schema mismatch".into());
        }
        LowerUuidV4::parse(object["request_id"].as_str()?)?;
        LowerUuidV4::parse(object["session_id"].as_str()?)?;
        for field in [
            "repository_id",
            "source_journal_head_sha256",
            "worktree_identity_sha256",
            "resources_sha256",
            "custody_empty_sha256",
            "broker_close_sha256",
        ] {
            Sha256Digest::parse(object[field].as_str()?)?;
        }
        let source_state = WorkspaceState::parse(object["source_state"].as_str()?)?;
        if !matches!(
            source_state,
            WorkspaceState::Integrated | WorkspaceState::Rejected | WorkspaceState::AbortedRetained
        ) {
            return Err("cleanup intent source state is not a settled outcome".into());
        }
        let sequence = object["source_journal_sequence"]
            .as_str()?
            .parse::<u64>()
            .map_err(|_| "cleanup intent journal sequence overflows u64".to_string())?;
        if sequence == 0 {
            return Err("cleanup intent journal sequence must be positive".into());
        }
        AbsPath::parse(object["worktree_root"].as_str()?)?;
        if object["branch_ref"].as_str()?.is_empty()
            || object["expected_worker_head"].as_str()?.is_empty()
        {
            return Err("cleanup intent Git identity is empty".into());
        }
        for field in ["retention_sha256", "expected_integration_checkout_head"] {
            match &object[field] {
                JcsValue::Null => {}
                JcsValue::String(value) if field == "retention_sha256" => {
                    Sha256Digest::parse(value)?;
                }
                JcsValue::String(value) if !value.is_empty() => {}
                _ => return Err(format!("cleanup intent {field} has invalid nullability")),
            }
        }
        schema::Timestamp::parse(object["created_at"].as_str()?)?;
        Ok(Self(value))
    }

    fn to_jcs(&self) -> JcsValue {
        self.0.clone()
    }
}

impl CleanupIntentV1 {
    fn object(&self) -> Result<&BTreeMap<String, JcsValue>, String> {
        match &self.0 {
            JcsValue::Object(object) => Ok(object),
            _ => Err("WorkspaceCleanupIntentV1 must be an object".into()),
        }
    }
}

impl WorkspaceManifestRecord {
    fn object(&self) -> Result<&BTreeMap<String, JcsValue>, String> {
        match &self.0 {
            JcsValue::Object(object) => Ok(object),
            _ => Err("WorkspaceManifestV1 must be an object".into()),
        }
    }

    fn object_mut(&mut self) -> Result<&mut BTreeMap<String, JcsValue>, String> {
        match &mut self.0 {
            JcsValue::Object(object) => Ok(object),
            _ => Err("WorkspaceManifestV1 must be an object".into()),
        }
    }

    fn state(&self) -> Result<WorkspaceState, String> {
        WorkspaceState::parse(self.object()?["state"].as_str()?)
    }

    fn journal_position(&self) -> Result<(u64, Option<String>), String> {
        let object = self.object()?;
        let sequence = object["journal_sequence"]
            .as_str()?
            .parse::<u64>()
            .map_err(|_| "manifest journal sequence overflows u64".to_string())?;
        let head = match &object["journal_head_sha256"] {
            JcsValue::Null => None,
            JcsValue::String(value) => {
                Sha256Digest::parse(value)?;
                Some(value.clone())
            }
            _ => return Err("manifest journal head has invalid nullability".into()),
        };
        if (sequence == 0) != head.is_none() {
            return Err("manifest journal sequence/head nullability mismatch".into());
        }
        Ok((sequence, head))
    }
}

fn read_manifest(path: &Path) -> Result<WorkspaceManifestRecord, String> {
    schema::read_jcs_file(path, None)
}

struct ManifestEventContext<'a> {
    session_dir: &'a Path,
    manifest_file: &'a Path,
    expected: WorkspaceState,
    next: WorkspaceState,
    kind: &'a str,
    created_at: &'a str,
}

fn mutate_manifest_event<F>(
    context: ManifestEventContext<'_>,
    payload: JcsValue,
    mutate: F,
) -> Result<WorkspaceManifestRecord, String>
where
    F: FnOnce(&mut BTreeMap<String, JcsValue>) -> Result<(), String>,
{
    let ManifestEventContext {
        session_dir,
        manifest_file,
        expected,
        next,
        kind,
        created_at,
    } = context;
    schema::Timestamp::parse(created_at)?;
    if !expected.may_transition_to(next) {
        return Err(format!(
            "workspace state transition {} -> {} is not admitted",
            expected.as_str(),
            next.as_str()
        ));
    }
    repository_gate::with_relay_store_rank(|| {
        let journal_lock = WorkspaceJournalLock::acquire(session_dir)?;
        let current = read_manifest(manifest_file)?;
        if current.state()? != expected {
            return Err(format!(
                "workspace state changed before {} publication",
                kind
            ));
        }
        let (sequence, head) = current.journal_position()?;
        let event = JournalEventV1 {
            sequence: sequence
                .checked_add(1)
                .ok_or_else(|| "workspace journal sequence exhausted".to_string())?,
            previous_sha256: head.clone(),
            kind: kind.to_string(),
            payload,
            created_at: created_at.to_string(),
        };
        let next_head =
            authority::append_journal_cas(&journal_lock, &event, sequence, head.as_deref())?;
        let mut replacement = current.clone();
        {
            let object = replacement.object_mut()?;
            object.insert("state".into(), JcsValue::String(next.as_str().into()));
            object.insert(
                "journal_sequence".into(),
                JcsValue::String(event.sequence.to_string()),
            );
            object.insert("journal_head_sha256".into(), JcsValue::String(next_head));
            object.insert("updated_at".into(), JcsValue::String(created_at.into()));
            mutate(object)?;
        }
        WorkspaceManifestRecord::from_jcs(replacement.0.clone())?;
        authority::replace_manifest_cas(
            &journal_lock,
            manifest_file,
            expected.as_str(),
            sequence,
            head.as_deref(),
            &replacement,
        )?;
        Ok(replacement)
    })
}

fn write_private_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("persist {}: {e}", path.display()))?;
    let parent = path
        .parent()
        .ok_or_else(|| "private record has no parent".to_string())?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(parent)
        .map_err(|e| format!("open {} for fsync: {e}", parent.display()))?;
    directory
        .sync_all()
        .map_err(|e| format!("fsync {}: {e}", parent.display()))
}
fn broker_socket_location(euid: u32, repository_id: &str, session_id: &str) -> PathBuf {
    let identity = sha256::hex_digest(
        format!("session-relay/broker-socket/v1\0{repository_id}\0{session_id}").as_bytes(),
    );
    PathBuf::from(format!("/tmp/sr-broker-{euid}"))
        .join(identity)
        .join("broker.sock")
}

fn broker_socket_path(euid: u32, repository_id: &str, session_id: &str) -> Result<PathBuf, String> {
    let path = broker_socket_location(euid, repository_id, session_id);
    let directory = path
        .parent()
        .ok_or_else(|| "broker socket has no parent".to_string())?;
    let root = directory
        .parent()
        .ok_or_else(|| "broker socket directory has no parent".to_string())?;
    authority::ensure_private_directory(root, euid)?;
    authority::ensure_private_directory(directory, euid)?;
    Ok(path)
}
#[cfg(target_os = "linux")]
fn custody_socket_path(session_dir: &Path) -> PathBuf {
    let euid = unsafe { libc::geteuid() };
    let root = PathBuf::from(format!("/tmp/sr-custody-{euid}"));
    let identity = sha256::hex_digest(
        [
            b"session-relay/custody-socket/v1\0".as_slice(),
            session_dir.as_os_str().as_bytes(),
        ]
        .concat()
        .as_slice(),
    );
    root.join(identity).join("command.sock")
}
#[cfg(not(target_os = "linux"))]
fn custody_socket_path(session_dir: &Path) -> PathBuf {
    session_dir.join("custody-command-v1.sock")
}

pub struct StartedWorkspace {
    pub result: WorkspaceStartResultV1,
    pub lease: WorkspaceLease,
    pub manifest_file: PathBuf,
    pub worker_capability_file: PathBuf,
    pub resources: resources::ResourceSet,
    pub tool_launch_file: PathBuf,
    pub tool_launch_sha256: String,
}

pub fn preserve_workspace(
    request_file: &Path,
    request_sha256: &str,
) -> Result<git::PreserveResult, String> {
    let roots = SystemAuthorityRootProvider.roots()?;
    preserve_workspace_with_roots(&roots, request_file, request_sha256)
}
pub fn preserve_workspace_with_roots(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
) -> Result<git::PreserveResult, String> {
    let request: schema::PreserveRequestV1 =
        schema::read_jcs_file(request_file, Some(request_sha256))?;
    let repository = git::OpenedRepository::open(Path::new(&request.repository_path))?;
    repository.validate_oid(&request.base_commit)?;
    let authority = WorkspaceAuthority::new(roots.clone())?;
    let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
    gate.admit_workspace_storage(roots, &repository.identity)?;
    let output = git::preserve(
        &repository,
        &request,
        request_sha256,
        &roots.data.join("preserved"),
    )?;
    drop(gate);
    drop(authority);
    Ok(output)
}

pub fn target_is_managed(path: &Path) -> Result<bool, String> {
    if std::env::var_os("DOCKS_WORKER_CAPABILITY_FILE").is_some() {
        return Ok(true);
    }
    if !path
        .ancestors()
        .any(|ancestor| fs::symlink_metadata(ancestor.join(".git")).is_ok())
    {
        return Ok(false);
    }
    let repository = match git::OpenedRepository::open(path) {
        Ok(repository) => repository,
        Err(error)
            if error.contains("not a git repository") || error.contains("not a repository") =>
        {
            return Ok(false);
        }
        Err(error) => {
            return Err(format!(
                "cannot prove mutation target is outside managed mode: {error}"
            ));
        }
    };
    let common = Path::new(&repository.identity.common_dir_realpath);
    if common.join("docks/workspace-admission-v1.json").exists() {
        return Ok(true);
    }
    let roots = SystemAuthorityRootProvider.roots()?;
    Ok(roots
        .authority
        .join("repositories")
        .join(&repository.identity.repository_id)
        .exists())
}

pub fn refuse_unsupported_managed_mutation(
    path: &Path,
    read_only: bool,
    entrypoint: &str,
) -> Result<(), String> {
    if read_only {
        return Ok(());
    }
    if target_is_managed(path)? {
        return Err(format!("{entrypoint} {MANAGED_MUTATION_REFUSAL}"));
    }
    Ok(())
}

pub fn start_workspace(
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: Option<&Path>,
) -> Result<StartedWorkspace, String> {
    let roots = SystemAuthorityRootProvider.roots()?;
    let executable =
        std::env::current_exe().map_err(|e| format!("resolve relay executable: {e}"))?;
    let digest = resources::executable_sha256(&executable)?;
    start_workspace_with_roots_and_verified_executable(
        &roots,
        request_file,
        request_sha256,
        coordinator_capability_file,
        &executable,
        &digest,
    )
}
pub fn start_workspace_with_roots(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: Option<&Path>,
) -> Result<StartedWorkspace, String> {
    let executable =
        std::env::current_exe().map_err(|e| format!("resolve relay executable: {e}"))?;
    let digest = resources::executable_sha256(&executable)?;
    start_workspace_with_roots_and_verified_executable(
        roots,
        request_file,
        request_sha256,
        coordinator_capability_file,
        &executable,
        &digest,
    )
}
pub fn start_workspace_with_roots_and_executable(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: Option<&Path>,
    relay_executable: &Path,
) -> Result<StartedWorkspace, String> {
    let digest = resources::executable_sha256(relay_executable)?;
    start_workspace_with_roots_and_verified_executable(
        roots,
        request_file,
        request_sha256,
        coordinator_capability_file,
        relay_executable,
        &digest,
    )
}
struct WorkspaceStartFaultContext<'a> {
    session_dir: &'a Path,
    session_id: &'a str,
    repository_id: &'a str,
    lease_path: &'a Path,
    lease_identity: &'a LeaseIdentity,
    relay_executable: &'a Path,
    relay_executable_sha256: &'a str,
}

fn persist_workspace_start_fault(
    context: &WorkspaceStartFaultContext<'_>,
    phase: &str,
    broker_started: bool,
    error: &str,
) -> Result<String, String> {
    let WorkspaceStartFaultContext {
        session_dir,
        session_id,
        repository_id,
        lease_path,
        lease_identity,
        relay_executable,
        relay_executable_sha256,
    } = context;
    if !matches!(phase, "broker_start" | "custody_start") {
        return Err("workspace start fault phase is outside the closed set".into());
    }
    let custody_fault_path = session_dir.join("custody-fault-v1.json");
    let custody_fault_sha256 = if custody_fault_path.exists() {
        let value = schema::parse_jcs(&capability::read_secure_bytes(&custody_fault_path)?, true)?;
        JcsValue::String(schema::jcs_sha256(&CanonicalRecord(value)))
    } else {
        JcsValue::Null
    };
    let created_at = authority::now_timestamp()?;
    let mut object = BTreeMap::from([
        ("broker_close_sha256".into(), JcsValue::Null),
        ("broker_started".into(), JcsValue::Bool(broker_started)),
        ("created_at".into(), JcsValue::String(created_at)),
        ("custody_fault_sha256".into(), custody_fault_sha256),
        ("error".into(), JcsValue::String(error.to_string())),
        ("evidence_sha256".into(), JcsValue::Null),
        (
            "lease_device".into(),
            JcsValue::String(lease_identity.device.to_string()),
        ),
        (
            "lease_inode".into(),
            JcsValue::String(lease_identity.inode.to_string()),
        ),
        (
            "lease_path".into(),
            JcsValue::String(lease_path.to_string_lossy().into_owned()),
        ),
        ("phase".into(), JcsValue::String(phase.to_string())),
        (
            "repository_id".into(),
            JcsValue::String(repository_id.to_string()),
        ),
        (
            "relay_executable".into(),
            JcsValue::String(relay_executable.to_string_lossy().into_owned()),
        ),
        (
            "relay_executable_sha256".into(),
            JcsValue::String(relay_executable_sha256.to_string()),
        ),
        (
            "schema".into(),
            JcsValue::String("WorkspaceStartFaultV1".into()),
        ),
        (
            "session_id".into(),
            JcsValue::String(session_id.to_string()),
        ),
    ]);
    let bare = schema::serialize_jcs(&JcsValue::Object(object.clone()));
    let evidence_sha256 = sha256::hex_digest(
        [
            b"session-relay/workspace-start-fault/v1\0".as_slice(),
            bare.as_bytes(),
        ]
        .concat()
        .as_slice(),
    );
    object.insert(
        "evidence_sha256".into(),
        JcsValue::String(evidence_sha256.clone()),
    );
    persist_runtime_record(
        session_dir,
        "workspace-start-fault-v1.json",
        JcsValue::Object(object),
    )?;
    Ok(evidence_sha256)
}

fn record_ready_start_fault(
    session_dir: &Path,
    manifest_file: &Path,
    phase: &str,
    evidence_sha256: &str,
    error: &str,
) -> Result<(), String> {
    Sha256Digest::parse(evidence_sha256)?;
    let created_at = authority::now_timestamp()?;
    repository_gate::with_relay_store_rank(|| {
        let journal_lock = WorkspaceJournalLock::acquire(session_dir)?;
        let current = read_manifest(manifest_file)?;
        if current.state()? != WorkspaceState::Ready {
            return Err("workspace left Ready before start fault publication".into());
        }
        let (sequence, head) = current.journal_position()?;
        let event = JournalEventV1 {
            sequence: sequence
                .checked_add(1)
                .ok_or_else(|| "workspace journal sequence exhausted".to_string())?,
            previous_sha256: head.clone(),
            kind: "StartFaultRetained".into(),
            payload: JcsValue::Object(BTreeMap::from([
                (
                    "evidence_sha256".into(),
                    JcsValue::String(evidence_sha256.into()),
                ),
                ("phase".into(), JcsValue::String(phase.into())),
            ])),
            created_at: created_at.clone(),
        };
        let next_head =
            authority::append_journal_cas(&journal_lock, &event, sequence, head.as_deref())?;
        let mut replacement = current;
        let object = replacement.object_mut()?;
        object.insert(
            "journal_sequence".into(),
            JcsValue::String(event.sequence.to_string()),
        );
        object.insert("journal_head_sha256".into(), JcsValue::String(next_head));
        object.insert("last_error".into(), JcsValue::String(error.into()));
        object.insert("updated_at".into(), JcsValue::String(created_at));
        WorkspaceManifestRecord::from_jcs(replacement.0.clone())?;
        authority::replace_manifest_cas(
            &journal_lock,
            manifest_file,
            WorkspaceState::Ready.as_str(),
            sequence,
            head.as_deref(),
            &replacement,
        )?;
        Ok(())
    })
}

fn verify_open_file_identity(path: &Path, file: &File, label: &str) -> Result<(), String> {
    let opened = file
        .metadata()
        .map_err(|error| format!("fstat {label}: {error}"))?;
    let named = fs::metadata(path)
        .map_err(|error| format!("stat {label} path {}: {error}", path.display()))?;
    if opened.dev() != named.dev() || opened.ino() != named.ino() {
        return Err(format!(
            "{label} path identity changed after pinned preflight"
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn fd_mount_id(file: &File, label: &str) -> Result<u64, String> {
    let mut statx = unsafe { std::mem::zeroed::<libc::statx>() };
    let rc = unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_MNT_ID,
            &mut statx,
        )
    };
    if rc != 0 || statx.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(format!(
            "statx mount identity for {label}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(statx.stx_mnt_id)
}

#[cfg(target_os = "linux")]
fn adopt_running_relay_fd(fd: RawFd) -> Result<File, String> {
    if fd <= libc::STDERR_FILENO {
        return Err("sealed relay FD is not above stdio".into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let sealed = file
        .metadata()
        .map_err(|error| format!("fstat sealed relay FD: {error}"))?;
    let running_file = File::open("/proc/self/exe")
        .map_err(|error| format!("open running relay executable: {error}"))?;
    let running = running_file
        .metadata()
        .map_err(|error| format!("fstat running relay executable: {error}"))?;
    if sealed.dev() != running.dev()
        || sealed.ino() != running.ino()
        || fd_mount_id(&file, "sealed relay FD")?
            != fd_mount_id(&running_file, "running relay executable")?
    {
        return Err("running relay executable differs from the sealed relay FD identity".into());
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } != 0 {
        return Err(format!(
            "seal relay FD after exec: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}

#[cfg(not(target_os = "linux"))]
fn adopt_running_relay_fd(fd: RawFd) -> Result<File, String> {
    let _ = fd;
    Err(platform::MACOS_STOP_REASON.into())
}

pub fn start_workspace_with_roots_and_verified_executable(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: Option<&Path>,
    relay_executable: &Path,
    relay_executable_sha256: &str,
) -> Result<StartedWorkspace, String> {
    if !relay_executable.is_absolute()
        || fs::canonicalize(relay_executable)
            .map_err(|e| format!("canonicalize relay executable: {e}"))?
            != relay_executable
    {
        return Err("relay executable must be an absolute canonical path".into());
    }
    let relay_file =
        resources::open_verified_executable(relay_executable, relay_executable_sha256)?;
    let mut request: WorkspaceStartRequestV1 =
        schema::read_jcs_file(request_file, Some(request_sha256))?;
    resources::preflight_tool_launch(&request.tool)?;
    resources::preflight_resources(roots, &request.resources)?;
    platform::admit_writable_custody()?;
    let wip: WipReceiptV1 = schema::read_jcs_file(
        Path::new(&request.wip_receipt_path),
        Some(&request.wip_receipt_sha256),
    )?;
    if wip.request_sha256.is_empty() || wip.base_commit != request.base_commit {
        return Err("start request and WIP receipt base/provenance differ".into());
    }
    let repository = git::OpenedRepository::open(Path::new(&request.repository_path))?;
    let integration = git::OpenedRepository::open(Path::new(&request.integration_root))?;
    if repository.identity != integration.identity || wip.repository != repository.identity {
        return Err("start repository, integration root, and WIP identity differ".into());
    }
    repository.validate_oid(&request.base_commit)?;
    repository_gate::admit_ext4_path(&roots.authority)?;
    repository_gate::admit_ext4_path(&roots.data)?;
    repository_gate::admit_ext4_path(Path::new(&repository.identity.common_dir_realpath))?;
    let authority = WorkspaceAuthority::new(roots.clone())?;
    let now = authority::now_timestamp()?;
    let worktree_root = roots
        .data
        .join(&repository.identity.repository_id)
        .join(format!("{}-{}", &request.request_id, &request.task_slug));
    let branch_ref = format!(
        "refs/heads/docks/{}/{}",
        &request.request_id, &request.task_slug
    );
    let requested_coordinator_paths = request.coordinator_owned_paths.clone();
    let (capability_path, bootstrap) = {
        let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
        gate.admit_workspace_storage(roots, &repository.identity)?;
        let fanout = crate::fanout::FanoutStore::new(crate::store::home_dir());
        let repository_dir = authority.repository_dir(&repository.identity.repository_id)?;
        let existing_capability = if repository_dir.exists() {
            let path = coordinator_capability_file.ok_or_else(|| {
                let current = authority
                    .read_repository(&repository.identity.repository_id)
                    .ok()
                    .and_then(|record| record.current_generation.parse::<u64>().ok())
                    .unwrap_or(1);
                format!(
                    "workspace authority exists; retry with --coordinator-capability-file {}",
                    authority
                        .capability_path(&repository.identity.repository_id, current)
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                )
            })?;
            let (_, _capability) =
                authority.authenticate(&repository.identity.repository_id, path, "start")?;
            Some(path.to_path_buf())
        } else {
            if coordinator_capability_file.is_some() {
                return Err("first workspace start must omit --coordinator-capability-file".into());
            }
            if !request.coordinator_owned_paths.is_empty()
                || !request.coordinator_owned_overrides.is_empty()
            {
                return Err(
                    "first workspace start cannot claim or override coordinator-owned paths".into(),
                );
            }
            if fanout.has_nonterminal_repository(&repository.identity)? {
                return Err(
                    "active legacy fanout authority prevents managed workspace bootstrap".into(),
                );
            }
            None
        };
        request.coordinator_owned_paths = authority::resolve_path_policy(
            Path::new(&request.integration_root),
            &request.owned_paths,
            &requested_coordinator_paths,
            &request.coordinator_owned_overrides,
            existing_capability.is_some(),
        )?;
        let result = if let Some(path) = existing_capability {
            (path, BootstrapOutcome::Existing)
        } else {
            let created = authority.bootstrap_coordinator(&repository.identity, &now)?;
            (created.capability_file, created.bootstrap)
        };
        gate.publish_workspace_marker(
            roots,
            &repository.identity,
            env!("CARGO_PKG_VERSION"),
            &now,
        )?;
        result
    };
    let repository_dir = authority.repository_dir(&repository.identity.repository_id)?;
    let leases = repository_dir.join("worktree-leases");
    authority::ensure_private_directory(&leases, roots.euid)?;
    let lease_key = sha256::hex_digest(
        format!(
            "session-relay/worktree-lease/v1\0{}\0{}\0{}",
            repository.identity.repository_id,
            worktree_root.to_string_lossy(),
            branch_ref,
        )
        .as_bytes(),
    );
    let lease_path = leases.join(format!("{lease_key}.lock"));
    let owner_path = leases.join(format!("{lease_key}.owner.json"));
    let lease = WorkspaceLease::acquire_owned(&lease_path, &owner_path, &request.request_id, &now)?;
    let lease_identity = lease.identity()?;
    let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
    gate.admit_workspace_storage(roots, &repository.identity)?;
    let sessions = repository_dir.join("sessions");
    let session_dir = sessions.join(&request.request_id);
    if session_dir.exists() {
        return Err("workspace session already exists; use inspect or explicit recovery".into());
    }
    let revalidated_coordinator_paths = authority::resolve_path_policy(
        Path::new(&request.integration_root),
        &request.owned_paths,
        &requested_coordinator_paths,
        &request.coordinator_owned_overrides,
        matches!(bootstrap, BootstrapOutcome::Existing),
    )?;
    if revalidated_coordinator_paths != request.coordinator_owned_paths {
        return Err("coordinator-owned path policy changed after start preflight".into());
    }
    reject_overlapping_session_claims(&sessions, &request)?;
    fs::create_dir(&session_dir).map_err(|e| format!("create workspace session: {e}"))?;
    fs::set_permissions(&session_dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("chmod workspace session: {e}"))?;
    for name in [
        "journal",
        "worker-capabilities",
        "broker-replays",
        "broker-intents",
        "broker-plans",
        "resources",
    ] {
        fs::create_dir(session_dir.join(name))
            .map_err(|e| format!("create workspace {name}: {e}"))?;
        fs::set_permissions(session_dir.join(name), fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("chmod workspace {name}: {e}"))?;
    }
    authority::atomic_create_jcs(&session_dir.join("start-request-v1.json"), &request, 0o600)?;
    authority::ensure_private_directory(worktree_root.parent().unwrap(), roots.euid)?;
    let manifest_context = ManifestContext {
        request: &request,
        repository: &repository,
        worktree_root: &worktree_root,
        branch_ref: &branch_ref,
        now: &now,
    };
    let reserved = manifest_for(
        manifest_context,
        ManifestStateContext {
            state: WorkspaceState::Reserved,
            identity: None,
            applied: None,
            worker_capability: None,
            lease_path: None,
            journal_sequence: "0",
            journal_head: None,
        },
    );
    let manifest_file = session_dir.join("manifest-v1.json");
    authority::atomic_create_jcs(&manifest_file, &reserved, 0o600)?;
    let worktree_identity = git::provision_worktree(
        &repository,
        &worktree_root,
        &branch_ref,
        &request.request_id,
        &request.task_slug,
        &request.base_commit,
    )?;
    let mut provisioning = manifest_for(
        manifest_context,
        ManifestStateContext {
            state: WorkspaceState::Provisioning,
            identity: Some(&worktree_identity),
            applied: None,
            worker_capability: None,
            lease_path: None,
            journal_sequence: "1",
            journal_head: None,
        },
    );
    // Provisioning deliberately retains null identity in its record until the lifetime lease is acquired.
    if let JcsValue::Object(object) = &mut provisioning.0 {
        object.insert("worktree_identity".into(), JcsValue::Null);
    }
    authority::atomic_replace_jcs(&manifest_file, &provisioning, 0o600)?;
    let lease_identity_record = CanonicalRecord(JcsValue::Object(BTreeMap::from([
        (
            "device".into(),
            JcsValue::String(lease_identity.device.to_string()),
        ),
        (
            "inode".into(),
            JcsValue::String(lease_identity.inode.to_string()),
        ),
        (
            "path".into(),
            JcsValue::String(lease_path.to_string_lossy().into_owned()),
        ),
        (
            "schema".into(),
            JcsValue::String("WorkspaceLeaseIdentityV1".into()),
        ),
    ])));
    authority::atomic_create_jcs(
        &session_dir.join("lease-identity-v1.json"),
        &lease_identity_record,
        0o600,
    )?;
    let applied = git::apply_wip(
        &repository,
        &worktree_root,
        &branch_ref,
        &request.base_commit,
        &wip,
        &request.created_at,
    )?;
    let broker_socket = broker_socket_path(
        roots.euid,
        &repository.identity.repository_id,
        &request.request_id,
    )?;
    let capability_file = git::actual_private_git_dir(&worktree_root)?
        .join("session-relay/worker-capabilities/00000000000000000001.json");
    authority::ensure_private_directory(capability_file.parent().unwrap(), roots.euid)?;
    let (worker_capability, record) = mint_worker(
        &repository.identity.repository_id,
        &request.request_id,
        1,
        &broker_socket,
        &now,
        "9999-12-31T23:59:59.999Z",
    )?;
    authority::atomic_create_jcs(&capability_file, &worker_capability, 0o600)?;
    authority::atomic_create_jcs(
        &session_dir.join("worker-capability-record-v1.json"),
        &record,
        0o600,
    )?;
    let git_shim = create_git_shim(&worktree_root, &capability_file, relay_executable)?;
    let event = JournalEventV1 {
        sequence: 1,
        previous_sha256: None,
        kind: "WipApplied".into(),
        payload: JcsValue::Object(BTreeMap::from([(
            "applied_wip_commit".into(),
            JcsValue::String(applied.clone()),
        )])),
        created_at: now.clone(),
    };
    let head = authority::append_journal(&session_dir, &event, 1, None)?;
    let manifest = manifest_for(
        manifest_context,
        ManifestStateContext {
            state: WorkspaceState::LeaseHeld,
            identity: Some(&worktree_identity),
            applied: Some(&applied),
            worker_capability: Some(&capability_file),
            lease_path: Some(&lease_path),
            journal_sequence: "1",
            journal_head: Some(&head),
        },
    );
    authority::atomic_replace_jcs(&manifest_file, &manifest, 0o600)?;
    let allocated = resources::allocate_resources(
        roots,
        &repository.identity.repository_id,
        &request.request_id,
        &request.resources,
        &session_dir.join("resources"),
        &now,
    )?;
    let prepared = resources::prepare_tool_launch(resources::ToolLaunchContext {
        tool: &request.tool,
        session_id: &request.request_id,
        workspace: &worktree_root,
        prompt: &request.task,
        generated_policy: WORKSPACE_WORKER_POLICY,
        git_shim_dir: git_shim
            .parent()
            .ok_or_else(|| "Git shim has no parent".to_string())?,
        worker_capability_file: &capability_file,
        resources: &allocated,
    })?;
    let tool_launch_file = session_dir.join("tool-launch-v1.json");
    let (_tool_launch, tool_launch_sha256) = resources::persist_tool_launch_decision(
        &tool_launch_file,
        &request.request_id,
        &request.tool,
        &prepared,
        &now,
    )?;
    let resource_digests = allocated
        .allocations
        .iter()
        .flat_map(|allocation| {
            [
                JcsValue::String(allocation.create_receipt_sha256.clone()),
                JcsValue::String(allocation.inspect_receipt_sha256.clone()),
            ]
        })
        .collect();
    let allocated_event = JournalEventV1 {
        sequence: 2,
        previous_sha256: Some(head.clone()),
        kind: "ResourcesAllocated".into(),
        payload: JcsValue::Object(BTreeMap::from([
            (
                "resource_receipt_sha256".into(),
                JcsValue::Array(resource_digests),
            ),
            (
                "tool_launch_sha256".into(),
                JcsValue::String(tool_launch_sha256.clone()),
            ),
        ])),
        created_at: now.clone(),
    };
    let ready_head = authority::append_journal(&session_dir, &allocated_event, 2, Some(&head))?;
    let ready = manifest_with_resource_allocations(
        manifest_for(
            manifest_context,
            ManifestStateContext {
                state: WorkspaceState::Ready,
                identity: Some(&worktree_identity),
                applied: Some(&applied),
                worker_capability: Some(&capability_file),
                lease_path: Some(&lease_path),
                journal_sequence: "2",
                journal_head: Some(&ready_head),
            },
        ),
        &allocated.allocations,
    );
    authority::atomic_replace_jcs(&manifest_file, &ready, 0o600)?;
    verify_open_file_identity(relay_executable, &relay_file, "relay executable")?;
    let start_fault_context = WorkspaceStartFaultContext {
        session_dir: &session_dir,
        session_id: &request.request_id,
        repository_id: &repository.identity.repository_id,
        lease_path: &lease_path,
        lease_identity: &lease_identity,
        relay_executable,
        relay_executable_sha256,
    };
    drop(gate);
    if let Err(error) = start_git_broker(GitBrokerStartContext {
        roots,
        relay_file: &relay_file,
        session_dir: &session_dir,
        worktree: &worktree_root,
        branch_ref: &branch_ref,
        capability_file: &capability_file,
        lease: &lease,
        resource_fds: &allocated.resource_fds,
    }) {
        let retained =
            persist_workspace_start_fault(&start_fault_context, "broker_start", false, &error)
                .and_then(|evidence| {
                    record_ready_start_fault(
                        &session_dir,
                        &manifest_file,
                        "broker_start",
                        &evidence,
                        &error,
                    )?;
                    Ok(evidence)
                });
        return Err(match retained {
            Err(persist_error) => {
                format!("{error}; persist durable Ready broker-start fault: {persist_error}")
            }
            Ok(_) => format!("{error}; Ready is retained for explicit recover resume_prelaunch"),
        });
    }
    let custody_active_sha256 = match start_custody_runtime(CustodyRuntimeStartContext {
        roots,
        relay_executable,
        relay_executable_sha256,
        relay_file: &relay_file,
        session_dir: &session_dir,
        session_id: &request.request_id,
        tool_launch_file: &tool_launch_file,
        lease: &lease,
        resource_fds: &allocated.resource_fds,
    }) {
        Ok(evidence) => evidence,
        Err(error) => {
            let error = schema::parse_jcs(
                &capability::read_secure_bytes(&session_dir.join("custody-fault-v1.json"))
                    .unwrap_or_default(),
                true,
            )
            .and_then(|value| {
                let object = value.object()?;
                Ok(format!(
                    "{error}; {}: {}",
                    object["code"].as_str()?,
                    object["error"].as_str()?
                ))
            })
            .unwrap_or(error);
            let retained =
                persist_workspace_start_fault(&start_fault_context, "custody_start", true, &error)
                    .and_then(|evidence| {
                        record_ready_start_fault(
                            &session_dir,
                            &manifest_file,
                            "custody_start",
                            &evidence,
                            &error,
                        )?;
                        Ok(evidence)
                    });
            return Err(match retained {
                Err(persist_error) => {
                    format!("{error}; persist durable Ready custody-start fault: {persist_error}")
                }
                Ok(_) => {
                    format!("{error}; Ready is retained for explicit recover resume_prelaunch")
                }
            });
        }
    };
    let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
    let running = mutate_manifest_event(
        ManifestEventContext {
            session_dir: &session_dir,
            manifest_file: &manifest_file,
            expected: WorkspaceState::Ready,
            next: WorkspaceState::Running,
            kind: "CustodyActivated",
            created_at: &authority::now_timestamp()?,
        },
        JcsValue::Object(BTreeMap::from([
            (
                "backend".into(),
                JcsValue::String(platform::LINUX_BACKEND.into()),
            ),
            (
                "custody_active_sha256".into(),
                JcsValue::String(custody_active_sha256.clone()),
            ),
        ])),
        |object| {
            object.insert(
                "custody_evidence".into(),
                JcsValue::Object(BTreeMap::from([
                    (
                        "active_sha256".into(),
                        JcsValue::String(custody_active_sha256.clone()),
                    ),
                    (
                        "backend".into(),
                        JcsValue::String(platform::LINUX_BACKEND.into()),
                    ),
                    ("empty_sha256".into(), JcsValue::Null),
                ])),
            );
            Ok(())
        },
    )?;
    if running.state()? != WorkspaceState::Running {
        return Err("custody activation did not durably publish Running".into());
    }
    drop(gate);
    let authority_record = authority.read_repository(&repository.identity.repository_id)?;
    let result = WorkspaceStartResultV1 {
        session_id: request.request_id.clone(),
        repository_id: repository.identity.repository_id.clone(),
        worktree_root: worktree_root.to_string_lossy().into_owned(),
        branch_ref,
        coordinator_capability_file: capability_path.to_string_lossy().into_owned(),
        coordinator_generation: authority_record.current_generation,
        bootstrap: match bootstrap {
            BootstrapOutcome::Created => "created",
            BootstrapOutcome::Existing => "existing",
        }
        .into(),
    };
    Ok(StartedWorkspace {
        result,
        lease,
        manifest_file,
        worker_capability_file: capability_file,
        resources: allocated,
        tool_launch_file,
        tool_launch_sha256,
    })
}

#[derive(Clone, Copy)]
struct ManifestContext<'a> {
    request: &'a WorkspaceStartRequestV1,
    repository: &'a git::OpenedRepository,
    worktree_root: &'a Path,
    branch_ref: &'a str,
    now: &'a str,
}

struct ManifestStateContext<'a> {
    state: WorkspaceState,
    identity: Option<&'a schema::WorktreeIdentityV1>,
    applied: Option<&'a str>,
    worker_capability: Option<&'a Path>,
    lease_path: Option<&'a Path>,
    journal_sequence: &'a str,
    journal_head: Option<&'a str>,
}

fn manifest_for(
    context: ManifestContext<'_>,
    state_context: ManifestStateContext<'_>,
) -> WorkspaceManifestRecord {
    let ManifestContext {
        request,
        repository,
        worktree_root,
        branch_ref,
        now,
    } = context;
    let ManifestStateContext {
        state,
        identity,
        applied,
        worker_capability,
        lease_path,
        journal_sequence,
        journal_head,
    } = state_context;
    let start = request.to_jcs().object().expect("start request object");
    let value = JcsValue::Object(BTreeMap::from([
        (
            "applied_wip_commit".into(),
            applied
                .map(|v| JcsValue::String(v.into()))
                .unwrap_or(JcsValue::Null),
        ),
        (
            "base_commit".into(),
            JcsValue::String(request.base_commit.clone()),
        ),
        ("branch_ref".into(), JcsValue::String(branch_ref.into())),
        (
            "coordinator_owned_paths".into(),
            start["coordinator_owned_paths"].clone(),
        ),
        (
            "created_at".into(),
            JcsValue::String(request.created_at.clone()),
        ),
        ("custody_evidence".into(), JcsValue::Null),
        ("integration_commits".into(), JcsValue::Array(Vec::new())),
        (
            "integration_root".into(),
            JcsValue::String(request.integration_root.clone()),
        ),
        (
            "journal_head_sha256".into(),
            journal_head
                .map(|v| JcsValue::String(v.into()))
                .unwrap_or(JcsValue::Null),
        ),
        (
            "journal_sequence".into(),
            JcsValue::String(journal_sequence.into()),
        ),
        ("last_error".into(), JcsValue::Null),
        (
            "lease_evidence".into(),
            lease_path
                .map(|v| JcsValue::String(v.to_string_lossy().into_owned()))
                .unwrap_or(JcsValue::Null),
        ),
        ("owned_paths".into(), start["owned_paths"].clone()),
        (
            "produced_commits".into(),
            applied
                .map(|oid| {
                    JcsValue::Array(vec![JcsValue::Object(BTreeMap::from([
                        ("oid".into(), JcsValue::String(oid.into())),
                        (
                            "parent_oid".into(),
                            JcsValue::String(request.base_commit.clone()),
                        ),
                        ("source".into(), JcsValue::String("applied_wip".into())),
                    ]))])
                })
                .unwrap_or(JcsValue::Array(Vec::new())),
        ),
        ("repository".into(), repository.identity.to_jcs()),
        ("resources".into(), start["resources"].clone()),
        ("retention_evidence".into(), JcsValue::Null),
        ("schema".into(), JcsValue::String(schema::SCHEMA_V1.into())),
        (
            "session_id".into(),
            JcsValue::String(request.request_id.clone()),
        ),
        ("state".into(), JcsValue::String(state.as_str().into())),
        ("task".into(), JcsValue::String(request.task.clone())),
        (
            "task_slug".into(),
            JcsValue::String(request.task_slug.clone()),
        ),
        ("tool".into(), start["tool"].clone()),
        ("updated_at".into(), JcsValue::String(now.into())),
        (
            "wip_receipt_path".into(),
            JcsValue::String(request.wip_receipt_path.clone()),
        ),
        (
            "wip_receipt_sha256".into(),
            JcsValue::String(request.wip_receipt_sha256.clone()),
        ),
        (
            "worker_base_commit".into(),
            applied
                .map(|v| JcsValue::String(v.into()))
                .unwrap_or(JcsValue::Null),
        ),
        (
            "worker_capability_file".into(),
            worker_capability
                .map(|v| JcsValue::String(v.to_string_lossy().into_owned()))
                .unwrap_or(JcsValue::Null),
        ),
        (
            "worktree_identity".into(),
            identity
                .map(schema::WorktreeIdentityV1::value)
                .unwrap_or(JcsValue::Null),
        ),
        (
            "worktree_root".into(),
            JcsValue::String(worktree_root.to_string_lossy().into_owned()),
        ),
    ]));
    WorkspaceManifestRecord(value)
}

fn manifest_with_resource_allocations(
    mut manifest: WorkspaceManifestRecord,
    allocations: &[schema::ResourceAllocationV1],
) -> WorkspaceManifestRecord {
    if let JcsValue::Object(object) = &mut manifest.0 {
        object.insert(
            "resources".into(),
            JcsValue::Array(allocations.iter().map(ClosedJcs::to_jcs).collect()),
        );
    }
    manifest
}

fn state_owns_mutation_claims(state: WorkspaceState) -> bool {
    matches!(
        state,
        WorkspaceState::Reserved
            | WorkspaceState::Provisioning
            | WorkspaceState::LeaseHeld
            | WorkspaceState::Ready
            | WorkspaceState::Running
            | WorkspaceState::HandbackReady
            | WorkspaceState::IntegrationQueued
    )
}

fn reject_overlapping_session_claims(
    sessions: &Path,
    request: &WorkspaceStartRequestV1,
) -> Result<(), String> {
    if !sessions.exists() {
        return Ok(());
    }
    let requested = request
        .owned_paths
        .iter()
        .chain(&request.coordinator_owned_paths)
        .map(|claim| claim.path.to_ascii_lowercase())
        .collect::<Vec<_>>();
    for entry in fs::read_dir(sessions).map_err(|e| format!("read workspace sessions: {e}"))? {
        let path = entry
            .map_err(|e| format!("read workspace session entry: {e}"))?
            .path()
            .join("manifest-v1.json");
        let manifest: WorkspaceManifestRecord = schema::read_jcs_file(&path, None)?;
        if !state_owns_mutation_claims(manifest.state()?) {
            continue;
        }
        let object = manifest.object()?;
        for field in ["owned_paths", "coordinator_owned_paths"] {
            let JcsValue::Array(existing) = &object[field] else {
                return Err(format!("manifest {field} is not an array"));
            };
            for value in existing {
                let claim = value.clone().object()?;
                let old = claim["path"].as_str()?.to_ascii_lowercase();
                for new in &requested {
                    if new == &old
                        || new.strip_prefix(&format!("{old}/")).is_some()
                        || old.strip_prefix(&format!("{new}/")).is_some()
                    {
                        return Err(format!(
                            "path claim {new} overlaps live session claim {old}"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn shell_quote_path(value: &Path) -> String {
    value.to_string_lossy().replace('\'', "'\"'\"'")
}

fn git_shim_body(capability_file: &Path, relay_executable: &Path) -> String {
    format!(
        "#!/bin/sh\nexec '{}' workspace __broker-client --worker-capability-file '{}' -- \"$@\"\n",
        shell_quote_path(relay_executable),
        shell_quote_path(capability_file)
    )
}

fn create_git_shim(
    worktree: &Path,
    capability_file: &Path,
    relay_executable: &Path,
) -> Result<PathBuf, String> {
    let private = git::actual_private_git_dir(worktree)?;
    let private_fd = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(&private)
        .map_err(|e| format!("securely open private Git directory for shim: {e}"))?;
    let session_relay = open_or_create_private_dir_at(&private_fd, "session-relay")?;
    let bin = open_or_create_private_dir_at(&session_relay, "bin")?;
    let body = git_shim_body(capability_file, relay_executable);
    let name = std::ffi::CString::new("git").unwrap();
    let fd = unsafe {
        libc::openat(
            bin.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o500,
        )
    };
    if fd < 0 {
        return Err(format!(
            "create Git shim: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut shim_file = unsafe { File::from_raw_fd(fd) };
    let metadata = shim_file
        .metadata()
        .map_err(|e| format!("fstat Git shim: {e}"))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o500
    {
        return Err("Git shim is not an EUID-owned single-link mode-0500 regular file".into());
    }
    shim_file
        .write_all(body.as_bytes())
        .and_then(|_| shim_file.sync_all())
        .map_err(|e| format!("persist Git shim: {e}"))?;
    bin.sync_all()
        .map_err(|e| format!("fsync Git shim directory: {e}"))?;
    Ok(private.join("session-relay/bin/git"))
}

fn open_or_create_private_dir_at(parent: &File, name: &str) -> Result<File, String> {
    let name = std::ffi::CString::new(name)
        .map_err(|_| "private directory component contains NUL".to_string())?;
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if created != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("create private directory component: {error}"));
        }
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
            0,
        )
    };
    if fd < 0 {
        return Err(format!(
            "securely open private directory component: {}",
            std::io::Error::last_os_error()
        ));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .map_err(|e| format!("fstat private directory component: {e}"))?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err("Git shim directory is not an EUID-owned mode-0700 real directory".into());
    }
    if created == 0 {
        parent
            .sync_all()
            .map_err(|e| format!("fsync parent after Git shim directory creation: {e}"))?
    }
    Ok(file)
}

pub fn list_workspaces(repository_path: &Path, capability_file: &Path) -> Result<JcsValue, String> {
    let repository = git::OpenedRepository::open(repository_path)?;
    let authority = WorkspaceAuthority::system()?;
    authority.authenticate(&repository.identity.repository_id, capability_file, "list")?;
    let sessions = authority
        .roots()
        .data
        .join("repositories")
        .join(&repository.identity.repository_id)
        .join("sessions");
    let mut values = Vec::new();
    if sessions.exists() {
        for entry in fs::read_dir(sessions).map_err(|e| format!("read workspace sessions: {e}"))? {
            let entry = entry.map_err(|e| format!("read workspace session: {e}"))?;
            if !entry
                .file_type()
                .map_err(|e| format!("inspect workspace session entry: {e}"))?
                .is_dir()
            {
                continue;
            }
            let path = entry.path().join("manifest-v1.json");
            let record: WorkspaceManifestRecord = schema::read_jcs_file(&path, None)?;
            values.push(record.0)
        }
    }
    values.sort_by_key(manifest_session);
    Ok(JcsValue::Object(BTreeMap::from([
        ("custody".into(), JcsValue::String("unproven".into())),
        ("schema".into(), JcsValue::String(schema::SCHEMA_V1.into())),
        ("workspaces".into(), JcsValue::Array(values)),
    ])))
}
fn manifest_session(value: &JcsValue) -> String {
    match value {
        JcsValue::Object(object) => object
            .get("session_id")
            .and_then(|value| value.as_str().ok())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}
pub fn inspect_workspace(
    session_id: &str,
    repository_path: &Path,
    capability_file: &Path,
) -> Result<JcsValue, String> {
    LowerUuidV4::parse(session_id)?;
    let repository = git::OpenedRepository::open(repository_path)?;
    let authority = WorkspaceAuthority::system()?;
    authority.authenticate(
        &repository.identity.repository_id,
        capability_file,
        "inspect",
    )?;
    let path = authority
        .roots()
        .data
        .join("repositories")
        .join(&repository.identity.repository_id)
        .join("sessions")
        .join(session_id)
        .join("manifest-v1.json");
    let manifest: WorkspaceManifestRecord = schema::read_jcs_file(&path, None)?;
    Ok(JcsValue::Object(BTreeMap::from([
        ("custody".into(), JcsValue::String("unproven".into())),
        ("schema".into(), JcsValue::String(schema::SCHEMA_V1.into())),
        ("workspace".into(), manifest.0),
    ])))
}

pub fn handback_workspace(
    request_file: &Path,
    request_sha256: &str,
    worker_capability_file: &Path,
) -> Result<JcsValue, String> {
    let request_bytes = capability::read_secure_bytes(request_file)
        .map_err(|e| format!("read handback request: {e}"))?;
    schema::Sha256Digest::parse(request_sha256)?;
    if !sha256::constant_time_eq(
        sha256::hex_digest(&request_bytes).as_bytes(),
        request_sha256.as_bytes(),
    ) {
        return Err("handback request SHA-256 mismatch".into());
    }
    let request = schema::parse_jcs(&request_bytes, true)?;
    let object = closed_object(
        &request,
        &[
            "schema",
            "request_id",
            "session_id",
            "expected_head",
            "created_at",
        ],
        "HandbackRequestV1",
    )?;
    if object["schema"].as_str() != Ok(schema::SCHEMA_V1) {
        return Err("HandbackRequestV1 schema mismatch".into());
    }
    LowerUuidV4::parse(object["request_id"].as_str()?)?;
    LowerUuidV4::parse(object["session_id"].as_str()?)?;
    schema::Timestamp::parse(object["created_at"].as_str()?)?;
    let capability: WorkerCapabilityV1 = schema::read_jcs_file(worker_capability_file, None)?;
    if capability.session_id != object["session_id"].as_str()? {
        return Err("handback capability session differs from request".into());
    }
    broker_exchange(
        &capability,
        "handback",
        vec![
            request_file.to_string_lossy().into_owned(),
            request_sha256.into(),
        ],
        std::env::current_dir().map_err(|e| format!("resolve handback cwd: {e}"))?,
    )
}

fn closed_object<'a>(
    value: &'a JcsValue,
    keys: &[&str],
    name: &str,
) -> Result<&'a BTreeMap<String, JcsValue>, String> {
    let JcsValue::Object(object) = value else {
        return Err(format!("{name} must be an object"));
    };
    if object.len() != keys.len() || keys.iter().any(|key| !object.contains_key(*key)) {
        return Err(format!("{name} keys differ from the closed schema"));
    }
    Ok(object)
}

fn broker_exchange(
    capability: &WorkerCapabilityV1,
    operation: &str,
    argv: Vec<String>,
    cwd: PathBuf,
) -> Result<JcsValue, String> {
    let request_id = crate::store::uuid_v4();
    let cwd = cwd
        .to_str()
        .ok_or_else(|| "broker cwd is not UTF-8".to_string())?
        .to_string();
    let bare = JcsValue::Object(BTreeMap::from([
        (
            "argv".into(),
            JcsValue::Array(argv.iter().cloned().map(JcsValue::String).collect()),
        ),
        (
            "capability_id".into(),
            JcsValue::String(capability.capability_id.clone()),
        ),
        ("cwd".into(), JcsValue::String(cwd.clone())),
        (
            "generation".into(),
            JcsValue::String(capability.generation.clone()),
        ),
        ("operation".into(), JcsValue::String(operation.into())),
        ("request_id".into(), JcsValue::String(request_id.clone())),
        ("schema".into(), JcsValue::String(schema::SCHEMA_V1.into())),
        (
            "session_id".into(),
            JcsValue::String(capability.session_id.clone()),
        ),
    ]));
    let request_sha256 = sha256::hex_digest(
        [
            b"session-relay/broker-request/v1\0".as_slice(),
            schema::serialize_jcs(&bare).as_bytes(),
        ]
        .concat()
        .as_slice(),
    );
    let mut request = bare.object()?;
    request.insert("request_sha256".into(), JcsValue::String(request_sha256));
    let request = JcsValue::Object(request);
    let nonce = capability::encode_base64url(&capability::random_secret()?);
    let secret = capability::decode_base64url(&capability.secret_b64url)?;
    let message = [
        b"session-relay/broker-envelope/v1\0".as_slice(),
        schema::serialize_jcs(&request).as_bytes(),
        nonce.as_bytes(),
    ]
    .concat();
    let mac = capability::encode_base64url(&sha256::hmac(&secret, &message));
    let envelope = JcsValue::Object(BTreeMap::from([
        ("mac".into(), JcsValue::String(mac)),
        ("nonce".into(), JcsValue::String(nonce)),
        ("request".into(), request),
        ("schema".into(), JcsValue::String(schema::SCHEMA_V1.into())),
    ]));
    let mut bytes = schema::serialize_jcs(&envelope).into_bytes();
    bytes.push(b'\n');
    let mut stream = UnixStream::connect(&capability.broker_socket)
        .map_err(|e| format!("connect Git broker {}: {e}", capability.broker_socket))?;
    stream
        .write_all(&bytes)
        .map_err(|e| format!("write Git broker request: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("finish Git broker request: {e}"))?;
    let mut response = Vec::new();
    stream
        .take(1024 * 1024)
        .read_to_end(&mut response)
        .map_err(|e| format!("read Git broker response: {e}"))?;
    let value = schema::parse_jcs(&response, true)?;
    let object = closed_object(
        &value,
        &[
            "schema",
            "request_id",
            "status",
            "exit_code",
            "stdout",
            "stderr",
            "receipt",
        ],
        "GitBrokerResponseV1",
    )?;
    if object["request_id"].as_str()? != request_id {
        return Err("Git broker response request ID mismatch".into());
    }
    if object["status"].as_str()? == "error" {
        return Err(object["stderr"].as_str()?.to_string());
    }
    Ok(value)
}
fn session_dir_for(
    roots: &AuthorityRoots,
    repository_id: &str,
    session_id: &str,
) -> Result<PathBuf, String> {
    Sha256Digest::parse(repository_id)?;
    LowerUuidV4::parse(session_id)?;
    let authority = WorkspaceAuthority::new(roots.clone())?;
    authority.read_repository(repository_id)?;
    let path = authority
        .repository_dir(repository_id)?
        .join("sessions")
        .join(session_id);
    match fs::symlink_metadata(&path) {
        Ok(_) => verify_existing_private_directory_path(&path, roots.euid)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("workspace session is not durably present".into());
        }
        Err(error) => {
            return Err(format!(
                "inspect durable workspace session {}: {error}",
                path.display()
            ));
        }
    }
    Ok(path)
}

fn verify_existing_private_directory_path(path: &Path, euid: u32) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "durable workspace session {} is not absolute",
            path.display()
        ));
    }
    let mut current = PathBuf::from("/");
    for component in path.components().skip(1) {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            format!(
                "inspect durable workspace session component {}: {error}",
                current.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "durable workspace session component {} is not a real directory",
                current.display()
            ));
        }
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map_err(|error| {
            format!(
                "securely open durable workspace session {}: {error}",
                path.display()
            )
        })?;
    let opened = directory
        .metadata()
        .map_err(|error| format!("fstat durable workspace session: {error}"))?;
    let named = fs::symlink_metadata(path)
        .map_err(|error| format!("revalidate durable workspace session: {error}"))?;
    if !opened.is_dir()
        || opened.uid() != euid
        || opened.mode() & 0o777 != 0o700
        || !named.is_dir()
        || named.file_type().is_symlink()
        || named.uid() != euid
        || named.dev() != opened.dev()
        || named.ino() != opened.ino()
    {
        return Err(format!(
            "{} is not an exact EUID-owned mode-0700 real workspace session directory",
            path.display()
        ));
    }
    Ok(())
}

fn manifest_produced_head(manifest: &WorkspaceManifestRecord) -> Result<String, String> {
    let object = manifest.object()?;
    if let JcsValue::Array(values) = &object["produced_commits"] {
        if let Some(last) = values.last() {
            return last.clone().object()?["oid"].as_str().map(str::to_string);
        }
    }
    object["worker_base_commit"].as_str().map(str::to_string)
}

fn ordered_request_from_manifest(
    manifest: &WorkspaceManifestRecord,
    integration_root: &Path,
    integration_branch_ref: &str,
    expected_integration_head: &str,
) -> Result<git::OrderedIntegrationRequest, String> {
    let object = manifest.object()?;
    let produced_commits = match &object["produced_commits"] {
        JcsValue::Array(values) => values
            .iter()
            .map(|value| {
                let object = value.clone().object()?;
                Ok(git::OrderedProducedCommit {
                    oid: object["oid"].as_str()?.into(),
                    parent_oid: object["parent_oid"].as_str()?.into(),
                    source: object["source"].as_str()?.into(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
        _ => return Err("workspace produced_commits is not an array".into()),
    };
    if produced_commits.is_empty() {
        return Err("workspace has no produced commit chain".into());
    }
    let expected_worker_head = produced_commits
        .last()
        .expect("nonempty produced commit chain")
        .oid
        .clone();
    Ok(git::OrderedIntegrationRequest {
        integration_root: integration_root.to_path_buf(),
        integration_branch_ref: integration_branch_ref.into(),
        expected_integration_head: expected_integration_head.into(),
        worker_root: PathBuf::from(object["worktree_root"].as_str()?),
        worker_branch_ref: object["branch_ref"].as_str()?.into(),
        expected_worker_head,
        base_commit: object["base_commit"].as_str()?.into(),
        produced_commits,
    })
}

fn integration_receipt_path(session_dir: &Path) -> PathBuf {
    session_dir.join("integration-receipt-v1.json")
}

const INTEGRATION_FAULT_POINTS: [&str; 6] = [
    "after_pre_index",
    "after_progress",
    "after_queue",
    "after_git_step",
    "after_step_progress",
    "after_receipt",
];

thread_local! {
    static INTEGRATION_FAULT_SELECTOR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[doc(hidden)]
pub fn set_integration_fault_for_test(session_id: &str, point: &str) -> Result<(), String> {
    if std::env::var_os("SESSION_RELAY_TEST_CGROUP_ROOT").is_none() {
        return Err("integration fault injection requires SESSION_RELAY_TEST_CGROUP_ROOT".into());
    }
    LowerUuidV4::parse(session_id)?;
    if !INTEGRATION_FAULT_POINTS.contains(&point) {
        return Err("integration fault point is outside the closed test set".into());
    }
    INTEGRATION_FAULT_SELECTOR.with(|selector| {
        selector.replace(Some(format!("{session_id}:{point}")));
    });
    Ok(())
}

fn integration_fault(session_id: &str, point: &str) -> Result<(), String> {
    if std::env::var_os("SESSION_RELAY_TEST_CGROUP_ROOT").is_none()
        || !INTEGRATION_FAULT_POINTS.contains(&point)
    {
        return Ok(());
    }
    let expected = format!("{session_id}:{point}");
    let injected = INTEGRATION_FAULT_SELECTOR.with(|selector| {
        let mut selector = selector.borrow_mut();
        if selector.as_deref() == Some(expected.as_str()) {
            selector.take();
            true
        } else {
            false
        }
    });
    if injected {
        Err(format!("injected integration fault at {point}"))
    } else {
        Ok(())
    }
}

#[derive(Clone)]
struct DurableIntegrationProgress {
    request_sha256: String,
    disposition: String,
    integration_branch_ref: String,
    prestate: git::OrderedIntegrationPrestate,
    outputs: Vec<String>,
    step_outputs: Vec<String>,
}

fn pristine_git_state_jcs(state: &git::PristineGitState) -> JcsValue {
    JcsValue::Object(BTreeMap::from([
        ("head_oid".into(), JcsValue::String(state.head_oid.clone())),
        (
            "head_tree_oid".into(),
            JcsValue::String(state.head_tree_oid.clone()),
        ),
        (
            "index_entries_sha256".into(),
            JcsValue::String(state.index_entries_sha256.clone()),
        ),
        (
            "index_file_sha256".into(),
            JcsValue::String(state.index_file_sha256.clone()),
        ),
        (
            "index_tree_oid".into(),
            JcsValue::String(state.index_tree_oid.clone()),
        ),
        (
            "status_sha256".into(),
            JcsValue::String(state.status_sha256.clone()),
        ),
    ]))
}

fn pristine_git_state_from_jcs(value: &JcsValue) -> Result<git::PristineGitState, String> {
    let object = closed_object(
        value,
        &[
            "head_oid",
            "head_tree_oid",
            "index_entries_sha256",
            "index_file_sha256",
            "index_tree_oid",
            "status_sha256",
        ],
        "IntegrationPrestateV1",
    )?;
    for key in ["index_entries_sha256", "index_file_sha256", "status_sha256"] {
        Sha256Digest::parse(object[key].as_str()?)?;
    }
    Ok(git::PristineGitState {
        head_oid: object["head_oid"].as_str()?.into(),
        head_tree_oid: object["head_tree_oid"].as_str()?.into(),
        index_tree_oid: object["index_tree_oid"].as_str()?.into(),
        index_file_sha256: object["index_file_sha256"].as_str()?.into(),
        index_entries_sha256: object["index_entries_sha256"].as_str()?.into(),
        status_sha256: object["status_sha256"].as_str()?.into(),
    })
}

fn integration_progress_record(progress: &DurableIntegrationProgress) -> CanonicalRecord {
    CanonicalRecord(JcsValue::Object(BTreeMap::from([
        (
            "disposition".into(),
            JcsValue::String(progress.disposition.clone()),
        ),
        (
            "integration_branch_ref".into(),
            JcsValue::String(progress.integration_branch_ref.clone()),
        ),
        (
            "outputs".into(),
            JcsValue::Array(
                progress
                    .outputs
                    .iter()
                    .cloned()
                    .map(JcsValue::String)
                    .collect(),
            ),
        ),
        (
            "step_outputs".into(),
            JcsValue::Array(
                progress
                    .step_outputs
                    .iter()
                    .cloned()
                    .map(JcsValue::String)
                    .collect(),
            ),
        ),
        (
            "pre_index_sha256".into(),
            JcsValue::String(
                progress
                    .prestate
                    .integration_state
                    .index_file_sha256
                    .clone(),
            ),
        ),
        (
            "prestate".into(),
            pristine_git_state_jcs(&progress.prestate.integration_state),
        ),
        (
            "request_sha256".into(),
            JcsValue::String(progress.request_sha256.clone()),
        ),
        (
            "schema".into(),
            JcsValue::String("IntegrationProgressV1".into()),
        ),
    ])))
}

fn read_integration_progress(
    session_dir: &Path,
    request_sha256: &str,
    disposition: &str,
    integration_branch_ref: &str,
) -> Result<DurableIntegrationProgress, String> {
    let path = session_dir.join("integration-progress-v1.json");
    let value = schema::parse_jcs(&capability::read_secure_bytes(&path)?, true)?;
    let object = closed_object(
        &value,
        &[
            "schema",
            "request_sha256",
            "disposition",
            "integration_branch_ref",
            "prestate",
            "pre_index_sha256",
            "outputs",
            "step_outputs",
        ],
        "IntegrationProgressV1",
    )?;
    if object["schema"].as_str()? != "IntegrationProgressV1"
        || object["request_sha256"].as_str()? != request_sha256
        || object["disposition"].as_str()? != disposition
        || object["integration_branch_ref"].as_str()? != integration_branch_ref
    {
        return Err("durable integration progress differs from the coordinator request".into());
    }
    let prestate_value = pristine_git_state_from_jcs(&object["prestate"])?;
    if object["pre_index_sha256"].as_str()? != prestate_value.index_file_sha256 {
        return Err("durable integration pre-index digest differs from its prestate".into());
    }
    let pre_index_path = session_dir.join("integration-pre-index-v1");
    let pre_index = capability::read_secure_bytes(&pre_index_path)?;
    if sha256::hex_digest(&pre_index) != prestate_value.index_file_sha256 {
        return Err("durable integration pre-index bytes differ from progress evidence".into());
    }
    let outputs = match &object["outputs"] {
        JcsValue::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("durable integration outputs must be an array".into()),
    };
    let step_outputs = match &object["step_outputs"] {
        JcsValue::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("durable integration step outputs must be an array".into()),
    };
    for output in outputs.iter().chain(&step_outputs) {
        schema::validate_git_oid(output)?;
    }
    Ok(DurableIntegrationProgress {
        request_sha256: request_sha256.into(),
        disposition: disposition.into(),
        integration_branch_ref: integration_branch_ref.into(),
        prestate: git::OrderedIntegrationPrestate {
            integration_state: prestate_value,
            integration_index_bytes: pre_index,
        },
        outputs,
        step_outputs,
    })
}

fn create_integration_progress(
    session_dir: &Path,
    session_id: &str,
    request_sha256: &str,
    disposition: &str,
    integration_branch_ref: &str,
    prestate: git::OrderedIntegrationPrestate,
) -> Result<DurableIntegrationProgress, String> {
    let index_path = session_dir.join("integration-pre-index-v1");
    if index_path.exists() {
        if capability::read_secure_bytes(&index_path)? != prestate.integration_index_bytes {
            return Err(
                "existing durable integration pre-index differs from the clean prestate".into(),
            );
        }
    } else {
        write_private_bytes(&index_path, &prestate.integration_index_bytes)?;
    }
    integration_fault(session_id, "after_pre_index")?;
    let progress = DurableIntegrationProgress {
        request_sha256: request_sha256.into(),
        disposition: disposition.into(),
        integration_branch_ref: integration_branch_ref.into(),
        prestate,
        outputs: Vec::new(),
        step_outputs: Vec::new(),
    };
    let path = session_dir.join("integration-progress-v1.json");
    if path.exists() {
        let existing = read_integration_progress(
            session_dir,
            request_sha256,
            disposition,
            integration_branch_ref,
        )?;
        if existing.outputs.is_empty()
            && existing.step_outputs.is_empty()
            && existing.prestate.integration_state == progress.prestate.integration_state
            && existing.prestate.integration_index_bytes
                == progress.prestate.integration_index_bytes
        {
            return Ok(existing);
        }
        return Err("existing durable integration progress differs from the clean prestate".into());
    }
    authority::atomic_create_jcs(&path, &integration_progress_record(&progress), 0o600)?;
    Ok(progress)
}

fn replace_integration_outputs(
    session_dir: &Path,
    progress: &mut DurableIntegrationProgress,
    outputs: &[String],
) -> Result<(), String> {
    if outputs.is_empty() {
        progress.outputs.clear();
    } else {
        if outputs.len() < progress.outputs.len()
            || progress.outputs != outputs[..progress.outputs.len()]
        {
            return Err("integration output evidence is not an append-only exact prefix".into());
        }
        progress
            .step_outputs
            .extend(outputs[progress.outputs.len()..].iter().cloned());
        progress.outputs = outputs.to_vec();
    }
    authority::atomic_replace_jcs(
        &session_dir.join("integration-progress-v1.json"),
        &integration_progress_record(progress),
        0o600,
    )
}

fn settle_integration_receipt(
    session_dir: &Path,
    manifest_path: &Path,
    request: &IntegrateRequestV1,
    receipt_path: &Path,
    receipt: &IntegrationReceiptV1,
) -> Result<(), String> {
    receipt.validate()?;
    if receipt.request_id != request.request_id || receipt.session_id != request.session_id {
        return Err("integration receipt belongs to a different coordinator request".into());
    }
    let next = match receipt.outcome.as_str() {
        "integrated" => WorkspaceState::Integrated,
        "rejected" => WorkspaceState::Rejected,
        "needs_user_action" => WorkspaceState::IntegrationBlocked,
        _ => return Err("integration receipt outcome is outside the closed set".into()),
    };
    let receipt_sha256 = sha256::hex_digest(&capability::read_secure_bytes(receipt_path)?);
    let integration_commits = receipt
        .integration_commits
        .iter()
        .cloned()
        .map(JcsValue::String)
        .collect::<Vec<_>>();
    mutate_manifest_event(
        ManifestEventContext {
            session_dir,
            manifest_file: manifest_path,
            expected: WorkspaceState::IntegrationQueued,
            next,
            kind: "IntegrationSettled",
            created_at: &request.created_at,
        },
        JcsValue::Object(BTreeMap::from([
            ("outcome".into(), JcsValue::String(receipt.outcome.clone())),
            ("receipt_sha256".into(), JcsValue::String(receipt_sha256)),
        ])),
        |object| {
            object.insert(
                "integration_commits".into(),
                JcsValue::Array(integration_commits),
            );
            Ok(())
        },
    )
    .map(|_| ())
}

pub fn integrate_workspace_with_roots(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: &Path,
) -> Result<IntegrationReceiptV1, String> {
    let request: IntegrateRequestV1 =
        schema::read_jcs_file(request_file, Some(request_sha256)).map_err(|error| {
            if error == "integration expected_state must be HandbackReady" {
                "integration expected state must be HandbackReady; IntegrationBlocked is already durably settled"
                    .to_string()
            } else {
                error
            }
        })?;
    let integration_root = PathBuf::from(&request.repository_path);
    let repository = git::OpenedRepository::open(&integration_root)?;
    let authority = WorkspaceAuthority::new(roots.clone())?;
    if request.repository_id != repository.identity.repository_id {
        return Err(
            "integration request repository identity differs from its canonical path".into(),
        );
    }
    authority.authenticate(
        &repository.identity.repository_id,
        coordinator_capability_file,
        "integrate",
    )?;
    let session_dir = session_dir_for(
        roots,
        &repository.identity.repository_id,
        &request.session_id,
    )?;
    let queue = authority.acquire_integration_queue(&repository.identity.repository_id)?;
    let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
    let manifest_path = session_dir.join("manifest-v1.json");
    let manifest: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    let state = manifest.state()?;
    let receipt_path = integration_receipt_path(&session_dir);
    if state == WorkspaceState::IntegrationBlocked {
        if receipt_path.exists() {
            let receipt: IntegrationReceiptV1 = schema::read_jcs_file(&receipt_path, None)?;
            if receipt.request_id == request.request_id
                && receipt.session_id == request.session_id
                && receipt.outcome == "needs_user_action"
            {
                return Ok(receipt);
            }
        }
        return Err(
            "integration is durably blocked and will not be retried without explicit recovery"
                .into(),
        );
    }
    if matches!(state, WorkspaceState::Integrated | WorkspaceState::Rejected) {
        let receipt: IntegrationReceiptV1 =
            schema::read_jcs_file(&receipt_path, None).map_err(|error| {
                format!("settled integration has no valid durable receipt: {error}")
            })?;
        let expected_outcome = if state == WorkspaceState::Integrated {
            "integrated"
        } else {
            "rejected"
        };
        if receipt.request_id != request.request_id
            || receipt.session_id != request.session_id
            || receipt.outcome != expected_outcome
        {
            return Err(
                "settled integration receipt differs from the exact coordinator request".into(),
            );
        }
        let integration_branch =
            git::run_git_text(&integration_root, &["symbolic-ref", "-q", "HEAD"])?;
        read_integration_progress(
            &session_dir,
            request_sha256,
            &request.disposition,
            &integration_branch,
        )?;
        return Ok(receipt);
    }
    if !matches!(
        state,
        WorkspaceState::HandbackReady | WorkspaceState::IntegrationQueued
    ) {
        return Err("integration coordinator request was already durably settled".into());
    }
    let manifest_object = manifest.object()?;
    if manifest_object["session_id"].as_str()? != request.session_id {
        return Err(
            "integration request session differs from the durable workspace manifest".into(),
        );
    }
    if state == WorkspaceState::HandbackReady
        && (state != request.expected_state
            || manifest_object["journal_head_sha256"].as_str()?
                != request.expected_journal_head_sha256)
    {
        return Err("integration request CAS differs from the durable workspace manifest".into());
    }
    if schema::RepositoryIdentityV1::from_jcs(manifest.object()?["repository"].clone())?
        != repository.identity
    {
        return Err("integration repository differs from the workspace manifest".into());
    }
    repository.validate_unchanged()?;
    let integration_branch = git::run_git_text(&integration_root, &["symbolic-ref", "-q", "HEAD"])?;
    let ordered = ordered_request_from_manifest(
        &manifest,
        &integration_root,
        &integration_branch,
        &request.expected_head,
    )?;
    let intent = CanonicalRecord(JcsValue::Object(BTreeMap::from([
        (
            "action".into(),
            JcsValue::String(request.disposition.clone()),
        ),
        (
            "request_sha256".into(),
            JcsValue::String(request_sha256.into()),
        ),
        (
            "schema".into(),
            JcsValue::String("IntegrationIntentV1".into()),
        ),
        (
            "session_id".into(),
            JcsValue::String(request.session_id.clone()),
        ),
    ])));
    let intent_path = session_dir.join("integration-intent-v1.json");
    if intent_path.exists() {
        let existing = schema::parse_jcs(&capability::read_secure_bytes(&intent_path)?, true)?;
        if existing != intent.0 {
            return Err("integration intent differs from the durable request".into());
        }
    } else {
        authority::atomic_create_jcs(&intent_path, &intent, 0o600)?;
    }

    let recovering = state == WorkspaceState::IntegrationQueued;
    let mut progress = if recovering {
        read_integration_progress(
            &session_dir,
            request_sha256,
            &request.disposition,
            &integration_branch,
        )?
    } else {
        let prestate = git::capture_ordered_integration_prestate(&repository, &ordered)?;
        let progress = create_integration_progress(
            &session_dir,
            &request.session_id,
            request_sha256,
            &request.disposition,
            &integration_branch,
            prestate,
        )?;
        integration_fault(&request.session_id, "after_progress")?;
        mutate_manifest_event(
            ManifestEventContext {
                session_dir: &session_dir,
                manifest_file: &manifest_path,
                expected: WorkspaceState::HandbackReady,
                next: WorkspaceState::IntegrationQueued,
                kind: "IntegrationQueued",
                created_at: &request.created_at,
            },
            JcsValue::Object(BTreeMap::from([(
                "request_sha256".into(),
                JcsValue::String(request_sha256.into()),
            )])),
            |_| Ok(()),
        )?;
        integration_fault(&request.session_id, "after_queue")?;
        progress
    };

    if receipt_path.exists() {
        let receipt: IntegrationReceiptV1 = schema::read_jcs_file(&receipt_path, None)?;
        if receipt.pre_integration_head != progress.prestate.integration_state.head_oid
            || receipt.worker_commits
                != ordered
                    .produced_commits
                    .iter()
                    .map(|commit| commit.oid.clone())
                    .collect::<Vec<_>>()
            || (receipt.outcome == "integrated" && receipt.integration_commits != progress.outputs)
        {
            return Err(
                "integration receipt differs from durable pre-head or per-step evidence".into(),
            );
        }
        git::verify_queued_integration_outputs(
            &repository,
            &ordered,
            &progress.prestate,
            &receipt.integration_commits,
        )?;
        if receipt.post_integration_head
            != receipt
                .integration_commits
                .last()
                .unwrap_or(&receipt.pre_integration_head)
                .clone()
        {
            return Err(
                "integration receipt post-head differs from its exact output evidence".into(),
            );
        }
        settle_integration_receipt(
            &session_dir,
            &manifest_path,
            &request,
            &receipt_path,
            &receipt,
        )?;
        drop(gate);
        drop(queue);
        return Ok(receipt);
    }

    if recovering {
        let durable_outputs = progress.outputs.clone();
        let durable_prestate = progress.prestate.clone();
        git::rollback_queued_integration(
            &repository,
            &ordered,
            &durable_prestate,
            &durable_outputs,
            |outputs| replace_integration_outputs(&session_dir, &mut progress, outputs),
        )?;
        if !progress.outputs.is_empty() {
            replace_integration_outputs(&session_dir, &mut progress, &[])?;
        }
    }
    let result = match request.disposition.as_str() {
        "integrate" => git::integrate_ordered_with_step(&repository, &ordered, |outputs| {
            if !outputs.is_empty() {
                integration_fault(&request.session_id, "after_git_step")?;
            }
            replace_integration_outputs(&session_dir, &mut progress, outputs)?;
            if !outputs.is_empty() {
                integration_fault(&request.session_id, "after_step_progress")?;
            }
            Ok(())
        })?,
        "reject" => git::reject_ordered(&repository, &ordered)?,
        _ => return Err("integration disposition is outside the closed set".into()),
    };
    let receipt = IntegrationReceiptV1 {
        request_id: request.request_id.clone(),
        session_id: request.session_id.clone(),
        outcome: result.outcome.as_str().into(),
        pre_integration_head: result.pre_head,
        worker_commits: ordered
            .produced_commits
            .iter()
            .map(|commit| commit.oid.clone())
            .collect(),
        integration_commits: result.output_oids,
        post_integration_head: result.post_head,
        conflict_paths: result.conflict_paths,
        created_at: request.created_at.clone(),
    };
    receipt.validate()?;
    authority::atomic_create_jcs(&receipt_path, &receipt, 0o600)?;
    integration_fault(&request.session_id, "after_receipt")?;
    settle_integration_receipt(
        &session_dir,
        &manifest_path,
        &request,
        &receipt_path,
        &receipt,
    )?;
    drop(gate);
    drop(queue);
    Ok(receipt)
}

fn hashed_file(path: &Path) -> Result<HashedFileV1, String> {
    let canonical =
        fs::canonicalize(path).map_err(|error| format!("canonicalize artifact: {error}"))?;
    let bytes =
        fs::read(&canonical).map_err(|error| format!("read {}: {error}", canonical.display()))?;
    let file = HashedFileV1 {
        path: canonical.to_string_lossy().into_owned(),
        sha256: sha256::hex_digest(&bytes),
        size: bytes.len().to_string(),
    };
    file.validate()?;
    Ok(file)
}

fn sync_file_and_parent(path: &Path) -> Result<(), String> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("fsync {}: {error}", path.display()))?;
    let parent = path
        .parent()
        .ok_or_else(|| "artifact path has no parent".to_string())?;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("fsync {}: {error}", parent.display()))
}

fn create_retention_proof(
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    reason: &str,
    proven_at: &str,
) -> Result<(RetentionProofV1, String), String> {
    let proof_path = session_dir.join("retention-proof-v1.json");
    if proof_path.exists() {
        let proof: RetentionProofV1 = schema::read_jcs_file(&proof_path, None)?;
        if proof.reason != reason {
            return Err("durable retention proof has a different reason".into());
        }
        return Ok((
            proof,
            sha256::hex_digest(&capability::read_secure_bytes(&proof_path)?),
        ));
    }
    let object = manifest.object()?;
    let worktree = PathBuf::from(object["worktree_root"].as_str()?);
    let branch_ref = object["branch_ref"].as_str()?.to_string();
    let head = git::run_git_text(&worktree, &["rev-parse", "--verify", "HEAD"])?;
    let bundle_path = session_dir.join("retained-work.bundle");
    if bundle_path.exists() {
        return Err("unreceipted retention bundle already exists".into());
    }
    let output = Command::new("git")
        .args(["bundle", "create"])
        .arg(&bundle_path)
        .arg(&branch_ref)
        .current_dir(&worktree)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("create retention bundle: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "create retention bundle failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    sync_file_and_parent(&bundle_path)?;
    let verify = Command::new("git")
        .args(["bundle", "verify"])
        .arg(&bundle_path)
        .current_dir(&worktree)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("verify retention bundle: {error}"))?;
    if !verify.status.success() {
        return Err(format!(
            "verify retention bundle failed: {}",
            String::from_utf8_lossy(&verify.stderr).trim()
        ));
    }
    let dirty = !git::run_git_bytes(&worktree, &["status", "--porcelain=v2", "-z"])?.is_empty();
    let dirty_artifact = if dirty {
        if reason != "abort" {
            return Err("dirty retention requires explicit abort".into());
        }
        let artifact = session_dir.join("dirty-worktree.tar");
        if artifact.exists() {
            return Err("unreceipted dirty retention artifact already exists".into());
        }
        let output = Command::new("tar")
            .args(["--format=posix", "-cf"])
            .arg(&artifact)
            .arg("-C")
            .arg(&worktree)
            .args(["--exclude=.git", "."])
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("create dirty retention artifact: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "create dirty retention artifact failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        sync_file_and_parent(&artifact)?;
        Some(hashed_file(&artifact)?)
    } else {
        None
    };
    let proof = RetentionProofV1 {
        session_id: object["session_id"].as_str()?.into(),
        branch_ref,
        head_oid: head.clone(),
        bundle: hashed_file(&bundle_path)?,
        dirty_artifact,
        reachable_oids: git::run_git_text(&worktree, &["rev-list", &head])?
            .lines()
            .map(str::to_string)
            .filter(|value| !value.is_empty())
            .collect(),
        reason: reason.into(),
        proven_at: proven_at.into(),
    };
    proof.validate()?;
    authority::atomic_create_jcs(&proof_path, &proof, 0o600)?;
    let digest = sha256::hex_digest(&capability::read_secure_bytes(&proof_path)?);
    Ok((proof, digest))
}

fn remove_revoked_worker_capability(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    capability::read_secure_bytes(path)?;
    fs::remove_file(path).map_err(|error| {
        format!(
            "remove revoked worker capability {}: {error}",
            path.display()
        )
    })?;
    if path.exists() {
        return Err("revoked worker capability remains after removal".into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "worker capability path has no parent".to_string())?;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("fsync worker capability directory: {error}"))
}

fn revoke_worker_if_needed(
    roots: &AuthorityRoots,
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    revoked_at: &str,
) -> Result<(), String> {
    let record_path = session_dir.join("worker-capability-record-v1.json");
    let record: schema::CapabilityRecordV1 = schema::read_jcs_file(&record_path, None)?;
    if record.revoked_at.is_some() {
        return Ok(());
    }
    let worker_path = PathBuf::from(manifest.object()?["worker_capability_file"].as_str()?);
    let worker: WorkerCapabilityV1 = schema::read_jcs_file(&worker_path, None)?;
    let authority = WorkspaceAuthority::new(roots.clone())?;
    let exclusion = authority.acquire_authority_exclusion(&repository.identity.repository_id)?;
    capability::revoke_worker_durable(
        &exclusion,
        &record_path,
        &worker.capability_id,
        worker
            .generation
            .parse()
            .map_err(|_| "worker capability generation overflow".to_string())?,
        revoked_at,
    )?;
    Ok(())
}

fn wait_for_durable_file(path: &Path, label: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(format!(
                "{label} was not published within fifteen seconds; work retained"
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn evidence_field(
    path: &Path,
    keys: &[&str],
    schema_name: &str,
    field: &str,
) -> Result<String, String> {
    let value = schema::parse_jcs(&capability::read_secure_bytes(path)?, true)?;
    let object = closed_object(&value, keys, schema_name)?;
    let digest = object[field].as_str()?.to_string();
    Sha256Digest::parse(&digest)?;
    Ok(digest)
}

fn cleanup_fault(session_dir: &Path, session_id: &str, point: &str) -> Result<(), String> {
    if std::env::var_os("SESSION_RELAY_TEST_CGROUP_ROOT").is_none() {
        return Ok(());
    }
    let Some(configured) = std::env::var_os("SESSION_RELAY_TEST_CLEANUP_FAULT") else {
        return Ok(());
    };
    let configured = configured
        .into_string()
        .map_err(|_| "cleanup fault selector is not UTF-8".to_string())?;
    if configured != format!("{session_id}:{point}") {
        return Ok(());
    }
    let admitted = [
        "before_capability_revoke",
        "after_capability_revoke",
        "before_empty",
        "after_empty",
        "before_resources",
        "after_resources",
        "before_git",
        "after_git",
        "before_lease_close",
        "after_lease_close",
        "before_closed",
        "after_closed",
        "before_closed_committed",
        "after_closed_committed",
    ];
    if !admitted.contains(&point) {
        return Err("cleanup fault point is outside the closed test set".into());
    }
    let marker = session_dir.join(format!("cleanup-fault-{point}"));
    if marker.exists() {
        capability::read_secure_bytes(&marker)?;
        return Ok(());
    }
    write_private_bytes(&marker, b"fault\n")?;
    Err(format!("injected cleanup fault at {point}"))
}

fn prepare_cleanup_evidence(
    roots: &AuthorityRoots,
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    now: &str,
) -> Result<(String, String), String> {
    let session_id = manifest.object()?["session_id"].as_str()?;
    cleanup_fault(session_dir, session_id, "before_capability_revoke")?;
    revoke_worker_if_needed(roots, repository, session_dir, manifest, now)?;
    cleanup_fault(session_dir, session_id, "after_capability_revoke")?;
    let empty_path = session_dir.join("custody-empty-v1.json");
    let broker_close_path = session_dir.join("broker-close-v1.json");
    cleanup_fault(session_dir, session_id, "before_empty")?;
    wait_for_durable_file(&broker_close_path, "broker close proof")?;
    if !empty_path.exists() {
        if dead_custody_identities(session_dir, session_id)?.is_some() {
            recover_dead_custody_empty(session_dir, session_id)?;
        } else {
            runtime_exchange(session_dir, "terminate", None)?;
        }
    }
    wait_for_durable_file(&empty_path, "custody EMPTY proof")?;
    cleanup_fault(session_dir, session_id, "after_empty")?;
    let broker_close_sha256 = evidence_field(
        &broker_close_path,
        &[
            "schema",
            "session_id",
            "capability_id",
            "revoked_at",
            "evidence_sha256",
        ],
        "BrokerCloseV1",
        "evidence_sha256",
    )?;
    let runtime_empty_sha256 = evidence_field(
        &empty_path,
        &["schema", "empty_sha256", "mode"],
        "CustodyEmptyV1",
        "empty_sha256",
    )?;
    let worker_path = PathBuf::from(manifest.object()?["worker_capability_file"].as_str()?);
    remove_revoked_worker_capability(&worker_path)?;
    Ok((runtime_empty_sha256, broker_close_sha256))
}

fn release_cleanup_resources(
    roots: &AuthorityRoots,
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    runtime_empty_sha256: &str,
    broker_close_sha256: &str,
    now: &str,
) -> Result<Vec<String>, String> {
    let session_id = manifest.object()?["session_id"].as_str()?;
    cleanup_fault(session_dir, session_id, "before_resources")?;
    let mut receipts = resources::release_resources(
        roots,
        &repository.identity.repository_id,
        session_id,
        &session_dir.join("resources"),
        &resources::ResourceReleaseEvidenceV1 {
            broker_close_sha256: broker_close_sha256.into(),
            runtime_empty_sha256: runtime_empty_sha256.into(),
        },
        now,
    )?;
    receipts.sort();
    receipts.dedup();
    cleanup_fault(session_dir, session_id, "after_resources")?;
    Ok(receipts)
}

fn lease_identity(session_dir: &Path) -> Result<(PathBuf, LeaseIdentity), String> {
    let lease_value = schema::parse_jcs(
        &capability::read_secure_bytes(&session_dir.join("lease-identity-v1.json"))?,
        true,
    )?;
    let lease = closed_object(
        &lease_value,
        &["schema", "path", "device", "inode"],
        "WorkspaceLeaseIdentityV1",
    )?;
    Ok((
        PathBuf::from(lease["path"].as_str()?),
        LeaseIdentity {
            device: lease["device"]
                .as_str()?
                .parse()
                .map_err(|_| "lease device overflow".to_string())?,
            inode: lease["inode"]
                .as_str()?
                .parse()
                .map_err(|_| "lease inode overflow".to_string())?,
        },
    ))
}

fn cleanup_expected_worker_head(
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    state: WorkspaceState,
) -> Result<String, String> {
    if state == WorkspaceState::AbortedRetained {
        let proof: RetentionProofV1 =
            schema::read_jcs_file(&session_dir.join("retention-proof-v1.json"), None)?;
        Ok(proof.head_oid)
    } else {
        manifest_produced_head(manifest)
    }
}

fn validate_retained_abort_cleanup(
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    expected_worker_head: &str,
    retention_sha256: &str,
) -> Result<(), String> {
    let object = manifest.object()?;
    let worker_root = PathBuf::from(object["worktree_root"].as_str()?);
    let worker_branch_ref = object["branch_ref"].as_str()?;
    let proof_path = session_dir.join("retention-proof-v1.json");
    let proof: RetentionProofV1 = schema::read_jcs_file(&proof_path, None)?;
    let durable_sha256 = sha256::hex_digest(&capability::read_secure_bytes(&proof_path)?);
    if durable_sha256 != retention_sha256
        || proof.session_id != object["session_id"].as_str()?
        || proof.branch_ref != worker_branch_ref
        || proof.head_oid != expected_worker_head
        || proof.reason != "abort"
    {
        return Err(
            "retained abort proof differs from the exact workspace identity or HEAD".into(),
        );
    }
    if hashed_file(Path::new(&proof.bundle.path))? != proof.bundle {
        return Err("retained abort bundle differs from its durable proof".into());
    }
    if let Some(artifact) = &proof.dirty_artifact {
        if hashed_file(Path::new(&artifact.path))? != *artifact {
            return Err("retained dirty-worktree artifact differs from its durable proof".into());
        }
    }
    let expected_identity =
        schema::WorktreeIdentityV1::from_value(object["worktree_identity"].clone())?;
    let actual_identity = git::verify_retained_worktree(
        repository,
        &worker_root,
        worker_branch_ref,
        expected_worker_head,
        retention_sha256,
    )?;
    if actual_identity != expected_identity {
        return Err("retained abort worktree identity differs from the durable manifest".into());
    }
    Ok(())
}

struct CleanupGitContext<'a> {
    state: WorkspaceState,
    expected_worker_head: &'a str,
    retention_sha256: Option<&'a str>,
    expected_integration_checkout_head: Option<&'a str>,
}

struct CleanupIntentContext<'a> {
    request_id: &'a str,
    git: &'a CleanupGitContext<'a>,
    runtime_empty_sha256: &'a str,
    broker_close_sha256: &'a str,
    created_at: &'a str,
}

fn cleanup_git_outcome(
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    context: &CleanupGitContext<'_>,
    execute: bool,
) -> Result<(bool, bool), String> {
    let state = context.state;
    let expected_worker_head = context.expected_worker_head;
    let retention_sha256 = context.retention_sha256;
    let expected_integration_checkout_head = context.expected_integration_checkout_head;
    let object = manifest.object()?;
    let worker_root = PathBuf::from(object["worktree_root"].as_str()?);
    let worker_branch_ref = object["branch_ref"].as_str()?;
    match state {
        WorkspaceState::Integrated => {
            if worker_root.exists() {
                let expected_identity =
                    schema::WorktreeIdentityV1::from_value(object["worktree_identity"].clone())?;
                if git::worktree_identity(&worker_root, worker_branch_ref)? != expected_identity {
                    return Err(
                        "integrated cleanup worktree identity differs from the durable manifest"
                            .into(),
                    );
                }
            } else if manifest.state()? != WorkspaceState::Releasing {
                return Err("integrated worktree disappeared before cleanup intent".into());
            }
            let integration_receipt: IntegrationReceiptV1 =
                schema::read_jcs_file(&integration_receipt_path(session_dir), None)?;
            let integration_branch =
                git::run_git_text(&repository.root, &["symbolic-ref", "-q", "HEAD"])?;
            let ordered = ordered_request_from_manifest(
                manifest,
                &repository.root,
                &integration_branch,
                &integration_receipt.pre_integration_head,
            )?;
            let proof = git::GitCleanupProof::Integrated(git::CoordinatorIntegrationResult {
                outcome: git::CoordinatorIntegrationOutcome::Integrated,
                pre_head: integration_receipt.pre_integration_head,
                post_head: integration_receipt.post_integration_head,
                output_oids: integration_receipt.integration_commits,
                conflict_paths: Vec::new(),
            });
            let checkout_head = expected_integration_checkout_head.ok_or_else(|| {
                "integrated cleanup requires the exact current integration HEAD".to_string()
            })?;
            if execute {
                git::cleanup_integrated_worktree_at(repository, &ordered, &proof, checkout_head)?;
            } else {
                git::verify_integrated_cleanup_at(repository, &ordered, &proof, checkout_head)?;
            }
            Ok((true, true))
        }
        WorkspaceState::Rejected => {
            let retention = retention_sha256
                .ok_or_else(|| "rejected cleanup requires retention proof".to_string())?;
            if execute {
                git::cleanup_retained_worktree(
                    repository,
                    &worker_root,
                    worker_branch_ref,
                    expected_worker_head,
                    retention,
                )?;
            } else {
                git::verify_retained_worktree(
                    repository,
                    &worker_root,
                    worker_branch_ref,
                    expected_worker_head,
                    retention,
                )?;
            }
            Ok((true, true))
        }
        WorkspaceState::AbortedRetained => {
            let retention = retention_sha256
                .ok_or_else(|| "retained abort requires retention proof".to_string())?;
            validate_retained_abort_cleanup(
                repository,
                session_dir,
                manifest,
                expected_worker_head,
                retention,
            )?;
            let proof: RetentionProofV1 =
                schema::read_jcs_file(&session_dir.join("retention-proof-v1.json"), None)?;
            let preserves_user_work = proof.dirty_artifact.is_some()
                || proof.head_oid != object["applied_wip_commit"].as_str()?;
            if execute && !preserves_user_work {
                git::cleanup_retained_worktree(
                    repository,
                    &worker_root,
                    worker_branch_ref,
                    expected_worker_head,
                    retention,
                )?;
            }
            Ok((!preserves_user_work, !preserves_user_work))
        }
        _ => Err("cleanup is outside the settled outcome states".into()),
    }
}

fn jcs_value_sha256(value: &JcsValue) -> String {
    sha256::hex_digest(schema::serialize_jcs(value).as_bytes())
}

fn cleanup_intent_value(
    repository: &git::OpenedRepository,
    manifest: &WorkspaceManifestRecord,
    context: &CleanupIntentContext<'_>,
) -> Result<CleanupIntentV1, String> {
    let request_id = context.request_id;
    let state = context.git.state;
    let expected_worker_head = context.git.expected_worker_head;
    let retention_sha256 = context.git.retention_sha256;
    let expected_integration_checkout_head = context.git.expected_integration_checkout_head;
    let runtime_empty_sha256 = context.runtime_empty_sha256;
    let broker_close_sha256 = context.broker_close_sha256;
    let created_at = context.created_at;
    let object = manifest.object()?;
    let (journal_sequence, journal_head) = manifest.journal_position()?;
    CleanupIntentV1::from_jcs(JcsValue::Object(BTreeMap::from([
        (
            "branch_ref".into(),
            JcsValue::String(object["branch_ref"].as_str()?.into()),
        ),
        (
            "broker_close_sha256".into(),
            JcsValue::String(broker_close_sha256.into()),
        ),
        ("created_at".into(), JcsValue::String(created_at.into())),
        (
            "custody_empty_sha256".into(),
            JcsValue::String(runtime_empty_sha256.into()),
        ),
        (
            "expected_integration_checkout_head".into(),
            expected_integration_checkout_head
                .map(|value| JcsValue::String(value.into()))
                .unwrap_or(JcsValue::Null),
        ),
        (
            "expected_worker_head".into(),
            JcsValue::String(expected_worker_head.into()),
        ),
        (
            "repository_id".into(),
            JcsValue::String(repository.identity.repository_id.clone()),
        ),
        ("request_id".into(), JcsValue::String(request_id.into())),
        (
            "resources_sha256".into(),
            JcsValue::String(jcs_value_sha256(&object["resources"])),
        ),
        (
            "retention_sha256".into(),
            retention_sha256
                .map(|value| JcsValue::String(value.into()))
                .unwrap_or(JcsValue::Null),
        ),
        (
            "schema".into(),
            JcsValue::String("WorkspaceCleanupIntentV1".into()),
        ),
        (
            "session_id".into(),
            JcsValue::String(object["session_id"].as_str()?.into()),
        ),
        (
            "source_journal_head_sha256".into(),
            JcsValue::String(
                journal_head
                    .ok_or_else(|| "cleanup source journal has no durable head".to_string())?,
            ),
        ),
        (
            "source_journal_sequence".into(),
            JcsValue::String(journal_sequence.to_string()),
        ),
        (
            "source_state".into(),
            JcsValue::String(state.as_str().into()),
        ),
        (
            "worktree_identity_sha256".into(),
            JcsValue::String(jcs_value_sha256(&object["worktree_identity"])),
        ),
        (
            "worktree_root".into(),
            JcsValue::String(object["worktree_root"].as_str()?.into()),
        ),
    ])))
}

fn verify_cleanup_intent(
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    intent: &CleanupIntentV1,
    context: &CleanupIntentContext<'_>,
) -> Result<(), String> {
    let request_id = context.request_id;
    let expected_state = context.git.state;
    let expected_worker_head = context.git.expected_worker_head;
    let retention_sha256 = context.git.retention_sha256;
    let expected_integration_checkout_head = context.git.expected_integration_checkout_head;
    let runtime_empty_sha256 = context.runtime_empty_sha256;
    let broker_close_sha256 = context.broker_close_sha256;
    let created_at = context.created_at;
    let object = intent.object()?;
    let manifest_object = manifest.object()?;
    let nullable = |field: &str| -> Result<Option<&str>, String> {
        match &object[field] {
            JcsValue::Null => Ok(None),
            JcsValue::String(value) => Ok(Some(value)),
            _ => Err(format!("cleanup intent {field} has invalid nullability")),
        }
    };
    if object["request_id"].as_str()? != request_id
        || object["session_id"].as_str()? != manifest_object["session_id"].as_str()?
        || object["repository_id"].as_str()? != repository.identity.repository_id
        || WorkspaceState::parse(object["source_state"].as_str()?)? != expected_state
        || object["worktree_root"].as_str()? != manifest_object["worktree_root"].as_str()?
        || object["branch_ref"].as_str()? != manifest_object["branch_ref"].as_str()?
        || object["expected_worker_head"].as_str()? != expected_worker_head
        || object["worktree_identity_sha256"].as_str()?
            != jcs_value_sha256(&manifest_object["worktree_identity"])
        || object["resources_sha256"].as_str()? != jcs_value_sha256(&manifest_object["resources"])
        || nullable("retention_sha256")? != retention_sha256
        || nullable("expected_integration_checkout_head")? != expected_integration_checkout_head
        || object["custody_empty_sha256"].as_str()? != runtime_empty_sha256
        || object["broker_close_sha256"].as_str()? != broker_close_sha256
        || object["created_at"].as_str()? != created_at
    {
        return Err(
            "cleanup intent differs from the exact coordinator CAS or cleanup proof".into(),
        );
    }
    let source_sequence = object["source_journal_sequence"]
        .as_str()?
        .parse::<u64>()
        .map_err(|_| "cleanup intent journal sequence overflows u64".to_string())?;
    let releasing_sequence = source_sequence
        .checked_add(1)
        .ok_or_else(|| "cleanup intent journal sequence exhausted".to_string())?;
    let event_path = session_dir
        .join("journal")
        .join(format!("{releasing_sequence:020}.json"));
    if manifest.state()? == WorkspaceState::Releasing {
        let position = authority::read_journal_position(session_dir)?;
        let (manifest_sequence, manifest_head) = manifest.journal_position()?;
        if position.sequence != manifest_sequence || position.head != manifest_head {
            return Err("Releasing manifest differs from its durable journal position".into());
        }
        let event: JournalEventV1 = schema::read_jcs_file(&event_path, None)?;
        let payload = closed_object(
            &event.payload,
            &["cleanup_intent_sha256"],
            "Releasing cleanup payload",
        )?;
        let intent_sha256 = schema::jcs_sha256(intent);
        if event.sequence != releasing_sequence
            || event.previous_sha256.as_deref()
                != Some(object["source_journal_head_sha256"].as_str()?)
            || event.kind != "Releasing"
            || payload["cleanup_intent_sha256"].as_str()? != intent_sha256
            || manifest_sequence != releasing_sequence
            || manifest_head.as_deref() != Some(schema::jcs_sha256(&event).as_str())
        {
            return Err("Releasing state is not bound to the exact cleanup intent".into());
        }
    }
    Ok(())
}

fn publish_cleanup_intent(
    session_dir: &Path,
    manifest_path: &Path,
    intent: &CleanupIntentV1,
    expected_state: WorkspaceState,
    created_at: &str,
) -> Result<WorkspaceManifestRecord, String> {
    let path = session_dir.join("cleanup-intent-v1.json");
    if path.exists() {
        let existing: CleanupIntentV1 = schema::read_jcs_file(&path, None)?;
        if existing != *intent {
            return Err("durable cleanup intent differs from the exact coordinator CAS".into());
        }
    } else {
        authority::atomic_create_jcs(&path, intent, 0o600)?;
    }
    let digest = sha256::hex_digest(&capability::read_secure_bytes(&path)?);
    mutate_manifest_event(
        ManifestEventContext {
            session_dir,
            manifest_file: manifest_path,
            expected: expected_state,
            next: WorkspaceState::Releasing,
            kind: "Releasing",
            created_at,
        },
        JcsValue::Object(BTreeMap::from([(
            "cleanup_intent_sha256".into(),
            JcsValue::String(digest),
        )])),
        |_| Ok(()),
    )
}

fn validate_lifecycle_cas(
    manifest: &WorkspaceManifestRecord,
    session_id: &str,
    expected_state: WorkspaceState,
    expected_journal_head: &str,
) -> Result<(), String> {
    let object = manifest.object()?;
    if object["session_id"].as_str()? != session_id
        || manifest.state()? != expected_state
        || object["journal_head_sha256"].as_str()? != expected_journal_head
    {
        return Err("coordinator lifecycle CAS differs from the durable manifest".into());
    }
    Ok(())
}

fn verify_retained_abort_event(
    session_dir: &Path,
    sequence: u64,
    head_sha256: &str,
    expected_previous_sha256: &str,
    expected_reason: &str,
) -> Result<String, String> {
    let event_path = session_dir
        .join("journal")
        .join(format!("{sequence:020}.json"));
    let event: JournalEventV1 = schema::read_jcs_file(&event_path, None)?;
    let payload = closed_object(
        &event.payload,
        &["reason", "retention_sha256"],
        "AbortedRetained payload",
    )?;
    let retention_sha256 = payload["retention_sha256"].as_str()?.to_string();
    Sha256Digest::parse(&retention_sha256)?;
    if event.sequence != sequence
        || event.previous_sha256.as_deref() != Some(expected_previous_sha256)
        || event.kind != "AbortedRetained"
        || payload["reason"].as_str()? != expected_reason
        || schema::jcs_sha256(&event) != head_sha256
    {
        return Err("AbortedRetained replay differs from the original coordinator CAS".into());
    }
    let proof_path = session_dir.join("retention-proof-v1.json");
    if sha256::hex_digest(&capability::read_secure_bytes(&proof_path)?) != retention_sha256 {
        return Err("AbortedRetained replay differs from its durable retention proof".into());
    }
    Ok(retention_sha256)
}

fn verify_intermediate_retained_abort(
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    expected_previous_sha256: &str,
    expected_reason: &str,
) -> Result<String, String> {
    if manifest.state()? != WorkspaceState::AbortedRetained {
        return Err("retained-abort replay requires AbortedRetained".into());
    }
    let position = authority::read_journal_position(session_dir)?;
    let (sequence, head) = manifest.journal_position()?;
    if position.sequence != sequence || position.head != head {
        return Err("AbortedRetained manifest differs from its durable journal position".into());
    }
    let head = head.ok_or_else(|| "AbortedRetained manifest has no journal head".to_string())?;
    verify_retained_abort_event(
        session_dir,
        sequence,
        &head,
        expected_previous_sha256,
        expected_reason,
    )
}

fn process_identity_gone(pid: libc::pid_t, start_token: &str) -> Result<bool, String> {
    let proc_path = PathBuf::from(format!("/proc/{pid}"));
    if !proc_path.exists() {
        return Ok(true);
    }
    match platform::linux::process_start_token(pid) {
        Ok(actual) if actual == start_token => {
            let Some(pidfd) = platform::linux::pidfd_open_existing(pid)? else {
                return Ok(true);
            };
            match platform::linux::process_start_token(pid) {
                Ok(confirmed) if confirmed == start_token => {
                    let identity = platform::linux::ProcessIdentity {
                        pid,
                        pidfd,
                        start_token: start_token.to_string(),
                    };
                    Ok(!platform::linux::pidfd_is_live(&identity)?)
                }
                Ok(_) => Ok(true),
                Err(_error) if !proc_path.exists() => Ok(true),
                Err(error) => Err(format!("confirm process {pid} identity: {error}")),
            }
        }
        Ok(_) => Ok(true),
        Err(_error) if !proc_path.exists() => Ok(true),
        Err(error) => Err(format!("prove process {pid} identity gone: {error}")),
    }
}

fn closed_custody_identities(
    session_dir: &Path,
    session_id: &str,
) -> Result<Vec<(libc::pid_t, String)>, String> {
    let path = session_dir.join("custody-active-v1.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let value = schema::parse_jcs(&capability::read_secure_bytes(&path)?, true)?;
    let object = closed_object(
        &value,
        &[
            "schema",
            "session_id",
            "backend",
            "cgroup_membership",
            "guardian_pid",
            "guardian_start_token",
            "supervisor_pid",
            "supervisor_start_token",
            "prepared_sha256",
            "activated_sha256",
        ],
        "CustodyActiveV1",
    )?;
    if object["schema"].as_str()? != "CustodyActiveV1"
        || object["session_id"].as_str()? != session_id
    {
        return Err("custody active identity differs from the Closed workspace".into());
    }
    let parse_pid = |field: &str| {
        object[field]
            .as_str()?
            .parse::<libc::pid_t>()
            .map_err(|_| format!("{field} overflows pid_t"))
    };
    Ok(vec![
        (
            parse_pid("guardian_pid")?,
            object["guardian_start_token"].as_str()?.into(),
        ),
        (
            parse_pid("supervisor_pid")?,
            object["supervisor_start_token"].as_str()?.into(),
        ),
    ])
}

fn dead_custody_identities(
    session_dir: &Path,
    session_id: &str,
) -> Result<Option<Vec<(libc::pid_t, String)>>, String> {
    let identities = closed_custody_identities(session_dir, session_id)?;
    if identities.len() != 2 {
        return Ok(None);
    }
    for (pid, start_token) in &identities {
        if !process_identity_gone(*pid, start_token)? {
            return Ok(None);
        }
    }
    Ok(Some(identities))
}

fn recover_dead_custody_empty(session_dir: &Path, session_id: &str) -> Result<String, String> {
    if dead_custody_identities(session_dir, session_id)?.is_none() {
        return Err(
            "custody runtime is unavailable without proof that both custodians died".into(),
        );
    }
    let cgroup = platform::linux::DelegatedCgroup::open_existing(session_id)?;
    let empty = cgroup.fence_and_wait_empty()?;
    persist_runtime_record(
        session_dir,
        "custody-empty-v1.json",
        JcsValue::Object(BTreeMap::from([
            (
                "empty_sha256".into(),
                JcsValue::String(empty.evidence_sha256),
            ),
            (
                "mode".into(),
                JcsValue::String("coordinator_dual_fault_recovery".into()),
            ),
            ("schema".into(), JcsValue::String("CustodyEmptyV1".into())),
        ])),
    )
}

fn close_dead_custody_leases(session_dir: &Path, session_id: &str) -> Result<String, String> {
    let identities = dead_custody_identities(session_dir, session_id)?.ok_or_else(|| {
        "custody lease close requires proof that both custodians died".to_string()
    })?;
    let empty_sha256 = evidence_field(
        &session_dir.join("custody-empty-v1.json"),
        &["schema", "empty_sha256", "mode"],
        "CustodyEmptyV1",
        "empty_sha256",
    )?;
    let broker_close_sha256 = evidence_field(
        &session_dir.join("broker-close-v1.json"),
        &[
            "schema",
            "session_id",
            "capability_id",
            "revoked_at",
            "evidence_sha256",
        ],
        "BrokerCloseV1",
        "evidence_sha256",
    )?;
    let close_digest = |role: &str, identity: &(libc::pid_t, String)| {
        sha256::hex_digest(
            format!(
                "custody-dead-lease-close-v1\0{session_id}\0{role}\0{}\0{}\0{empty_sha256}\0{broker_close_sha256}",
                identity.0, identity.1,
            )
            .as_bytes(),
        )
    };
    persist_runtime_record(
        session_dir,
        "custody-lease-closed-v1.json",
        JcsValue::Object(BTreeMap::from([
            (
                "guardian_close_sha256".into(),
                JcsValue::String(close_digest("guardian", &identities[0])),
            ),
            (
                "supervisor_close_sha256".into(),
                JcsValue::String(close_digest("supervisor", &identities[1])),
            ),
            (
                "schema".into(),
                JcsValue::String("CustodyLeaseClosedV1".into()),
            ),
        ])),
    )
}

fn remove_stale_custody_socket(socket: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "inspect stale custody command socket {}: {error}",
                socket.display()
            ));
        }
    };
    if metadata.file_type().is_symlink()
        || metadata.mode() & libc::S_IFMT != libc::S_IFSOCK
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err("refuse to remove unproven stale custody command socket".into());
    }
    fs::remove_file(socket)
        .map_err(|error| format!("remove stale custody command socket: {error}"))?;
    let parent = socket
        .parent()
        .ok_or_else(|| "custody command socket has no parent".to_string())?;
    fs::remove_dir(parent)
        .map_err(|error| format!("remove stale custody command socket directory: {error}"))
}

fn reconcile_closed_custody(
    roots: &AuthorityRoots,
    repository: &git::OpenedRepository,
    session_dir: &Path,
    session_id: &str,
    receipt_sha256: &str,
) -> Result<(), String> {
    let _ = roots;
    cleanup_fault(session_dir, session_id, "before_closed_committed")?;
    let identities = closed_custody_identities(session_dir, session_id)?;
    let guardian_gone = match identities.first() {
        Some((pid, token)) => process_identity_gone(*pid, token)?,
        None => true,
    };
    let socket = custody_socket_path(session_dir);
    if socket.exists() {
        if guardian_gone {
            remove_stale_custody_socket(&socket)?;
        } else {
            runtime_exchange(session_dir, "closed_committed", Some(receipt_sha256))?;
        }
    }
    let broker_socket = broker_socket_location(
        unsafe { libc::geteuid() },
        &repository.identity.repository_id,
        session_id,
    );
    let cgroup = platform::linux::delegated_root()?.join(session_id);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let processes_gone = identities
            .iter()
            .map(|(pid, token)| process_identity_gone(*pid, token))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .all(|gone| gone);
        if processes_gone && !socket.exists() && !broker_socket.exists() && cgroup.exists() {
            platform::linux::reconcile_empty_delegated_cgroup(session_id)?;
        }
        if !socket.exists() && !broker_socket.exists() && !cgroup.exists() && processes_gone {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Closed custody did not prove guardian, supervisor, sockets, and cgroup gone: \
                 processes_gone={processes_gone}, custody_socket_exists={}, \
                 broker_socket_exists={}, cgroup_exists={}",
                socket.exists(),
                broker_socket.exists(),
                cgroup.exists()
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
    cleanup_fault(session_dir, session_id, "after_closed_committed")
}

fn closed_cleanup_replay(
    roots: &AuthorityRoots,
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    request_id: &str,
    session_id: &str,
) -> Result<Option<CleanupReceiptV1>, String> {
    if manifest.state()? != WorkspaceState::Closed {
        return Ok(None);
    }
    let receipt_path = session_dir.join("cleanup-receipt-v1.json");
    let receipt: CleanupReceiptV1 = schema::read_jcs_file(&receipt_path, None)?;
    if receipt.request_id != request_id || receipt.session_id != session_id {
        return Err("cleanup receipt belongs to a different coordinator request".into());
    }
    let receipt_sha256 = sha256::hex_digest(&capability::read_secure_bytes(&receipt_path)?);
    reconcile_closed_custody(roots, repository, session_dir, session_id, &receipt_sha256)?;
    Ok(Some(receipt))
}

#[derive(Clone, Copy)]
struct CleanupContext<'a> {
    roots: &'a AuthorityRoots,
    repository: &'a git::OpenedRepository,
    session_dir: &'a Path,
    request_id: &'a str,
    created_at: &'a str,
}

struct ClosedManifestCommitContext<'a> {
    roots: &'a AuthorityRoots,
    gate: &'a repository_gate::RepositoryGate,
    exclusion: &'a authority::AuthorityExclusionLock,
    probe: &'a WorkspaceLeaseProbe,
    session_dir: &'a Path,
    manifest_path: &'a Path,
    intent_sha256: &'a str,
    receipt_sha256: &'a str,
    custody_empty_sha256: &'a str,
    created_at: &'a str,
}

fn commit_closed_manifest(context: ClosedManifestCommitContext<'_>) -> Result<(), String> {
    let ClosedManifestCommitContext {
        roots,
        gate,
        exclusion,
        probe,
        session_dir,
        manifest_path,
        intent_sha256,
        receipt_sha256,
        custody_empty_sha256,
        created_at,
    } = context;
    schema::Timestamp::parse(created_at)?;
    let lock = WorkspaceJournalLock::acquire(session_dir)?;
    let current = read_manifest(manifest_path)?;
    if current.state()? != WorkspaceState::Releasing {
        return Err("cleanup state changed before Closed publication".into());
    }
    let (sequence, head) = current.journal_position()?;
    let event = JournalEventV1 {
        sequence: sequence
            .checked_add(1)
            .ok_or_else(|| "workspace journal sequence exhausted".to_string())?,
        previous_sha256: head.clone(),
        kind: "Closed".into(),
        payload: JcsValue::Object(BTreeMap::from([
            (
                "cleanup_intent_sha256".into(),
                JcsValue::String(intent_sha256.into()),
            ),
            (
                "cleanup_receipt_sha256".into(),
                JcsValue::String(receipt_sha256.into()),
            ),
        ])),
        created_at: created_at.into(),
    };
    let next_head = authority::append_journal_cas(&lock, &event, sequence, head.as_deref())?;
    let mut replacement = current.clone();
    {
        let object = replacement.object_mut()?;
        object.insert(
            "state".into(),
            JcsValue::String(WorkspaceState::Closed.as_str().into()),
        );
        object.insert(
            "journal_sequence".into(),
            JcsValue::String(event.sequence.to_string()),
        );
        object.insert("journal_head_sha256".into(), JcsValue::String(next_head));
        object.insert("updated_at".into(), JcsValue::String(created_at.into()));
        object.insert(
            "custody_evidence".into(),
            JcsValue::Object(BTreeMap::from([
                (
                    "active_sha256".into(),
                    object["custody_evidence"].clone().object()?["active_sha256"].clone(),
                ),
                (
                    "empty_sha256".into(),
                    JcsValue::String(custody_empty_sha256.into()),
                ),
            ])),
        );
    }
    WorkspaceManifestRecord::from_jcs(replacement.to_jcs())?;
    authority::commit_closed_manifest_cas(authority::ClosedManifestCas {
        gate,
        roots,
        exclusion,
        probe,
        lock: &lock,
        path: manifest_path,
        expected_state: WorkspaceState::Releasing.as_str(),
        expected_sequence: sequence,
        expected_head: head.as_deref(),
        replacement: &replacement,
    })
}

fn finalize_closed(
    context: CleanupContext<'_>,
    expected_state: WorkspaceState,
    retention_sha256: Option<String>,
    expected_integration_checkout_head: Option<&str>,
) -> Result<CleanupReceiptV1, String> {
    let CleanupContext {
        roots,
        repository,
        session_dir,
        request_id,
        created_at,
    } = context;
    let manifest_path = session_dir.join("manifest-v1.json");
    let manifest: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    let session_id = manifest.object()?["session_id"].as_str()?.to_string();
    if let Some(receipt) = closed_cleanup_replay(
        roots,
        repository,
        session_dir,
        &manifest,
        request_id,
        &session_id,
    )? {
        return Ok(receipt);
    }
    if manifest.state()? != expected_state && manifest.state()? != WorkspaceState::Releasing {
        return Err("cleanup state differs from the exact settled outcome or intent".into());
    }

    if manifest.state()? == WorkspaceState::Integrated {
        let expected_worker_head =
            cleanup_expected_worker_head(session_dir, &manifest, WorkspaceState::Integrated)?;
        cleanup_git_outcome(
            repository,
            session_dir,
            &manifest,
            &CleanupGitContext {
                state: WorkspaceState::Integrated,
                expected_worker_head: &expected_worker_head,
                retention_sha256: retention_sha256.as_deref(),
                expected_integration_checkout_head,
            },
            false,
        )?;
    }

    let (custody_empty_sha256, broker_close_sha256) =
        prepare_cleanup_evidence(roots, repository, session_dir, &manifest, created_at)?;
    let authority = WorkspaceAuthority::new(roots.clone())?;
    let queue = authority.acquire_integration_queue(&repository.identity.repository_id)?;
    let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
    let locked: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    if locked.state()? != expected_state && locked.state()? != WorkspaceState::Releasing {
        return Err("cleanup state changed while acquiring integration locks".into());
    }
    let retention_sha256 = if locked.state()? == expected_state
        && expected_state == WorkspaceState::Rejected
        && retention_sha256.is_none()
    {
        Some(create_retention_proof(session_dir, &locked, "rejected", created_at)?.1)
    } else {
        retention_sha256
    };
    repository.validate_unchanged()?;
    let expected_worker_head = cleanup_expected_worker_head(session_dir, &locked, expected_state)?;
    repository.validate_oid(&expected_worker_head)?;
    let cleanup_git_context = CleanupGitContext {
        state: expected_state,
        expected_worker_head: &expected_worker_head,
        retention_sha256: retention_sha256.as_deref(),
        expected_integration_checkout_head,
    };
    let cleanup_intent_context = CleanupIntentContext {
        request_id,
        git: &cleanup_git_context,
        runtime_empty_sha256: &custody_empty_sha256,
        broker_close_sha256: &broker_close_sha256,
        created_at,
    };

    let intent = if locked.state()? == expected_state {
        cleanup_git_outcome(
            repository,
            session_dir,
            &locked,
            &cleanup_git_context,
            false,
        )?;
        let candidate = cleanup_intent_value(repository, &locked, &cleanup_intent_context)?;
        let releasing = publish_cleanup_intent(
            session_dir,
            &manifest_path,
            &candidate,
            expected_state,
            created_at,
        )?;
        verify_cleanup_intent(
            repository,
            session_dir,
            &releasing,
            &candidate,
            &cleanup_intent_context,
        )?;
        candidate
    } else {
        let durable: CleanupIntentV1 =
            schema::read_jcs_file(&session_dir.join("cleanup-intent-v1.json"), None)?;
        verify_cleanup_intent(
            repository,
            session_dir,
            &locked,
            &durable,
            &cleanup_intent_context,
        )?;
        durable
    };
    let intent_object = intent.object()?;

    let resource_receipts = release_cleanup_resources(
        roots,
        repository,
        session_dir,
        &locked,
        &custody_empty_sha256,
        &broker_close_sha256,
        created_at,
    )?;
    cleanup_fault(session_dir, &session_id, "before_git")?;
    let (worktree_removed, branch_removed) =
        cleanup_git_outcome(repository, session_dir, &locked, &cleanup_git_context, true)?;
    cleanup_fault(session_dir, &session_id, "after_git")?;

    let exclusion = authority.acquire_authority_exclusion(&repository.identity.repository_id)?;
    cleanup_fault(session_dir, &session_id, "before_lease_close")?;
    let lease_close_path = session_dir.join("custody-lease-closed-v1.json");
    if !lease_close_path.exists() {
        if dead_custody_identities(session_dir, &session_id)?.is_some() {
            close_dead_custody_leases(session_dir, &session_id)?;
        } else {
            runtime_exchange(session_dir, "close_lease", None)?;
        }
    }
    wait_for_durable_file(&lease_close_path, "custody lease close proof")?;
    let lease_close = schema::parse_jcs(&capability::read_secure_bytes(&lease_close_path)?, true)?;
    let lease_close = closed_object(
        &lease_close,
        &["schema", "guardian_close_sha256", "supervisor_close_sha256"],
        "CustodyLeaseClosedV1",
    )?;
    if lease_close["schema"].as_str()? != "CustodyLeaseClosedV1" {
        return Err("CustodyLeaseClosedV1 schema mismatch".into());
    }
    Sha256Digest::parse(lease_close["guardian_close_sha256"].as_str()?)?;
    Sha256Digest::parse(lease_close["supervisor_close_sha256"].as_str()?)?;
    let (lease_path, expected_lease) = lease_identity(session_dir)?;
    let probe = WorkspaceLeaseProbe::acquire(&lease_path, expected_lease)?;
    probe.revalidate()?;
    exclusion.revalidate()?;
    cleanup_fault(session_dir, &session_id, "after_lease_close")?;

    let receipt = CleanupReceiptV1 {
        request_id: request_id.into(),
        session_id: session_id.clone(),
        retention_sha256,
        resource_receipts,
        worktree_removed,
        branch_removed,
        capabilities_revoked: true,
        custody_empty_sha256,
        lease_released: true,
        outcome: "closed".into(),
        created_at: created_at.into(),
    };
    receipt.validate()?;
    let receipt_path = session_dir.join("cleanup-receipt-v1.json");
    if receipt_path.exists() {
        let durable: CleanupReceiptV1 = schema::read_jcs_file(&receipt_path, None)?;
        if durable != receipt {
            return Err("durable cleanup receipt differs from the exact cleanup intent".into());
        }
    } else {
        authority::atomic_create_jcs(&receipt_path, &receipt, 0o600)?;
    }
    let receipt_sha256 = sha256::hex_digest(&capability::read_secure_bytes(&receipt_path)?);
    cleanup_fault(session_dir, &session_id, "before_closed")?;
    probe.revalidate()?;
    exclusion.revalidate()?;
    commit_closed_manifest(ClosedManifestCommitContext {
        roots,
        gate: &gate,
        exclusion: &exclusion,
        probe: &probe,
        session_dir,
        manifest_path: &manifest_path,
        intent_sha256: &schema::jcs_sha256(&intent),
        receipt_sha256: &receipt_sha256,
        custody_empty_sha256: &receipt.custody_empty_sha256,
        created_at,
    })?;
    drop(probe);
    drop(exclusion);
    drop(gate);
    drop(queue);
    cleanup_fault(session_dir, &session_id, "after_closed")?;
    if intent_object["request_id"].as_str()? != request_id {
        return Err("cleanup intent request changed before Closed reconciliation".into());
    }
    reconcile_closed_custody(roots, repository, session_dir, &session_id, &receipt_sha256)?;
    Ok(receipt)
}

fn finish_routes_to_retained_abort(
    state: WorkspaceState,
    acknowledge_needs_user_action: bool,
) -> Result<bool, String> {
    if state == WorkspaceState::IntegrationBlocked {
        return if acknowledge_needs_user_action {
            Ok(true)
        } else {
            Err(
                "IntegrationBlocked finish requires explicit needs-user-action acknowledgement"
                    .into(),
            )
        };
    }
    if acknowledge_needs_user_action {
        return Err(
            "needs-user-action acknowledgement is invalid outside IntegrationBlocked".into(),
        );
    }
    Ok(false)
}

fn retain_abort_and_close(
    context: CleanupContext<'_>,
    manifest: &WorkspaceManifestRecord,
    expected_state: WorkspaceState,
    expected_worker_head: &str,
    reason: &str,
) -> Result<CleanupReceiptV1, String> {
    let CleanupContext {
        roots,
        repository,
        session_dir,
        request_id: _,
        created_at,
    } = context;
    let manifest_path = session_dir.join("manifest-v1.json");
    if git::run_git_text(
        Path::new(manifest.object()?["worktree_root"].as_str()?),
        &["rev-parse", "--verify", "HEAD"],
    )? != expected_worker_head
    {
        return Err("abort expected worker HEAD differs from the exact checkout".into());
    }
    revoke_worker_if_needed(roots, repository, session_dir, manifest, created_at)?;
    let session_id = manifest.object()?["session_id"].as_str()?;
    if !session_dir.join("custody-empty-v1.json").exists() {
        if dead_custody_identities(session_dir, session_id)?.is_some() {
            recover_dead_custody_empty(session_dir, session_id)?;
        } else {
            runtime_exchange(session_dir, "terminate", None)?;
        }
    }
    wait_for_durable_file(
        &session_dir.join("custody-empty-v1.json"),
        "abort custody EMPTY proof",
    )?;
    let retention = create_retention_proof(session_dir, manifest, "abort", created_at)?.1;
    mutate_manifest_event(
        ManifestEventContext {
            session_dir,
            manifest_file: &manifest_path,
            expected: expected_state,
            next: WorkspaceState::AbortedRetained,
            kind: "AbortedRetained",
            created_at,
        },
        JcsValue::Object(BTreeMap::from([
            ("reason".into(), JcsValue::String(reason.into())),
            (
                "retention_sha256".into(),
                JcsValue::String(retention.clone()),
            ),
        ])),
        |_| Ok(()),
    )?;
    finalize_closed(
        context,
        WorkspaceState::AbortedRetained,
        Some(retention),
        None,
    )
}

pub fn finish_workspace_with_roots(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: &Path,
) -> Result<CleanupReceiptV1, String> {
    let request: FinishRequestV1 = schema::read_jcs_file(request_file, Some(request_sha256))?;
    let repository = git::OpenedRepository::open(Path::new(&request.repository_path))?;
    if repository.identity.repository_id != request.repository_id {
        return Err("finish repository identity differs from its canonical path".into());
    }
    let authority = WorkspaceAuthority::new(roots.clone())?;
    authority.authenticate(
        &request.repository_id,
        coordinator_capability_file,
        "finish",
    )?;
    let session_dir = session_dir_for(roots, &request.repository_id, &request.session_id)?;
    let cleanup_context = CleanupContext {
        roots,
        repository: &repository,
        session_dir: &session_dir,
        request_id: &request.request_id,
        created_at: &request.created_at,
    };
    let manifest: WorkspaceManifestRecord =
        schema::read_jcs_file(&session_dir.join("manifest-v1.json"), None)?;
    if let Some(receipt) = closed_cleanup_replay(
        roots,
        &repository,
        &session_dir,
        &manifest,
        &request.request_id,
        &request.session_id,
    )? {
        return Ok(receipt);
    }
    if manifest.state()? == WorkspaceState::Releasing {
        let intent: CleanupIntentV1 =
            schema::read_jcs_file(&session_dir.join("cleanup-intent-v1.json"), None)?;
        let object = intent.object()?;
        let source_state = WorkspaceState::parse(object["source_state"].as_str()?)?;
        let retention = match &object["retention_sha256"] {
            JcsValue::Null => None,
            JcsValue::String(value) => Some(value.clone()),
            _ => return Err("cleanup intent retention has invalid nullability".into()),
        };
        let integration_head = match &object["expected_integration_checkout_head"] {
            JcsValue::Null => None,
            JcsValue::String(value) => Some(value.clone()),
            _ => return Err("cleanup intent integration HEAD has invalid nullability".into()),
        };
        if source_state != request.expected_state {
            if source_state != WorkspaceState::AbortedRetained
                || request.expected_state != WorkspaceState::IntegrationBlocked
                || !request.acknowledge_needs_user_action
            {
                return Err("cleanup intent source state differs from the finish request".into());
            }
            let sequence = object["source_journal_sequence"]
                .as_str()?
                .parse::<u64>()
                .map_err(|_| "cleanup intent journal sequence overflows u64".to_string())?;
            let proven = verify_retained_abort_event(
                &session_dir,
                sequence,
                object["source_journal_head_sha256"].as_str()?,
                &request.expected_journal_head_sha256,
                "acknowledged integration needs user action",
            )?;
            if retention.as_deref() != Some(proven.as_str()) {
                return Err("cleanup intent retention differs from AbortedRetained replay".into());
            }
        }
        return finalize_closed(
            cleanup_context,
            source_state,
            retention,
            integration_head.as_deref(),
        );
    }
    if manifest.state()? == WorkspaceState::AbortedRetained
        && request.expected_state == WorkspaceState::IntegrationBlocked
        && request.acknowledge_needs_user_action
    {
        let retention = verify_intermediate_retained_abort(
            &session_dir,
            &manifest,
            &request.expected_journal_head_sha256,
            "acknowledged integration needs user action",
        )?;
        return finalize_closed(
            cleanup_context,
            WorkspaceState::AbortedRetained,
            Some(retention),
            None,
        );
    }
    validate_lifecycle_cas(
        &manifest,
        &request.session_id,
        request.expected_state,
        &request.expected_journal_head_sha256,
    )?;
    let repository_head = git::run_git_text(&repository.root, &["rev-parse", "--verify", "HEAD"])?;
    if repository_head != request.expected_head {
        return Err("finish expected HEAD differs from the integration checkout".into());
    }
    if finish_routes_to_retained_abort(
        request.expected_state,
        request.acknowledge_needs_user_action,
    )? {
        let expected_worker_head = manifest_produced_head(&manifest)?;
        return retain_abort_and_close(
            cleanup_context,
            &manifest,
            WorkspaceState::IntegrationBlocked,
            &expected_worker_head,
            "acknowledged integration needs user action",
        );
    }
    let retention = if request.expected_state == WorkspaceState::Rejected {
        None
    } else if request.expected_state == WorkspaceState::AbortedRetained {
        Some(sha256::hex_digest(&capability::read_secure_bytes(
            &session_dir.join("retention-proof-v1.json"),
        )?))
    } else {
        None
    };
    finalize_closed(
        cleanup_context,
        request.expected_state,
        retention,
        Some(&request.expected_head),
    )
}

pub fn abort_workspace_with_roots(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: &Path,
) -> Result<CleanupReceiptV1, String> {
    let request: AbortRequestV1 = schema::read_jcs_file(request_file, Some(request_sha256))?;
    let repository = git::OpenedRepository::open(Path::new(&request.repository_path))?;
    if repository.identity.repository_id != request.repository_id {
        return Err("abort repository identity differs from its canonical path".into());
    }
    let authority = WorkspaceAuthority::new(roots.clone())?;
    authority.authenticate(&request.repository_id, coordinator_capability_file, "abort")?;
    let session_dir = session_dir_for(roots, &request.repository_id, &request.session_id)?;
    let cleanup_context = CleanupContext {
        roots,
        repository: &repository,
        session_dir: &session_dir,
        request_id: &request.request_id,
        created_at: &request.created_at,
    };
    let manifest_path = session_dir.join("manifest-v1.json");
    let manifest: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    if let Some(receipt) = closed_cleanup_replay(
        roots,
        &repository,
        &session_dir,
        &manifest,
        &request.request_id,
        &request.session_id,
    )? {
        return Ok(receipt);
    }
    if manifest.state()? == WorkspaceState::Releasing {
        let intent: CleanupIntentV1 =
            schema::read_jcs_file(&session_dir.join("cleanup-intent-v1.json"), None)?;
        let object = intent.object()?;
        let source_state = WorkspaceState::parse(object["source_state"].as_str()?)?;
        let retention = match &object["retention_sha256"] {
            JcsValue::Null => None,
            JcsValue::String(value) => Some(value.clone()),
            _ => return Err("cleanup intent retention has invalid nullability".into()),
        };
        let integration_head = match &object["expected_integration_checkout_head"] {
            JcsValue::Null => None,
            JcsValue::String(value) => Some(value.clone()),
            _ => return Err("cleanup intent integration HEAD has invalid nullability".into()),
        };
        if source_state != request.expected_state {
            if source_state != WorkspaceState::AbortedRetained {
                return Err("cleanup intent source state differs from the abort request".into());
            }
            let sequence = object["source_journal_sequence"]
                .as_str()?
                .parse::<u64>()
                .map_err(|_| "cleanup intent journal sequence overflows u64".to_string())?;
            let proven = verify_retained_abort_event(
                &session_dir,
                sequence,
                object["source_journal_head_sha256"].as_str()?,
                &request.expected_journal_head_sha256,
                &request.reason,
            )?;
            if retention.as_deref() != Some(proven.as_str()) {
                return Err("cleanup intent retention differs from AbortedRetained replay".into());
            }
        }
        return finalize_closed(
            cleanup_context,
            source_state,
            retention,
            integration_head.as_deref(),
        );
    }
    if manifest.state()? == WorkspaceState::AbortedRetained
        && request.expected_state != WorkspaceState::AbortedRetained
    {
        let retention = verify_intermediate_retained_abort(
            &session_dir,
            &manifest,
            &request.expected_journal_head_sha256,
            &request.reason,
        )?;
        return finalize_closed(
            cleanup_context,
            WorkspaceState::AbortedRetained,
            Some(retention),
            None,
        );
    }
    validate_lifecycle_cas(
        &manifest,
        &request.session_id,
        request.expected_state,
        &request.expected_journal_head_sha256,
    )?;
    retain_abort_and_close(
        cleanup_context,
        &manifest,
        request.expected_state,
        &request.expected_head,
        &request.reason,
    )
}

fn recovery_inspect_value(
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
) -> Result<JcsValue, String> {
    let evidence = |name: &str| -> Result<JcsValue, String> {
        let path = session_dir.join(name);
        if path.exists() {
            Ok(JcsValue::String(sha256::hex_digest(
                &capability::read_secure_bytes(&path)?,
            )))
        } else {
            Ok(JcsValue::Null)
        }
    };
    Ok(JcsValue::Object(BTreeMap::from([
        (
            "broker_close_sha256".into(),
            evidence("broker-close-v1.json")?,
        ),
        (
            "custody_active_sha256".into(),
            evidence("custody-active-v1.json")?,
        ),
        (
            "custody_empty_sha256".into(),
            evidence("custody-empty-v1.json")?,
        ),
        ("custody_status".into(), JcsValue::String("unproven".into())),
        ("manifest".into(), manifest.to_jcs()),
        (
            "retention_sha256".into(),
            evidence("retention-proof-v1.json")?,
        ),
        (
            "schema".into(),
            JcsValue::String("RecoveryInspectV1".into()),
        ),
    ])))
}

#[cfg(target_os = "linux")]
fn terminate_exact_fault_process(pid: libc::pid_t, start_token: &str) -> Result<(), String> {
    let proc_path = PathBuf::from(format!("/proc/{pid}"));
    let current = match platform::linux::process_start_token(pid) {
        Ok(token) => token,
        Err(_) if !proc_path.exists() => return Ok(()),
        Err(error) => return Err(format!("inspect retained custodian {pid}: {error}")),
    };
    if current != start_token {
        return Err(format!(
            "retained custodian {pid} start token changed; refusing PID-only recovery"
        ));
    }
    let identity = platform::linux::ProcessIdentity {
        pid,
        pidfd: platform::linux::pidfd_open(pid)?,
        start_token: start_token.to_string(),
    };
    if platform::linux::pidfd_is_live(&identity)? {
        platform::linux::signal_pidfd(&identity, libc::SIGKILL)?;
    }
    let deadline = Instant::now() + RUNTIME_EXCHANGE_DEADLINE;
    while platform::linux::pidfd_is_live(&identity)? {
        if Instant::now() >= deadline {
            return Err(format!(
                "retained custodian {pid} did not exit after explicit recovery fence"
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn reconcile_ready_start_fault_for_retry(
    roots: &AuthorityRoots,
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
) -> Result<(PathBuf, String), String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (roots, repository, session_dir, manifest);
        return Err(platform::MACOS_STOP_REASON.into());
    }
    #[cfg(target_os = "linux")]
    {
        if manifest.state()? != WorkspaceState::Ready {
            return Err("Ready retry reconciliation requires exact Ready state".into());
        }
        let value = schema::parse_jcs(
            &capability::read_secure_bytes(&session_dir.join("workspace-start-fault-v1.json"))?,
            true,
        )?;
        let object = closed_object(
            &value,
            &[
                "schema",
                "session_id",
                "repository_id",
                "phase",
                "broker_started",
                "broker_close_sha256",
                "custody_fault_sha256",
                "lease_path",
                "lease_device",
                "lease_inode",
                "relay_executable",
                "relay_executable_sha256",
                "created_at",
                "error",
                "evidence_sha256",
            ],
            "WorkspaceStartFaultV1",
        )?;
        let manifest_object = manifest.object()?;
        let session_id = manifest_object["session_id"].as_str()?;
        if object["schema"].as_str()? != "WorkspaceStartFaultV1"
            || object["session_id"].as_str()? != session_id
            || object["repository_id"].as_str()? != repository.identity.repository_id
            || manifest_object["last_error"].as_str()? != object["error"].as_str()?
        {
            return Err("Ready start fault differs from durable session authority".into());
        }
        let mut bare = object.clone();
        let evidence = bare
            .insert("evidence_sha256".into(), JcsValue::Null)
            .and_then(|value| value.as_str().ok().map(str::to_string))
            .ok_or_else(|| "Ready start fault evidence is missing".to_string())?;
        let expected = sha256::hex_digest(
            [
                b"session-relay/workspace-start-fault/v1\0".as_slice(),
                schema::serialize_jcs(&JcsValue::Object(bare)).as_bytes(),
            ]
            .concat()
            .as_slice(),
        );
        if !sha256::constant_time_eq(evidence.as_bytes(), expected.as_bytes()) {
            return Err("Ready start fault evidence digest mismatch".into());
        }
        let relay_executable = PathBuf::from(object["relay_executable"].as_str()?);
        let relay_executable_sha256 = object["relay_executable_sha256"].as_str()?.to_string();
        resources::verify_executable(&relay_executable, &relay_executable_sha256)?;
        revoke_worker_if_needed(
            roots,
            repository,
            session_dir,
            manifest,
            &authority::now_timestamp()?,
        )?;
        let worker_path = PathBuf::from(manifest_object["worker_capability_file"].as_str()?);
        let worker: WorkerCapabilityV1 = schema::read_jcs_file(&worker_path, None)?;
        let capability_record: schema::CapabilityRecordV1 =
            schema::read_jcs_file(&session_dir.join("worker-capability-record-v1.json"), None)?;
        let revoked_at = capability_record
            .revoked_at
            .ok_or_else(|| "Ready recovery worker capability is not durably revoked".to_string())?;
        let phase = object["phase"].as_str()?;
        let broker_started = match object["broker_started"] {
            JcsValue::Bool(value) => value,
            _ => return Err("Ready start fault broker_started is not a boolean".into()),
        };
        let broker_close_path = session_dir.join("broker-close-v1.json");
        if phase == "broker_start" && !broker_started {
            if Path::new(&worker.broker_socket).exists() {
                return Err("broker-start fault left a broker socket; retry remains fenced".into());
            }
            let broker_close_sha256 = sha256::hex_digest(
                format!(
                    "workspace-broker-close-v1\0{}\0{}\0{}",
                    worker.session_id, worker.capability_id, revoked_at
                )
                .as_bytes(),
            );
            persist_runtime_record(
                session_dir,
                "broker-close-v1.json",
                JcsValue::Object(BTreeMap::from([
                    (
                        "capability_id".into(),
                        JcsValue::String(worker.capability_id.clone()),
                    ),
                    (
                        "evidence_sha256".into(),
                        JcsValue::String(broker_close_sha256),
                    ),
                    ("revoked_at".into(), JcsValue::String(revoked_at.clone())),
                    ("schema".into(), JcsValue::String("BrokerCloseV1".into())),
                    (
                        "session_id".into(),
                        JcsValue::String(worker.session_id.clone()),
                    ),
                ])),
            )?;
            if custody_socket_path(session_dir).exists()
                || session_dir.join("custody-fault-v1.json").exists()
                || session_dir.join("custody-active-v1.json").exists()
            {
                return Err("broker-start fault has unexplained custody side effects".into());
            }
            let empty_sha256 = sha256::hex_digest(
                format!("custody-ready-retry-empty-v1\0{session_id}\0no-custody").as_bytes(),
            );
            persist_runtime_record(
                session_dir,
                "custody-empty-v1.json",
                JcsValue::Object(BTreeMap::from([
                    ("empty_sha256".into(), JcsValue::String(empty_sha256)),
                    (
                        "mode".into(),
                        JcsValue::String("ready_retry_no_custody".into()),
                    ),
                    ("schema".into(), JcsValue::String("CustodyEmptyV1".into())),
                ])),
            )?;
        } else if phase == "custody_start" && broker_started {
            wait_for_durable_file(&broker_close_path, "broker close proof")?;
            let custody_fault_path = session_dir.join("custody-fault-v1.json");
            let expected_fault = match &object["custody_fault_sha256"] {
                JcsValue::String(value) => value,
                _ => {
                    return Err(
                        "custody-start fault lacks exact durable fault evidence; no redispatch"
                            .into(),
                    );
                }
            };
            let custody_fault_value =
                schema::parse_jcs(&capability::read_secure_bytes(&custody_fault_path)?, true)?;
            if schema::jcs_sha256(&CanonicalRecord(custody_fault_value.clone())) != *expected_fault
            {
                return Err("custody-start fault evidence changed before recovery".into());
            }
            let fault = closed_object(
                &custody_fault_value,
                &[
                    "schema",
                    "session_id",
                    "generation",
                    "cgroup_membership",
                    "lease_device",
                    "lease_inode",
                    "guardian_pid",
                    "guardian_start_token",
                    "supervisor_pid",
                    "supervisor_start_token",
                    "code",
                    "error",
                    "evidence_sha256",
                    "empty_sha256",
                ],
                "CustodyFaultV1",
            )?;
            terminate_exact_fault_process(
                fault["guardian_pid"]
                    .as_str()?
                    .parse()
                    .map_err(|_| "retained guardian PID overflow".to_string())?,
                fault["guardian_start_token"].as_str()?,
            )?;
            terminate_exact_fault_process(
                fault["supervisor_pid"]
                    .as_str()?
                    .parse()
                    .map_err(|_| "retained supervisor PID overflow".to_string())?,
                fault["supervisor_start_token"].as_str()?,
            )?;
            let socket = custody_socket_path(session_dir);
            if socket.exists() {
                fs::remove_file(&socket)
                    .map_err(|error| format!("remove fenced custody socket: {error}"))?;
            }
            if let Some(parent) = socket.parent() {
                match fs::remove_dir(parent) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!("remove fenced custody socket directory: {error}"));
                    }
                }
            }
            platform::linux::DelegatedCgroup::create(session_id)?.remove()?;
        } else {
            return Err("Ready start fault phase and broker proof are inconsistent".into());
        }
        let broker_close_sha256 = evidence_field(
            &broker_close_path,
            &[
                "schema",
                "session_id",
                "capability_id",
                "revoked_at",
                "evidence_sha256",
            ],
            "BrokerCloseV1",
            "evidence_sha256",
        )?;
        let empty_sha256 = evidence_field(
            &session_dir.join("custody-empty-v1.json"),
            &["schema", "empty_sha256", "mode"],
            "CustodyEmptyV1",
            "empty_sha256",
        )?;
        let lease = LeaseIdentity {
            device: object["lease_device"]
                .as_str()?
                .parse()
                .map_err(|_| "Ready fault lease device overflow".to_string())?,
            inode: object["lease_inode"]
                .as_str()?
                .parse()
                .map_err(|_| "Ready fault lease inode overflow".to_string())?,
        };
        let probe = WorkspaceLeaseProbe::acquire(Path::new(object["lease_path"].as_str()?), lease)?;
        probe.revalidate()?;
        resources::release_resources(
            roots,
            &repository.identity.repository_id,
            session_id,
            &session_dir.join("resources"),
            &resources::ResourceReleaseEvidenceV1 {
                broker_close_sha256,
                runtime_empty_sha256: empty_sha256,
            },
            &authority::now_timestamp()?,
        )?;
        remove_revoked_worker_capability(&worker_path)?;
        probe.revalidate()?;
        Ok((relay_executable, relay_executable_sha256))
    }
}

fn resume_prelaunch(
    roots: &AuthorityRoots,
    repository: &git::OpenedRepository,
    session_dir: &Path,
    manifest: &WorkspaceManifestRecord,
    coordinator_capability_file: &Path,
) -> Result<WorkspaceStartResultV1, String> {
    let state = manifest.state()?;
    if !matches!(
        state,
        WorkspaceState::Reserved
            | WorkspaceState::Provisioning
            | WorkspaceState::LeaseHeld
            | WorkspaceState::Ready
    ) {
        return Err(
            "resume_prelaunch requires exact Reserved, Provisioning, LeaseHeld, or retained Ready fault proof; work retained"
                .into(),
        );
    }
    let retry_relay = if state == WorkspaceState::Ready {
        Some(reconcile_ready_start_fault_for_retry(
            roots,
            repository,
            session_dir,
            manifest,
        )?)
    } else {
        None
    };
    if state != WorkspaceState::Ready
        && (session_dir.join("custody-active-v1.json").exists()
            || custody_socket_path(session_dir).exists()
            || session_dir.join("tool-launch-v1.json").exists()
            || session_dir.join("broker-close-v1.json").exists())
    {
        return Err("resume_prelaunch found runtime/resource side effects; work retained".into());
    }
    let object = manifest.object()?;
    let worktree = PathBuf::from(object["worktree_root"].as_str()?);
    if worktree.exists() {
        let branch_ref = object["branch_ref"].as_str()?;
        let head = git::run_git_text(&worktree, &["rev-parse", "--verify", "HEAD"])?;
        if matches!(state, WorkspaceState::LeaseHeld | WorkspaceState::Ready)
            && object["worker_base_commit"].as_str()? != head
        {
            return Err("prelaunch worker HEAD drifted; work retained".into());
        }
        let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
        git::cleanup_prelaunch_worktree(repository, &worktree, branch_ref, &head)?;
        drop(gate);
    }
    if session_dir.join("lease-identity-v1.json").exists() {
        let lease_value = schema::parse_jcs(
            &capability::read_secure_bytes(&session_dir.join("lease-identity-v1.json"))?,
            true,
        )?;
        let lease = closed_object(
            &lease_value,
            &["schema", "path", "device", "inode"],
            "WorkspaceLeaseIdentityV1",
        )?;
        let probe = WorkspaceLeaseProbe::acquire(
            Path::new(lease["path"].as_str()?),
            LeaseIdentity {
                device: lease["device"]
                    .as_str()?
                    .parse()
                    .map_err(|_| "lease device overflow".to_string())?,
                inode: lease["inode"]
                    .as_str()?
                    .parse()
                    .map_err(|_| "lease inode overflow".to_string())?,
            },
        )?;
        probe.revalidate()?;
    }
    let start_bytes = capability::read_secure_bytes(&session_dir.join("start-request-v1.json"))?;
    let start_sha256 = sha256::hex_digest(&start_bytes);
    let restart_file = session_dir
        .parent()
        .ok_or_else(|| "session directory has no parent".to_string())?
        .join(format!(
            ".resume-{}-start-request-v1.json",
            manifest.object()?["session_id"].as_str()?
        ));
    write_private_bytes(&restart_file, &start_bytes)?;
    fs::remove_dir_all(session_dir)
        .map_err(|error| format!("remove proven prelaunch session: {error}"))?;
    let restarted = match retry_relay {
        Some((relay_executable, relay_executable_sha256)) => {
            start_workspace_with_roots_and_verified_executable(
                roots,
                &restart_file,
                &start_sha256,
                Some(coordinator_capability_file),
                &relay_executable,
                &relay_executable_sha256,
            )
        }
        None => start_workspace_with_roots(
            roots,
            &restart_file,
            &start_sha256,
            Some(coordinator_capability_file),
        ),
    };
    match restarted {
        Ok(started) => {
            fs::remove_file(&restart_file)
                .map_err(|error| format!("remove consumed resume request: {error}"))?;
            Ok(started.result)
        }
        Err(error) => Err(format!(
            "prelaunch session was exactly reset but restart failed: {error}; request retained at {}",
            restart_file.display()
        )),
    }
}

pub fn recover_workspace_with_roots(
    roots: &AuthorityRoots,
    request_file: &Path,
    request_sha256: &str,
    coordinator_capability_file: &Path,
) -> Result<JcsValue, String> {
    let request: RecoverRequestV1 = schema::read_jcs_file(request_file, Some(request_sha256))?;
    let repository = git::OpenedRepository::open(Path::new(&request.repository_path))?;
    if repository.identity.repository_id != request.repository_id {
        return Err("recover repository identity differs from its canonical path".into());
    }
    let authority = WorkspaceAuthority::new(roots.clone())?;
    authority.authenticate(
        &request.repository_id,
        coordinator_capability_file,
        "recover",
    )?;
    let session_dir = session_dir_for(roots, &request.repository_id, &request.session_id)?;
    let manifest_path = session_dir.join("manifest-v1.json");
    let manifest: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    validate_lifecycle_cas(
        &manifest,
        &request.session_id,
        request.expected_state,
        &request.expected_journal_head_sha256,
    )?;
    match request.action.as_str() {
        "inspect" => recovery_inspect_value(&session_dir, &manifest),
        "rotate_coordinator" => {
            let exclusion = authority.acquire_authority_exclusion(&request.repository_id)?;
            let rotated = authority.rotate_coordinator_cas(
                &exclusion,
                &request.repository_id,
                coordinator_capability_file,
                &request.created_at,
            )?;
            Ok(JcsValue::Object(BTreeMap::from([
                (
                    "coordinator_capability_file".into(),
                    JcsValue::String(rotated.capability_file.to_string_lossy().into_owned()),
                ),
                (
                    "generation".into(),
                    JcsValue::String(rotated.capability.generation),
                ),
                (
                    "schema".into(),
                    JcsValue::String("CoordinatorRotationV1".into()),
                ),
            ])))
        }
        "resume_prelaunch" => Ok(resume_prelaunch(
            roots,
            &repository,
            &session_dir,
            &manifest,
            coordinator_capability_file,
        )?
        .to_jcs()),
        "retain_abort" => {
            let synthetic = AbortRequestV1 {
                request_id: request.request_id,
                repository_path: request.repository_path,
                repository_id: request.repository_id,
                session_id: request.session_id,
                expected_state: request.expected_state,
                expected_journal_head_sha256: request.expected_journal_head_sha256,
                expected_head: request.expected_head,
                reason: "recover retain_abort".into(),
                created_at: request.created_at,
            };
            let path = session_dir.join("recover-abort-request-v1.json");
            authority::atomic_create_jcs(&path, &synthetic, 0o600)?;
            let digest = sha256::hex_digest(&capability::read_secure_bytes(&path)?);
            Ok(
                abort_workspace_with_roots(roots, &path, &digest, coordinator_capability_file)?
                    .to_jcs(),
            )
        }
        _ => Err("recover action is outside the closed set".into()),
    }
}

pub fn execute(command: WorkspaceCommand) -> Result<String, String> {
    let value = match command {
        WorkspaceCommand::Preserve {
            request_file,
            request_sha256,
        } => {
            let result = preserve_workspace(&request_file, &request_sha256)?;
            JcsValue::Object(BTreeMap::from([
                (
                    "receipt_file".into(),
                    JcsValue::String(result.receipt_file.to_string_lossy().into_owned()),
                ),
                (
                    "receipt_sha256".into(),
                    JcsValue::String(result.receipt_sha256),
                ),
                ("schema".into(), JcsValue::String(schema::SCHEMA_V1.into())),
            ]))
        }
        WorkspaceCommand::Start {
            request_file,
            request_sha256,
            coordinator_capability_file,
        } => {
            let started = start_workspace(
                &request_file,
                &request_sha256,
                coordinator_capability_file.as_deref(),
            )?;
            started.result.to_jcs()
        }
        WorkspaceCommand::List {
            repository,
            coordinator_capability_file,
        } => list_workspaces(&repository, &coordinator_capability_file)?,
        WorkspaceCommand::Inspect {
            session_id,
            repository,
            coordinator_capability_file,
        } => inspect_workspace(&session_id, &repository, &coordinator_capability_file)?,
        WorkspaceCommand::Handback {
            request_file,
            request_sha256,
            worker_capability_file,
        } => handback_workspace(&request_file, &request_sha256, &worker_capability_file)?,
        WorkspaceCommand::Coordinator {
            operation,
            request_file,
            request_sha256,
            coordinator_capability_file,
        } => {
            let roots = SystemAuthorityRootProvider.roots()?;
            match operation {
                CoordinatorMutation::Integrate => integrate_workspace_with_roots(
                    &roots,
                    &request_file,
                    &request_sha256,
                    &coordinator_capability_file,
                )?
                .to_jcs(),
                CoordinatorMutation::Recover => recover_workspace_with_roots(
                    &roots,
                    &request_file,
                    &request_sha256,
                    &coordinator_capability_file,
                )?,
                CoordinatorMutation::Finish => finish_workspace_with_roots(
                    &roots,
                    &request_file,
                    &request_sha256,
                    &coordinator_capability_file,
                )?
                .to_jcs(),
                CoordinatorMutation::Abort => abort_workspace_with_roots(
                    &roots,
                    &request_file,
                    &request_sha256,
                    &coordinator_capability_file,
                )?
                .to_jcs(),
            }
        }
    };
    Ok(format!("{}\n", schema::serialize_jcs(&value)))
}

fn encode_fd_list(fds: &[RawFd]) -> String {
    if fds.is_empty() {
        "none".into()
    } else {
        fds.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn take_broker_stderr(child: &mut Child) -> String {
    let Some(stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut text = String::new();
    let _ = stderr.take(64 * 1024).read_to_string(&mut text);
    text.trim().to_string()
}

fn drain_broker_stderr(child: &mut Child) {
    if let Some(mut stderr) = child.stderr.take() {
        thread::spawn(move || {
            let _ = std::io::copy(&mut stderr, &mut std::io::stderr());
        });
    }
}

struct GitBrokerStartContext<'a> {
    roots: &'a AuthorityRoots,
    relay_file: &'a File,
    session_dir: &'a Path,
    worktree: &'a Path,
    branch_ref: &'a str,
    capability_file: &'a Path,
    lease: &'a WorkspaceLease,
    resource_fds: &'a [RawFd],
}

fn start_git_broker(context: GitBrokerStartContext<'_>) -> Result<(), String> {
    let GitBrokerStartContext {
        roots,
        relay_file,
        session_dir,
        worktree,
        branch_ref,
        capability_file,
        lease,
        resource_fds,
    } = context;
    let authority_root = roots
        .authority
        .to_str()
        .ok_or_else(|| "authority root is not UTF-8".to_string())?;
    let data_root = roots
        .data
        .to_str()
        .ok_or_else(|| "data root is not UTF-8".to_string())?;
    let session_dir_arg = session_dir
        .to_str()
        .ok_or_else(|| "session dir is not UTF-8".to_string())?;
    let worktree_arg = worktree
        .to_str()
        .ok_or_else(|| "worktree is not UTF-8".to_string())?;
    let capability_arg = capability_file
        .to_str()
        .ok_or_else(|| "capability path is not UTF-8".to_string())?;
    let capability: WorkerCapabilityV1 = schema::read_jcs_file(capability_file, None)?;
    let (mut readiness, ready_writer) = broker_readiness_pair(&capability)?;
    let lease_fd = lease.as_raw_fd();
    let relay_fd = relay_file.as_raw_fd();
    if relay_fd <= libc::STDERR_FILENO
        || lease_fd <= libc::STDERR_FILENO
        || resource_fds.iter().any(|fd| *fd <= libc::STDERR_FILENO)
        || resource_fds.contains(&lease_fd)
        || resource_fds.contains(&relay_fd)
        || relay_fd == lease_fd
    {
        return Err("broker source descriptor inventory is invalid".into());
    }
    let mut exact_resource_fds = resource_fds.to_vec();
    exact_resource_fds.sort_unstable();
    if exact_resource_fds.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("broker source resource descriptors contain duplicates".into());
    }
    let resource_fd_arg = encode_fd_list(resource_fds);
    let ready_fd = ready_writer.as_raw_fd();
    if ready_fd == lease_fd || resource_fds.contains(&ready_fd) {
        return Err("broker readiness descriptor collides with held descriptors".into());
    }
    let inherited = std::iter::once(relay_fd)
        .chain(std::iter::once(lease_fd))
        .chain(resource_fds.iter().copied())
        .chain(std::iter::once(ready_fd))
        .collect::<Vec<_>>();
    let prior = set_spawn_inheritance(&inherited, true)?;
    let lease_fd_arg = lease_fd.to_string();
    let ready_fd_arg = ready_fd.to_string();
    let relay_fd_arg = relay_fd.to_string();
    let relay_proc_path = format!("/proc/self/fd/{relay_fd}");
    let spawned = Command::new(&relay_proc_path)
        .args([
            "workspace",
            "__broker",
            "--authority-root",
            authority_root,
            "--data-root",
            data_root,
            "--session-dir",
            session_dir_arg,
            "--worktree",
            worktree_arg,
            "--branch-ref",
            branch_ref,
            "--worker-capability-file",
            capability_arg,
            "--relay-fd",
            &relay_fd_arg,
            "--lease-fd",
            &lease_fd_arg,
            "--resource-fds",
            &resource_fd_arg,
            "--ready-fd",
            &ready_fd_arg,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let restore_error = restore_spawn_inheritance(&prior).err();
    let mut child = spawned.map_err(|error| format!("spawn Git broker: {error}"))?;
    if let Some(error) = restore_error {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    drop(ready_writer);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match readiness.observe() {
            Ok(BrokerReadinessState::Ready) => {
                if let Some(status) = child
                    .try_wait()
                    .map_err(|error| format!("inspect Git broker: {error}"))?
                {
                    let stderr = take_broker_stderr(&mut child);
                    return Err(format!(
                        "Git broker exited after publishing readiness ({status}): {stderr}"
                    ));
                }
                drain_broker_stderr(&mut child);
                return Ok(());
            }
            Ok(BrokerReadinessState::Pending) => {}
            Ok(BrokerReadinessState::Closed) => {
                let _ = child.kill();
                let status = child
                    .wait()
                    .map_err(|error| format!("wait for failed Git broker: {error}"))?;
                let stderr = take_broker_stderr(&mut child);
                return Err(format!(
                    "Git broker closed readiness without publishing ({status}): {stderr}"
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let stderr = take_broker_stderr(&mut child);
                return Err(format!("{error}: {stderr}"));
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("inspect Git broker: {error}"))?
        {
            let stderr = take_broker_stderr(&mut child);
            return Err(format!(
                "Git broker exited before publishing readiness ({status}): {stderr}"
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    let stderr = take_broker_stderr(&mut child);
    Err(format!(
        "Git broker did not publish authenticated readiness within three seconds: {stderr}"
    ))
}

#[cfg(target_os = "linux")]
fn set_spawn_inheritance(fds: &[RawFd], inheritable: bool) -> Result<Vec<(RawFd, i32)>, String> {
    let mut prior = Vec::with_capacity(fds.len());
    for fd in fds {
        let flags = unsafe { libc::fcntl(*fd, libc::F_GETFD) };
        if flags < 0 {
            restore_spawn_inheritance(&prior)?;
            return Err(format!(
                "inspect custody descriptor {fd}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let next = if inheritable {
            flags & !libc::FD_CLOEXEC
        } else {
            flags | libc::FD_CLOEXEC
        };
        if unsafe { libc::fcntl(*fd, libc::F_SETFD, next) } < 0 {
            restore_spawn_inheritance(&prior)?;
            return Err(format!(
                "set custody descriptor {fd} inheritance: {}",
                std::io::Error::last_os_error()
            ));
        }
        prior.push((*fd, flags));
    }
    Ok(prior)
}

#[cfg(target_os = "linux")]
fn restore_spawn_inheritance(prior: &[(RawFd, i32)]) -> Result<(), String> {
    let mut error = None;
    for (fd, flags) in prior.iter().copied() {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 && error.is_none() {
            error = Some(format!(
                "restore custody descriptor {fd} flags: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(target_os = "linux")]
fn wait_supervisor_bootstrap_ready(mut read_end: File, child: &mut Child) -> Result<(), String> {
    let mut poll = libc::pollfd {
        fd: read_end.as_raw_fd(),
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("inspect custody supervisor bootstrap: {error}"))?
        {
            return Err(format!(
                "custody supervisor exited before bootstrap readiness ({status})"
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(
                "custody supervisor did not become bootstrap-ready within five seconds".to_string(),
            );
        }
        let rc = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().min(100) as i32) };
        if rc < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!(
                "wait for custody supervisor bootstrap readiness: {error}"
            ));
        }
        if rc > 0 {
            let mut bytes = Vec::new();
            read_end
                .read_to_end(&mut bytes)
                .map_err(|error| format!("read custody supervisor bootstrap readiness: {error}"))?;
            if bytes != [0x01] {
                return Err("custody supervisor bootstrap readiness is not exact".to_string());
            }
            return Ok(());
        }
    }
}

#[cfg(target_os = "linux")]
fn reap_child_async(mut child: Child) {
    drain_broker_stderr(&mut child);
    thread::spawn(move || {
        let _ = child.wait();
    });
}

#[cfg(target_os = "linux")]
fn wait_pipe_record(mut read_end: File, mut child: Child) -> Result<String, String> {
    let mut poll = libc::pollfd {
        fd: read_end.as_raw_fd(),
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("inspect custody guardian: {error}"))?
        {
            let stderr = take_broker_stderr(&mut child);
            return Err(format!(
                "custody guardian exited before activation ({status}): {stderr}"
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            reap_child_async(child);
            return Err(
                "custody activation was not proven within fifteen seconds; lease and work retained"
                    .into(),
            );
        }
        let milliseconds = remaining.as_millis().min(100) as i32;
        let rc = unsafe { libc::poll(&mut poll, 1, milliseconds) };
        if rc < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            reap_child_async(child);
            return Err(format!("wait for custody activation: {error}"));
        }
        if rc > 0 {
            let mut bytes = Vec::new();
            read_end
                .read_to_end(&mut bytes)
                .map_err(|error| format!("read custody activation proof: {error}"))?;
            if bytes.len() == 71 && bytes.starts_with(b"fault:") && bytes.last() == Some(&b'\n') {
                let digest = std::str::from_utf8(&bytes[6..70])
                    .map_err(|_| "custody bootstrap fault digest is not UTF-8".to_string())?;
                Sha256Digest::parse(digest)?;
                reap_child_async(child);
                return Err(format!(
                    "custody bootstrap fault {digest} is retained for explicit recover resume_prelaunch"
                ));
            }
            if bytes.len() != 65 || bytes.last() != Some(&b'\n') {
                let _ = child.kill();
                let _ = child.wait();
                let stderr = take_broker_stderr(&mut child);
                return Err(format!(
                    "custody guardian activation proof is not exact SHA-256+LF: {stderr}"
                ));
            }
            let digest = std::str::from_utf8(&bytes[..64])
                .map_err(|_| "custody activation digest is not UTF-8".to_string())?;
            Sha256Digest::parse(digest)?;
            reap_child_async(child);
            return Ok(digest.to_string());
        }
    }
}

struct CustodyRuntimeStartContext<'a> {
    roots: &'a AuthorityRoots,
    relay_executable: &'a Path,
    relay_executable_sha256: &'a str,
    relay_file: &'a File,
    session_dir: &'a Path,
    session_id: &'a str,
    tool_launch_file: &'a Path,
    lease: &'a WorkspaceLease,
    resource_fds: &'a [RawFd],
}

fn start_custody_runtime(context: CustodyRuntimeStartContext<'_>) -> Result<String, String> {
    let CustodyRuntimeStartContext {
        roots,
        relay_executable,
        relay_executable_sha256,
        relay_file,
        session_dir,
        session_id,
        tool_launch_file,
        lease,
        resource_fds,
    } = context;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (
            roots,
            relay_executable,
            relay_executable_sha256,
            relay_file,
            session_dir,
            session_id,
            tool_launch_file,
            lease,
            resource_fds,
        );
        return Err(platform::MACOS_STOP_REASON.into());
    }
    #[cfg(target_os = "linux")]
    {
        LowerUuidV4::parse(session_id)?;
        let runtime_key = capability::random_secret()?;
        write_private_bytes(&session_dir.join("runtime-control-key-v1"), &runtime_key)?;
        let mut pipe = [-1; 2];
        if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(format!(
                "create custody activation pipe: {}",
                std::io::Error::last_os_error()
            ));
        }
        let read_end = unsafe { File::from_raw_fd(pipe[0]) };
        let write_end = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
        let relay_fd = relay_file.as_raw_fd();
        let lease_fd = lease.as_raw_fd();
        if relay_fd <= libc::STDERR_FILENO
            || relay_fd == lease_fd
            || resource_fds.contains(&relay_fd)
        {
            return Err("custody relay descriptor inventory is invalid".into());
        }
        let mut inherited = vec![relay_fd, lease_fd, write_end.as_raw_fd()];
        inherited.extend_from_slice(resource_fds);
        let prior = set_spawn_inheritance(&inherited, true)?;
        let resource_text = encode_fd_list(resource_fds);
        let relay_proc_path = format!("/proc/self/fd/{relay_fd}");
        let spawned = Command::new(&relay_proc_path)
            .args([
                "workspace",
                "__guardian",
                "--authority-root",
                roots
                    .authority
                    .to_str()
                    .ok_or_else(|| "authority root is not UTF-8".to_string())?,
                "--data-root",
                roots
                    .data
                    .to_str()
                    .ok_or_else(|| "data root is not UTF-8".to_string())?,
                "--session-dir",
                session_dir
                    .to_str()
                    .ok_or_else(|| "session dir is not UTF-8".to_string())?,
                "--session-id",
                session_id,
                "--tool-launch-file",
                tool_launch_file
                    .to_str()
                    .ok_or_else(|| "tool launch path is not UTF-8".to_string())?,
                "--relay-executable",
                relay_executable
                    .to_str()
                    .ok_or_else(|| "relay executable path is not UTF-8".to_string())?,
                "--relay-executable-sha256",
                relay_executable_sha256,
                "--relay-fd",
                &relay_fd.to_string(),
                "--lease-fd",
                &lease_fd.to_string(),
                "--resource-fds",
                &resource_text,
                "--ready-fd",
                &write_end.as_raw_fd().to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let restore = restore_spawn_inheritance(&prior);
        let child = spawned.map_err(|error| format!("spawn custody guardian: {error}"))?;
        if let Err(error) = restore {
            return Err(format!(
                "{error}; custody guardian may hold retained proof and requires recovery"
            ));
        }
        drop(write_end);
        wait_pipe_record(read_end, child)
    }
}

#[cfg(target_os = "linux")]
fn parse_fd_list(value: &str) -> Result<Vec<RawFd>, String> {
    if value.is_empty() || value == "none" {
        return Ok(Vec::new());
    }
    let mut fds = value
        .split(',')
        .map(|value| {
            let fd = value
                .parse::<RawFd>()
                .map_err(|_| "custody FD list is not decimal".to_string())?;
            if fd < 3 || fd.to_string() != value {
                return Err("custody FD list is not canonical".into());
            }
            Ok(fd)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let before = fds.len();
    fds.sort_unstable();
    fds.dedup();
    if fds.len() != before {
        return Err("custody FD list contains duplicates".into());
    }
    Ok(fds)
}

#[cfg(target_os = "linux")]
fn runtime_key(session_dir: &Path) -> Result<[u8; 32], String> {
    let bytes = capability::read_secure_bytes(&session_dir.join("runtime-control-key-v1"))?;
    bytes
        .try_into()
        .map_err(|_| "runtime control key is not exactly 32 bytes".to_string())
}

#[cfg(target_os = "linux")]
fn persist_runtime_record(
    session_dir: &Path,
    name: &str,
    value: JcsValue,
) -> Result<String, String> {
    let path = session_dir.join(name);
    let record = CanonicalRecord(value);
    if path.exists() {
        let existing = schema::parse_jcs(&capability::read_secure_bytes(&path)?, true)?;
        if existing != record.0 {
            return Err(format!("{name} differs from the durable custody evidence"));
        }
    } else {
        authority::atomic_create_jcs(&path, &record, 0o600)?;
    }
    Ok(schema::jcs_sha256(&record))
}

#[cfg(target_os = "linux")]
fn persist_bootstrap_fault(
    session_dir: &Path,
    evidence: &crate::supervisor::RetainedBootstrapEvidence,
) -> Result<(), String> {
    persist_runtime_record(
        session_dir,
        "custody-fault-v1.json",
        JcsValue::Object(BTreeMap::from([
            (
                "cgroup_membership".into(),
                JcsValue::String(evidence.cgroup_membership.clone()),
            ),
            ("code".into(), JcsValue::String(evidence.code.clone())),
            (
                "empty_sha256".into(),
                JcsValue::String(evidence.empty_sha256.clone()),
            ),
            ("error".into(), JcsValue::String(evidence.error.clone())),
            (
                "evidence_sha256".into(),
                JcsValue::String(evidence.evidence_sha256.clone()),
            ),
            (
                "generation".into(),
                JcsValue::String(evidence.generation.to_string()),
            ),
            (
                "lease_device".into(),
                JcsValue::String(evidence.lease_device.to_string()),
            ),
            (
                "lease_inode".into(),
                JcsValue::String(evidence.lease_inode.to_string()),
            ),
            ("schema".into(), JcsValue::String("CustodyFaultV1".into())),
            (
                "session_id".into(),
                JcsValue::String(evidence.session_id.clone()),
            ),
            (
                "guardian_pid".into(),
                JcsValue::String(evidence.guardian_pid.to_string()),
            ),
            (
                "guardian_start_token".into(),
                JcsValue::String(evidence.guardian_start_token.clone()),
            ),
            (
                "supervisor_pid".into(),
                JcsValue::String(evidence.supervisor_pid.to_string()),
            ),
            (
                "supervisor_start_token".into(),
                JcsValue::String(evidence.supervisor_start_token.clone()),
            ),
        ])),
    )?;
    persist_runtime_record(
        session_dir,
        "custody-empty-v1.json",
        JcsValue::Object(BTreeMap::from([
            (
                "empty_sha256".into(),
                JcsValue::String(evidence.empty_sha256.clone()),
            ),
            ("mode".into(), JcsValue::String("bootstrap_fault".into())),
            ("schema".into(), JcsValue::String("CustodyEmptyV1".into())),
        ])),
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_bootstrap_fault(
    session_dir: &Path,
    session_id: &str,
    cgroup: &platform::linux::DelegatedCgroup,
    lease: &custody::LeaseReference,
    guardian: &custody::PeerIdentity,
    supervisor: &custody::PeerIdentity,
    retained: &custody::RetainedBootstrapFault,
) -> Result<(), String> {
    let value = schema::parse_jcs(
        &capability::read_secure_bytes(&session_dir.join("custody-fault-v1.json"))?,
        true,
    )?;
    let object = closed_object(
        &value,
        &[
            "schema",
            "session_id",
            "generation",
            "cgroup_membership",
            "lease_device",
            "lease_inode",
            "guardian_pid",
            "guardian_start_token",
            "supervisor_pid",
            "supervisor_start_token",
            "code",
            "error",
            "evidence_sha256",
            "empty_sha256",
        ],
        "CustodyFaultV1",
    )?;
    let (lease_device, lease_inode) = lease.identity();
    if object["schema"].as_str()? != "CustodyFaultV1"
        || object["session_id"].as_str()? != session_id
        || object["generation"].as_str()? != "1"
        || object["cgroup_membership"].as_str()? != cgroup.membership()
        || object["lease_device"].as_str()? != lease_device.to_string()
        || object["lease_inode"].as_str()? != lease_inode.to_string()
        || object["guardian_pid"].as_str()? != guardian.pid.to_string()
        || object["guardian_start_token"].as_str()? != guardian.start_token
        || object["supervisor_pid"].as_str()? != supervisor.pid.to_string()
        || object["supervisor_start_token"].as_str()? != supervisor.start_token
        || object["code"].as_str()? != retained.code
        || object["evidence_sha256"].as_str()? != retained.evidence_sha256
        || object["empty_sha256"].as_str()? != retained.empty_sha256
    {
        return Err(
            "authenticated bootstrap fault differs from durable custody evidence".to_string(),
        );
    }
    Sha256Digest::parse(object["evidence_sha256"].as_str()?)?;
    Sha256Digest::parse(object["empty_sha256"].as_str()?)?;
    let empty = schema::parse_jcs(
        &capability::read_secure_bytes(&session_dir.join("custody-empty-v1.json"))?,
        true,
    )?;
    let empty = closed_object(
        &empty,
        &["schema", "empty_sha256", "mode"],
        "CustodyEmptyV1",
    )?;
    if empty["schema"].as_str()? != "CustodyEmptyV1"
        || empty["mode"].as_str()? != "bootstrap_fault"
        || empty["empty_sha256"].as_str()? != retained.empty_sha256
    {
        return Err("durable bootstrap EMPTY evidence differs from the authenticated fault".into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn process_peer(pid: libc::pid_t) -> Result<custody::PeerIdentity, String> {
    Ok(custody::PeerIdentity {
        pid,
        euid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        start_token: platform::linux::process_start_token(pid)?,
    })
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct RetainedRuntimeFault {
    evidence_sha256: String,
    empty_sha256: String,
}

#[cfg(target_os = "linux")]
fn classify_runtime_custody_fault(error: &str) -> &'static str {
    if error == custody::HEARTBEAT_FENCE_ERROR {
        "supervisor_heartbeat_deadline"
    } else {
        "supervisor_control_fault"
    }
}

#[cfg(target_os = "linux")]
struct RuntimeCustodyFaultContext<'a> {
    roots: &'a AuthorityRoots,
    session_dir: &'a Path,
    session_id: &'a str,
    cgroup: &'a platform::linux::DelegatedCgroup,
    worker_root: &'a platform::linux::ProcessIdentity,
    guardian: &'a custody::PeerIdentity,
    supervisor: &'a custody::PeerIdentity,
    lease: &'a custody::LeaseReference,
    supervisor_child: &'a mut Child,
    error: &'a str,
}

#[cfg(target_os = "linux")]
fn retain_runtime_custody_fault(
    context: RuntimeCustodyFaultContext<'_>,
) -> Result<RetainedRuntimeFault, String> {
    let RuntimeCustodyFaultContext {
        roots,
        session_dir,
        session_id,
        cgroup,
        worker_root,
        guardian,
        supervisor,
        lease,
        supervisor_child,
        error,
    } = context;
    let code = classify_runtime_custody_fault(error);
    if supervisor_child
        .try_wait()
        .map_err(|wait_error| format!("inspect failed custody supervisor: {wait_error}"))?
        .is_none()
    {
        terminate_exact_fault_process(supervisor.pid, &supervisor.start_token)?;
        supervisor_child
            .wait()
            .map_err(|wait_error| format!("reap failed custody supervisor: {wait_error}"))?;
    }
    let empty = cgroup.kill_and_wait_empty(worker_root)?;
    let (lease_device, lease_inode) = lease.identity();
    let evidence_sha256 = sha256::hex_digest(
        format!(
            "custody-runtime-fault-v1\0{session_id}\01\0{}\0{lease_device}\0{lease_inode}\0{}\0{}\0{}\0{}\0{code}\0{error}\0{}",
            cgroup.membership(),
            guardian.pid,
            guardian.start_token,
            supervisor.pid,
            supervisor.start_token,
            empty.evidence_sha256,
        )
        .as_bytes(),
    );
    persist_runtime_record(
        session_dir,
        "custody-fault-v1.json",
        JcsValue::Object(BTreeMap::from([
            (
                "cgroup_membership".into(),
                JcsValue::String(cgroup.membership().into()),
            ),
            ("code".into(), JcsValue::String(code.into())),
            (
                "empty_sha256".into(),
                JcsValue::String(empty.evidence_sha256.clone()),
            ),
            ("error".into(), JcsValue::String(error.into())),
            (
                "evidence_sha256".into(),
                JcsValue::String(evidence_sha256.clone()),
            ),
            ("generation".into(), JcsValue::String("1".into())),
            (
                "guardian_pid".into(),
                JcsValue::String(guardian.pid.to_string()),
            ),
            (
                "guardian_start_token".into(),
                JcsValue::String(guardian.start_token.clone()),
            ),
            (
                "lease_device".into(),
                JcsValue::String(lease_device.to_string()),
            ),
            (
                "lease_inode".into(),
                JcsValue::String(lease_inode.to_string()),
            ),
            ("schema".into(), JcsValue::String("CustodyFaultV1".into())),
            ("session_id".into(), JcsValue::String(session_id.into())),
            (
                "supervisor_pid".into(),
                JcsValue::String(supervisor.pid.to_string()),
            ),
            (
                "supervisor_start_token".into(),
                JcsValue::String(supervisor.start_token.clone()),
            ),
        ])),
    )?;
    persist_runtime_record(
        session_dir,
        "custody-empty-v1.json",
        JcsValue::Object(BTreeMap::from([
            (
                "empty_sha256".into(),
                JcsValue::String(empty.evidence_sha256.clone()),
            ),
            ("mode".into(), JcsValue::String("supervisor_fault".into())),
            ("schema".into(), JcsValue::String("CustodyEmptyV1".into())),
        ])),
    )?;

    let manifest = read_manifest(&session_dir.join("manifest-v1.json"))?;
    let manifest_object = manifest.object()?;
    if manifest_object["session_id"].as_str()? != session_id {
        return Err("runtime custody fault session differs from durable manifest".into());
    }
    let repository =
        git::OpenedRepository::open(Path::new(manifest_object["worktree_root"].as_str()?))?;
    if manifest_object["repository"] != repository.identity.to_jcs() {
        return Err("runtime custody fault repository differs from durable manifest".into());
    }
    revoke_worker_if_needed(
        roots,
        &repository,
        session_dir,
        &manifest,
        &authority::now_timestamp()?,
    )?;
    remove_revoked_worker_capability(Path::new(
        manifest_object["worker_capability_file"].as_str()?,
    ))?;
    wait_for_durable_file(
        &session_dir.join("broker-close-v1.json"),
        "runtime fault broker close proof",
    )?;
    Ok(RetainedRuntimeFault {
        evidence_sha256,
        empty_sha256: empty.evidence_sha256,
    })
}

#[cfg(target_os = "linux")]
fn close_retained_runtime_lease(
    session_dir: &Path,
    session_id: &str,
    supervisor: &custody::PeerIdentity,
    fault: &RetainedRuntimeFault,
    guardian_lease: custody::LeaseReference,
) -> Result<String, String> {
    if !process_identity_gone(supervisor.pid, &supervisor.start_token)? {
        return Err("faulted custody supervisor remains live before lease close".into());
    }
    let guardian_close = guardian_lease.close();
    let supervisor_close_sha256 = sha256::hex_digest(
        format!(
            "custody-dead-supervisor-lease-close-v1\0{session_id}\0{}\0{}\0{}\0{}",
            supervisor.pid, supervisor.start_token, fault.evidence_sha256, fault.empty_sha256,
        )
        .as_bytes(),
    );
    persist_runtime_record(
        session_dir,
        "custody-lease-closed-v1.json",
        JcsValue::Object(BTreeMap::from([
            (
                "guardian_close_sha256".into(),
                JcsValue::String(guardian_close.evidence_sha256),
            ),
            (
                "supervisor_close_sha256".into(),
                JcsValue::String(supervisor_close_sha256),
            ),
            (
                "schema".into(),
                JcsValue::String("CustodyLeaseClosedV1".into()),
            ),
        ])),
    )
}

#[cfg(target_os = "linux")]
fn run_guardian(raw: &[String]) -> Result<(), String> {
    use custody::{ControlEndpoint, CustodyController, LeaseReference, PayloadValue, Sender};
    use platform::linux::DelegatedCgroup;

    let flags = Flags::parse(
        raw,
        &[
            "--authority-root",
            "--data-root",
            "--session-dir",
            "--session-id",
            "--tool-launch-file",
            "--relay-executable",
            "--relay-executable-sha256",
            "--relay-fd",
            "--lease-fd",
            "--resource-fds",
            "--ready-fd",
        ],
    )?;
    let roots = AuthorityRoots {
        authority: flags.absolute("--authority-root")?,
        data: flags.absolute("--data-root")?,
        euid: unsafe { libc::geteuid() },
    };
    let session_dir = flags.absolute("--session-dir")?;
    let session_id = flags.value("--session-id")?.to_string();
    LowerUuidV4::parse(&session_id)?;
    let tool_launch_file = flags.absolute("--tool-launch-file")?;
    let relay_executable = flags.absolute("--relay-executable")?;
    let relay_executable_sha256 = flags.value("--relay-executable-sha256")?.to_string();
    Sha256Digest::parse(&relay_executable_sha256)?;
    let relay_fd = flags
        .value("--relay-fd")?
        .parse::<RawFd>()
        .map_err(|_| "guardian relay FD is not decimal".to_string())?;
    let relay_file = adopt_running_relay_fd(relay_fd)?;
    let lease_fd = flags
        .value("--lease-fd")?
        .parse::<RawFd>()
        .map_err(|_| "guardian lease FD is not decimal".to_string())?;
    let resource_fds = parse_fd_list(flags.value("--resource-fds")?)?;
    let ready_fd = flags
        .value("--ready-fd")?
        .parse::<RawFd>()
        .map_err(|_| "guardian ready FD is not decimal".to_string())?;
    if lease_fd < 3
        || ready_fd < 3
        || resource_fds.contains(&lease_fd)
        || resource_fds.contains(&relay_fd)
        || relay_fd == lease_fd
        || relay_fd == ready_fd
    {
        return Err("guardian inherited FD inventory is invalid".into());
    }
    let lease_fd = unsafe { OwnedFd::from_raw_fd(lease_fd) };
    let resource_files = resource_fds
        .iter()
        .map(|fd| unsafe { File::from_raw_fd(*fd) })
        .collect::<Vec<_>>();
    custody::set_fd_inheritable_for_exec(lease_fd.as_raw_fd(), false)?;
    for resource in &resource_files {
        custody::set_fd_inheritable_for_exec(resource.as_raw_fd(), false)?;
    }
    let mut ready = unsafe { File::from_raw_fd(ready_fd) };
    custody::set_fd_inheritable_for_exec(ready.as_raw_fd(), false)?;
    resources::validate_held_resource_fds(&session_dir.join("resources"), &resource_fds)?;

    let socket = custody_socket_path(&session_dir);
    let socket_parent = socket
        .parent()
        .ok_or_else(|| "custody command socket has no parent".to_string())?;
    let socket_root = socket_parent
        .parent()
        .ok_or_else(|| "custody command socket root is missing".to_string())?;
    authority::ensure_private_directory(socket_root, unsafe { libc::geteuid() })?;
    authority::ensure_private_directory(socket_parent, unsafe { libc::geteuid() })?;
    if socket.exists() {
        return Err("custody command socket already exists; explicit recovery is required".into());
    }
    let listener = UnixListener::bind(&socket)
        .map_err(|error| format!("bind custody command socket {}: {error}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod custody command socket: {error}"))?;

    let cgroup = DelegatedCgroup::create(&session_id)?;
    let guardian_lease = LeaseReference::from_owned_fd(lease_fd)?;
    let (guardian_fd, supervisor_fd) = ControlEndpoint::pair()?;
    let (key_fd, key) = custody::create_control_key_memfd()?;
    let mut supervisor_ready_pipe = [-1; 2];
    if unsafe { libc::pipe2(supervisor_ready_pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!(
            "create custody supervisor readiness pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    let supervisor_ready_read = unsafe { File::from_raw_fd(supervisor_ready_pipe[0]) };
    let supervisor_ready_write = unsafe { OwnedFd::from_raw_fd(supervisor_ready_pipe[1]) };
    let guardian_identity = custody::PeerIdentity::current()?;
    let inherited_fds = std::iter::once(relay_fd)
        .chain(std::iter::once(supervisor_fd.as_raw_fd()))
        .chain(std::iter::once(key_fd.as_raw_fd()))
        .chain(std::iter::once(supervisor_ready_write.as_raw_fd()))
        .chain(resource_fds.iter().copied())
        .collect::<Vec<_>>();
    let prior = set_spawn_inheritance(&inherited_fds, true)?;
    let resource_text = encode_fd_list(&resource_fds);
    let relay_proc_path = format!("/proc/self/fd/{relay_fd}");
    let spawned = Command::new(&relay_proc_path)
        .args([
            "workspace",
            "__custody-supervisor",
            "--session-dir",
            session_dir
                .to_str()
                .ok_or_else(|| "session dir is not UTF-8".to_string())?,
            "--session-id",
            &session_id,
            "--tool-launch-file",
            tool_launch_file
                .to_str()
                .ok_or_else(|| "tool launch path is not UTF-8".to_string())?,
            "--relay-executable",
            relay_executable
                .to_str()
                .ok_or_else(|| "relay executable path is not UTF-8".to_string())?,
            "--relay-executable-sha256",
            &relay_executable_sha256,
            "--relay-fd",
            &relay_fd.to_string(),
            "--control-fd",
            &supervisor_fd.as_raw_fd().to_string(),
            "--key-fd",
            &key_fd.as_raw_fd().to_string(),
            "--bootstrap-ready-fd",
            &supervisor_ready_write.as_raw_fd().to_string(),
            "--resource-fds",
            &resource_text,
            "--guardian-pid",
            &guardian_identity.pid.to_string(),
            "--guardian-euid",
            &guardian_identity.euid.to_string(),
            "--guardian-gid",
            &guardian_identity.gid.to_string(),
            "--guardian-start-token",
            &guardian_identity.start_token,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let restore = restore_spawn_inheritance(&prior);
    let mut supervisor = spawned.map_err(|error| format!("spawn custody supervisor: {error}"))?;
    drop(supervisor_ready_write);
    drop(supervisor_fd);
    drop(key_fd);
    drop(resource_files);
    drop(relay_file);
    if let Err(error) = restore {
        let _ = supervisor.kill();
        let _ = supervisor.wait();
        let cleanup = cgroup.remove();
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; remove unactivated custody cgroup: {cleanup_error}"
            )),
        };
    }
    let supervisor_identity = process_peer(supervisor.id() as libc::pid_t)?;
    let endpoint = ControlEndpoint::new(
        guardian_fd,
        key,
        session_id.clone(),
        1,
        Sender::Guardian,
        supervisor_identity.clone(),
    )?;
    let mut controller = CustodyController::new(endpoint);
    let mut supervisor_ready_read = Some(supervisor_ready_read);
    let mut wait_until_prepared = || {
        wait_supervisor_bootstrap_ready(
            supervisor_ready_read
                .take()
                .ok_or_else(|| "custody supervisor readiness was already awaited".to_string())?,
            &mut supervisor,
        )
    };
    let activation = match crate::supervisor::run_workspace_guardian_bootstrap(
        &mut controller,
        &cgroup,
        &guardian_lease,
        &mut wait_until_prepared,
    ) {
        Ok(activation) => activation,
        Err(bootstrap_error) => {
            let retained = controller
                .accept_bootstrap_fault()
                .map_err(|recovery_error| {
                    format!(
                        "{bootstrap_error}; authenticate retained bootstrap fault: {recovery_error}"
                    )
                })?;
            validate_bootstrap_fault(
                &session_dir,
                &session_id,
                &cgroup,
                &guardian_lease,
                &guardian_identity,
                &supervisor_identity,
                &retained,
            )?;
            ready
                .write_all(format!("fault:{}\n", retained.evidence_sha256).as_bytes())
                .and_then(|_| ready.flush())
                .map_err(|error| format!("publish retained custody bootstrap fault: {error}"))?;
            drop(ready);
            finish_guardian_runtime(GuardianRuntimeContext {
                session_dir: &session_dir,
                listener,
                controller: &mut controller,
                guardian_lease: Some(guardian_lease),
                supervisor: &mut supervisor,
                cgroup: &cgroup,
                roots: &roots,
                session_id: &session_id,
                guardian_identity: &guardian_identity,
                supervisor_identity: &supervisor_identity,
                worker_root: None,
            })?;
            return Ok(());
        }
    };
    let active = JcsValue::Object(BTreeMap::from([
        (
            "activated_sha256".into(),
            match activation.activated.get("evidence_sha256") {
                Some(PayloadValue::String(value)) => JcsValue::String(value.clone()),
                _ => return Err("ACTIVATED evidence payload is not exact".into()),
            },
        ),
        (
            "backend".into(),
            JcsValue::String(platform::LINUX_BACKEND.into()),
        ),
        (
            "cgroup_membership".into(),
            JcsValue::String(cgroup.membership().into()),
        ),
        (
            "guardian_pid".into(),
            JcsValue::String(guardian_identity.pid.to_string()),
        ),
        (
            "guardian_start_token".into(),
            JcsValue::String(guardian_identity.start_token.clone()),
        ),
        (
            "prepared_sha256".into(),
            JcsValue::String(activation.prepared_evidence_sha256.clone()),
        ),
        ("schema".into(), JcsValue::String("CustodyActiveV1".into())),
        ("session_id".into(), JcsValue::String(session_id.clone())),
        (
            "supervisor_pid".into(),
            JcsValue::String(supervisor_identity.pid.to_string()),
        ),
        (
            "supervisor_start_token".into(),
            JcsValue::String(supervisor_identity.start_token.clone()),
        ),
    ]));
    let active_sha256 = persist_runtime_record(&session_dir, "custody-active-v1.json", active)?;
    ready
        .write_all(format!("{active_sha256}\n").as_bytes())
        .and_then(|_| ready.flush())
        .map_err(|error| format!("publish custody activation proof: {error}"))?;
    drop(ready);
    finish_guardian_runtime(GuardianRuntimeContext {
        session_dir: &session_dir,
        listener,
        controller: &mut controller,
        guardian_lease: Some(guardian_lease),
        supervisor: &mut supervisor,
        cgroup: &cgroup,
        roots: &roots,
        session_id: &session_id,
        guardian_identity: &guardian_identity,
        supervisor_identity: &supervisor_identity,
        worker_root: Some(&activation.root),
    })
}

#[cfg(target_os = "linux")]
struct GuardianRuntimeContext<'a> {
    session_dir: &'a Path,
    listener: UnixListener,
    controller: &'a mut custody::CustodyController,
    guardian_lease: Option<custody::LeaseReference>,
    supervisor: &'a mut Child,
    cgroup: &'a platform::linux::DelegatedCgroup,
    roots: &'a AuthorityRoots,
    session_id: &'a str,
    guardian_identity: &'a custody::PeerIdentity,
    supervisor_identity: &'a custody::PeerIdentity,
    worker_root: Option<&'a platform::linux::ProcessIdentity>,
}

#[cfg(target_os = "linux")]
fn finish_guardian_runtime(mut context: GuardianRuntimeContext<'_>) -> Result<(), String> {
    run_guardian_commands(&mut context)?;
    if context.cgroup.path().exists() {
        return Err(format!(
            "custody supervisor exited without removing empty cgroup {}",
            context.cgroup.path().display(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verified_git_shim_runtime(
    environment: &BTreeMap<String, String>,
    relay_executable: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let capability_path = PathBuf::from(
        environment
            .get("DOCKS_WORKER_CAPABILITY_FILE")
            .ok_or_else(|| "tool launch lacks the pinned worker capability path".to_string())?,
    );
    if !capability_path.is_absolute() {
        return Err("worker capability path is not absolute".into());
    }
    let capability_parent = capability_path
        .parent()
        .ok_or_else(|| "worker capability path has no parent".to_string())?;
    if capability_parent.file_name() != Some(std::ffi::OsStr::new("worker-capabilities")) {
        return Err("worker capability is outside the private Git session-relay hierarchy".into());
    }
    let session_relay = capability_parent
        .parent()
        .ok_or_else(|| "worker capability hierarchy has no session-relay root".to_string())?;
    if session_relay.file_name() != Some(std::ffi::OsStr::new("session-relay")) {
        return Err("worker capability hierarchy is not the exact session-relay root".into());
    }
    let shim = session_relay.join("bin/git");
    let expected_path = format!(
        "{}:/usr/local/bin:/usr/bin:/bin",
        shim.parent()
            .ok_or_else(|| "Git shim has no bin directory".to_string())?
            .display()
    );
    if environment.get("PATH") != Some(&expected_path) {
        return Err("tool PATH differs from the exact private-Git shim chain".into());
    }
    let metadata = fs::symlink_metadata(&shim)
        .map_err(|error| format!("inspect private Git shim {}: {error}", shim.display()))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o500
    {
        return Err("Git shim is not an EUID-owned single-link mode-0500 regular file".into());
    }
    let body = fs::read(&shim).map_err(|error| format!("read private Git shim: {error}"))?;
    if body != git_shim_body(&capability_path, relay_executable).as_bytes() {
        return Err(
            "private Git shim command chain differs from its pinned relay and capability".into(),
        );
    }
    capability::read_secure_bytes(&capability_path)?;
    Ok((shim, capability_path))
}

#[cfg(target_os = "linux")]
fn launch_from_record(
    path: &Path,
    resource_fds: &[RawFd],
    relay_executable: &Path,
    relay_executable_sha256: &str,
) -> Result<platform::linux::VerifiedWorkerLaunch, String> {
    let decision: resources::ToolLaunchDecisionV1 = schema::read_jcs_file(path, None)?;
    let recorded = decision
        .resource_fds
        .iter()
        .map(|value| {
            value
                .parse::<RawFd>()
                .map_err(|_| "tool launch resource FD is not decimal".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if recorded != resource_fds {
        return Err(
            "custody inherited resource FDs differ from the durable launch decision".into(),
        );
    }
    Sha256Digest::parse(relay_executable_sha256)?;
    let running_relay = fs::canonicalize("/proc/self/exe")
        .map_err(|error| format!("resolve running custody supervisor executable: {error}"))?;
    let admitted_relay = fs::canonicalize(relay_executable)
        .map_err(|error| format!("revalidate admitted relay executable: {error}"))?;
    if running_relay != admitted_relay {
        return Err(
            "custody supervisor executable identity differs from the admitted relay".into(),
        );
    }
    let tool_executable = PathBuf::from(&decision.executable_path);
    let relay_is_tool = fs::canonicalize(&tool_executable)
        .map_err(|error| format!("revalidate tool executable identity: {error}"))?
        == admitted_relay;
    if relay_is_tool
        && !sha256::constant_time_eq(
            decision.executable_sha256.as_bytes(),
            relay_executable_sha256.as_bytes(),
        )
    {
        return Err(
            "tool and relay resolve to one executable with different pinned digests".into(),
        );
    }
    let expected_executable_sha256 = decision.executable_sha256.clone();
    let environment = decision
        .environment
        .into_iter()
        .map(|value| (value.name, value.value))
        .collect::<BTreeMap<_, _>>();
    let (git_shim, capability_path) = verified_git_shim_runtime(&environment, relay_executable)?;
    let launch = platform::linux::WorkerLaunch {
        executable: tool_executable,
        arguments: decision.arguments,
        environment,
        cwd: PathBuf::from(&decision.cwd),
        resource_fds: resource_fds.to_vec(),
        sandbox: platform::linux::LandlockPolicy {
            workspace: PathBuf::from(decision.cwd),
            readable: Vec::new(),
            executable_runtime: if relay_is_tool {
                vec![git_shim]
            } else {
                vec![git_shim, admitted_relay]
            },
            pinned_readable: vec![capability_path],
            writable_resources: decision
                .writable_resources
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        },
    };
    if relay_is_tool {
        platform::linux::VerifiedWorkerLaunch::prepare_without_digest(launch)
    } else {
        platform::linux::VerifiedWorkerLaunch::prepare(launch, &expected_executable_sha256)
    }
}

#[cfg(target_os = "linux")]
fn run_custody_supervisor(raw: &[String]) -> Result<(), String> {
    use custody::{ControlEndpoint, CustodianServer, PeerIdentity, Sender};

    let flags = Flags::parse(
        raw,
        &[
            "--session-id",
            "--session-dir",
            "--tool-launch-file",
            "--relay-executable",
            "--relay-executable-sha256",
            "--relay-fd",
            "--control-fd",
            "--key-fd",
            "--resource-fds",
            "--bootstrap-ready-fd",
            "--guardian-pid",
            "--guardian-euid",
            "--guardian-gid",
            "--guardian-start-token",
        ],
    )?;
    let session_dir = flags.absolute("--session-dir")?;
    let session_id = flags.value("--session-id")?.to_string();
    LowerUuidV4::parse(&session_id)?;
    let relay_executable = flags.absolute("--relay-executable")?;
    let relay_executable_sha256 = flags.value("--relay-executable-sha256")?.to_string();
    Sha256Digest::parse(&relay_executable_sha256)?;
    let relay_fd = flags
        .value("--relay-fd")?
        .parse::<RawFd>()
        .map_err(|_| "supervisor relay FD is not decimal".to_string())?;
    let _relay_file = adopt_running_relay_fd(relay_fd)?;
    let control_fd = flags
        .value("--control-fd")?
        .parse::<RawFd>()
        .map_err(|_| "supervisor control FD is not decimal".to_string())?;
    let key_fd = flags
        .value("--key-fd")?
        .parse::<RawFd>()
        .map_err(|_| "supervisor key FD is not decimal".to_string())?;
    let bootstrap_ready_fd = flags
        .value("--bootstrap-ready-fd")?
        .parse::<RawFd>()
        .map_err(|_| "supervisor bootstrap readiness FD is not decimal".to_string())?;
    let resource_fds = parse_fd_list(flags.value("--resource-fds")?)?;
    if bootstrap_ready_fd < 3
        || bootstrap_ready_fd == control_fd
        || bootstrap_ready_fd == key_fd
        || resource_fds.contains(&bootstrap_ready_fd)
        || bootstrap_ready_fd == relay_fd
        || control_fd == relay_fd
        || key_fd == relay_fd
        || resource_fds.contains(&relay_fd)
    {
        return Err("supervisor bootstrap readiness FD inventory is invalid".to_string());
    }
    let bootstrap_ready = unsafe { File::from_raw_fd(bootstrap_ready_fd) };
    let resource_files = resource_fds
        .iter()
        .map(|fd| unsafe { File::from_raw_fd(*fd) })
        .collect::<Vec<_>>();
    let key = custody::read_control_key_memfd(key_fd)?;
    unsafe { libc::close(key_fd) };
    let guardian = PeerIdentity {
        pid: flags
            .value("--guardian-pid")?
            .parse()
            .map_err(|_| "guardian PID is not decimal".to_string())?,
        euid: flags
            .value("--guardian-euid")?
            .parse()
            .map_err(|_| "guardian EUID is not decimal".to_string())?,
        gid: flags
            .value("--guardian-gid")?
            .parse()
            .map_err(|_| "guardian GID is not decimal".to_string())?,
        start_token: flags.value("--guardian-start-token")?.to_string(),
    };
    let actual = process_peer(guardian.pid)?;
    if actual != guardian {
        return Err("guardian peer identity drifted before supervisor bootstrap".into());
    }
    let endpoint = ControlEndpoint::new(
        unsafe { OwnedFd::from_raw_fd(control_fd) },
        key,
        session_id,
        1,
        Sender::Supervisor,
        guardian,
    )?;
    let tool_launch_file = flags.absolute("--tool-launch-file")?;
    let mut prepared_launch = Some(launch_from_record(
        &tool_launch_file,
        &resource_fds,
        &relay_executable,
        &relay_executable_sha256,
    )?);
    let mut prepare_launch = || {
        prepared_launch
            .take()
            .ok_or_else(|| "verified worker launch was already consumed".to_string())
    };
    let mut server = CustodianServer::new(endpoint);
    let mut bootstrap_ready = Some(bootstrap_ready);
    let mut publish_bootstrap_ready = || {
        let mut ready = bootstrap_ready
            .take()
            .ok_or_else(|| "custody supervisor readiness was already published".to_string())?;
        ready
            .write_all(&[0x01])
            .map_err(|error| format!("publish custody supervisor bootstrap readiness: {error}"))
    };
    let mut bootstrap_fault = |evidence: &crate::supervisor::RetainedBootstrapEvidence| {
        persist_bootstrap_fault(&session_dir, evidence)
    };
    let custody = crate::supervisor::run_workspace_supervisor_entrypoint(
        &mut server,
        &mut prepare_launch,
        &mut publish_bootstrap_ready,
        &mut bootstrap_fault,
    )?;
    let duplicate_pidfd =
        unsafe { libc::fcntl(custody.process.pidfd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate_pidfd < 0 {
        return Err(format!(
            "duplicate worker pidfd for resource close: {}",
            std::io::Error::last_os_error()
        ));
    }
    let closer = thread::spawn(move || {
        let pidfd = unsafe { OwnedFd::from_raw_fd(duplicate_pidfd) };
        let mut poll = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let rc = unsafe { libc::poll(&mut poll, 1, -1) };
            if rc > 0 {
                break;
            }
            if rc < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
        drop(resource_files);
        drop(pidfd);
    });
    let mut runtime_fault = |_error: &str| {};
    let released = crate::supervisor::run_workspace_supervisor_release_protocol(
        &mut server,
        custody,
        &mut runtime_fault,
    )?;
    closer
        .join()
        .map_err(|_| "resource closer thread panicked".to_string())?;
    released.cgroup.remove()
}

#[cfg(target_os = "linux")]
fn runtime_bare_request(
    request_id: &str,
    action: &str,
    evidence_sha256: Option<&str>,
    nonce: &str,
) -> JcsValue {
    JcsValue::Object(BTreeMap::from([
        ("action".into(), JcsValue::String(action.into())),
        (
            "evidence_sha256".into(),
            evidence_sha256
                .map(|value| JcsValue::String(value.into()))
                .unwrap_or(JcsValue::Null),
        ),
        ("nonce".into(), JcsValue::String(nonce.into())),
        ("request_id".into(), JcsValue::String(request_id.into())),
        (
            "schema".into(),
            JcsValue::String("WorkspaceRuntimeCommandV1".into()),
        ),
    ]))
}

#[cfg(target_os = "linux")]
fn runtime_exchange(
    session_dir: &Path,
    action: &str,
    evidence_sha256: Option<&str>,
) -> Result<String, String> {
    if !matches!(
        action,
        "quiesce" | "terminate" | "close_lease" | "closed_committed"
    ) {
        return Err("runtime action is outside the closed set".into());
    }
    if let Some(value) = evidence_sha256 {
        Sha256Digest::parse(value)?;
    }
    let request_id = crate::store::uuid_v4();
    let nonce = capability::encode_base64url(&capability::random_secret()?);
    let bare = runtime_bare_request(&request_id, action, evidence_sha256, &nonce);
    let key = runtime_key(session_dir)?;
    let message = [
        b"session-relay/runtime-command/v1\0".as_slice(),
        schema::serialize_jcs(&bare).as_bytes(),
    ]
    .concat();
    let mac = sha256::hex_digest(&sha256::hmac(&key, &message));
    let mut object = bare.object()?;
    object.insert("mac".into(), JcsValue::String(mac));
    let mut bytes = schema::serialize_jcs(&JcsValue::Object(object)).into_bytes();
    bytes.push(b'\n');
    let mut stream = UnixStream::connect(custody_socket_path(session_dir))
        .map_err(|error| format!("connect custody runtime: {error}"))?;
    stream
        .set_write_timeout(Some(RUNTIME_EXCHANGE_DEADLINE))
        .map_err(|error| format!("set custody runtime write deadline: {error}"))?;
    stream
        .set_read_timeout(Some(RUNTIME_EXCHANGE_DEADLINE))
        .map_err(|error| format!("set custody runtime read deadline: {error}"))?;
    stream
        .write_all(&bytes)
        .map_err(|error| format!("write custody runtime command: {error}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|error| format!("finish custody runtime command: {error}"))?;
    let mut response = Vec::new();
    std::io::Read::by_ref(&mut stream)
        .take(64 * 1024)
        .read_to_end(&mut response)
        .map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                format!(
                    "custody runtime {action} response deadline elapsed after {} ms; custody is retained and explicit recovery is required",
                    RUNTIME_EXCHANGE_DEADLINE.as_millis(),
                )
            } else {
                format!("read custody runtime response: {error}")
            }
        })?;
    let response = schema::parse_jcs(&response, true)?;
    let object = closed_object(
        &response,
        &["schema", "request_id", "status", "evidence_sha256", "error"],
        "WorkspaceRuntimeResponseV1",
    )?;
    if object["request_id"].as_str()? != request_id {
        return Err("custody runtime response request ID mismatch".into());
    }
    if object["status"].as_str()? != "ok" {
        return Err(format!(
            "custody runtime refused {action}: {}",
            object["error"].as_str()?
        ));
    }
    let digest = object["evidence_sha256"].as_str()?;
    Sha256Digest::parse(digest)?;
    Ok(digest.into())
}

#[cfg(not(target_os = "linux"))]
fn runtime_exchange(
    _session_dir: &Path,
    _action: &str,
    _evidence_sha256: Option<&str>,
) -> Result<String, String> {
    Err(platform::MACOS_STOP_REASON.into())
}

#[cfg(target_os = "linux")]
fn runtime_response(request_id: &str, result: Result<String, String>) -> JcsValue {
    match result {
        Ok(evidence) => JcsValue::Object(BTreeMap::from([
            ("error".into(), JcsValue::String(String::new())),
            ("evidence_sha256".into(), JcsValue::String(evidence)),
            ("request_id".into(), JcsValue::String(request_id.into())),
            (
                "schema".into(),
                JcsValue::String("WorkspaceRuntimeResponseV1".into()),
            ),
            ("status".into(), JcsValue::String("ok".into())),
        ])),
        Err(error) => JcsValue::Object(BTreeMap::from([
            ("error".into(), JcsValue::String(error)),
            ("evidence_sha256".into(), JcsValue::String("0".repeat(64))),
            ("request_id".into(), JcsValue::String(request_id.into())),
            (
                "schema".into(),
                JcsValue::String("WorkspaceRuntimeResponseV1".into()),
            ),
            ("status".into(), JcsValue::String("error".into())),
        ])),
    }
}

#[cfg(target_os = "linux")]
fn peer_is_current_euid(stream: &UnixStream) -> Result<(), String> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
        || length as usize != std::mem::size_of::<libc::ucred>()
        || credentials.uid != unsafe { libc::geteuid() }
    {
        return Err("custody runtime peer credential mismatch".into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_guardian_commands(context: &mut GuardianRuntimeContext<'_>) -> Result<(), String> {
    use custody::PayloadValue;
    let session_dir = context.session_dir;
    let key = runtime_key(session_dir)?;
    let mut guardian_lease = context.guardian_lease.take();
    let listener = &context.listener;
    let controller = &mut *context.controller;
    let supervisor = &mut *context.supervisor;
    let cgroup = context.cgroup;
    let roots = context.roots;
    let session_id = context.session_id;
    let guardian_identity = context.guardian_identity;
    let supervisor_identity = context.supervisor_identity;
    let worker_root = context.worker_root;
    let mut retained_fault = None;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set custody runtime listener nonblocking: {error}"))?;
    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if retained_fault.is_none()
                    && matches!(
                        controller.phase(),
                        custody::CustodyPhase::Active
                            | custody::CustodyPhase::Empty
                            | custody::CustodyPhase::ReleasePrepared
                            | custody::CustodyPhase::LeaseClosed
                    )
                {
                    if let Err(error) = controller.heartbeat() {
                        let worker_root = worker_root.ok_or_else(|| {
                            format!(
                                "custody supervisor failed outside Running without worker identity: {error}"
                            )
                        })?;
                        retained_fault =
                            Some(retain_runtime_custody_fault(RuntimeCustodyFaultContext {
                                roots,
                                session_dir,
                                session_id,
                                cgroup,
                                worker_root,
                                guardian: guardian_identity,
                                supervisor: supervisor_identity,
                                lease: guardian_lease.as_ref().ok_or_else(|| {
                                    "custody supervisor failed after guardian lease close"
                                        .to_string()
                                })?,
                                supervisor_child: supervisor,
                                error: &error,
                            })?);
                    }
                    thread::sleep(custody::HEARTBEAT_INTERVAL);
                } else {
                    thread::sleep(Duration::from_millis(20));
                }
                continue;
            }
            Err(error) => {
                return Err(format!("accept custody runtime command: {error}"));
            }
        };
        stream
            .set_read_timeout(Some(RUNTIME_COMMAND_IO_DEADLINE))
            .map_err(|error| format!("set custody runtime request deadline: {error}"))?;
        stream
            .set_write_timeout(Some(RUNTIME_COMMAND_IO_DEADLINE))
            .map_err(|error| format!("set custody runtime response deadline: {error}"))?;
        peer_is_current_euid(&stream)?;
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut stream)
            .take(64 * 1024)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read custody runtime command: {error}"))?;
        let value = schema::parse_jcs(&bytes, true)?;
        let object = closed_object(
            &value,
            &[
                "schema",
                "request_id",
                "action",
                "evidence_sha256",
                "nonce",
                "mac",
            ],
            "WorkspaceRuntimeCommandV1",
        )?;
        let request_id = object["request_id"].as_str()?.to_string();
        LowerUuidV4::parse(&request_id)?;
        let action = object["action"].as_str()?.to_string();
        let evidence = match &object["evidence_sha256"] {
            JcsValue::Null => None,
            JcsValue::String(value) => {
                Sha256Digest::parse(value)?;
                Some(value.clone())
            }
            _ => return Err("runtime command evidence has invalid nullability".into()),
        };
        let bare = runtime_bare_request(
            &request_id,
            &action,
            evidence.as_deref(),
            object["nonce"].as_str()?,
        );
        let message = [
            b"session-relay/runtime-command/v1\0".as_slice(),
            schema::serialize_jcs(&bare).as_bytes(),
        ]
        .concat();
        let expected = sha256::hex_digest(&sha256::hmac(&key, &message));
        if !sha256::constant_time_eq(expected.as_bytes(), object["mac"].as_str()?.as_bytes()) {
            return Err("runtime command MAC mismatch".into());
        }
        let result = match action.as_str() {
            "quiesce" => (|| -> Result<String, String> {
                if let Some(fault) = retained_fault.as_ref() {
                    return Ok(fault.empty_sha256.clone());
                }
                let payload = controller.quiesce()?;
                let empty = match payload.get("evidence_sha256") {
                    Some(PayloadValue::String(value)) => value.clone(),
                    _ => return Err("QUIESCED evidence payload is not exact".into()),
                };
                controller.confirm_empty(&empty)?;
                persist_runtime_record(
                    session_dir,
                    "custody-empty-v1.json",
                    JcsValue::Object(BTreeMap::from([
                        ("empty_sha256".into(), JcsValue::String(empty)),
                        ("mode".into(), JcsValue::String("quiesce".into())),
                        ("schema".into(), JcsValue::String("CustodyEmptyV1".into())),
                    ])),
                )
            })(),
            "terminate" => (|| -> Result<String, String> {
                if let Some(fault) = retained_fault.as_ref() {
                    return Ok(fault.empty_sha256.clone());
                }
                let payload = controller.terminate()?;
                let empty = match payload.get("evidence_sha256") {
                    Some(PayloadValue::String(value)) => value.clone(),
                    _ => return Err("EMPTY evidence payload is not exact".into()),
                };
                persist_runtime_record(
                    session_dir,
                    "custody-empty-v1.json",
                    JcsValue::Object(BTreeMap::from([
                        ("empty_sha256".into(), JcsValue::String(empty)),
                        ("mode".into(), JcsValue::String("terminate".into())),
                        ("schema".into(), JcsValue::String("CustodyEmptyV1".into())),
                    ])),
                )
            })(),
            "close_lease" => {
                if !session_dir.join("custody-empty-v1.json").exists() {
                    return Err("custody EMPTY proof is missing before lease close".into());
                }
                if let Some(fault) = retained_fault.as_ref() {
                    close_retained_runtime_lease(
                        session_dir,
                        session_id,
                        supervisor_identity,
                        fault,
                        guardian_lease
                            .take()
                            .ok_or_else(|| "guardian lease was already closed".to_string())?,
                    )
                } else {
                    controller.prepare_release()?;
                    let supervisor_close = controller.close_lease()?;
                    let supervisor_close = match supervisor_close.get("evidence_sha256") {
                        Some(PayloadValue::String(value)) => value.clone(),
                        _ => return Err("supervisor LEASE_CLOSED evidence is not exact".into()),
                    };
                    let guardian_close = guardian_lease
                        .take()
                        .ok_or_else(|| "guardian lease was already closed".to_string())?
                        .close();
                    persist_runtime_record(
                        session_dir,
                        "custody-lease-closed-v1.json",
                        JcsValue::Object(BTreeMap::from([
                            (
                                "guardian_close_sha256".into(),
                                JcsValue::String(guardian_close.evidence_sha256),
                            ),
                            (
                                "supervisor_close_sha256".into(),
                                JcsValue::String(supervisor_close),
                            ),
                            (
                                "schema".into(),
                                JcsValue::String("CustodyLeaseClosedV1".into()),
                            ),
                        ])),
                    )
                }
            }
            "closed_committed" => {
                let evidence = evidence
                    .as_deref()
                    .ok_or_else(|| "CLOSED_COMMITTED requires evidence".to_string())?;
                if retained_fault.is_some() {
                    if guardian_lease.is_some() {
                        return Err(
                            "faulted custody guardian lease is still held at CLOSED_COMMITTED"
                                .into(),
                        );
                    }
                    platform::linux::reconcile_empty_delegated_cgroup(session_id)?;
                } else {
                    controller.closed_committed(evidence)?;
                    let status = supervisor
                        .wait()
                        .map_err(|error| format!("wait custody supervisor: {error}"))?;
                    if !status.success() {
                        return Err(format!(
                            "custody supervisor failed after CLOSED_COMMITTED ({status})"
                        ));
                    }
                }
                Ok(evidence.to_string())
            }
            _ => Err("runtime action is outside the closed set".into()),
        };
        let close = action == "closed_committed" && result.is_ok();
        let response = runtime_response(&request_id, result);
        let mut response_bytes = schema::serialize_jcs(&response).into_bytes();
        response_bytes.push(b'\n');
        if let Err(error) = stream.write_all(&response_bytes) {
            eprintln!(
                "custody runtime response delivery failed after durable {action}: {error}; durable custody remains recoverable"
            );
            if !close {
                continue;
            }
        }
        if close {
            let socket = custody_socket_path(session_dir);
            fs::remove_file(&socket)
                .map_err(|error| format!("remove custody command socket: {error}"))?;
            fs::remove_dir(
                socket
                    .parent()
                    .ok_or_else(|| "custody command socket has no parent".to_string())?,
            )
            .map_err(|error| format!("remove custody command socket directory: {error}"))?;
            return Ok(());
        }
    }
}
fn produced_commit_values(
    repository: &git::OpenedRepository,
    receipt: &HandbackReceiptV1,
    applied_wip_commit: &str,
    durable_produced: &JcsValue,
) -> Result<JcsValue, String> {
    if receipt.produced_commits.first().map(String::as_str) != Some(applied_wip_commit) {
        return Err(
            "handback produced commit chain does not begin at the applied WIP commit".into(),
        );
    }
    let JcsValue::Array(existing) = durable_produced else {
        return Err("durable produced_commits is not an array".into());
    };
    if existing.len() != 1 {
        return Err(
            "Running workspace must contain exactly the applied WIP commit before handback".into(),
        );
    }
    let JcsValue::Object(applied_record) = &existing[0] else {
        return Err("durable applied WIP commit record is not an object".into());
    };
    let keys = ["oid", "parent_oid", "source"];
    if applied_record.len() != keys.len()
        || keys.iter().any(|key| !applied_record.contains_key(*key))
        || applied_record["oid"].as_str()? != applied_wip_commit
        || applied_record["source"].as_str()? != "applied_wip"
    {
        return Err(
            "durable produced commit chain does not contain the exact applied WIP anchor".into(),
        );
    }
    repository.validate_oid(applied_record["parent_oid"].as_str()?)?;
    let mut values = Vec::with_capacity(receipt.produced_commits.len());
    values.push(existing[0].clone());
    let mut previous = applied_wip_commit;
    for oid in receipt.produced_commits.iter().skip(1) {
        repository.validate_oid(oid)?;
        if oid == applied_wip_commit {
            return Err("handback duplicates the applied WIP commit".into());
        }
        let parent_text = repository.run_git_text(&["show", "-s", "--format=%P", oid])?;
        let parents = parent_text.split_whitespace().collect::<Vec<_>>();
        if parents.as_slice() != [previous] {
            return Err("handback worker commits are not exact single-parent descendants".into());
        }
        values.push(JcsValue::Object(BTreeMap::from([
            ("oid".into(), JcsValue::String(oid.clone())),
            ("parent_oid".into(), JcsValue::String(previous.into())),
            ("source".into(), JcsValue::String("worker".into())),
        ])));
        previous = oid;
    }
    Ok(JcsValue::Array(values))
}

fn complete_handback_quiescence(
    roots: &AuthorityRoots,
    session_dir: &Path,
    repository: &git::OpenedRepository,
) -> Result<(), String> {
    let manifest_path = session_dir.join("manifest-v1.json");
    let current: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    if current.state()? == WorkspaceState::HandbackReady {
        return Ok(());
    }
    if current.state()? != WorkspaceState::Running {
        return Err("handback quiescence requires Running or durable HandbackReady".into());
    }
    let receipt_path = session_dir.join("handback-receipt-v1.json");
    let receipt: HandbackReceiptV1 = schema::read_jcs_file(&receipt_path, None)?;
    let receipt_sha256 = sha256::hex_digest(&capability::read_secure_bytes(&receipt_path)?);
    let empty_sha256 = if session_dir.join("custody-empty-v1.json").exists() {
        let value = schema::parse_jcs(
            &capability::read_secure_bytes(&session_dir.join("custody-empty-v1.json"))?,
            true,
        )?;
        let object = closed_object(
            &value,
            &["schema", "empty_sha256", "mode"],
            "CustodyEmptyV1",
        )?;
        object["empty_sha256"].as_str()?.to_string()
    } else {
        runtime_exchange(session_dir, "quiesce", None)?
    };
    let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
    let locked: WorkspaceManifestRecord = schema::read_jcs_file(&manifest_path, None)?;
    if locked.state()? == WorkspaceState::HandbackReady {
        return Ok(());
    }
    if locked.state()? != WorkspaceState::Running {
        return Err("handback state changed while acquiring the lifecycle locks".into());
    }
    if locked.object()?["session_id"].as_str()? != receipt.session_id
        || schema::RepositoryIdentityV1::from_jcs(locked.object()?["repository"].clone())?
            != repository.identity
    {
        return Err("handback receipt identity differs from the durable workspace".into());
    }
    repository.validate_unchanged()?;
    let object = locked.object()?;
    let branch_ref = object["branch_ref"].as_str()?;
    if repository.run_git_text(&["symbolic-ref", "-q", "HEAD"])? != branch_ref {
        return Err("workspace branch changed during handback quiescence".into());
    }
    if repository.run_git_text(&["rev-parse", "--verify", "HEAD"])? != receipt.head_oid {
        return Err("workspace HEAD changed during handback quiescence".into());
    }
    let applied = object["applied_wip_commit"].as_str()?;
    if object["worker_base_commit"].as_str()? != applied {
        return Err("worker base commit differs from the applied WIP commit".into());
    }
    let produced =
        produced_commit_values(repository, &receipt, applied, &object["produced_commits"])?;
    let now = authority::now_timestamp()?;
    let payload = JcsValue::Object(BTreeMap::from([
        (
            "custody_empty_sha256".into(),
            JcsValue::String(empty_sha256.clone()),
        ),
        ("receipt_sha256".into(), JcsValue::String(receipt_sha256)),
    ]));
    mutate_manifest_event(
        ManifestEventContext {
            session_dir,
            manifest_file: &manifest_path,
            expected: WorkspaceState::Running,
            next: WorkspaceState::HandbackReady,
            kind: "HandbackReady",
            created_at: &now,
        },
        payload,
        |object| {
            object.insert("produced_commits".into(), produced.clone());
            let custody = object["custody_evidence"].clone().object()?;
            object.insert(
                "custody_evidence".into(),
                JcsValue::Object(BTreeMap::from([
                    ("active_sha256".into(), custody["active_sha256"].clone()),
                    (
                        "empty_sha256".into(),
                        JcsValue::String(empty_sha256.clone()),
                    ),
                ])),
            );
            Ok(())
        },
    )?;
    drop(gate);
    Ok(())
}

fn adopt_broker_descriptors(
    lease_fd: RawFd,
    resource_fds: Vec<RawFd>,
) -> Result<(File, Vec<File>), String> {
    let descriptors = std::iter::once(lease_fd)
        .chain(resource_fds.iter().copied())
        .collect::<Vec<_>>();
    if descriptors.iter().any(|fd| *fd <= libc::STDERR_FILENO) {
        return Err("broker inherited descriptor collides with stdio".into());
    }
    set_spawn_inheritance(&descriptors, false)?;
    let lease = unsafe { File::from_raw_fd(lease_fd) };
    let resources = resource_fds
        .into_iter()
        .map(|fd| unsafe { File::from_raw_fd(fd) })
        .collect();
    Ok((lease, resources))
}

fn run_broker(raw: &[String]) -> Result<(), String> {
    let flags = Flags::parse(
        raw,
        &[
            "--authority-root",
            "--data-root",
            "--session-dir",
            "--worktree",
            "--branch-ref",
            "--worker-capability-file",
            "--relay-fd",
            "--lease-fd",
            "--resource-fds",
            "--ready-fd",
        ],
    )?;
    let roots = AuthorityRoots {
        authority: flags.absolute("--authority-root")?,
        data: flags.absolute("--data-root")?,
        euid: unsafe { libc::geteuid() },
    };
    let session_dir = flags.absolute("--session-dir")?;
    let worktree = flags.absolute("--worktree")?;
    let branch_ref = flags.value("--branch-ref")?.to_string();
    if !branch_ref.starts_with("refs/heads/docks/") {
        return Err("broker branch ref is invalid".into());
    }
    let capability_file = flags.absolute("--worker-capability-file")?;
    let relay_fd = flags
        .value("--relay-fd")?
        .parse::<RawFd>()
        .map_err(|_| "broker relay FD is not decimal".to_string())?;
    let _relay_file = adopt_running_relay_fd(relay_fd)?;
    let lease_fd = flags
        .value("--lease-fd")?
        .parse::<RawFd>()
        .map_err(|_| "broker lease fd is not decimal".to_string())?;
    let resource_fds = parse_fd_list(flags.value("--resource-fds")?)?;
    let ready_fd = flags
        .value("--ready-fd")?
        .parse::<RawFd>()
        .map_err(|_| "broker readiness fd is not decimal".to_string())?;
    if resource_fds.contains(&lease_fd)
        || ready_fd == lease_fd
        || resource_fds.contains(&ready_fd)
        || relay_fd == lease_fd
        || relay_fd == ready_fd
        || resource_fds.contains(&relay_fd)
    {
        return Err("broker inherited descriptors collide".into());
    }
    let resource_fds_for_validation = resource_fds.clone();
    let (lease, held_resources) = adopt_broker_descriptors(lease_fd, resource_fds)?;
    resources::validate_held_resource_fds(
        &session_dir.join("resources"),
        &resource_fds_for_validation,
    )?;
    let repository = git::OpenedRepository::open(&worktree)?;
    let capability: WorkerCapabilityV1 = schema::read_jcs_file(&capability_file, None)?;
    let record_path = session_dir.join("worker-capability-record-v1.json");
    let initial_record: schema::CapabilityRecordV1 = schema::read_jcs_file(&record_path, None)?;
    capability::authenticate_worker(
        &capability,
        &initial_record,
        &capability.repository_id,
        &capability.session_id,
        capability
            .generation
            .parse()
            .map_err(|_| "broker generation overflow".to_string())?,
        "git_index",
        &authority::now_timestamp()?,
    )?;
    for name in ["broker-replays", "broker-intents", "broker-plans"] {
        authority::ensure_private_directory(&session_dir.join(name), roots.euid)?;
    }
    let socket = PathBuf::from(&capability.broker_socket);
    let socket_parent = socket
        .parent()
        .ok_or_else(|| "Git broker socket has no parent directory".to_string())?;
    authority::ensure_private_directory(socket_parent, roots.euid)?;
    if socket.exists() {
        return Err("broker socket already exists; refusing replacement".into());
    }
    let listener = UnixListener::bind(&socket)
        .map_err(|error| format!("bind Git broker {}: {error}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod Git broker socket: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set Git broker nonblocking: {error}"))?;
    publish_broker_ready(ready_fd, &capability)?;

    let revoked_at = loop {
        let record: schema::CapabilityRecordV1 = schema::read_jcs_file(&record_path, None)?;
        if let Some(revoked_at) = record.revoked_at {
            break revoked_at;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let envelope = read_broker_request(&mut stream);
                let response_request_id = envelope
                    .as_ref()
                    .ok()
                    .and_then(broker_request_id)
                    .unwrap_or_else(|| "00000000-0000-4000-8000-000000000000".into());
                let handled = envelope.and_then(|envelope| {
                    handle_broker_request(
                        &roots,
                        &session_dir,
                        &repository,
                        &worktree,
                        &branch_ref,
                        &capability,
                        envelope,
                    )
                });
                let (response, accepted_handback) = match handled {
                    Ok(value) => value,
                    Err(error) => (
                        broker_response(
                            &response_request_id,
                            "error",
                            1,
                            "",
                            &error,
                            JcsValue::Null,
                        ),
                        false,
                    ),
                };
                let mut bytes = schema::serialize_jcs(&response).into_bytes();
                bytes.push(b'\n');
                let _ = stream.write_all(&bytes);
                drop(stream);
                if accepted_handback {
                    if let Err(error) =
                        complete_handback_quiescence(&roots, &session_dir, &repository)
                    {
                        let fault = CanonicalRecord(JcsValue::Object(BTreeMap::from([
                            ("error".into(), JcsValue::String(error)),
                            (
                                "schema".into(),
                                JcsValue::String("HandbackQuiescenceFaultV1".into()),
                            ),
                        ])));
                        if !session_dir
                            .join("handback-quiescence-fault-v1.json")
                            .exists()
                        {
                            authority::atomic_create_jcs(
                                &session_dir.join("handback-quiescence-fault-v1.json"),
                                &fault,
                                0o600,
                            )?;
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("accept Git broker connection: {error}")),
        }
    };
    drop(listener);
    fs::remove_file(&socket)
        .map_err(|error| format!("remove drained Git broker socket: {error}"))?;
    fs::remove_dir(socket_parent).map_err(|error| {
        format!(
            "remove drained Git broker directory {}: {error}",
            socket_parent.display()
        )
    })?;
    drop(held_resources);
    drop(lease);
    let evidence = sha256::hex_digest(
        format!(
            "workspace-broker-close-v1\0{}\0{}\0{}",
            capability.session_id, capability.capability_id, revoked_at
        )
        .as_bytes(),
    );
    persist_runtime_record(
        &session_dir,
        "broker-close-v1.json",
        JcsValue::Object(BTreeMap::from([
            (
                "capability_id".into(),
                JcsValue::String(capability.capability_id),
            ),
            ("evidence_sha256".into(), JcsValue::String(evidence)),
            ("revoked_at".into(), JcsValue::String(revoked_at)),
            ("schema".into(), JcsValue::String("BrokerCloseV1".into())),
            ("session_id".into(), JcsValue::String(capability.session_id)),
        ])),
    )?;
    Ok(())
}

fn read_broker_request(stream: &mut UnixStream) -> Result<JcsValue, String> {
    let mut bytes = Vec::new();
    stream
        .take(1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read Git broker request: {e}"))?;
    schema::parse_jcs(&bytes, true)
}
fn broker_request_id(envelope: &JcsValue) -> Option<String> {
    let JcsValue::Object(envelope) = envelope else {
        return None;
    };
    let JcsValue::Object(request) = envelope.get("request")? else {
        return None;
    };
    let JcsValue::String(request_id) = request.get("request_id")? else {
        return None;
    };
    LowerUuidV4::parse(request_id).ok()?;
    Some(request_id.clone())
}

fn broker_replay_record(request_sha256: &str, response: &JcsValue) -> CanonicalRecord {
    CanonicalRecord(JcsValue::Object(BTreeMap::from([
        (
            "request_sha256".into(),
            JcsValue::String(request_sha256.into()),
        ),
        ("response".into(), response.clone()),
        (
            "schema".into(),
            JcsValue::String("GitBrokerReplayV1".into()),
        ),
    ])))
}

fn broker_replay_path(session_dir: &Path, request_id: &str) -> PathBuf {
    session_dir
        .join("broker-replays")
        .join(format!("{request_id}.record.json"))
}

fn parse_broker_replay(
    value: &JcsValue,
    request_sha256: &str,
    request_id: &str,
) -> Result<JcsValue, String> {
    let object = closed_object(
        value,
        &["schema", "request_sha256", "response"],
        "GitBrokerReplayV1",
    )?;
    if object["schema"].as_str()? != "GitBrokerReplayV1" {
        return Err("Git broker replay schema mismatch".into());
    }
    if object["request_sha256"].as_str()? != request_sha256 {
        return Err("changed broker request replay is refused".into());
    }
    let response = object["response"].clone();
    let response_object = closed_object(
        &response,
        &[
            "schema",
            "request_id",
            "status",
            "exit_code",
            "stdout",
            "stderr",
            "receipt",
        ],
        "GitBrokerResponseV1",
    )?;
    if response_object["request_id"].as_str()? != request_id {
        return Err("Git broker replay response request ID mismatch".into());
    }
    Ok(response)
}

fn durable_broker_replay(
    session_dir: &Path,
    request_id: &str,
    request_sha256: &str,
    operation: &str,
) -> Result<Option<JcsValue>, String> {
    let replay_path = broker_replay_path(session_dir, request_id);
    if replay_path.exists() {
        let value = schema::parse_jcs(&capability::read_secure_bytes(&replay_path)?, true)?;
        return parse_broker_replay(&value, request_sha256, request_id).map(Some);
    }

    let legacy_response_path = session_dir
        .join("broker-replays")
        .join(format!("{request_id}.json"));
    if !legacy_response_path.exists() {
        return Ok(None);
    }
    let intent_path = session_dir
        .join("broker-intents")
        .join(format!("{request_id}.json"));
    let intent = schema::parse_jcs(&capability::read_secure_bytes(&intent_path)?, true)
        .map_err(|error| {
            format!(
                "legacy broker replay cannot be reconstructed without durable mutation intent: {error}"
            )
        })?;
    let intent = closed_object(
        &intent,
        &[
            "schema",
            "request_id",
            "request_sha256",
            "operation",
            "argv",
            "details",
        ],
        "GitBrokerIntentV1",
    )?;
    if intent["request_id"].as_str()? != request_id
        || intent["request_sha256"].as_str()? != request_sha256
        || intent["operation"].as_str()? != operation
    {
        return Err("changed broker request conflicts with durable mutation intent".into());
    }
    let legacy_digest_path = session_dir
        .join("broker-replays")
        .join(format!("{request_id}.sha256"));
    if legacy_digest_path.exists()
        && capability::read_secure_bytes(&legacy_digest_path)?
            != format!("{request_sha256}\n").as_bytes()
    {
        return Err("legacy broker replay digest conflicts with durable mutation intent".into());
    }
    let response = schema::parse_jcs(&capability::read_secure_bytes(&legacy_response_path)?, true)?;
    let record = broker_replay_record(request_sha256, &response);
    let response = parse_broker_replay(&record.0, request_sha256, request_id)?;
    authority::atomic_create_jcs(&replay_path, &record, 0o600)?;
    Ok(Some(response))
}

fn persist_broker_replay(
    session_dir: &Path,
    request_id: &str,
    request_sha256: &str,
    response: &JcsValue,
) -> Result<(), String> {
    let record = broker_replay_record(request_sha256, response);
    parse_broker_replay(&record.0, request_sha256, request_id)?;
    authority::atomic_create_jcs(&broker_replay_path(session_dir, request_id), &record, 0o600)
}

fn recover_unrecorded_handback_response(
    session_dir: &Path,
    request_id: &str,
    request_sha256: &str,
    argv: &[String],
) -> Result<Option<JcsValue>, String> {
    let intent_path = session_dir
        .join("broker-intents")
        .join(format!("{request_id}.json"));
    if !intent_path.exists() {
        return Ok(None);
    }
    let intent = schema::parse_jcs(&capability::read_secure_bytes(&intent_path)?, true)?;
    let intent = closed_object(
        &intent,
        &[
            "schema",
            "request_id",
            "request_sha256",
            "operation",
            "argv",
            "details",
        ],
        "GitBrokerIntentV1",
    )?;
    let expected_argv = JcsValue::Array(argv.iter().cloned().map(JcsValue::String).collect());
    if intent["request_id"].as_str()? != request_id
        || intent["request_sha256"].as_str()? != request_sha256
        || intent["operation"].as_str()? != "handback"
        || intent["argv"] != expected_argv
    {
        return Err("changed handback replay conflicts with durable mutation intent".into());
    }
    let receipt_path = session_dir.join("handback-receipt-v1.json");
    if !receipt_path.exists() {
        return Ok(None);
    }
    let receipt: HandbackReceiptV1 = schema::read_jcs_file(&receipt_path, None)?;
    receipt.validate()?;
    Ok(Some(broker_response(
        request_id,
        "ok",
        0,
        "",
        "",
        receipt.to_jcs(),
    )))
}
fn handle_broker_request(
    roots: &AuthorityRoots,
    session_dir: &Path,
    repository: &git::OpenedRepository,
    worktree: &Path,
    branch_ref: &str,
    capability: &WorkerCapabilityV1,
    envelope: JcsValue,
) -> Result<(JcsValue, bool), String> {
    let envelope = closed_object(
        &envelope,
        &["schema", "request", "nonce", "mac"],
        "GitBrokerEnvelopeV1",
    )?;
    let request = envelope["request"].clone();
    let object = closed_object(
        &request,
        &[
            "schema",
            "request_id",
            "session_id",
            "generation",
            "operation",
            "argv",
            "cwd",
            "capability_id",
            "request_sha256",
        ],
        "GitBrokerRequestV1",
    )?;
    let request_id = object["request_id"].as_str()?.to_string();
    LowerUuidV4::parse(&request_id)?;
    if object["session_id"].as_str()? != capability.session_id
        || object["capability_id"].as_str()? != capability.capability_id
    {
        return Err("broker request capability identity mismatch".into());
    }
    let operation = object["operation"].as_str()?;
    if !matches!(operation, "git_index" | "git_commit" | "handback") {
        return Err("broker operation is outside the closed set".into());
    }
    let mut bare = object.clone();
    let supplied_digest = bare.remove("request_sha256").unwrap().as_str()?.to_string();
    let bare = JcsValue::Object(bare);
    let expected_digest = sha256::hex_digest(
        [
            b"session-relay/broker-request/v1\0".as_slice(),
            schema::serialize_jcs(&bare).as_bytes(),
        ]
        .concat()
        .as_slice(),
    );
    if !sha256::constant_time_eq(supplied_digest.as_bytes(), expected_digest.as_bytes()) {
        return Err("broker request digest mismatch".into());
    }
    let nonce = envelope["nonce"].as_str()?;
    let mac = envelope["mac"].as_str()?;
    let secret = capability::decode_base64url(&capability.secret_b64url)?;
    let message = [
        b"session-relay/broker-envelope/v1\0".as_slice(),
        schema::serialize_jcs(&request).as_bytes(),
        nonce.as_bytes(),
    ]
    .concat();
    let expected_mac = capability::encode_base64url(&sha256::hmac(&secret, &message));
    if !sha256::constant_time_eq(mac.as_bytes(), expected_mac.as_bytes()) {
        return Err("broker envelope MAC mismatch".into());
    }
    if let Some(response) =
        durable_broker_replay(session_dir, &request_id, &supplied_digest, operation)?
    {
        return Ok((response, operation == "handback"));
    }
    let argv = match &object["argv"] {
        JcsValue::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("broker argv must be an array".into()),
    };
    if operation == "handback" {
        if let Some(response) =
            recover_unrecorded_handback_response(session_dir, &request_id, &supplied_digest, &argv)?
        {
            persist_broker_replay(session_dir, &request_id, &supplied_digest, &response)?;
            return Ok((response, true));
        }
    }
    let record_path = session_dir.join("worker-capability-record-v1.json");
    let record: schema::CapabilityRecordV1 = schema::read_jcs_file(&record_path, None)?;
    capability::authenticate_worker(
        capability,
        &record,
        &capability.repository_id,
        &capability.session_id,
        capability
            .generation
            .parse()
            .map_err(|_| "broker generation overflow".to_string())?,
        operation,
        &authority::now_timestamp()?,
    )?;
    let cwd = PathBuf::from(object["cwd"].as_str()?);
    if fs::canonicalize(&cwd).map_err(|error| format!("canonicalize broker cwd: {error}"))?
        != worktree
    {
        return Err("broker cwd differs from the exact workspace root".into());
    }
    let (manifest, _) = manifest_claims(session_dir)?;
    validate_broker_repository_binding(
        capability, &manifest, repository, worktree, branch_ref, operation,
    )?;
    let gate = repository_gate::RepositoryGate::acquire(roots, &repository.identity)?;
    let response = match operation {
        "git_index" => broker_git_index(
            repository,
            worktree,
            branch_ref,
            session_dir,
            &request_id,
            &supplied_digest,
            &argv,
        ),
        "git_commit" => broker_git_commit(
            repository,
            worktree,
            branch_ref,
            session_dir,
            &request_id,
            &supplied_digest,
            &argv,
        ),
        "handback" => broker_handback(
            repository,
            worktree,
            branch_ref,
            session_dir,
            &request_id,
            &supplied_digest,
            &argv,
        ),
        _ => unreachable!(),
    }?;
    if operation == "handback" {
        let authority = WorkspaceAuthority::new(roots.clone())?;
        let exclusion =
            authority.acquire_authority_exclusion(&repository.identity.repository_id)?;
        capability::revoke_worker_durable(
            &exclusion,
            &record_path,
            &capability.capability_id,
            capability
                .generation
                .parse()
                .map_err(|_| "worker generation overflow".to_string())?,
            &authority::now_timestamp()?,
        )?;
    }
    drop(gate);
    persist_broker_replay(session_dir, &request_id, &supplied_digest, &response)?;
    Ok((response, operation == "handback"))
}

fn manifest_claims(
    session_dir: &Path,
) -> Result<(WorkspaceManifestRecord, Vec<schema::PathClaimRequestV1>), String> {
    let path = session_dir.join("manifest-v1.json");
    let manifest: WorkspaceManifestRecord = schema::read_jcs_file(&path, None)?;
    let object = match &manifest.0 {
        JcsValue::Object(object) => object,
        _ => unreachable!(),
    };
    let claims = match &object["owned_paths"] {
        JcsValue::Array(values) => values
            .iter()
            .map(|value| {
                let object = value.clone().object()?;
                Ok(schema::PathClaimRequestV1 {
                    path: object["path"].as_str()?.into(),
                    path_type: object["path_type"].as_str()?.into(),
                    mode: object["mode"].as_str()?.into(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
        _ => return Err("manifest owned_paths is not an array".into()),
    };
    Ok((manifest, claims))
}

fn validate_broker_identity_values(
    capability_repository_id: &str,
    actual_repository: &schema::RepositoryIdentityV1,
    expected_repository: &schema::RepositoryIdentityV1,
    actual_worktree: &schema::WorktreeIdentityV1,
    expected_worktree: &schema::WorktreeIdentityV1,
) -> Result<(), String> {
    if capability_repository_id != actual_repository.repository_id
        || expected_repository != actual_repository
    {
        return Err("broker repository identity differs from capability or manifest".into());
    }
    if actual_worktree != expected_worktree {
        return Err("broker worktree or private Git identity differs from manifest".into());
    }
    Ok(())
}
fn validate_broker_repository_binding(
    capability: &WorkerCapabilityV1,
    manifest: &WorkspaceManifestRecord,
    repository: &git::OpenedRepository,
    _worktree: &Path,
    branch_ref: &str,
    operation: &str,
) -> Result<(), String> {
    if manifest.state()? != WorkspaceState::Running {
        return Err(format!(
            "broker {operation} requires a durably Running workspace"
        ));
    }
    let object = manifest.object()?;
    let expected_repository = schema::RepositoryIdentityV1::from_jcs(object["repository"].clone())?;
    let expected_worktree =
        schema::WorktreeIdentityV1::from_value(object["worktree_identity"].clone())?;
    let actual_worktree = repository.worktree_identity(branch_ref)?;
    validate_broker_identity_values(
        &capability.repository_id,
        &repository.identity,
        &expected_repository,
        &actual_worktree,
        &expected_worktree,
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BrokerIndexCommand {
    git_args: Vec<String>,
    paths: Vec<String>,
}
fn parse_broker_index_argv(argv: &[String]) -> Result<BrokerIndexCommand, String> {
    let (mut git_args, rest) = match argv {
        [operation, rest @ ..] if operation == "add" => (vec!["add".into()], rest),
        [operation, cached, rest @ ..] if operation == "rm" && cached == "--cached" => {
            (vec!["rm".into(), "--cached".into()], rest)
        }
        [operation, staged, rest @ ..] if operation == "restore" && staged == "--staged" => {
            (vec!["restore".into(), "--staged".into()], rest)
        }
        _ => return Err("Git broker permits exactly add, rm --cached, or restore --staged".into()),
    };
    let paths = if rest.first().is_some_and(|arg| arg == "--") {
        &rest[1..]
    } else {
        rest
    };
    if paths.is_empty() || paths.iter().any(|arg| arg.starts_with('-')) {
        return Err("Git broker index operation has invalid path grammar".into());
    }
    for path in paths {
        schema::RelPath::parse(path)?;
        if path.starts_with(':')
            || path
                .bytes()
                .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'\\'))
        {
            return Err("Git broker requires literal relative path arguments".into());
        }
    }
    git_args.push("--".into());
    git_args.extend(paths.iter().cloned());
    Ok(BrokerIndexCommand {
        git_args,
        paths: paths.to_vec(),
    })
}

fn broker_intent<F>(
    session_dir: &Path,
    request_id: &str,
    request_sha256: &str,
    operation: &str,
    argv: &[String],
    build_details: F,
) -> Result<(JcsValue, bool), String>
where
    F: FnOnce() -> Result<JcsValue, String>,
{
    let path = session_dir
        .join("broker-intents")
        .join(format!("{request_id}.json"));
    let expected_argv = JcsValue::Array(argv.iter().cloned().map(JcsValue::String).collect());
    if path.exists() {
        let record = schema::parse_jcs(&capability::read_secure_bytes(&path)?, true)?;
        let object = closed_object(
            &record,
            &[
                "schema",
                "request_id",
                "request_sha256",
                "operation",
                "argv",
                "details",
            ],
            "GitBrokerIntentV1",
        )?;
        if object["schema"].as_str() != Ok(schema::SCHEMA_V1)
            || object["request_id"].as_str() != Ok(request_id)
            || object["request_sha256"].as_str() != Ok(request_sha256)
            || object["operation"].as_str() != Ok(operation)
            || object["argv"] != expected_argv
        {
            return Err("changed broker request conflicts with durable mutation intent".into());
        }
        return Ok((object["details"].clone(), true));
    }
    let details = build_details()?;
    let record = CanonicalRecord(JcsValue::Object(BTreeMap::from([
        ("argv".into(), expected_argv),
        ("details".into(), details.clone()),
        ("operation".into(), JcsValue::String(operation.into())),
        ("request_id".into(), JcsValue::String(request_id.into())),
        (
            "request_sha256".into(),
            JcsValue::String(request_sha256.into()),
        ),
        ("schema".into(), JcsValue::String(schema::SCHEMA_V1.into())),
    ])));
    authority::atomic_create_jcs(&path, &record, 0o600)?;
    Ok((details, false))
}

fn read_optional_git_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "securely open Git file {}: {error}",
                path.display()
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|e| format!("fstat Git file {}: {e}", path.display()))?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        return Err(format!(
            "Git file {} is not an EUID-owned single-link regular file",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("read Git file {}: {e}", path.display()))?;
    Ok(Some(bytes))
}
fn optional_digest(bytes: &Option<Vec<u8>>) -> JcsValue {
    bytes
        .as_ref()
        .map(|bytes| JcsValue::String(sha256::hex_digest(bytes)))
        .unwrap_or(JcsValue::Null)
}
fn digest_matches(value: &JcsValue, bytes: &Option<Vec<u8>>) -> Result<bool, String> {
    match (value, bytes) {
        (JcsValue::Null, None) => Ok(true),
        (JcsValue::String(expected), Some(bytes)) => {
            Sha256Digest::parse(expected)?;
            Ok(sha256::hex_digest(bytes) == *expected)
        }
        (JcsValue::Null, Some(_)) | (JcsValue::String(_), None) => Ok(false),
        _ => Err("broker intent index digest has invalid nullability".into()),
    }
}

fn prepare_index_plan(
    repository: &git::OpenedRepository,
    plan: &Path,
    index_before: &Option<Vec<u8>>,
    command: &BrokerIndexCommand,
) -> Result<String, String> {
    if let Some(bytes) = index_before {
        write_private_bytes(plan, bytes)?
    } else {
        let output = repository.run_git_output_with_index(&["read-tree", "HEAD"], plan)?;
        if !output.status.success() {
            return Err(format!(
                "prepare empty Git index plan failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    let args = command
        .git_args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let output = repository.run_git_output_with_index(&args, plan)?;
    if !output.status.success() {
        fs::remove_file(plan).ok();
        return Err(format!(
            "Git index operation failed before publication: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    fs::set_permissions(plan, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod Git index plan: {e}"))?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(plan)
        .map_err(|e| format!("open Git index plan: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("fsync Git index plan: {e}"))?;
    let bytes =
        read_optional_git_file(plan)?.ok_or_else(|| "Git index plan disappeared".to_string())?;
    Ok(sha256::hex_digest(&bytes))
}

fn publish_index_plan(
    index: &Path,
    plan: &Path,
    before: &JcsValue,
    planned_sha256: &str,
) -> Result<(), String> {
    let current = read_optional_git_file(index)?;
    if current
        .as_ref()
        .is_some_and(|bytes| sha256::hex_digest(bytes) == planned_sha256)
    {
        return Ok(());
    }
    if !digest_matches(before, &current)? {
        return Err("Git index differs from both durable intent precondition and planned result; refusing ambiguous replay".into());
    }
    let planned = read_optional_git_file(plan)?
        .ok_or_else(|| "durable Git index plan is missing".to_string())?;
    if sha256::hex_digest(&planned) != planned_sha256 {
        return Err("durable Git index plan digest differs from intent".into());
    }
    let parent = index
        .parent()
        .ok_or_else(|| "Git index has no parent".to_string())?;
    let staging = parent.join(format!(".session-relay-index-{}", crate::store::uuid_v4()));
    write_private_bytes(&staging, &planned)?;
    let still_current = read_optional_git_file(index)?;
    if !digest_matches(before, &still_current)? {
        fs::remove_file(&staging).ok();
        return Err("Git index changed while publishing durable broker intent".into());
    }
    fs::rename(&staging, index).map_err(|e| format!("publish planned Git index: {e}"))?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(parent)
        .map_err(|e| format!("open Git index directory for fsync: {e}"))?;
    directory
        .sync_all()
        .map_err(|e| format!("fsync Git index directory: {e}"))?;
    let published = read_optional_git_file(index)?
        .ok_or_else(|| "published Git index disappeared".to_string())?;
    if sha256::hex_digest(&published) != planned_sha256 {
        return Err("published Git index differs from durable plan".into());
    }
    Ok(())
}

fn broker_git_index(
    repository: &git::OpenedRepository,
    worktree: &Path,
    branch_ref: &str,
    session_dir: &Path,
    request_id: &str,
    request_sha256: &str,
    argv: &[String],
) -> Result<JcsValue, String> {
    let (_, claims) = manifest_claims(session_dir)?;
    let command = parse_broker_index_argv(argv)?;
    for path in &command.paths {
        let exact_file = claims
            .iter()
            .any(|claim| claim.path_type == "file" && claim.path == *path);
        if exact_file && worktree.join(path).is_dir() {
            return Err("file claim cannot address a directory pathspec".into());
        }
    }
    let paths = command
        .paths
        .iter()
        .map(|path| git::NameStatusChange {
            status: "A".into(),
            source: None,
            destination: path.clone(),
        })
        .collect::<Vec<_>>();
    git::validate_changed_paths(&paths, &claims)?;
    if repository.run_git_text(&["symbolic-ref", "-q", "HEAD"])? != branch_ref {
        return Err("workspace branch changed before broker index mutation".into());
    }
    let index = PathBuf::from(repository.run_git_text(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "index",
    ])?);
    let plan = session_dir
        .join("broker-plans")
        .join(format!("{request_id}.index"));
    let (details, _) = broker_intent(
        session_dir,
        request_id,
        request_sha256,
        "git_index",
        argv,
        || {
            let before = read_optional_git_file(&index)?;
            let planned_sha256 = prepare_index_plan(repository, &plan, &before, &command)?;
            Ok(JcsValue::Object(BTreeMap::from([
                (
                    "planned_index_sha256".into(),
                    JcsValue::String(planned_sha256),
                ),
                ("pre_index_sha256".into(), optional_digest(&before)),
            ])))
        },
    )?;
    let details = closed_object(
        &details,
        &["pre_index_sha256", "planned_index_sha256"],
        "GitIndexIntentV1",
    )?;
    let planned_sha256 = details["planned_index_sha256"].as_str()?;
    Sha256Digest::parse(planned_sha256)?;
    publish_index_plan(&index, &plan, &details["pre_index_sha256"], planned_sha256)?;
    let changed = git::parse_name_status_z(&repository.run_git_bytes(&[
        "diff",
        "--cached",
        "--name-status",
        "-z",
        "--find-renames",
        "--find-copies",
    ])?)?;
    git::validate_changed_paths(&changed, &claims)?;
    Ok(broker_response(request_id, "ok", 0, "", "", JcsValue::Null))
}

fn validate_planned_commit(
    repository: &git::OpenedRepository,
    commit: &str,
    pre_head: &str,
    tree: &str,
    timestamp: &str,
    message: &str,
) -> Result<(), String> {
    if repository.run_git_text(&["rev-parse", "--verify", &format!("{commit}^")])? != pre_head
        || repository.run_git_text(&["rev-parse", "--verify", &format!("{commit}^{{tree}}")])?
            != tree
    {
        return Err("advanced broker branch does not match the durable commit intent".into());
    }
    for (format, expected) in [
        ("%an", "Session Relay"),
        ("%ae", "session-relay@localhost"),
        ("%cn", "Session Relay"),
        ("%ce", "session-relay@localhost"),
    ] {
        if repository.run_git_text(&["show", "-s", &format!("--format={format}"), commit])?
            != expected
        {
            return Err("advanced broker commit identity differs from durable intent".into());
        }
    }
    let expected_time = format!("{}+00:00", &timestamp[..19]);
    for format in ["%aI", "%cI"] {
        if repository.run_git_text(&["show", "-s", &format!("--format={format}"), commit])?
            != expected_time
        {
            return Err("advanced broker commit timestamp differs from durable intent".into());
        }
    }
    if repository.run_git_text(&["show", "-s", "--format=%B", commit])?
        != message.trim_end_matches(['\r', '\n'])
    {
        return Err("advanced broker commit message differs from durable intent".into());
    }
    Ok(())
}

fn broker_git_commit(
    repository: &git::OpenedRepository,
    _worktree: &Path,
    branch_ref: &str,
    session_dir: &Path,
    request_id: &str,
    request_sha256: &str,
    argv: &[String],
) -> Result<JcsValue, String> {
    let (_, claims) = manifest_claims(session_dir)?;
    if argv.len() != 3 || argv[0] != "commit" || argv[1] != "-m" {
        return Err("Git broker commit grammar is exactly commit -m <message>".into());
    }
    let changed = git::parse_name_status_z(&repository.run_git_bytes(&[
        "diff",
        "--cached",
        "--name-status",
        "-z",
        "--find-renames",
        "--find-copies",
    ])?)?;
    if changed.is_empty() {
        return Err("Git broker refuses an empty worker commit".into());
    }
    git::validate_changed_paths(&changed, &claims)?;
    if repository.run_git_text(&["symbolic-ref", "-q", "HEAD"])? != branch_ref {
        return Err("workspace branch changed before broker commit".into());
    }
    let (details, _) = broker_intent(
        session_dir,
        request_id,
        request_sha256,
        "git_commit",
        argv,
        || {
            let pre_head = repository.run_git_text(&["rev-parse", "--verify", "HEAD"])?;
            let tree = repository.run_git_text(&["write-tree"])?;
            let timestamp = authority::now_timestamp()?;
            Ok(JcsValue::Object(BTreeMap::from([
                ("message".into(), JcsValue::String(argv[2].clone())),
                ("pre_head".into(), JcsValue::String(pre_head)),
                ("timestamp".into(), JcsValue::String(timestamp)),
                ("tree".into(), JcsValue::String(tree)),
            ])))
        },
    )?;
    let details = closed_object(
        &details,
        &["message", "pre_head", "timestamp", "tree"],
        "GitCommitIntentV1",
    )?;
    let pre_head = details["pre_head"].as_str()?;
    let tree = details["tree"].as_str()?;
    let timestamp = details["timestamp"].as_str()?;
    let message = details["message"].as_str()?;
    schema::Timestamp::parse(timestamp)?;
    repository.validate_oid(pre_head)?;
    repository.validate_oid(tree)?;
    if repository.run_git_text(&["write-tree"])? != tree {
        return Err("Git index tree changed after durable commit intent".into());
    }
    let head = repository.run_git_text(&["rev-parse", "--verify", "HEAD"])?;
    let commit = if head == pre_head {
        git::create_worker_commit(repository, branch_ref, message, timestamp)?
    } else {
        validate_planned_commit(repository, &head, pre_head, tree, timestamp, message)?;
        head
    };
    Ok(broker_response(
        request_id,
        "ok",
        0,
        &format!("{commit}\n"),
        "",
        JcsValue::Null,
    ))
}

fn broker_handback(
    repository: &git::OpenedRepository,
    _worktree: &Path,
    branch_ref: &str,
    session_dir: &Path,
    request_id: &str,
    request_sha256: &str,
    argv: &[String],
) -> Result<JcsValue, String> {
    if argv.len() != 2 {
        return Err("broker handback requires request path and digest".into());
    }
    let request_path = Path::new(&argv[0]);
    let bytes = capability::read_secure_bytes(request_path)
        .map_err(|error| format!("read handback request: {error}"))?;
    if sha256::hex_digest(&bytes) != argv[1] {
        return Err("handback request digest mismatch".into());
    }
    let request = HandbackRequestV1::from_jcs(schema::parse_jcs(&bytes, true)?)?;
    if request.session_id
        != session_dir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("")
    {
        return Err("handback request session differs from broker".into());
    }
    if repository.run_git_text(&["symbolic-ref", "-q", "HEAD"])? != branch_ref {
        return Err("workspace branch changed before handback".into());
    }
    if !repository
        .run_git_bytes(&["status", "--porcelain=v2", "-z"])?
        .is_empty()
    {
        return Err("workspace is dirty at handback".into());
    }
    let head = repository.run_git_text(&["rev-parse", "--verify", "HEAD"])?;
    if head != request.expected_head {
        return Err("handback expected HEAD differs from workspace HEAD".into());
    }
    repository.validate_oid(&head)?;
    let (manifest, claims) = manifest_claims(session_dir)?;
    if manifest.state()? != WorkspaceState::Running {
        return Err("handback requires a durably Running workspace".into());
    }
    let manifest_object = manifest.object()?;
    let base = manifest_object["worker_base_commit"].as_str()?;
    let changes = git::parse_name_status_z(&repository.run_git_bytes(&[
        "diff",
        "--name-status",
        "-z",
        "--find-renames",
        "--find-copies",
        &format!("{base}..{head}"),
    ])?)?;
    git::validate_changed_paths(&changes, &claims)?;
    let worker_commits = repository
        .run_git_text(&["rev-list", "--reverse", &format!("{base}..{head}")])?
        .lines()
        .map(str::to_string)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    for commit in &worker_commits {
        let parents = repository.run_git_text(&["rev-list", "--parents", "-n", "1", commit])?;
        if parents.split_whitespace().count() != 2 {
            return Err("handback produced history is not linear and merge-free".into());
        }
    }
    let applied = manifest_object["applied_wip_commit"].as_str()?.to_string();
    let mut produced_commits = Vec::with_capacity(worker_commits.len() + 1);
    produced_commits.push(applied);
    produced_commits.extend(worker_commits);
    let receipt = HandbackReceiptV1 {
        request_id: request.request_id,
        session_id: request.session_id,
        head_oid: head,
        outcome: "validated".into(),
        produced_commits,
        created_at: request.created_at,
    };
    receipt.validate()?;
    let receipt_path = session_dir.join("handback-receipt-v1.json");
    let (_, existing_intent) = broker_intent(
        session_dir,
        request_id,
        request_sha256,
        "handback",
        argv,
        || Ok(JcsValue::Object(BTreeMap::new())),
    )?;
    if receipt_path.exists() {
        if !existing_intent {
            return Err("handback receipt predates its durable broker intent".into());
        }
        let existing: HandbackReceiptV1 = schema::read_jcs_file(&receipt_path, None)?;
        if existing != receipt {
            return Err("handback receipt differs from durable replay result".into());
        }
        return Ok(broker_response(
            request_id,
            "ok",
            0,
            "",
            "",
            existing.to_jcs(),
        ));
    }
    authority::atomic_create_jcs(&receipt_path, &receipt, 0o600)?;
    Ok(broker_response(
        request_id,
        "ok",
        0,
        "",
        "",
        receipt.to_jcs(),
    ))
}

fn broker_response(
    request_id: &str,
    status: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    receipt: JcsValue,
) -> JcsValue {
    JcsValue::Object(BTreeMap::from([
        ("exit_code".into(), JcsValue::String(exit_code.to_string())),
        ("receipt".into(), receipt),
        ("request_id".into(), JcsValue::String(request_id.into())),
        ("schema".into(), JcsValue::String(schema::SCHEMA_V1.into())),
        ("status".into(), JcsValue::String(status.into())),
        ("stderr".into(), JcsValue::String(stderr.into())),
        ("stdout".into(), JcsValue::String(stdout.into())),
    ]))
}

fn run_broker_client(raw: &[String]) -> Result<i32, String> {
    let divider = raw
        .iter()
        .position(|arg| arg == "--")
        .ok_or_else(|| "broker client requires -- before Git arguments".to_string())?;
    let flags = Flags::parse(&raw[..divider], &["--worker-capability-file"])?;
    let capability_file = flags.absolute("--worker-capability-file")?;
    let argv = raw[divider + 1..].to_vec();
    let operation = match argv.first().map(String::as_str) {
        Some("add" | "rm" | "restore") => {
            parse_broker_index_argv(&argv)?;
            "git_index"
        }
        Some("commit") => "git_commit",
        Some(other) => {
            return Err(format!(
                "Git operation {other} is refused by the closed broker"
            ));
        }
        None => return Err("Git broker requires an operation".into()),
    };
    let capability: WorkerCapabilityV1 = schema::read_jcs_file(&capability_file, None)?;
    let response = broker_exchange(
        &capability,
        operation,
        argv,
        std::env::current_dir().map_err(|e| format!("resolve broker client cwd: {e}"))?,
    )?;
    let object = match response {
        JcsValue::Object(object) => object,
        _ => return Err("broker response is not an object".into()),
    };
    print!("{}", object["stdout"].as_str()?);
    eprint!("{}", object["stderr"].as_str()?);
    object["exit_code"]
        .as_str()?
        .parse()
        .map_err(|_| "broker exit code is invalid".to_string())
}
pub fn run(raw: Vec<String>) -> ! {
    if raw.first().map(String::as_str) == Some("__guardian") {
        match run_guardian(&raw[1..]) {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1)
            }
        }
    }
    if raw.first().map(String::as_str) == Some("__custody-supervisor") {
        match run_custody_supervisor(&raw[1..]) {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1)
            }
        }
    }
    if raw.first().map(String::as_str) == Some("__broker") {
        match run_broker(&raw[1..]) {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1)
            }
        }
    }
    if raw.first().map(String::as_str) == Some("__broker-client") {
        match run_broker_client(&raw[1..]) {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1)
            }
        }
    }
    let result = parse_command(&raw).and_then(execute);
    match result {
        Ok(output) => {
            print!("{output}");
            std::process::exit(0)
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;
    fn readiness_capability(secret: &str) -> WorkerCapabilityV1 {
        WorkerCapabilityV1 {
            capability_id: "00000000-0000-4000-8000-000000000001".into(),
            repository_id: "a".repeat(64),
            session_id: "00000000-0000-4000-8000-000000000002".into(),
            generation: "1".into(),
            actions: vec!["git_index".into(), "git_commit".into(), "handback".into()],
            secret_b64url: secret.into(),
            broker_socket: "/tmp/broker.sock".into(),
            issued_at: "2026-07-22T12:34:56.789Z".into(),
            expires_at: "9999-12-31T23:59:59.999Z".into(),
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn running_relay_rejects_a_mismatched_sealed_fd() {
        let mismatched = File::open("/dev/null").unwrap().into_raw_fd();
        assert_eq!(
            adopt_running_relay_fd(mismatched).unwrap_err(),
            "running relay executable differs from the sealed relay FD identity"
        );
    }
    #[test]
    fn broker_readiness_is_authenticated_to_the_worker_capability() {
        let first = readiness_capability(&capability::encode_base64url(&[7; 32]));
        let other = readiness_capability(&capability::encode_base64url(&[8; 32]));
        let (mut reader, writer) = broker_readiness_pair(&first).unwrap();
        publish_broker_ready(writer.into_raw_fd(), &other).unwrap();
        assert_eq!(
            reader.observe().unwrap_err(),
            "Git broker readiness authentication failed"
        );
    }
    #[test]
    fn broker_readiness_requires_durable_listener_publication_signal() {
        let capability = readiness_capability(&capability::encode_base64url(&[7; 32]));
        let (mut reader, writer) = broker_readiness_pair(&capability).unwrap();
        publish_broker_ready(writer.into_raw_fd(), &capability).unwrap();
        assert_eq!(reader.observe().unwrap(), BrokerReadinessState::Ready);
    }
    #[test]
    fn broker_readiness_refuses_a_child_that_closes_without_publishing() {
        let capability = readiness_capability(&capability::encode_base64url(&[7; 32]));
        let (mut reader, writer) = broker_readiness_pair(&capability).unwrap();
        drop(writer);
        assert_eq!(reader.observe().unwrap(), BrokerReadinessState::Closed);
    }
    #[test]
    fn exact_router_is_closed() {
        let args = vec![
            "preserve".into(),
            "--request-file".into(),
            "/tmp/request.json".into(),
            "--request-sha256".into(),
            "a".repeat(64),
        ];
        assert!(matches!(
            parse_command(&args),
            Ok(WorkspaceCommand::Preserve { .. })
        ));
        let mut bad = args;
        bad.extend(["--extra".into(), "x".into()]);
        assert!(parse_command(&bad).is_err());
    }
    #[test]
    fn manifest_wip_fields_are_state_coupled() {
        let oid = JcsValue::String("a".repeat(40));
        assert!(validate_manifest_wip_nullability(true, &JcsValue::Null, &JcsValue::Null).is_ok());
        assert!(validate_manifest_wip_nullability(false, &oid, &oid).is_ok());
        assert!(validate_manifest_wip_nullability(true, &JcsValue::Null, &oid).is_err());
        assert!(validate_manifest_wip_nullability(true, &oid, &JcsValue::Null).is_err());
        assert!(
            validate_manifest_wip_nullability(false, &JcsValue::Null, &JcsValue::Null).is_err()
        );
    }
    #[test]
    fn broker_index_grammar_is_exact() {
        let strings = |values: &[&str]| {
            values
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            parse_broker_index_argv(&strings(&["add", "--", "src/a.rs"]))
                .unwrap()
                .git_args,
            strings(&["add", "--", "src/a.rs"])
        );
        assert_eq!(
            parse_broker_index_argv(&strings(&["rm", "--cached", "src/a.rs"]))
                .unwrap()
                .git_args,
            strings(&["rm", "--cached", "--", "src/a.rs"])
        );
        assert_eq!(
            parse_broker_index_argv(&strings(&["restore", "--staged", "--", "src/a.rs"]))
                .unwrap()
                .git_args,
            strings(&["restore", "--staged", "--", "src/a.rs"])
        );
        for refused in [
            strings(&["mv", "a", "b"]),
            strings(&["rm", "a"]),
            strings(&["restore", "a"]),
            strings(&["add", "-A"]),
        ] {
            assert!(parse_broker_index_argv(&refused).is_err())
        }
    }
    #[test]
    fn broker_identity_binds_repository_and_private_git_inode() {
        use schema::{ObjectFormat, RepositoryIdentityV1, WorktreeIdentityV1};
        let repository = RepositoryIdentityV1 {
            repository_id: "a".repeat(64),
            common_dir_realpath: "/repo/.git".into(),
            common_dir_dev: "1".into(),
            common_dir_ino: "2".into(),
            common_dir_owner_euid: "3".into(),
            euid: "3".into(),
            object_format: ObjectFormat::Sha1,
        };
        let worktree = WorktreeIdentityV1 {
            identity_sha256: "b".repeat(64),
            root_realpath: "/repo/w".into(),
            root_dev: "4".into(),
            root_ino: "5".into(),
            root_owner_euid: "3".into(),
            private_git_dir_realpath: "/repo/.git/worktrees/w".into(),
            private_git_dir_dev: "1".into(),
            private_git_dir_ino: "6".into(),
            branch_ref: "refs/heads/docks/x/task".into(),
        };
        assert!(
            validate_broker_identity_values(
                &repository.repository_id,
                &repository,
                &repository,
                &worktree,
                &worktree
            )
            .is_ok()
        );
        let mut replaced = worktree.clone();
        replaced.private_git_dir_ino = "7".into();
        assert!(
            validate_broker_identity_values(
                &repository.repository_id,
                &repository,
                &repository,
                &replaced,
                &worktree
            )
            .is_err()
        );
        assert!(
            validate_broker_identity_values(
                &"c".repeat(64),
                &repository,
                &repository,
                &worktree,
                &worktree
            )
            .is_err()
        );
    }
    #[test]
    fn durable_broker_intent_precedes_and_authenticates_replay() {
        let root =
            std::env::temp_dir().join(format!("session-relay-intent-{}", crate::store::uuid_v4()));
        authority::ensure_private_directory(&root.join("broker-intents"), unsafe {
            libc::geteuid()
        })
        .unwrap();
        let request_id = "11111111-1111-4111-8111-111111111111";
        let argv = vec!["commit".into(), "-m".into(), "message".into()];
        let details = JcsValue::Object(BTreeMap::from([(
            "pre_head".into(),
            JcsValue::String("a".repeat(40)),
        )]));
        let (created, existing) = broker_intent(
            &root,
            request_id,
            &"b".repeat(64),
            "git_commit",
            &argv,
            || Ok(details.clone()),
        )
        .unwrap();
        assert_eq!(created, details);
        assert!(!existing);
        assert!(
            root.join("broker-intents")
                .join(format!("{request_id}.json"))
                .exists()
        );
        let (replayed, existing) = broker_intent(
            &root,
            request_id,
            &"b".repeat(64),
            "git_commit",
            &argv,
            || panic!("durable replay must not rebuild intent"),
        )
        .unwrap();
        assert_eq!(replayed, details);
        assert!(existing);
        assert!(
            broker_intent(
                &root,
                request_id,
                &"c".repeat(64),
                "git_commit",
                &argv,
                || Ok(JcsValue::Null)
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn broker_replay_record_is_atomic_byte_stable_and_changed_requests_fail_closed() {
        let root =
            std::env::temp_dir().join(format!("session-relay-replay-{}", crate::store::uuid_v4()));
        authority::ensure_private_directory(&root.join("broker-replays"), unsafe {
            libc::geteuid()
        })
        .unwrap();
        authority::ensure_private_directory(&root.join("broker-intents"), unsafe {
            libc::geteuid()
        })
        .unwrap();
        let request_id = "21111111-1111-4111-8111-111111111111";
        let digest = "b".repeat(64);
        let response = broker_response(request_id, "ok", 0, "result\n", "", JcsValue::Null);
        persist_broker_replay(&root, request_id, &digest, &response).unwrap();
        assert!(
            !root
                .join("broker-replays")
                .join(format!("{request_id}.sha256"))
                .exists(),
            "new replay publication split the request digest from the response"
        );
        let record_path = broker_replay_path(&root, request_id);
        let first = fs::read(&record_path).unwrap();
        assert_eq!(
            durable_broker_replay(&root, request_id, &digest, "git_commit").unwrap(),
            Some(response.clone())
        );
        assert_eq!(fs::read(&record_path).unwrap(), first);
        assert!(durable_broker_replay(&root, request_id, &"c".repeat(64), "git_commit").is_err());
        assert_eq!(fs::read(&record_path).unwrap(), first);

        let legacy_id = "31111111-1111-4111-8111-111111111111";
        let legacy_digest = "d".repeat(64);
        let argv = vec!["commit".into(), "-m".into(), "message".into()];
        broker_intent(
            &root,
            legacy_id,
            &legacy_digest,
            "git_commit",
            &argv,
            || Ok(JcsValue::Object(BTreeMap::new())),
        )
        .unwrap();
        let legacy_response = broker_response(legacy_id, "ok", 0, "legacy\n", "", JcsValue::Null);
        authority::atomic_create_jcs(
            &root
                .join("broker-replays")
                .join(format!("{legacy_id}.json")),
            &CanonicalRecord(legacy_response.clone()),
            0o600,
        )
        .unwrap();
        assert_eq!(
            durable_broker_replay(&root, legacy_id, &legacy_digest, "git_commit").unwrap(),
            Some(legacy_response)
        );
        assert!(
            broker_replay_path(&root, legacy_id).exists(),
            "legacy response without a digest was not reconstructed from durable mutation intent"
        );
        assert!(durable_broker_replay(&root, legacy_id, &"e".repeat(64), "git_commit").is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn revoked_handback_reconstructs_response_only_from_intent_and_receipt() {
        let root = std::env::temp_dir().join(format!(
            "session-relay-handback-replay-{}",
            crate::store::uuid_v4()
        ));
        authority::ensure_private_directory(&root.join("broker-intents"), unsafe {
            libc::geteuid()
        })
        .unwrap();
        let broker_request_id = "41111111-1111-4111-8111-111111111111";
        let coordinator_request_id = "51111111-1111-4111-8111-111111111111";
        let session_id = "61111111-1111-4111-8111-111111111111";
        let digest = "f".repeat(64);
        let argv = vec!["/tmp/handback.json".into(), "a".repeat(64)];
        broker_intent(&root, broker_request_id, &digest, "handback", &argv, || {
            Ok(JcsValue::Object(BTreeMap::new()))
        })
        .unwrap();
        let receipt = HandbackReceiptV1 {
            request_id: coordinator_request_id.into(),
            session_id: session_id.into(),
            head_oid: "a".repeat(40),
            outcome: "validated".into(),
            produced_commits: vec!["a".repeat(40)],
            created_at: "2026-07-22T00:00:00.000Z".into(),
        };
        authority::atomic_create_jcs(&root.join("handback-receipt-v1.json"), &receipt, 0o600)
            .unwrap();
        let response =
            recover_unrecorded_handback_response(&root, broker_request_id, &digest, &argv)
                .unwrap()
                .unwrap();
        let object = response.object().unwrap();
        assert_eq!(object["request_id"].as_str().unwrap(), broker_request_id);
        assert_eq!(object["receipt"], receipt.to_jcs());
        assert!(
            recover_unrecorded_handback_response(&root, broker_request_id, &"e".repeat(64), &argv)
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn durable_index_plan_reconciles_pre_and_post_mutation_states() {
        let root = std::env::temp_dir().join(format!(
            "session-relay-index-plan-{}",
            crate::store::uuid_v4()
        ));
        fs::create_dir(&root).unwrap();
        let index = root.join("index");
        let plan = root.join("plan");
        fs::write(&index, b"before").unwrap();
        fs::write(&plan, b"planned").unwrap();
        let before = JcsValue::String(sha256::hex_digest(b"before"));
        let planned = sha256::hex_digest(b"planned");
        publish_index_plan(&index, &plan, &before, &planned).unwrap();
        assert_eq!(fs::read(&index).unwrap(), b"planned");
        publish_index_plan(&index, &plan, &before, &planned).unwrap();
        fs::write(&index, b"unrelated").unwrap();
        assert!(publish_index_plan(&index, &plan, &before, &planned).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn git_shim_is_create_new_no_follow_and_fsynced_at_final_mode() {
        use std::os::unix::fs::symlink;
        let root =
            std::env::temp_dir().join(format!("session-relay-shim-{}", crate::store::uuid_v4()));
        fs::create_dir(&root).unwrap();
        let output = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let capability = root.join("capability.json");
        let shim = create_git_shim(&root, &capability, Path::new("/bin/true")).unwrap();
        let metadata = fs::symlink_metadata(&shim).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.mode() & 0o777, 0o500);
        assert_eq!(metadata.nlink(), 1);
        fs::remove_file(&shim).unwrap();
        let outside = root.join("outside");
        fs::write(&outside, b"unchanged").unwrap();
        symlink(&outside, &shim).unwrap();
        assert!(create_git_shim(&root, &capability, Path::new("/bin/true")).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"unchanged");
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn git_shim_runtime_is_exact_and_capability_is_pinned_readable() {
        let root =
            std::env::temp_dir().join(format!("session-relay-runtime-{}", crate::store::uuid_v4()));
        let session_relay = root.join("private-git/session-relay");
        let capabilities = session_relay.join("worker-capabilities");
        let bin = session_relay.join("bin");
        for directory in [
            &root,
            root.join("private-git").as_path(),
            &session_relay,
            &capabilities,
            &bin,
        ] {
            authority::ensure_private_directory(directory, unsafe { libc::geteuid() }).unwrap();
        }
        let capability_path = capabilities.join("00000000000000000001.json");
        write_private_bytes(&capability_path, b"{}").unwrap();
        let relay = std::env::current_exe().unwrap();
        let shim = bin.join("git");
        fs::write(&shim, git_shim_body(&capability_path, &relay)).unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o500)).unwrap();
        let environment = BTreeMap::from([
            (
                "DOCKS_WORKER_CAPABILITY_FILE".into(),
                capability_path.to_string_lossy().into_owned(),
            ),
            (
                "PATH".into(),
                format!("{}:/usr/local/bin:/usr/bin:/bin", bin.display()),
            ),
        ]);
        assert_eq!(
            verified_git_shim_runtime(&environment, &relay).unwrap(),
            (shim.clone(), capability_path)
        );
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&shim, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(
            verified_git_shim_runtime(&environment, &relay)
                .unwrap_err()
                .contains("command chain differs")
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn broker_adoption_restores_cloexec_before_git_children() {
        let fd = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD, 3) };
        assert!(fd >= 3);
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
            0
        );
        let (lease, _) = adopt_broker_descriptors(fd, Vec::new()).unwrap();
        let adopted = unsafe { libc::fcntl(lease.as_raw_fd(), libc::F_GETFD) };
        assert!(
            adopted & libc::FD_CLOEXEC != 0,
            "broker lease remained inheritable"
        );
        let alias = format!(
            "alias.fdcheck=!if test -e /proc/self/fd/{}; then exit 1; fi",
            lease.as_raw_fd()
        );
        let output = Command::new("git")
            .args(["-c", &alias, "fdcheck"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Git child inherited broker lease FD: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn broker_spawn_preserves_exact_descriptor_numbers_and_parent_flags() {
        let lease = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
        let resource = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
        assert!(lease > libc::STDERR_FILENO && resource > libc::STDERR_FILENO && lease != resource);
        let descriptors = [lease, resource];
        let before = descriptors.map(|fd| unsafe { libc::fcntl(fd, libc::F_GETFD) });
        let prior = set_spawn_inheritance(&descriptors, true).unwrap();
        assert!(
            descriptors
                .iter()
                .all(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } & libc::FD_CLOEXEC == 0)
        );
        restore_spawn_inheritance(&prior).unwrap();
        assert_eq!(
            descriptors.map(|fd| unsafe { libc::fcntl(fd, libc::F_GETFD) }),
            before
        );
        for fd in descriptors {
            unsafe { libc::close(fd) };
        }
    }
    #[test]
    fn acknowledged_integration_blocked_finish_routes_only_to_retained_abort() {
        assert_eq!(
            finish_routes_to_retained_abort(WorkspaceState::IntegrationBlocked, true),
            Ok(true)
        );
        assert!(
            finish_routes_to_retained_abort(WorkspaceState::IntegrationBlocked, false).is_err()
        );
        assert_eq!(
            finish_routes_to_retained_abort(WorkspaceState::Integrated, false),
            Ok(false)
        );
        assert!(finish_routes_to_retained_abort(WorkspaceState::Integrated, true).is_err());
        assert!(
            !WorkspaceState::IntegrationBlocked
                .may_transition_to(WorkspaceState::IntegrationQueued)
        );
        assert!(!WorkspaceState::IntegrationBlocked.may_transition_to(WorkspaceState::Running));
    }
    #[test]
    fn empty_broker_resource_fds_have_a_nonempty_internal_encoding() {
        assert_eq!(encode_fd_list(&[]), "none");
        assert!(parse_fd_list("none").unwrap().is_empty());
        assert!(parse_fd_list("").unwrap().is_empty());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_custody_fault_classification_is_exact() {
        assert_eq!(
            classify_runtime_custody_fault(custody::HEARTBEAT_FENCE_ERROR),
            "supervisor_heartbeat_deadline"
        );
        assert_eq!(
            classify_runtime_custody_fault("custody packet credentials changed"),
            "supervisor_control_fault"
        );
    }
    #[test]
    fn broker_socket_path_is_private_and_bounded_for_unix_domain_sockets() {
        let repository_id = sha256::hex_digest(crate::store::uuid_v4().as_bytes());
        let session_id = crate::store::uuid_v4();
        let socket =
            broker_socket_path(unsafe { libc::geteuid() }, &repository_id, &session_id).unwrap();
        assert!(socket.is_absolute());
        assert!(socket.as_os_str().len() < 100);
        let parent = socket.parent().unwrap();
        assert_eq!(fs::metadata(parent).unwrap().mode() & 0o777, 0o700);
        fs::remove_dir(parent).unwrap();
        fs::remove_dir(parent.parent().unwrap()).ok();
    }
}
