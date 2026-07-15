use super::git::RepoIdentity;
use crate::store::{self, Entry};
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
    pub worktree: String,
    pub branch: String,
    pub base_sha: String,
    pub worker_id: Option<String>,
    pub generation: Option<String>,
    pub runtime_session_id: Option<String>,
    pub handback_head: Option<String>,
    pub handback_status: Option<String>,
    pub handback_note: Option<String>,
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
        object.insert("worktree".into(), JsonValue::from(self.worktree.clone()));
        object.insert("branch".into(), JsonValue::from(self.branch.clone()));
        object.insert("base_sha".into(), JsonValue::from(self.base_sha.clone()));
        insert_optional(&mut object, "worker_id", &self.worker_id);
        insert_optional(&mut object, "generation", &self.generation);
        insert_optional(&mut object, "runtime_session_id", &self.runtime_session_id);
        insert_optional(&mut object, "handback_head", &self.handback_head);
        insert_optional(&mut object, "handback_status", &self.handback_status);
        insert_optional(&mut object, "handback_note", &self.handback_note);
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
        let string = |key: &str| object.get(key)?.get::<String>().cloned();
        let depth = string("depth")?.parse::<u8>().ok()?;
        if depth > 1 {
            return None;
        }
        let version = string("version")?;
        canonical_version(&version).ok()?;
        Some(Self {
            reservation_id: string("reservation_id")?,
            parent_session_id: string("parent_session_id")?,
            root_reservation_id: string("root_reservation_id")?,
            depth,
            state: FanoutState::parse(&string("state")?)?,
            version,
            repo_common_dir: string("repo_common_dir")?,
            repo_dev: string("repo_dev")?,
            repo_ino: string("repo_ino")?,
            worktree: string("worktree")?,
            branch: string("branch")?,
            base_sha: string("base_sha")?,
            worker_id: optional_string(object, "worker_id"),
            generation: optional_string(object, "generation"),
            runtime_session_id: optional_string(object, "runtime_session_id"),
            handback_head: optional_string(object, "handback_head"),
            handback_status: optional_string(object, "handback_status"),
            handback_note: optional_string(object, "handback_note"),
            collection_phase: match optional_string(object, "collection_phase") {
                Some(phase) => Some(CollectionPhase::parse(&phase)?),
                None => None,
            },
            last_error: optional_string(object, "last_error"),
        })
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
            Ok(active_leaf_count(records, lifecycle, root_reservation_id))
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
        self.transaction(|records, lifecycle, _| {
            let mut record = records
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
            let worker_id = record
                .worker_id
                .as_deref()
                .ok_or_else(|| "fanout reservation has no managed worker".to_string())?;
            let worker = lifecycle_worker(lifecycle, worker_id)
                .ok_or_else(|| "managed worker is missing from lifecycle authority".to_string())?;
            if optional_string(worker, "generation") != record.generation
                || optional_string(worker, "runtime_session_id").as_deref()
                    != Some(runtime_session_id)
                || optional_string(worker, "state").as_deref() != Some("Active")
            {
                return Err("managed worker is not the exact Active fanout birth".to_string());
            }
            record.runtime_session_id = Some(runtime_session_id.to_string());
            record.state = FanoutState::Running;
            increment_record_version(&mut record)?;
            records.insert(reservation_id.to_string(), record.clone());
            Ok(record)
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
                    if active_leaf_count(records, lifecycle, &root.root_reservation_id)
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
                worktree: request.worktree.to_string_lossy().into_owned(),
                branch: request.branch.to_string(),
                base_sha: request.base_sha.to_string(),
                worker_id: None,
                generation: None,
                runtime_session_id: None,
                handback_head: None,
                handback_status: None,
                handback_note: None,
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
    records: &HashMap<String, FanoutRecord>,
    lifecycle: &HashMap<String, JsonValue>,
    root_reservation_id: &str,
) -> usize {
    records
        .values()
        .filter(|record| record.depth == 1 && record.root_reservation_id == root_reservation_id)
        .filter(|record| slot_consuming(record, lifecycle))
        .count()
}

fn slot_consuming(record: &FanoutRecord, lifecycle: &HashMap<String, JsonValue>) -> bool {
    if matches!(
        record.state,
        FanoutState::Collected | FanoutState::FailedNoProcess
    ) {
        return false;
    }
    let Some(worker_id) = record.worker_id.as_deref() else {
        return true;
    };
    lifecycle_worker(lifecycle, worker_id)
        .and_then(|worker| optional_string(worker, "state"))
        .as_deref()
        != Some("TerminalReleasable")
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

pub(super) fn optional_string(object: &HashMap<String, JsonValue>, key: &str) -> Option<String> {
    object.get(key)?.get::<String>().cloned()
}
