use super::git::RepoIdentity;
use crate::protocol::{
    ClaimOrigin, ClaimState, ClaimStatusV1, DeliveryState, MessageKind, ProtocolStore,
    TerminalStatus, WorkerResultV1, jcs_from_tinyjson,
};
use crate::store::{self, Entry};
use crate::workspace::schema::{ClosedJcs, ObjectFormat, RepositoryIdentityV1, read_jcs_file};
use rustix::fs::{FlockOperation, flock};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tinyjson::JsonValue;

const FANOUT_FILE: &str = "fanout-v1.json";
const FANOUT_SCHEMA: &str = "1";
const FANOUT_CAP: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanoutMode {
    Root,
    Child,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanoutState {
    Reserved,
    Running,
    HandedBack,
    Collecting,
    Collected,
    FailedNoProcess,
}

impl FanoutState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "Reserved",
            Self::Running => "Running",
            Self::HandedBack => "HandedBack",
            Self::Collecting => "Collecting",
            Self::Collected => "Collected",
            Self::FailedNoProcess => "FailedNoProcess",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "Reserved" => Self::Reserved,
            "Running" => Self::Running,
            "HandedBack" => Self::HandedBack,
            "Collecting" => Self::Collecting,
            "Collected" => Self::Collected,
            "FailedNoProcess" => Self::FailedNoProcess,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectionPhase {
    Prepared,
    Merged,
    WorktreeRemoved,
}

impl CollectionPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "Prepared",
            Self::Merged => "Merged",
            Self::WorktreeRemoved => "WorktreeRemoved",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "Prepared" => Self::Prepared,
            "Merged" => Self::Merged,
            "WorktreeRemoved" => Self::WorktreeRemoved,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FanoutRecord {
    pub reservation_id: String,
    pub parent_session_id: String,
    pub root_reservation_id: String,
    pub depth: u8,
    pub state: FanoutState,
    pub version: String,
    pub repo_common_dir: String,
    pub repo_dev: String,
    pub repo_ino: String,
    pub object_format: Option<String>,
    pub worktree: String,
    pub branch: String,
    pub base_sha: String,
    pub worker_id: Option<String>,
    pub generation: Option<String>,
    pub runtime_session_id: Option<String>,
    pub correlation_id: Option<String>,
    pub request_message_id: Option<String>,
    pub handback_head: Option<String>,
    pub handback_status: Option<String>,
    pub handback_note: Option<String>,
    pub worker_result: Option<WorkerResultV1>,
    pub worker_result_sha256: Option<String>,
    pub collection_phase: Option<CollectionPhase>,
    pub last_error: Option<String>,
}

impl FanoutRecord {
    fn to_json(&self) -> JsonValue {
        let mut object = HashMap::new();
        object.insert(
            "reservation_id".into(),
            JsonValue::from(self.reservation_id.clone()),
        );
        object.insert(
            "parent_session_id".into(),
            JsonValue::from(self.parent_session_id.clone()),
        );
        object.insert(
            "root_reservation_id".into(),
            JsonValue::from(self.root_reservation_id.clone()),
        );
        object.insert("depth".into(), JsonValue::from(self.depth.to_string()));
        object.insert(
            "state".into(),
            JsonValue::from(self.state.as_str().to_string()),
        );
        object.insert("version".into(), JsonValue::from(self.version.clone()));
        object.insert(
            "repo_common_dir".into(),
            JsonValue::from(self.repo_common_dir.clone()),
        );
        object.insert("repo_dev".into(), JsonValue::from(self.repo_dev.clone()));
        object.insert("repo_ino".into(), JsonValue::from(self.repo_ino.clone()));
        insert_optional(&mut object, "object_format", &self.object_format);
        object.insert("worktree".into(), JsonValue::from(self.worktree.clone()));
        object.insert("branch".into(), JsonValue::from(self.branch.clone()));
        object.insert("base_sha".into(), JsonValue::from(self.base_sha.clone()));
        insert_optional(&mut object, "worker_id", &self.worker_id);
        insert_optional(&mut object, "generation", &self.generation);
        insert_optional(&mut object, "runtime_session_id", &self.runtime_session_id);
        insert_optional(&mut object, "correlation_id", &self.correlation_id);
        insert_optional(&mut object, "request_message_id", &self.request_message_id);
        insert_optional(&mut object, "handback_head", &self.handback_head);
        insert_optional(&mut object, "handback_status", &self.handback_status);
        insert_optional(&mut object, "handback_note", &self.handback_note);
        object.insert(
            "worker_result".into(),
            self.worker_result
                .as_ref()
                .map(worker_result_to_json)
                .unwrap_or(JsonValue::from(())),
        );
        insert_optional(
            &mut object,
            "worker_result_sha256",
            &self.worker_result_sha256,
        );
        object.insert(
            "collection_phase".into(),
            self.collection_phase
                .map(|phase| JsonValue::from(phase.as_str().to_string()))
                .unwrap_or(JsonValue::from(())),
        );
        insert_optional(&mut object, "last_error", &self.last_error);
        JsonValue::from(object)
    }

    fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.get::<HashMap<String, JsonValue>>()?;
        const LEGACY_KEYS: [&str; 20] = [
            "reservation_id",
            "parent_session_id",
            "root_reservation_id",
            "depth",
            "state",
            "version",
            "repo_common_dir",
            "repo_dev",
            "repo_ino",
            "worktree",
            "branch",
            "base_sha",
            "worker_id",
            "generation",
            "runtime_session_id",
            "handback_head",
            "handback_status",
            "handback_note",
            "collection_phase",
            "last_error",
        ];
        let has_exact_keys = |extras: &[&str]| {
            object.len() == LEGACY_KEYS.len() + extras.len()
                && LEGACY_KEYS.iter().all(|key| object.contains_key(*key))
                && extras.iter().all(|key| object.contains_key(*key))
        };
        if !has_exact_keys(&[])
            && !has_exact_keys(&["object_format"])
            && !has_exact_keys(&[
                "object_format",
                "correlation_id",
                "request_message_id",
                "worker_result",
                "worker_result_sha256",
            ])
        {
            return None;
        }
        let string = |key: &str| object.get(key)?.get::<String>().cloned();
        let depth = string("depth")?.parse::<u8>().ok()?;
        if depth > 1 {
            return None;
        }
        let version = string("version")?;
        canonical_version(&version).ok()?;
        let record = Self {
            reservation_id: string("reservation_id")?,
            parent_session_id: string("parent_session_id")?,
            root_reservation_id: string("root_reservation_id")?,
            depth,
            state: FanoutState::parse(&string("state")?)?,
            version,
            repo_common_dir: string("repo_common_dir")?,
            repo_dev: string("repo_dev")?,
            repo_ino: string("repo_ino")?,
            object_format: optional_object_format(object)?,
            worktree: string("worktree")?,
            branch: string("branch")?,
            base_sha: string("base_sha")?,
            worker_id: optional_string(object, "worker_id"),
            generation: optional_string(object, "generation"),
            runtime_session_id: optional_string(object, "runtime_session_id"),
            correlation_id: optional_uuid(object, "correlation_id")?,
            request_message_id: optional_uuid(object, "request_message_id")?,
            handback_head: optional_string(object, "handback_head"),
            handback_status: optional_string(object, "handback_status"),
            handback_note: optional_string(object, "handback_note"),
            worker_result: optional_worker_result(object)?,
            worker_result_sha256: optional_sha256(object, "worker_result_sha256")?,
            collection_phase: match optional_string(object, "collection_phase") {
                Some(phase) => Some(CollectionPhase::parse(&phase)?),
                None => None,
            },
            last_error: optional_string(object, "last_error"),
        };
        let has_result = record.worker_result.is_some();
        if has_result != record.worker_result_sha256.is_some() {
            return None;
        }
        if record.correlation_id.is_none() {
            if record.request_message_id.is_some() || has_result {
                return None;
            }
        } else {
            record.object_format.as_ref()?;
            let terminal_record = matches!(
                record.state,
                FanoutState::HandedBack | FanoutState::Collecting | FanoutState::Collected
            );
            if has_result != terminal_record {
                return None;
            }
        }
        Some(record)
    }
}

pub(super) struct ReservationRequest<'a> {
    pub(super) parent_session_id: &'a str,
    pub(super) mode: FanoutMode,
    pub(super) repo: &'a RepoIdentity,
    pub(super) worktree: &'a Path,
    pub(super) branch: &'a str,
    pub(super) base_sha: &'a str,
    pub(super) reservation_id: &'a str,
    pub(super) expected_parent_dir: &'a str,
}

#[derive(Clone, Debug)]
pub struct FanoutStore {
    root: PathBuf,
}

impl FanoutStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn read(&self, reservation_id: &str) -> Result<Option<FanoutRecord>, String> {
        self.read_transaction(|records, _| Ok(records.get(reservation_id).cloned()))
    }

    pub fn active_leaf_count(&self, root_reservation_id: &str) -> Result<usize, String> {
        self.read_transaction(|records, lifecycle| {
            Ok(active_leaf_count(
                &self.root,
                records,
                lifecycle,
                root_reservation_id,
            ))
        })
    }

    pub fn has_nonterminal_repository(
        &self,
        repository: &RepositoryIdentityV1,
    ) -> Result<bool, String> {
        self.read_transaction(|records, _| {
            Ok(records.values().any(|record| {
                record.repo_dev == repository.common_dir_dev
                    && record.repo_ino == repository.common_dir_ino
                    && !matches!(
                        record.state,
                        FanoutState::Collected | FanoutState::FailedNoProcess
                    )
            }))
        })
    }

    pub(crate) fn record_error(
        &self,
        reservation_id: &str,
        error: &str,
    ) -> Result<FanoutRecord, String> {
        let bounded = error.chars().take(4096).collect::<String>();
        self.transaction(|records, _, _| {
            let record = records
                .get_mut(reservation_id)
                .ok_or_else(|| "fanout reservation not found".to_string())?;
            record.last_error = Some(bounded);
            increment_record_version(record)?;
            Ok(record.clone())
        })
    }

    pub fn bind_managed(
        &self,
        reservation_id: &str,
        worker_id: &str,
        generation: &str,
    ) -> Result<FanoutRecord, String> {
        if !store::is_uuid(worker_id) || !store::is_uuid(generation) {
            return Err("fanout managed identity must be UUID-shaped".to_string());
        }
        self.transaction(|records, lifecycle, _| {
            let mut record = records
                .get(reservation_id)
                .cloned()
                .ok_or_else(|| "fanout reservation not found".to_string())?;
            if record.state != FanoutState::Reserved {
                return Err("fanout reservation is not Reserved".to_string());
            }
            if let (Some(current_worker), Some(current_generation)) =
                (&record.worker_id, &record.generation)
            {
                if current_worker == worker_id && current_generation == generation {
                    return Ok(record);
                }
                return Err("fanout reservation already has different managed authority".into());
            }
            let worker = lifecycle_worker(lifecycle, worker_id)
                .ok_or_else(|| "managed worker is missing from lifecycle authority".to_string())?;
            if optional_string(worker, "generation").as_deref() != Some(generation) {
                return Err("managed worker generation does not match fanout reservation".into());
            }
            record.worker_id = Some(worker_id.to_string());
            record.generation = Some(generation.to_string());
            increment_record_version(&mut record)?;
            records.insert(reservation_id.to_string(), record.clone());
            Ok(record)
        })
    }

    pub fn attach_runtime(
        &self,
        reservation_id: &str,
        runtime_session_id: &str,
    ) -> Result<FanoutRecord, String> {
        if !store::is_uuid(runtime_session_id) {
            return Err("fanout runtime session id must be UUID-shaped".to_string());
        }
        let snapshot = self.read_transaction(|records, lifecycle| {
            let record = records
                .get(reservation_id)
                .cloned()
                .ok_or_else(|| "fanout reservation not found".to_string())?;
            if record.state == FanoutState::Running
                && record.runtime_session_id.as_deref() == Some(runtime_session_id)
            {
                return Ok(record);
            }
            if record.state != FanoutState::Reserved {
                return Err("fanout reservation is not Reserved".to_string());
            }
            if !exact_active_birth(&record, lifecycle, runtime_session_id) {
                return Err("managed worker is not the exact Active fanout birth".to_string());
            }
            Ok(record)
        })?;
        let request_message_id = ensure_fanout_request(&self.root, &snapshot, runtime_session_id)?;
        self.transaction(|records, lifecycle, _| {
            let mut record = records
                .get(reservation_id)
                .cloned()
                .ok_or_else(|| "fanout reservation not found".to_string())?;
            if record.state == FanoutState::Running
                && record.runtime_session_id.as_deref() == Some(runtime_session_id)
            {
                return bind_request_message_id(records, record, request_message_id.as_deref());
            }
            if record.state != FanoutState::Reserved {
                return Err("fanout reservation is not Reserved".to_string());
            }
            if record.version != snapshot.version
                || record.worker_id != snapshot.worker_id
                || record.generation != snapshot.generation
                || record.correlation_id != snapshot.correlation_id
                || !exact_active_birth(&record, lifecycle, runtime_session_id)
            {
                return Err("fanout reservation changed during runtime attachment".to_string());
            }
            record.runtime_session_id = Some(runtime_session_id.to_string());
            record.request_message_id = request_message_id;
            record.state = FanoutState::Running;
            increment_record_version(&mut record)?;
            records.insert(reservation_id.to_string(), record.clone());
            Ok(record)
        })
    }

    pub(crate) fn reconcile_fanout_claim(
        &self,
        reservation_id: &str,
    ) -> Result<FanoutRecord, String> {
        let snapshot = self
            .read(reservation_id)?
            .ok_or_else(|| "fanout reservation not found".to_string())?;
        if snapshot.correlation_id.is_none() {
            return Ok(snapshot);
        }
        if !matches!(
            snapshot.state,
            FanoutState::Running | FanoutState::HandedBack
        ) {
            return Err(
                "fanout reservation cannot reconcile its request in this state".to_string(),
            );
        }
        let runtime_session_id = snapshot
            .runtime_session_id
            .as_deref()
            .ok_or_else(|| "fanout reservation has no runtime session".to_string())?;
        let request_message_id = ensure_fanout_request(&self.root, &snapshot, runtime_session_id)?
            .ok_or_else(|| "correlated fanout request did not produce a message id".to_string())?;
        if snapshot.request_message_id.as_deref() == Some(request_message_id.as_str()) {
            return Ok(snapshot);
        }
        self.transaction(|records, _, _| {
            let record = records
                .get(reservation_id)
                .cloned()
                .ok_or_else(|| "fanout reservation not found".to_string())?;
            if record.correlation_id != snapshot.correlation_id
                || record.parent_session_id != snapshot.parent_session_id
                || record.worker_id != snapshot.worker_id
                || record.generation != snapshot.generation
                || record.runtime_session_id != snapshot.runtime_session_id
            {
                return Err("fanout request binding changed during reconciliation".to_string());
            }
            bind_request_message_id(records, record, Some(&request_message_id))
        })
    }

    pub(crate) fn ensure_worker_result_enqueued(
        &self,
        reservation_id: &str,
    ) -> Result<FanoutRecord, String> {
        let mut record = self
            .read(reservation_id)?
            .ok_or_else(|| "fanout reservation not found".to_string())?;
        if record.correlation_id.is_none() {
            return Ok(record);
        }
        if record.request_message_id.is_none() {
            record = self.reconcile_fanout_claim(reservation_id)?;
        }
        let (result, _) = validated_worker_result(&record)?;
        let protocol = ProtocolStore::new(self.root.clone());
        let claim = protocol
            .read_claim(&result.correlation_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "fanout request authority is missing".to_string())?;
        if !fanout_request_claim_matches(&record, &claim, &worker_request_body(&record)?) {
            return Err("fanout request authority binding mismatch".to_string());
        }
        protocol
            .publish_worker_result(result, &worker_result_notification_body(&record)?)
            .map_err(|error| error.to_string())?;
        if !self.result_delivery_ready(reservation_id)? {
            return Err("fanout worker result delivery is not terminally proven".to_string());
        }
        Ok(record)
    }

    pub fn result_delivery_ready(&self, reservation_id: &str) -> Result<bool, String> {
        self.read_transaction(|records, _| {
            let record = records
                .get(reservation_id)
                .ok_or_else(|| "fanout reservation not found".to_string())?;
            Ok(result_delivery_ready_unlocked(&self.root, record))
        })
    }

    pub(super) fn reserve(&self, request: ReservationRequest<'_>) -> Result<FanoutRecord, String> {
        self.transaction(|records, lifecycle, registry| {
            if records.contains_key(request.reservation_id) {
                return Err("fanout reservation already exists".to_string());
            }
            let parent = resolve_entry(registry, request.parent_session_id)
                .ok_or_else(|| "fanout parent is not a registered session".to_string())?;
            if parent.id != request.parent_session_id
                || parent.dir.as_deref() != Some(request.expected_parent_dir)
            {
                return Err("fanout parent registration changed during preflight".to_string());
            }
            let managed_parent = records.values().find(|record| {
                record.runtime_session_id.as_deref() == Some(request.parent_session_id)
            });
            let (root_reservation_id, depth) = match request.mode {
                FanoutMode::Root => {
                    if managed_parent.is_some() {
                        return Err(
                            "fanout root parent is already a managed fanout worker".to_string()
                        );
                    }
                    (request.reservation_id.to_string(), 0)
                }
                FanoutMode::Child => {
                    let root = managed_parent
                        .ok_or_else(|| "fanout child parent is not a managed root".to_string())?;
                    if root.depth != 0 || root.state != FanoutState::Running {
                        return Err("fanout child parent is not an active depth-0 root".to_string());
                    }
                    let worker_id = root.worker_id.as_deref().ok_or_else(|| {
                        "fanout child parent is not an exact Active managed root".to_string()
                    })?;
                    let worker = lifecycle_worker(lifecycle, worker_id).ok_or_else(|| {
                        "fanout child parent is not an exact Active managed root".to_string()
                    })?;
                    if optional_string(worker, "generation") != root.generation
                        || optional_string(worker, "runtime_session_id").as_deref()
                            != Some(request.parent_session_id)
                        || optional_string(worker, "state").as_deref() != Some("Active")
                    {
                        return Err(
                            "fanout child parent is not an exact Active managed root".to_string()
                        );
                    }
                    if !request.repo.matches_record(root) {
                        return Err("fanout child repository differs from its root".to_string());
                    }
                    if active_leaf_count(&self.root, records, lifecycle, &root.root_reservation_id)
                        >= FANOUT_CAP
                    {
                        return Err("fanout cap reached (2 active descendants)".to_string());
                    }
                    (root.root_reservation_id.clone(), 1)
                }
            };
            let record = FanoutRecord {
                reservation_id: request.reservation_id.to_string(),
                parent_session_id: request.parent_session_id.to_string(),
                root_reservation_id,
                depth,
                state: FanoutState::Reserved,
                version: "1".to_string(),
                repo_common_dir: request.repo.common_dir.clone(),
                repo_dev: request.repo.dev.clone(),
                repo_ino: request.repo.ino.clone(),
                object_format: Some(request.repo.object_format.as_str().to_string()),
                worktree: request.worktree.to_string_lossy().into_owned(),
                branch: request.branch.to_string(),
                base_sha: request.base_sha.to_string(),
                worker_id: None,
                generation: None,
                runtime_session_id: None,
                correlation_id: Some(store::uuid_v4()),
                request_message_id: None,
                handback_head: None,
                handback_status: None,
                handback_note: None,
                worker_result: None,
                worker_result_sha256: None,
                collection_phase: None,
                last_error: None,
            };
            records.insert(request.reservation_id.to_string(), record.clone());
            Ok(record)
        })
    }

    pub(super) fn read_transaction<T>(
        &self,
        f: impl FnOnce(&HashMap<String, FanoutRecord>, &HashMap<String, JsonValue>) -> Result<T, String>,
    ) -> Result<T, String> {
        store::with_lock_at(&self.root, || {
            let records = read_records(&self.root)?;
            let lifecycle = store::read_lifecycle_authority_at(&self.root)?.unwrap_or_default();
            f(&records, &lifecycle)
        })
    }

    pub(super) fn transaction<T>(
        &self,
        f: impl FnOnce(
            &mut HashMap<String, FanoutRecord>,
            &HashMap<String, JsonValue>,
            &store::Registry,
        ) -> Result<T, String>,
    ) -> Result<T, String> {
        store::with_lock_at(&self.root, || {
            let mut records = read_records(&self.root)?;
            let lifecycle = store::read_lifecycle_authority_at(&self.root)?.unwrap_or_default();
            let registry = store::read_registry_at(&self.root);
            let output = f(&mut records, &lifecycle, &registry)?;
            write_records(&self.root, &records)?;
            Ok(output)
        })
    }
}

fn bind_request_message_id(
    records: &mut HashMap<String, FanoutRecord>,
    mut record: FanoutRecord,
    request_message_id: Option<&str>,
) -> Result<FanoutRecord, String> {
    match (record.request_message_id.as_deref(), request_message_id) {
        (Some(current), Some(expected)) if current == expected => Ok(record),
        (Some(_), Some(_)) => Err("fanout request message id binding changed".to_string()),
        (None, Some(expected)) => {
            record.request_message_id = Some(expected.to_string());
            increment_record_version(&mut record)?;
            records.insert(record.reservation_id.clone(), record.clone());
            Ok(record)
        }
        (None, None) => Ok(record),
        (Some(_), None) => {
            Err("legacy fanout record unexpectedly has a request message id".to_string())
        }
    }
}

fn ensure_fanout_request(
    root: &Path,
    record: &FanoutRecord,
    runtime_session_id: &str,
) -> Result<Option<String>, String> {
    let Some(correlation_id) = record.correlation_id.as_deref() else {
        if record.request_message_id.is_some() {
            return Err("legacy fanout record has correlated request authority".to_string());
        }
        return Ok(None);
    };
    let body = worker_request_body(record)?;
    let protocol = ProtocolStore::new(root.to_path_buf());
    let mut claim = protocol
        .read_claim(correlation_id)
        .map_err(|error| error.to_string())?;
    if claim.is_none() {
        if record.request_message_id.is_some() {
            return Err("fanout request authority disappeared".to_string());
        }
        protocol
            .open_fanout_claim(
                correlation_id,
                &record.parent_session_id,
                runtime_session_id,
                &body,
            )
            .map_err(|error| error.to_string())?;
        claim = protocol
            .read_claim(correlation_id)
            .map_err(|error| error.to_string())?;
    }
    let claim = claim.ok_or_else(|| "fanout request authority is missing".to_string())?;
    let mut bound_record = record.clone();
    bound_record.runtime_session_id = Some(runtime_session_id.to_string());
    if !fanout_request_claim_matches(&bound_record, &claim, &body) {
        return Err("correlation_conflict: fanout request authority differs".to_string());
    }
    Ok(Some(claim.request.id))
}

fn worker_request_body(record: &FanoutRecord) -> Result<String, String> {
    let worker_id = record
        .worker_id
        .as_deref()
        .ok_or_else(|| "fanout reservation has no managed worker".to_string())?;
    let generation = record
        .generation
        .as_deref()
        .ok_or_else(|| "fanout reservation has no managed generation".to_string())?;
    Ok(format!(
        "fanout_request reservation_id={} worker_id={worker_id} generation={generation} parent_session_id={}",
        record.reservation_id, record.parent_session_id
    ))
}

fn fanout_request_claim_matches(record: &FanoutRecord, claim: &ClaimStatusV1, body: &str) -> bool {
    claim.origin == ClaimOrigin::Fanout
        && matches!(
            claim.state,
            ClaimState::Open
                | ClaimState::ReplyPending
                | ClaimState::ReplyEnqueued
                | ClaimState::ReplyConsumed
        )
        && claim.correlation_id == record.correlation_id.as_deref().unwrap_or_default()
        && claim.requester_session_id == record.parent_session_id
        && claim.responder_session_id == record.runtime_session_id.as_deref().unwrap_or_default()
        && claim.request_delivery == DeliveryState::NotApplicable
        && claim.request.kind == MessageKind::Request
        && claim.request.correlation_id == claim.correlation_id
        && claim.request.from_session_id == claim.requester_session_id
        && claim.request.to_session_id == claim.responder_session_id
        && claim.request.reply_to.is_none()
        && claim.request.terminal_status.is_none()
        && claim.request.result_sha256.is_none()
        && claim.request.body == body
        && record
            .request_message_id
            .as_deref()
            .is_none_or(|message_id| message_id == claim.request.id)
}

pub(super) fn validated_worker_result(
    record: &FanoutRecord,
) -> Result<(&WorkerResultV1, &str), String> {
    let result = record
        .worker_result
        .as_ref()
        .ok_or_else(|| "correlated fanout record has no worker result".to_string())?;
    let digest = record
        .worker_result_sha256
        .as_deref()
        .ok_or_else(|| "correlated fanout record has no worker result digest".to_string())?;
    if result.sha256() != digest {
        return Err("worker result digest binding mismatch".to_string());
    }
    let correlation_id = record
        .correlation_id
        .as_deref()
        .ok_or_else(|| "worker result record has no correlation id".to_string())?;
    if result.correlation_id != correlation_id
        || result.reservation_id != record.reservation_id
        || result.root_reservation_id != record.root_reservation_id
        || result.parent_session_id != record.parent_session_id
    {
        return Err("worker result identity binding mismatch".to_string());
    }
    if result.worker_id != record.worker_id.as_deref().unwrap_or_default() {
        return Err("worker result worker binding mismatch".to_string());
    }
    if result.generation != record.generation.as_deref().unwrap_or_default() {
        return Err("worker result generation binding mismatch".to_string());
    }
    if result.runtime_session_id != record.runtime_session_id.as_deref().unwrap_or_default() {
        return Err("worker result runtime binding mismatch".to_string());
    }
    let object_format = record
        .object_format
        .as_deref()
        .ok_or_else(|| "correlated fanout record has no object format".to_string())?;
    if result.repo_common_dir != record.repo_common_dir
        || result.repo_dev != record.repo_dev
        || result.repo_ino != record.repo_ino
        || result.object_format.as_str() != object_format
    {
        return Err("worker result repository binding mismatch".to_string());
    }
    if result.base_commit != record.base_sha {
        return Err("worker result base binding mismatch".to_string());
    }
    let handback_head = record
        .handback_head
        .as_deref()
        .ok_or_else(|| "correlated fanout record has no handback HEAD".to_string())?;
    if result.handback_commit != handback_head {
        return Err("worker result handback HEAD binding mismatch".to_string());
    }
    let status = record
        .handback_status
        .as_deref()
        .ok_or_else(|| "correlated fanout record has no handback status".to_string())
        .and_then(TerminalStatus::parse)?;
    if result.status != status {
        return Err("worker result status binding mismatch".to_string());
    }
    let handback_note = record
        .handback_note
        .as_deref()
        .ok_or_else(|| "correlated fanout record has no handback summary".to_string())?;
    if result.summary != handback_note {
        return Err("worker result summary binding mismatch".to_string());
    }
    Ok((result, digest))
}

pub(super) fn worker_result_notification_body(record: &FanoutRecord) -> Result<String, String> {
    let worker_id = record
        .worker_id
        .as_deref()
        .ok_or_else(|| "fanout record has no managed worker".to_string())?;
    let generation = record
        .generation
        .as_deref()
        .ok_or_else(|| "fanout record has no managed generation".to_string())?;
    let runtime_session_id = record
        .runtime_session_id
        .as_deref()
        .ok_or_else(|| "fanout record has no runtime session".to_string())?;
    Ok(format!(
        "worker_result reservation_id={} worker_id={worker_id} generation={generation} \
runtime_session_id={runtime_session_id} parent_session_id={}; collect: relay collect \
{runtime_session_id} --from {}",
        record.reservation_id, record.parent_session_id, record.parent_session_id
    ))
}

fn result_delivery_ready_unlocked(root: &Path, record: &FanoutRecord) -> bool {
    if validated_worker_result(record).is_err() {
        return false;
    }
    let Some(correlation_id) = record.correlation_id.as_deref() else {
        return false;
    };
    let protocol_root = root.join("protocol-v1");
    for directory in ["pending", "open"] {
        let path = protocol_root
            .join(directory)
            .join(format!("{correlation_id}.json"));
        match fs::symlink_metadata(path) {
            Ok(_) => return false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    let path = protocol_root
        .join("terminal")
        .join(format!("{correlation_id}.json"));
    let Ok(claim) = read_jcs_file::<ClaimStatusV1>(&path, None) else {
        return false;
    };
    terminal_result_claim_matches(record, &claim)
}

fn terminal_result_claim_matches(record: &FanoutRecord, claim: &ClaimStatusV1) -> bool {
    let Ok((result, digest)) = validated_worker_result(record) else {
        return false;
    };
    let Ok(body) = worker_result_notification_body(record) else {
        return false;
    };
    let Ok(request_body) = worker_request_body(record) else {
        return false;
    };
    let delivery_matches = matches!(
        (claim.state, claim.reply_delivery),
        (ClaimState::ReplyEnqueued, Some(DeliveryState::Enqueued))
            | (ClaimState::ReplyConsumed, Some(DeliveryState::Consumed))
    );
    let Some(reply) = claim.reply.as_ref() else {
        return false;
    };
    let reply_sha256 = reply.sha256();
    delivery_matches
        && fanout_request_claim_matches(record, claim, &request_body)
        && record.request_message_id.as_deref() == Some(claim.request.id.as_str())
        && reply.kind == MessageKind::WorkerResult
        && reply.correlation_id == result.correlation_id
        && reply.reply_to.as_deref() == Some(claim.request.id.as_str())
        && reply.from_session_id == result.runtime_session_id
        && reply.to_session_id == result.parent_session_id
        && reply.terminal_status == Some(result.status)
        && reply.body == body
        && reply.result_sha256.as_deref() == Some(digest)
        && claim.reply_sha256.as_deref() == Some(reply_sha256.as_str())
}

pub(super) fn acquire_collection_lock(
    root: &Path,
    reservation_id: &str,
) -> Result<fs::File, String> {
    if !store::is_uuid(reservation_id) {
        return Err("fanout reservation id is not UUID-shaped".to_string());
    }
    let locks = root.join("locks");
    fs::create_dir_all(&locks).map_err(|error| format!("create fanout lock dir: {error}"))?;
    let path = locks.join(format!("fanout-collect-{reservation_id}.lock"));
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("open fanout collection lock {}: {error}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod fanout collection lock {}: {error}", path.display()))?;
    loop {
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(file),
            Err(error) if error == rustix::io::Errno::INTR => continue,
            Err(error) if error == rustix::io::Errno::AGAIN => {
                return Err("fanout collection already in progress".to_string());
            }
            Err(error) => {
                return Err(format!(
                    "lock fanout collection file {}: {error}",
                    path.display()
                ));
            }
        }
    }
}

pub(super) fn registered_entry(fanout: &FanoutStore, session_id: &str) -> Result<Entry, String> {
    store::with_lock_at(fanout.root(), || {
        resolve_entry(&store::read_registry_at(fanout.root()), session_id)
            .ok_or_else(|| "fanout parent is not a registered session".to_string())
    })
}

pub(super) fn resolve_entry(registry: &store::Registry, session_id: &str) -> Option<Entry> {
    registry.agents.get(session_id).and_then(Entry::from_json)
}

pub(super) fn record_by_runtime_session_id<'a>(
    records: &'a HashMap<String, FanoutRecord>,
    runtime_session_id: &str,
) -> Result<&'a FanoutRecord, String> {
    records
        .values()
        .find(|record| record.runtime_session_id.as_deref() == Some(runtime_session_id))
        .ok_or_else(|| "runtime session is not a fanout worker".to_string())
}

fn active_leaf_count(
    root: &Path,
    records: &HashMap<String, FanoutRecord>,
    lifecycle: &HashMap<String, JsonValue>,
    root_reservation_id: &str,
) -> usize {
    records
        .values()
        .filter(|record| record.depth == 1 && record.root_reservation_id == root_reservation_id)
        .filter(|record| slot_consuming(root, record, lifecycle))
        .count()
}

fn slot_consuming(
    root: &Path,
    record: &FanoutRecord,
    lifecycle: &HashMap<String, JsonValue>,
) -> bool {
    match record.state {
        FanoutState::Reserved | FanoutState::Running => true,
        FanoutState::Collected | FanoutState::FailedNoProcess => false,
        FanoutState::HandedBack | FanoutState::Collecting => {
            let Some(worker_id) = record.worker_id.as_deref() else {
                return true;
            };
            let released = lifecycle_worker(lifecycle, worker_id)
                .and_then(|worker| optional_string(worker, "state"))
                .as_deref()
                == Some("TerminalReleasable");
            if !released || record.correlation_id.is_none() {
                return !released;
            }
            !result_delivery_ready_unlocked(root, record)
        }
    }
}

fn exact_active_birth(
    record: &FanoutRecord,
    lifecycle: &HashMap<String, JsonValue>,
    runtime_session_id: &str,
) -> bool {
    let Some(worker_id) = record.worker_id.as_deref() else {
        return false;
    };
    let Some(worker) = lifecycle_worker(lifecycle, worker_id) else {
        return false;
    };
    optional_string(worker, "generation") == record.generation
        && optional_string(worker, "runtime_session_id").as_deref() == Some(runtime_session_id)
        && optional_string(worker, "state").as_deref() == Some("Active")
}

pub(super) fn lifecycle_worker<'a>(
    lifecycle: &'a HashMap<String, JsonValue>,
    worker_id: &str,
) -> Option<&'a HashMap<String, JsonValue>> {
    lifecycle
        .get("managed_workers")?
        .get::<HashMap<String, JsonValue>>()?
        .get(worker_id)?
        .get::<HashMap<String, JsonValue>>()
}

fn read_records(root: &Path) -> Result<HashMap<String, FanoutRecord>, String> {
    let path = root.join(FANOUT_FILE);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(format!("read fanout authority {}: {error}", path.display())),
    };
    let value = raw
        .parse::<JsonValue>()
        .map_err(|error| format!("malformed fanout authority: {error}"))?;
    let object = value
        .get::<HashMap<String, JsonValue>>()
        .ok_or_else(|| "malformed fanout authority: root is not an object".to_string())?;
    if object.len() != 2
        || optional_string(object, "schema_version").as_deref() != Some(FANOUT_SCHEMA)
    {
        return Err("malformed fanout authority: unsupported or inexact schema".to_string());
    }
    let rows = object
        .get("records")
        .and_then(JsonValue::get::<HashMap<String, JsonValue>>)
        .ok_or_else(|| "malformed fanout authority: records is not an object".to_string())?;
    rows.iter()
        .map(|(id, value)| {
            let record = FanoutRecord::from_json(value)
                .ok_or_else(|| format!("malformed fanout record {id}"))?;
            if record.reservation_id != *id {
                return Err(format!("fanout record key mismatch {id}"));
            }
            Ok((id.clone(), record))
        })
        .collect()
}

fn write_records(root: &Path, records: &HashMap<String, FanoutRecord>) -> Result<(), String> {
    let rows = records
        .iter()
        .map(|(id, record)| (id.clone(), record.to_json()))
        .collect::<HashMap<_, _>>();
    let mut object = HashMap::new();
    object.insert(
        "schema_version".into(),
        JsonValue::from(FANOUT_SCHEMA.to_string()),
    );
    object.insert("records".into(), JsonValue::from(rows));
    let text = JsonValue::from(object)
        .format()
        .map_err(|error| format!("fanout authority serialize: {error}"))?;
    store::atomic_write_private(&root.join(FANOUT_FILE), &text)
}

pub(super) fn increment_record_version(record: &mut FanoutRecord) -> Result<(), String> {
    record.version = canonical_version(&record.version)?
        .checked_add(1)
        .ok_or_else(|| "fanout version overflow".to_string())?
        .to_string();
    Ok(())
}

fn canonical_version(value: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| "fanout version is not a canonical u64".to_string())?;
    if parsed.to_string() == value {
        Ok(parsed)
    } else {
        Err("fanout version is not canonical".to_string())
    }
}

fn insert_optional(object: &mut HashMap<String, JsonValue>, key: &str, value: &Option<String>) {
    object.insert(
        key.to_string(),
        value
            .as_ref()
            .map(|value| JsonValue::from(value.clone()))
            .unwrap_or(JsonValue::from(())),
    );
}
fn worker_result_to_json(result: &WorkerResultV1) -> JsonValue {
    String::from_utf8(result.canonical_bytes())
        .expect("canonical WorkerResultV1 is UTF-8")
        .parse()
        .expect("canonical WorkerResultV1 is JSON")
}

fn optional_object_format(object: &HashMap<String, JsonValue>) -> Option<Option<String>> {
    let value = optional_nullable_string(object, "object_format")?;
    if value
        .as_deref()
        .is_some_and(|value| ObjectFormat::parse(value).is_err())
    {
        return None;
    }
    Some(value)
}

fn optional_uuid(object: &HashMap<String, JsonValue>, key: &str) -> Option<Option<String>> {
    let value = optional_nullable_string(object, key)?;
    if value.as_deref().is_some_and(|value| !store::is_uuid(value)) {
        return None;
    }
    Some(value)
}

fn optional_sha256(object: &HashMap<String, JsonValue>, key: &str) -> Option<Option<String>> {
    let value = optional_nullable_string(object, key)?;
    if value.as_deref().is_some_and(|value| {
        value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    }) {
        return None;
    }
    Some(value)
}

fn optional_nullable_string(
    object: &HashMap<String, JsonValue>,
    key: &str,
) -> Option<Option<String>> {
    match object.get(key) {
        None => Some(None),
        Some(value) if value.get::<()>().is_some() => Some(None),
        Some(value) => value.get::<String>().cloned().map(Some),
    }
}

fn optional_worker_result(object: &HashMap<String, JsonValue>) -> Option<Option<WorkerResultV1>> {
    match object.get("worker_result") {
        None => Some(None),
        Some(value) if value.get::<()>().is_some() => Some(None),
        Some(value) => WorkerResultV1::from_jcs(jcs_from_tinyjson(value).ok()?)
            .ok()
            .map(Some),
    }
}

pub(super) fn optional_string(object: &HashMap<String, JsonValue>, key: &str) -> Option<String> {
    object.get(key)?.get::<String>().cloned()
}
