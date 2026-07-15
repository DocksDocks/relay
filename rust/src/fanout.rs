//! Bounded worktree fan-out authority and explicit handback collection.
//!
//! Fan-out state is deliberately separate from `lifecycle-v1.json` so an
//! already-running older relay never encounters an unknown lifecycle key.
//! Both authorities share the store's kernel lock; cross-file ordering always
//! retains capacity on an interrupted write.

use crate::lifecycle::LifecycleStore;
use crate::store::{self, Entry};
use rustix::fs::{FlockOperation, flock};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct RepoIdentity {
    common_dir: String,
    dev: String,
    ino: String,
}

struct ReservationRequest<'a> {
    parent_session_id: &'a str,
    mode: FanoutMode,
    repo: &'a RepoIdentity,
    worktree: &'a Path,
    branch: &'a str,
    base_sha: &'a str,
    reservation_id: &'a str,
    expected_parent_dir: &'a str,
}

impl RepoIdentity {
    fn matches_record(&self, record: &FanoutRecord) -> bool {
        self.dev == record.repo_dev && self.ino == record.repo_ino
    }
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

    fn reserve(&self, request: ReservationRequest<'_>) -> Result<FanoutRecord, String> {
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

    fn read_transaction<T>(
        &self,
        f: impl FnOnce(&HashMap<String, FanoutRecord>, &HashMap<String, JsonValue>) -> Result<T, String>,
    ) -> Result<T, String> {
        store::with_lock_at(&self.root, || {
            let records = read_records(&self.root)?;
            let lifecycle = store::read_lifecycle_authority_at(&self.root)?.unwrap_or_default();
            f(&records, &lifecycle)
        })
    }

    fn transaction<T>(
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

pub fn prepare_worktree(
    fanout: &FanoutStore,
    repo: &Path,
    parent_session_id: &str,
    mode: FanoutMode,
) -> Result<FanoutRecord, String> {
    if !store::is_uuid(parent_session_id) {
        return Err("--from must resolve to a registered session UUID".to_string());
    }
    let repo = fs::canonicalize(repo)
        .map_err(|error| format!("resolve fanout repository {}: {error}", repo.display()))?;
    let target_identity = repo_identity(&repo)?;
    let parent = registered_entry(fanout, parent_session_id)?;
    let parent_dir = parent
        .dir
        .ok_or_else(|| "fanout parent has no registered directory".to_string())?;
    let parent_identity = repo_identity(Path::new(&parent_dir))?;
    if target_identity != parent_identity {
        return Err("fanout repository differs from the registered parent".to_string());
    }
    let base_sha = run_git(&repo, &["rev-parse", "--verify", "HEAD"])?;
    validate_sha(&base_sha)?;
    let reservation_id = store::uuid_v4();
    let branch = format!("relay/fanout-{reservation_id}");
    let worktrees = fanout.root.join("worktrees");
    ensure_worktree_root(&worktrees)?;
    let worktree = worktrees.join(&reservation_id);
    let record = fanout.reserve(ReservationRequest {
        parent_session_id,
        mode,
        repo: &target_identity,
        worktree: &worktree,
        branch: &branch,
        base_sha: &base_sha,
        reservation_id: &reservation_id,
        expected_parent_dir: &parent_dir,
    })?;
    let add = run_git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &record.worktree,
            &base_sha,
        ],
    );
    if let Err(error) = add {
        let _ = fanout.transaction(|records, _, _| {
            if let Some(record) = records.get_mut(&reservation_id) {
                record.last_error = Some(format!("git worktree add failed: {error}"));
                increment_record_version(record)?;
            }
            Ok(())
        });
        return Err(error);
    }
    let created_identity = repo_identity(&worktree)?;
    if created_identity != target_identity {
        return Err("created worktree repository identity changed".to_string());
    }
    Ok(record)
}

pub(crate) fn rollback_before_process_start(
    fanout: &FanoutStore,
    reservation_id: &str,
    worker_id: &str,
    generation: &str,
    reason: &str,
) -> Result<FanoutRecord, String> {
    let snapshot = fanout.read_transaction(|records, _| {
        let record = records
            .get(reservation_id)
            .cloned()
            .ok_or_else(|| "fanout reservation not found".to_string())?;
        if record.state != FanoutState::Reserved
            || record.worker_id.as_deref() != Some(worker_id)
            || record.generation.as_deref() != Some(generation)
            || record.runtime_session_id.is_some()
        {
            return Err("fanout reservation is not an unstarted managed birth".to_string());
        }
        Ok(record)
    })?;
    let expected_worktree = fanout.root.join("worktrees").join(reservation_id);
    if Path::new(&snapshot.worktree) != expected_worktree {
        return Err("fanout worktree path is not the exact reserved path".to_string());
    }
    let worktree = Path::new(&snapshot.worktree);
    ensure_clean(worktree, "unstarted fanout worktree")?;
    let head = run_git(worktree, &["rev-parse", "--verify", "HEAD"])?;
    if head != snapshot.base_sha || !repo_identity(worktree)?.matches_record(&snapshot) {
        return Err("unstarted fanout worktree changed before rollback".to_string());
    }
    let parent_dir = registered_entry(fanout, &snapshot.parent_session_id)?
        .dir
        .ok_or_else(|| "fanout parent has no registered directory".to_string())?;
    run_git(
        Path::new(&parent_dir),
        &["worktree", "remove", snapshot.worktree.as_str()],
    )?;
    if worktree.exists() {
        return Err("unstarted fanout worktree still exists after removal".to_string());
    }
    LifecycleStore::new(fanout.root.clone())
        .discard_unclaimed_owned_process_worker(worker_id, generation)?;
    fanout.transaction(|records, _, _| {
        let current = records
            .get_mut(reservation_id)
            .ok_or_else(|| "fanout reservation disappeared".to_string())?;
        if current.version != snapshot.version || current.state != FanoutState::Reserved {
            return Err("fanout reservation changed during no-process rollback".to_string());
        }
        current.state = FanoutState::FailedNoProcess;
        current.last_error = Some(format!("spawn never returned a child: {reason}"));
        increment_record_version(current)?;
        Ok(current.clone())
    })
}

pub fn handback(
    fanout: &FanoutStore,
    runtime_session_id: &str,
    status: &str,
    note: &str,
) -> Result<FanoutRecord, String> {
    if !matches!(status, "completed" | "failed") {
        return Err("handback status must be completed|failed".to_string());
    }
    if note.len() > 4096 || note.contains('\0') {
        return Err("handback note must be at most 4096 bytes without NUL".to_string());
    }
    let snapshot = fanout.read_transaction(|records, _| {
        let record = record_by_runtime_session_id(records, runtime_session_id)?;
        if record.state == FanoutState::HandedBack {
            return Ok(record.clone());
        }
        if record.state != FanoutState::Running {
            return Err("fanout worker is not Running".to_string());
        }
        if record.depth == 0
            && records.values().any(|child| {
                child.depth == 1
                    && child.root_reservation_id == record.root_reservation_id
                    && !matches!(
                        child.state,
                        FanoutState::Collected | FanoutState::FailedNoProcess
                    )
            })
        {
            return Err("fanout root has uncollected children".to_string());
        }
        Ok(record.clone())
    })?;
    ensure_clean(Path::new(&snapshot.worktree), "handback worktree")?;
    let head = run_git(
        Path::new(&snapshot.worktree),
        &["rev-parse", "--verify", "HEAD"],
    )?;
    validate_sha(&head)?;
    fanout.transaction(|records, _, _| {
        let mut record = records
            .get(&snapshot.reservation_id)
            .cloned()
            .ok_or_else(|| "fanout reservation disappeared".to_string())?;
        if record.version != snapshot.version || record.state != FanoutState::Running {
            return Err("fanout handback authority changed".to_string());
        }
        record.state = FanoutState::HandedBack;
        record.handback_head = Some(head);
        record.handback_status = Some(status.to_string());
        record.handback_note = Some(note.to_string());
        increment_record_version(&mut record)?;
        records.insert(record.reservation_id.clone(), record.clone());
        Ok(record)
    })
}

pub fn collect(
    fanout: &FanoutStore,
    runtime_session_id: &str,
    parent_session_id: &str,
) -> Result<FanoutRecord, String> {
    let reservation_id = fanout.read_transaction(|records, _| {
        let record = record_by_runtime_session_id(records, runtime_session_id)?;
        if record.parent_session_id != parent_session_id {
            return Err("fanout collect parent does not own this worker".to_string());
        }
        Ok(record.reservation_id.clone())
    })?;
    let _collection_lock = acquire_collection_lock(fanout.root(), &reservation_id)?;
    let mut record = fanout.transaction(|records, lifecycle, registry| {
        let mut record = record_by_runtime_session_id(records, runtime_session_id)?.clone();
        if record.parent_session_id != parent_session_id {
            return Err("fanout collect parent does not own this worker".to_string());
        }
        if record.state == FanoutState::Collected {
            return Ok(record);
        }
        if !matches!(
            record.state,
            FanoutState::HandedBack | FanoutState::Collecting
        ) {
            return Err("fanout worker has no collectible handback".to_string());
        }
        let worker_id = record
            .worker_id
            .as_deref()
            .ok_or_else(|| "fanout record has no managed worker".to_string())?;
        let worker = lifecycle_worker(lifecycle, worker_id)
            .ok_or_else(|| "managed worker is missing from lifecycle authority".to_string())?;
        if optional_string(worker, "generation") != record.generation
            || optional_string(worker, "state").as_deref() != Some("TerminalReleasable")
        {
            return Err("fanout worker is not TerminalReleasable".to_string());
        }
        let parent = resolve_entry(registry, parent_session_id)
            .ok_or_else(|| "fanout collect parent is not registered".to_string())?;
        if parent.id != parent_session_id {
            return Err("fanout collect parent must resolve to its exact UUID".to_string());
        }
        if record.state == FanoutState::HandedBack {
            record.state = FanoutState::Collecting;
            record.collection_phase = Some(CollectionPhase::Prepared);
            increment_record_version(&mut record)?;
            records.insert(record.reservation_id.clone(), record.clone());
        }
        Ok(record)
    })?;

    if record.state == FanoutState::Collected {
        return Ok(record);
    }

    let parent = registered_entry(fanout, parent_session_id)?;
    let parent_dir = PathBuf::from(
        parent
            .dir
            .ok_or_else(|| "fanout collect parent has no directory".to_string())?,
    );
    if !repo_identity(&parent_dir)?.matches_record(&record) {
        return Err("fanout collect parent repository identity changed".to_string());
    }
    let worktree = PathBuf::from(&record.worktree);
    if record.collection_phase == Some(CollectionPhase::Prepared) {
        ensure_clean(&parent_dir, "collect parent")?;
        ensure_clean(&worktree, "collect child")?;
        if !repo_identity(&worktree)?.matches_record(&record) {
            return Err("fanout collect child repository identity changed".to_string());
        }
        let head = record
            .handback_head
            .clone()
            .ok_or_else(|| "fanout handback has no head".to_string())?;
        let current_head = run_git(&worktree, &["rev-parse", "--verify", "HEAD"])?;
        if current_head != head {
            return Err(format!(
                "fanout child HEAD changed after handback; restore {} to {head} before retrying collection",
                worktree.display()
            ));
        }
        if let Err(merge_error) = run_git(&parent_dir, &["merge", "--no-ff", "--no-edit", &head]) {
            let abort = run_git(&parent_dir, &["merge", "--abort"]);
            let clean = ensure_clean(&parent_dir, "collect parent after merge abort");
            if abort.is_ok() && clean.is_ok() {
                fanout.transaction(|records, _, _| {
                    let current = records
                        .get_mut(&record.reservation_id)
                        .ok_or_else(|| "fanout reservation disappeared".to_string())?;
                    if current.state != FanoutState::Collecting
                        || current.collection_phase != Some(CollectionPhase::Prepared)
                    {
                        return Err("fanout collection changed during merge abort".to_string());
                    }
                    current.state = FanoutState::HandedBack;
                    current.collection_phase = None;
                    current.last_error = Some("merge failed and was aborted".to_string());
                    increment_record_version(current)?;
                    Ok(())
                })?;
                return Err(format!("merge failed and was aborted: {merge_error}"));
            }
            return Err(format!(
                "merge failed and abort could not restore a clean parent at {}; run `git merge --abort`, clean the checkout, then retry: {merge_error}",
                parent_dir.display()
            ));
        }
        record = advance_collection(fanout, &record, CollectionPhase::Merged)?;
    }
    if record.collection_phase == Some(CollectionPhase::Merged) {
        let tracked = git_tracks_worktree(&parent_dir, &worktree)?;
        match fs::symlink_metadata(&worktree) {
            Ok(_) => {
                if !tracked {
                    return Err(
                        "fanout collect child exists but is no longer registered".to_string()
                    );
                }
                ensure_clean(&worktree, "collect child")?;
                if !repo_identity(&worktree)?.matches_record(&record) {
                    return Err("fanout collect child repository identity changed".to_string());
                }
                run_git(
                    &parent_dir,
                    &["worktree", "remove", record.worktree.as_str()],
                )?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !tracked => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "fanout worktree {} is missing but remains registered; repair the git worktree metadata before retrying",
                    worktree.display()
                ));
            }
            Err(error) => {
                return Err(format!(
                    "inspect fanout worktree {}: {error}",
                    worktree.display()
                ));
            }
        }
        record = advance_collection(fanout, &record, CollectionPhase::WorktreeRemoved)?;
    }
    if record.collection_phase == Some(CollectionPhase::WorktreeRemoved) {
        record = fanout.transaction(|records, _, _| {
            let current = records
                .get_mut(&record.reservation_id)
                .ok_or_else(|| "fanout reservation disappeared".to_string())?;
            if current.version != record.version
                || current.state != FanoutState::Collecting
                || current.collection_phase != Some(CollectionPhase::WorktreeRemoved)
            {
                return Err("fanout collection authority changed before finalize".to_string());
            }
            current.state = FanoutState::Collected;
            increment_record_version(current)?;
            Ok(current.clone())
        })?;
    }
    Ok(record)
}

pub fn run_handback(raw: Vec<String>) -> ! {
    let args = crate::cli::Args(raw);
    let runtime_session_id = args
        .flag("from")
        .unwrap_or_else(|| fanout_die("usage: relay handback --from <managed-session> --status completed|failed [--note <text>]"));
    let status = args
        .flag("status")
        .unwrap_or_else(|| fanout_die("handback requires --status completed|failed"));
    let note = args.flag("note").unwrap_or("");
    let record = handback(
        &FanoutStore::new(store::home_dir()),
        runtime_session_id,
        status,
        note,
    )
    .unwrap_or_else(|error| fanout_die(&error));
    println!(
        "handed back {} at {}",
        runtime_session_id,
        record.handback_head.as_deref().unwrap_or("unknown")
    );
    std::process::exit(0);
}

pub fn run_collect(raw: Vec<String>) -> ! {
    let args = crate::cli::Args(raw);
    let positions = args.positionals(1);
    let runtime_session_id = positions.first().copied().unwrap_or_else(|| {
        fanout_die("usage: relay collect <managed-session> --from <parent-session>")
    });
    let parent_session_id = args
        .flag("from")
        .unwrap_or_else(|| fanout_die("collect requires --from <parent-session>"));
    let record = collect(
        &FanoutStore::new(store::home_dir()),
        runtime_session_id,
        parent_session_id,
    )
    .unwrap_or_else(|error| fanout_die(&error));
    println!(
        "collected {} into {}",
        runtime_session_id, parent_session_id
    );
    if record.state != FanoutState::Collected {
        fanout_die("fanout collection did not reach Collected");
    }
    std::process::exit(0);
}

fn fanout_die(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

fn advance_collection(
    fanout: &FanoutStore,
    snapshot: &FanoutRecord,
    phase: CollectionPhase,
) -> Result<FanoutRecord, String> {
    fanout.transaction(|records, _, _| {
        let current = records
            .get_mut(&snapshot.reservation_id)
            .ok_or_else(|| "fanout reservation disappeared".to_string())?;
        if current.version != snapshot.version || current.state != FanoutState::Collecting {
            return Err("fanout collection authority changed".to_string());
        }
        current.collection_phase = Some(phase);
        increment_record_version(current)?;
        Ok(current.clone())
    })
}

fn acquire_collection_lock(root: &Path, reservation_id: &str) -> Result<fs::File, String> {
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
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(error) if error == rustix::io::Errno::AGAIN || error == rustix::io::Errno::INTR => {
            Err("fanout collection already in progress".to_string())
        }
        Err(error) => Err(format!(
            "lock fanout collection file {}: {error}",
            path.display()
        )),
    }
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

fn registered_entry(fanout: &FanoutStore, session_id: &str) -> Result<Entry, String> {
    store::with_lock_at(&fanout.root, || {
        resolve_entry(&store::read_registry_at(&fanout.root), session_id)
            .ok_or_else(|| "fanout parent is not a registered session".to_string())
    })
}

fn resolve_entry(registry: &store::Registry, session_id: &str) -> Option<Entry> {
    registry.agents.get(session_id).and_then(Entry::from_json)
}

fn record_by_runtime_session_id<'a>(
    records: &'a HashMap<String, FanoutRecord>,
    runtime_session_id: &str,
) -> Result<&'a FanoutRecord, String> {
    records
        .values()
        .find(|record| record.runtime_session_id.as_deref() == Some(runtime_session_id))
        .ok_or_else(|| "runtime session is not a fanout worker".to_string())
}

fn repo_identity(repo: &Path) -> Result<RepoIdentity, String> {
    let common = run_git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = fs::canonicalize(&common)
        .map_err(|error| format!("resolve git common dir {common}: {error}"))?;
    let metadata = fs::metadata(&common)
        .map_err(|error| format!("stat git common dir {}: {error}", common.display()))?;
    Ok(RepoIdentity {
        common_dir: common.to_string_lossy().into_owned(),
        dev: metadata.dev().to_string(),
        ino: metadata.ino().to_string(),
    })
}

fn ensure_worktree_root(path: &Path) -> Result<(), String> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err("fanout worktree root must not be a symlink".to_string());
    }
    fs::create_dir_all(path)
        .map_err(|error| format!("create fanout worktree root {}: {error}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("stat fanout worktree root {}: {error}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("fanout worktree root is not a real directory".to_string());
    }
    Ok(())
}

fn ensure_clean(repo: &Path, label: &str) -> Result<(), String> {
    let status = run_git(repo, &["status", "--porcelain"])?;
    if status.is_empty() {
        Ok(())
    } else {
        Err(format!("{label} is dirty"))
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("run git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map(|output| output.trim().to_string())
        .map_err(|_| "git output was not UTF-8".to_string())
}

fn git_tracks_worktree(repo: &Path, worktree: &Path) -> Result<bool, String> {
    let expected = worktree.to_string_lossy();
    Ok(run_git(repo, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|registered| registered == expected))
}

fn validate_sha(value: &str) -> Result<(), String> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("git object id is not a full hexadecimal SHA-1".to_string())
    }
}

fn lifecycle_worker<'a>(
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
    atomic_write_private(&root.join(FANOUT_FILE), &text)
}

fn atomic_write_private(path: &Path, text: &str) -> Result<(), String> {
    let temp = PathBuf::from(format!(
        "{}.{}.{}.tmp",
        path.display(),
        std::process::id(),
        store::uuid_v4()
    ));
    let result = (|| {
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|error| format!("create {}: {error}", temp.display()))?;
        output
            .write_all(text.as_bytes())
            .map_err(|error| format!("write {}: {error}", temp.display()))?;
        output
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", temp.display()))?;
        fs::rename(&temp, path).map_err(|error| format!("rename {}: {error}", path.display()))?;
        fs::File::open(
            path.parent()
                .ok_or_else(|| "fanout authority has no parent".to_string())?,
        )
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync fanout authority parent: {error}"))
    })();
    if result.is_err() {
        fs::remove_file(temp).ok();
    }
    result
}

fn increment_record_version(record: &mut FanoutRecord) -> Result<(), String> {
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

fn optional_string(object: &HashMap<String, JsonValue>, key: &str) -> Option<String> {
    object.get(key)?.get::<String>().cloned()
}
