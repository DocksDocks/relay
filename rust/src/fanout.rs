//! Bounded worktree fan-out authority and explicit handback collection.
//!
//! Fan-out state is deliberately separate from `lifecycle-v1.json` so an
//! already-running older relay never encounters an unknown lifecycle key.
//! Both authorities share the store's kernel lock; cross-file ordering always
//! retains capacity on an interrupted write.

mod authority;
mod git;

pub use authority::{CollectionPhase, FanoutMode, FanoutRecord, FanoutState, FanoutStore};

use crate::lifecycle::{LifecycleStore, ProcessObservation, process_observation_is_live};
use crate::protocol::{TerminalStatus, WorkerResultV1};
use crate::store;
use crate::workspace::authority::{
    AuthorityRootProvider, AuthorityRoots, SystemAuthorityRootProvider, WorkspaceAuthority,
};
use crate::workspace::repository_gate::RepositoryGate;
use crate::workspace::schema::{ClosedJcs, ObjectFormat, parse_jcs};
use authority::{
    ReservationRequest, acquire_collection_lock, increment_record_version, lifecycle_worker,
    optional_string, record_by_runtime_session_id, registered_entry, resolve_entry,
    validated_worker_result,
};
use git::{
    PreparedMergeOutcome, add_worktree, canonicalize_repository, changed_paths, ensure_clean,
    ensure_worktree_root, merge_prepared_handback, remove_merged_worktree,
    remove_unstarted_worktree, repo_identity, repo_key_from_repo, repository_head,
    uncollected_commit_count, validate_sha,
};
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, open, openat, statat};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tinyjson::JsonValue;

fn acquire_legacy_gate(
    identity: &git::RepoIdentity,
) -> Result<(RepositoryGate, AuthorityRoots), String> {
    let roots = SystemAuthorityRootProvider.roots()?;
    WorkspaceAuthority::new(roots.clone())?;
    let gate = RepositoryGate::acquire(&roots, identity.workspace_identity())?;
    gate.refuse_legacy_if_managed(&roots, identity.workspace_identity())?;
    Ok((gate, roots))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FanoutGcReason {
    LegacyShape,
    RepositoryIdentityChanged,
    WorktreeChanged,
    UncollectedCommits,
    CommitInspectionFailed,
    WorktreeNotClean,
    RemovalFailed,
    WorktreesSurfaceUnavailable,
}

impl FanoutGcReason {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::LegacyShape => "legacy_shape",
            Self::RepositoryIdentityChanged => "repository_identity_changed",
            Self::WorktreeChanged => "worktree_changed",
            Self::UncollectedCommits => "uncollected_commits",
            Self::CommitInspectionFailed => "commit_inspection_failed",
            Self::WorktreeNotClean => "worktree_not_clean",
            Self::RemovalFailed => "removal_failed",
            Self::WorktreesSurfaceUnavailable => "worktrees_surface_unavailable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FanoutGcReportEntry {
    pub(crate) branch: Option<String>,
    pub(crate) reason: FanoutGcReason,
}

#[derive(Default)]
pub(crate) struct FanoutGcReport {
    pub(crate) removed_worktrees: usize,
    pub(crate) retained_branches: Vec<String>,
    pub(crate) entries: Vec<FanoutGcReportEntry>,
}

impl FanoutGcReport {
    pub(crate) fn worktrees_surface_unavailable() -> Self {
        Self {
            entries: vec![FanoutGcReportEntry {
                branch: None,
                reason: FanoutGcReason::WorktreesSurfaceUnavailable,
            }],
            ..Self::default()
        }
    }

    fn record(&mut self, outcome: FanoutGcOutcome) {
        match outcome {
            FanoutGcOutcome::Skipped => {}
            FanoutGcOutcome::Retained { branch, reason } => {
                self.retained_branches.push(branch.clone());
                self.entries.push(FanoutGcReportEntry {
                    branch: Some(branch),
                    reason,
                });
            }
            FanoutGcOutcome::Removed(branch) => {
                self.removed_worktrees += 1;
                self.retained_branches.push(branch);
            }
        }
    }
}

enum FanoutGcOutcome {
    Skipped,
    Retained {
        branch: String,
        reason: FanoutGcReason,
    },
    Removed(String),
}

impl FanoutGcOutcome {
    fn retained(branch: String, reason: FanoutGcReason) -> Self {
        Self::Retained { branch, reason }
    }
}

fn commit_inspection_reason(uncollected_commits: Result<u64, String>) -> Option<FanoutGcReason> {
    match uncollected_commits {
        Err(_) => Some(FanoutGcReason::CommitInspectionFailed),
        Ok(count) if count != 0 => Some(FanoutGcReason::UncollectedCommits),
        Ok(_) => None,
    }
}

fn cleanliness_reason(cleanliness: Result<(), String>) -> Option<FanoutGcReason> {
    cleanliness
        .is_err()
        .then_some(FanoutGcReason::WorktreeNotClean)
}

fn removal_outcome(branch: String, removal: Result<(), String>) -> FanoutGcOutcome {
    match removal {
        Ok(()) => FanoutGcOutcome::Removed(branch),
        Err(_) => FanoutGcOutcome::retained(branch, FanoutGcReason::RemovalFailed),
    }
}

fn changed_candidate_reason(
    repository_identity_matches: bool,
    worktree_snapshot_matches: Option<bool>,
) -> Option<FanoutGcReason> {
    if !repository_identity_matches {
        Some(FanoutGcReason::RepositoryIdentityChanged)
    } else if worktree_snapshot_matches == Some(false) {
        Some(FanoutGcReason::WorktreeChanged)
    } else {
        None
    }
}

fn legacy_shape_reason(components: &[String]) -> Option<FanoutGcReason> {
    (components.len() == 1).then_some(FanoutGcReason::LegacyShape)
}

// `FanoutRecord` dwarfs the other variants (≈968 vs 24 bytes), so the candidate
// payload is boxed: every `Skipped`/`Retained` in the reap loop would otherwise
// carry the full record's footprint.
struct FanoutGcCandidate {
    record: FanoutRecord,
    worktree_snapshot: WorktreeSnapshot,
}

enum FanoutGcDecision {
    Skipped,
    Retained {
        branch: String,
        reason: FanoutGcReason,
    },
    Candidate(Box<FanoutGcCandidate>),
}

impl FanoutGcDecision {
    fn retained(branch: String, reason: FanoutGcReason) -> Self {
        Self::Retained { branch, reason }
    }
}

struct ReapContext<'a> {
    fanout: &'a FanoutStore,
    worktrees: &'a store::GcSurfaceDir,
    identity: &'a git::RepoIdentity,
    cutoff: SystemTime,
}

#[derive(Clone, Copy)]
struct WorktreeSnapshot {
    dev: i128,
    ino: i128,
    mtime: i128,
    mtime_nsec: i128,
}

impl WorktreeSnapshot {
    fn is_old(self, cutoff: SystemTime) -> bool {
        let cutoff = cutoff.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        let cutoff_secs = i128::from(cutoff.as_secs());
        self.mtime < cutoff_secs
            || (self.mtime == cutoff_secs && self.mtime_nsec <= i128::from(cutoff.subsec_nanos()))
    }
}

// Every rejection carries its own message so each rule is separately observable:
// several inputs trip more than one rule, so a boolean assertion would survive
// deleting the rule under test.
const WORKTREE_PATH_OUTSIDE: &str = "fanout worktree path is outside the worktrees root";
const WORKTREE_PATH_EMPTY: &str = "fanout worktree path has an empty component";
const WORKTREE_PATH_RELATIVE: &str = "fanout worktree path has a relative component";
const WORKTREE_PATH_CHARSET: &str = "fanout worktree path component is not permitted";
const WORKTREE_PATH_DEPTH: &str = "fanout worktree path depth is out of range";
const WORKTREE_PATH_LEAF: &str = "fanout worktree path leaf is not the reservation id";
const WORKTREE_PATH_ABSENT: &str = "fanout worktree path is absent";
const WORKTREE_PATH_NOT_DIRECTORY: &str =
    "fanout worktree path component is a symlink or not a directory";

/// Split the persisted worktree path into its components below `worktrees_root`.
///
/// The prefix test is `Path::strip_prefix`, never `str::starts_with`: a textual
/// prefix would admit `<root>/worktreesEVIL/<id>`, which shares the prefix but is a
/// different directory. `worktrees_root` is derived from the store root by the
/// caller and never read from the record, which is what makes it a boundary.
///
/// Accepts one component (the legacy flat shape) through three (`owner/repo` plus
/// the reservation), because the derivation emits at most two key segments.
fn worktree_path_components(
    worktrees_root: &Path,
    persisted: &str,
    reservation_id: &str,
) -> Result<Vec<String>, String> {
    let relative = Path::new(persisted)
        .strip_prefix(worktrees_root)
        .map_err(|_| WORKTREE_PATH_OUTSIDE.to_string())?;
    let raw = relative
        .to_str()
        .ok_or_else(|| WORKTREE_PATH_OUTSIDE.to_string())?;
    // Split manually rather than through `Path::components`, which silently
    // normalises empty components away and would make that rule unobservable.
    let parts: Vec<&str> = raw.split('/').collect();
    for part in &parts {
        if part.is_empty() {
            return Err(WORKTREE_PATH_EMPTY.to_string());
        }
        if *part == "." || *part == ".." {
            return Err(WORKTREE_PATH_RELATIVE.to_string());
        }
        if !part.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'.'
                || byte == b'-'
                || byte == b'_'
        }) {
            return Err(WORKTREE_PATH_CHARSET.to_string());
        }
    }
    if parts.is_empty() || parts.len() > 3 {
        return Err(WORKTREE_PATH_DEPTH.to_string());
    }
    if parts[parts.len() - 1] != reservation_id {
        return Err(WORKTREE_PATH_LEAF.to_string());
    }
    Ok(parts.into_iter().map(str::to_string).collect())
}

/// Walk the validated components from the `worktrees` directory descriptor,
/// refusing a symlink at EVERY component rather than only the leaf.
///
/// `Ok(None)` means absent, which the snapshot helpers surface as "no worktree";
/// a symlinked or non-directory component is an error, never an absence.
fn resolve_worktree_leaf(
    worktrees_fd: BorrowedFd<'_>,
    components: &[String],
) -> Result<Option<rustix::fs::Stat>, String> {
    let (leaf, parents) = components
        .split_last()
        .ok_or_else(|| WORKTREE_PATH_DEPTH.to_string())?;
    let mut held: Option<OwnedFd> = None;
    for parent in parents {
        let at = held.as_ref().map_or(worktrees_fd, |fd| fd.as_fd());
        match openat(
            at,
            parent.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => held = Some(fd),
            Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
            Err(error)
                if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR =>
            {
                return Err(WORKTREE_PATH_NOT_DIRECTORY.to_string());
            }
            Err(error) => {
                return Err(format!("open fanout worktree component {parent}: {error}"));
            }
        }
    }
    let at = held.as_ref().map_or(worktrees_fd, |fd| fd.as_fd());
    match statat(at, leaf.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
        Err(error) => Err(format!("stat fanout worktree {leaf}: {error}")),
    }
}

/// Validate a record-supplied worktree path and resolve it symlink-safely.
///
/// This replaces the exact-equality check the flat layout allowed: once the path is
/// repo-keyed there is no independently recomputable expectation, so containment
/// comes from the `worktrees_root` prefix plus a bounded per-component walk. It is
/// a defence-in-depth layer, not the primary gate - `repo_identity(..)
/// .matches_record(..)` remains that and is untouched.
pub fn resolve_worktree_path(
    worktrees_root: &Path,
    worktrees_fd: BorrowedFd<'_>,
    persisted: &str,
    reservation_id: &str,
) -> Result<PathBuf, String> {
    let components = worktree_path_components(worktrees_root, persisted, reservation_id)?;
    if resolve_worktree_leaf(worktrees_fd, &components)?.is_none() {
        return Err(WORKTREE_PATH_ABSENT.to_string());
    }
    let mut resolved = worktrees_root.to_path_buf();
    for component in &components {
        resolved.push(component);
    }
    Ok(resolved)
}

// Both helpers resolve the record's persisted path through step 1's primitives
// rather than recomputing a flat one. They deliberately use
// `worktree_path_components` + `resolve_worktree_leaf` and NOT
// `resolve_worktree_path`: the composed form collapses absence into an error,
// while these two must keep absence and structural failure distinct. Reporting a
// malformed path as "no worktree" would silently widen what the reaper deletes.
fn old_worktree_snapshot(
    worktrees_root: &Path,
    worktrees: &store::GcSurfaceDir,
    persisted: &str,
    reservation_id: &str,
    cutoff: SystemTime,
) -> Result<Option<WorktreeSnapshot>, String> {
    let components = worktree_path_components(worktrees_root, persisted, reservation_id)?;
    let Some(stat) = resolve_worktree_leaf(worktrees.fd.as_fd(), &components)? else {
        return Ok(None);
    };
    if !FileType::from_raw_mode(stat.st_mode).is_dir() {
        return Ok(None);
    }
    let snapshot = WorktreeSnapshot {
        dev: i128::from(stat.st_dev),
        ino: i128::from(stat.st_ino),
        mtime: i128::from(stat.st_mtime),
        mtime_nsec: i128::from(stat.st_mtime_nsec),
    };
    Ok(snapshot.is_old(cutoff).then_some(snapshot))
}

fn worktree_matches_snapshot(
    worktrees_root: &Path,
    worktrees: &store::GcSurfaceDir,
    persisted: &str,
    reservation_id: &str,
    snapshot: WorktreeSnapshot,
) -> Result<bool, String> {
    let components = worktree_path_components(worktrees_root, persisted, reservation_id)?;
    let Some(stat) = resolve_worktree_leaf(worktrees.fd.as_fd(), &components)? else {
        return Ok(false);
    };
    Ok(FileType::from_raw_mode(stat.st_mode).is_dir()
        && i128::from(stat.st_dev) == snapshot.dev
        && i128::from(stat.st_ino) == snapshot.ino
        && i128::from(stat.st_mtime) == snapshot.mtime
        && i128::from(stat.st_mtime_nsec) == snapshot.mtime_nsec)
}

// Liveness gates worktree DELETION, so it must not drift from the rest of the
// crate: the start-token comparison inside `process_observation_is_live` is the
// anti-pid-recycling guard, and a divergent copy that reported a live worker as
// dead would delete that worker's worktree. Reuse the lifecycle adapter rather
// than re-parsing the same JSON shape here.
fn process_value_is_live(value: &JsonValue) -> Option<bool> {
    Some(process_observation_is_live(&ProcessObservation::from_json(
        value,
    )?))
}

fn operation_may_have_live_process(
    operation: &JsonValue,
    worker_id: &str,
    generation: &str,
) -> bool {
    let Some(operation) = operation.get::<HashMap<String, JsonValue>>() else {
        return true;
    };
    if optional_string(operation, "worker_id").as_deref() != Some(worker_id) {
        return false;
    }
    if optional_string(operation, "generation").as_deref() != Some(generation) {
        return false;
    }
    let Some(custody) = operation
        .get("custody")
        .and_then(JsonValue::get::<HashMap<String, JsonValue>>)
    else {
        return true;
    };
    match optional_string(custody, "kind").as_deref() {
        Some("None" | "ChildReaped") => false,
        Some("ChildStarting") => true,
        Some("ChildOwned" | "ChildCancelRequested") => custody
            .get("process")
            .and_then(process_value_is_live)
            .unwrap_or(true),
        Some("LostAuthority") => {
            // Unknown liveness must retain the worktree; treating lost custody
            // as dead could delete a child that is still running.
            custody
                .get("last_observation")
                .filter(|value| value.get::<()>().is_none())
                .map(|value| process_value_is_live(value).unwrap_or(true))
                .unwrap_or(true)
        }
        _ => true,
    }
}

fn worker_process_is_present(
    record: &FanoutRecord,
    lifecycle: &HashMap<String, JsonValue>,
) -> bool {
    let (Some(worker_id), Some(generation)) =
        (record.worker_id.as_deref(), record.generation.as_deref())
    else {
        return false;
    };
    let Some(operations) = lifecycle
        .get("active_operations")
        .and_then(JsonValue::get::<HashMap<String, JsonValue>>)
    else {
        return false;
    };
    operations
        .values()
        .any(|operation| operation_may_have_live_process(operation, worker_id, generation))
}

fn abandoned_worktree_decision(
    ctx: &ReapContext<'_>,
    reservation_id: &str,
    expected_record: &FanoutRecord,
    expected_worktree_snapshot: Option<WorktreeSnapshot>,
    records: &HashMap<String, FanoutRecord>,
    lifecycle: &HashMap<String, JsonValue>,
) -> Result<FanoutGcDecision, String> {
    let Some(current) = records.get(reservation_id) else {
        return Ok(FanoutGcDecision::Skipped);
    };
    let worktrees_root = ctx.fanout.root().join("worktrees");
    // A structurally invalid persisted path is skipped, exactly as a flat-path
    // mismatch was skipped before. This check precedes the snapshot lookup on
    // purpose: `old_worktree_snapshot` reports a malformed path as `Err`, and
    // reaching it first would turn today's skip into a hard failure.
    let components =
        match worktree_path_components(&worktrees_root, &current.worktree, &current.reservation_id)
        {
            Ok(components) => components,
            Err(_) => return Ok(FanoutGcDecision::Skipped),
        };
    let branch = current.branch.clone();
    if let Some(reason) = legacy_shape_reason(&components) {
        return Ok(FanoutGcDecision::retained(branch, reason));
    }
    let worktree_snapshot = match expected_worktree_snapshot {
        Some(snapshot) => snapshot,
        None => {
            let Some(snapshot) = old_worktree_snapshot(
                &worktrees_root,
                ctx.worktrees,
                &current.worktree,
                reservation_id,
                ctx.cutoff,
            )?
            else {
                return Ok(FanoutGcDecision::Skipped);
            };
            snapshot
        }
    };
    if current != expected_record
        || current.collection_phase.is_some()
        || current.state == FanoutState::Collected
        || worker_process_is_present(current, lifecycle)
    {
        return Ok(FanoutGcDecision::Skipped);
    }

    // Branch refs are never deleted by GC. Reporting the ref happens for every
    // old, process-free reservation whether its worktree is removed or retained.
    let worktree_snapshot_matches = if expected_worktree_snapshot.is_some() {
        Some(worktree_matches_snapshot(
            &worktrees_root,
            ctx.worktrees,
            &current.worktree,
            reservation_id,
            worktree_snapshot,
        )?)
    } else {
        None
    };
    if let Some(reason) = changed_candidate_reason(
        ctx.identity.matches_record(current),
        worktree_snapshot_matches,
    ) {
        return Ok(FanoutGcDecision::retained(branch, reason));
    }
    Ok(FanoutGcDecision::Candidate(Box::new(FanoutGcCandidate {
        record: current.clone(),
        worktree_snapshot,
    })))
}

pub(crate) fn reap_abandoned_worktrees(
    fanout: &FanoutStore,
    worktrees: &store::GcSurfaceDir,
    cutoff: SystemTime,
) -> Result<FanoutGcReport, String> {
    let mut reservation_ids =
        fanout.read_transaction(|records, _| Ok(records.keys().cloned().collect::<Vec<_>>()))?;
    reservation_ids.sort();
    let mut report = FanoutGcReport::default();

    for reservation_id in reservation_ids {
        let Some(snapshot) = fanout.read(&reservation_id)? else {
            continue;
        };
        let worktrees_root = fanout.root().join("worktrees");
        if snapshot.collection_phase.is_some() || snapshot.state == FanoutState::Collected {
            continue;
        }
        let components = match worktree_path_components(
            &worktrees_root,
            &snapshot.worktree,
            &snapshot.reservation_id,
        ) {
            Ok(components) => components,
            Err(_) => continue,
        };
        if old_worktree_snapshot(
            &worktrees_root,
            worktrees,
            &snapshot.worktree,
            &reservation_id,
            cutoff,
        )?
        .is_none()
        {
            continue;
        }
        if let Some(reason) = legacy_shape_reason(&components) {
            report.record(FanoutGcOutcome::retained(snapshot.branch, reason));
            continue;
        }
        let _collection_lock = match acquire_collection_lock(fanout.root(), &reservation_id) {
            Ok(lock) => lock,
            Err(error) if error == "fanout collection already in progress" => continue,
            Err(error) => return Err(error),
        };
        let identity = match repo_identity(Path::new(&snapshot.worktree)) {
            Ok(identity) if identity.matches_record(&snapshot) => identity,
            Ok(_) | Err(_) => continue,
        };
        let Ok((_repository_gate, _roots)) = acquire_legacy_gate(&identity) else {
            continue;
        };
        let ctx = ReapContext {
            fanout,
            worktrees,
            identity: &identity,
            cutoff,
        };

        let decision = fanout.read_transaction(|records, lifecycle| {
            abandoned_worktree_decision(&ctx, &reservation_id, &snapshot, None, records, lifecycle)
        })?;
        let outcome = match decision {
            FanoutGcDecision::Skipped => FanoutGcOutcome::Skipped,
            FanoutGcDecision::Retained { branch, reason } => {
                FanoutGcOutcome::retained(branch, reason)
            }
            FanoutGcDecision::Candidate(candidate) => {
                let FanoutGcCandidate {
                    record,
                    worktree_snapshot,
                } = *candidate;
                let branch = record.branch.clone();
                if let Some(reason) = commit_inspection_reason(uncollected_commit_count(
                    Path::new(&record.worktree),
                    &record.base_sha,
                    &record.branch,
                )) {
                    FanoutGcOutcome::retained(branch, reason)
                } else if let Some(reason) = cleanliness_reason(ensure_clean(
                    Path::new(&record.worktree),
                    "abandoned fanout worktree",
                )) {
                    FanoutGcOutcome::retained(branch, reason)
                } else {
                    let revalidated = fanout.read_transaction(|records, lifecycle| {
                        abandoned_worktree_decision(
                            &ctx,
                            &reservation_id,
                            &record,
                            Some(worktree_snapshot),
                            records,
                            lifecycle,
                        )
                    })?;
                    match revalidated {
                        FanoutGcDecision::Skipped => FanoutGcOutcome::Skipped,
                        FanoutGcDecision::Retained { branch, reason } => {
                            FanoutGcOutcome::retained(branch, reason)
                        }
                        FanoutGcDecision::Candidate(candidate) => {
                            let record = candidate.record;
                            let branch = record.branch.clone();
                            removal_outcome(
                                branch,
                                remove_unstarted_worktree(
                                    Path::new(&record.repo_common_dir),
                                    Path::new(&record.worktree),
                                ),
                            )
                        }
                    }
                }
            }
        };

        report.record(outcome);
    }
    Ok(report)
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
    let repo = canonicalize_repository(repo)?;
    let target_identity = repo_identity(&repo)?;
    let parent = registered_entry(fanout, parent_session_id)?;
    let parent_dir = parent
        .dir
        .ok_or_else(|| "fanout parent has no registered directory".to_string())?;
    let parent_identity = repo_identity(Path::new(&parent_dir))?;
    if target_identity != parent_identity {
        return Err("fanout repository differs from the registered parent".to_string());
    }
    let (_repository_gate, _roots) = acquire_legacy_gate(&target_identity)?;
    let base_sha = repository_head(&repo)?;
    validate_sha(&base_sha, target_identity.object_format)?;
    let reservation_id = store::uuid_v4();
    let branch = format!("relay/fanout-{reservation_id}");
    let worktrees = fanout.root().join("worktrees");
    // Derived exactly once, before the reservation, so the record and the directory
    // that gets created cannot disagree. This runs while the repository gate is held
    // but outside the store transaction, which is deliberate: `repository_head`
    // above already runs git at this point, so no new lock is taken.
    let repo_key = repo_key_from_repo(&repo)?;
    ensure_worktree_root(&worktrees, &repo_key)?;
    let worktree = worktrees.join(&repo_key).join(&reservation_id);
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
    let add = add_worktree(&repo, &branch, &record.worktree, &base_sha);
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
    // The flat expectation this replaced was the only containment in front of the
    // `remove_unstarted_worktree` call below. It cannot become a `config.cwd`
    // comparison: both callers run inside `run_fanout_supervisor`, a separate
    // process whose whole config arrives on stdin with the `cwd` unvalidated, so
    // the caller would supply both sides of the equality. `worktrees_root` here is
    // derived from the store root instead, and never read from the record.
    let worktrees_root = fanout.root().join("worktrees");
    let worktrees_fd = open(
        &worktrees_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| format!("open fanout worktrees root: {error}"))?;
    resolve_worktree_path(
        &worktrees_root,
        worktrees_fd.as_fd(),
        &snapshot.worktree,
        reservation_id,
    )?;
    let worktree = Path::new(&snapshot.worktree);
    let worktree_identity = repo_identity(worktree)?;
    let (_repository_gate, _roots) = acquire_legacy_gate(&worktree_identity)?;
    ensure_clean(worktree, "unstarted fanout worktree")?;
    let head = repository_head(worktree)?;
    if head != snapshot.base_sha || !worktree_identity.matches_record(&snapshot) {
        return Err("unstarted fanout worktree changed before rollback".to_string());
    }
    let parent_dir = registered_entry(fanout, &snapshot.parent_session_id)?
        .dir
        .ok_or_else(|| "fanout parent has no registered directory".to_string())?;
    remove_unstarted_worktree(Path::new(&parent_dir), worktree)?;
    LifecycleStore::new(fanout.root().to_path_buf())
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
    let terminal_status = match status {
        "completed" => TerminalStatus::Completed,
        "failed" => TerminalStatus::Failed,
        _ => return Err("handback status must be completed|failed".to_string()),
    };
    if note.len() > 4096 || note.contains('\0') {
        return Err("handback note must be at most 4096 bytes without NUL".to_string());
    }
    let mut snapshot = fanout.read_transaction(|records, _| {
        let record = record_by_runtime_session_id(records, runtime_session_id)?;
        if record.state == FanoutState::HandedBack {
            if record.correlation_id.is_some() {
                let (result, _) = validated_worker_result(record)?;
                if result.status != terminal_status || result.summary != note {
                    return Err(
                        "correlation_conflict: fanout worker result is already immutable"
                            .to_string(),
                    );
                }
            }
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
    if snapshot.state == FanoutState::HandedBack {
        if snapshot.correlation_id.is_some() {
            return fanout.ensure_worker_result_enqueued(&snapshot.reservation_id);
        }
        return Ok(snapshot);
    }
    if snapshot.correlation_id.is_some() {
        snapshot = fanout.reconcile_fanout_claim(&snapshot.reservation_id)?;
    }
    let worktree = Path::new(&snapshot.worktree);
    let worktree_identity = repo_identity(worktree)?;
    if !worktree_identity.matches_record(&snapshot) {
        return Err("fanout handback repository identity changed".to_string());
    }
    let (_repository_gate, _roots) = acquire_legacy_gate(&worktree_identity)?;
    ensure_clean(worktree, "handback worktree")?;
    let head = repository_head(worktree)?;
    validate_sha(&head, worktree_identity.object_format)?;

    let worker_result = if let Some(correlation_id) = snapshot.correlation_id.as_ref() {
        let stored_format = snapshot
            .object_format
            .as_deref()
            .ok_or_else(|| "correlated fanout record has no object format".to_string())
            .and_then(ObjectFormat::parse)?;
        if stored_format != worktree_identity.object_format {
            return Err("fanout handback object format changed".to_string());
        }
        let result = WorkerResultV1 {
            schema: 1,
            result_id: store::uuid_v4(),
            correlation_id: correlation_id.clone(),
            reservation_id: snapshot.reservation_id.clone(),
            root_reservation_id: snapshot.root_reservation_id.clone(),
            parent_session_id: snapshot.parent_session_id.clone(),
            worker_id: snapshot
                .worker_id
                .clone()
                .ok_or_else(|| "fanout record has no managed worker".to_string())?,
            generation: snapshot
                .generation
                .clone()
                .ok_or_else(|| "fanout record has no managed generation".to_string())?,
            runtime_session_id: snapshot
                .runtime_session_id
                .clone()
                .ok_or_else(|| "fanout record has no runtime session".to_string())?,
            repo_common_dir: snapshot.repo_common_dir.clone(),
            repo_dev: snapshot.repo_dev.clone(),
            repo_ino: snapshot.repo_ino.clone(),
            object_format: stored_format,
            base_commit: snapshot.base_sha.clone(),
            handback_commit: head.clone(),
            status: terminal_status,
            summary: note.to_string(),
            changed_paths: changed_paths(worktree, &snapshot.base_sha, &head)?,
            created_at: store::iso_now(),
        };
        WorkerResultV1::from_jcs(parse_jcs(&result.canonical_bytes(), false)?)?;
        Some(result)
    } else {
        None
    };
    let worker_result_sha256 = worker_result.as_ref().map(WorkerResultV1::sha256);
    let handed = match fanout.transaction(|records, _, _| {
        let mut record = records
            .get(&snapshot.reservation_id)
            .cloned()
            .ok_or_else(|| "fanout reservation disappeared".to_string())?;
        if record.version != snapshot.version || record.state != FanoutState::Running {
            return Err("fanout handback authority changed".to_string());
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
        record.state = FanoutState::HandedBack;
        record.handback_head = Some(head.clone());
        record.handback_status = Some(status.to_string());
        record.handback_note = Some(note.to_string());
        record.worker_result = worker_result;
        record.worker_result_sha256 = worker_result_sha256;
        increment_record_version(&mut record)?;
        records.insert(record.reservation_id.clone(), record.clone());
        Ok(record)
    }) {
        Ok(record) => record,
        Err(error)
            if error == "fanout handback authority changed"
                && snapshot.correlation_id.is_some() =>
        {
            let current = fanout
                .read(&snapshot.reservation_id)?
                .ok_or_else(|| "fanout reservation disappeared".to_string())?;
            if current.state == FanoutState::HandedBack {
                let (result, _) = validated_worker_result(&current)?;
                if result.status == terminal_status
                    && result.summary == note
                    && result.handback_commit == head
                {
                    current
                } else {
                    return Err(
                        "correlation_conflict: fanout worker result is already immutable"
                            .to_string(),
                    );
                }
            } else {
                return Err(error);
            }
        }
        Err(error) => return Err(error),
    };
    if handed.correlation_id.is_some() {
        fanout.ensure_worker_result_enqueued(&handed.reservation_id)
    } else {
        Ok(handed)
    }
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
    let parent_before_lock = registered_entry(fanout, parent_session_id)?;
    let parent_dir_before_lock = PathBuf::from(
        parent_before_lock
            .dir
            .ok_or_else(|| "fanout collect parent has no directory".to_string())?,
    );
    let _collection_lock = acquire_collection_lock(fanout.root(), &reservation_id)?;
    let parent_identity = repo_identity(&parent_dir_before_lock)?;
    let (_repository_gate, _roots) = acquire_legacy_gate(&parent_identity)?;
    let mut validated_record = fanout
        .read(&reservation_id)?
        .ok_or_else(|| "fanout reservation disappeared".to_string())?;
    if validated_record.parent_session_id != parent_session_id
        || validated_record.runtime_session_id.as_deref() != Some(runtime_session_id)
    {
        return Err("fanout collect authority binding changed".to_string());
    }
    if validated_record.correlation_id.is_some() {
        validated_record = fanout.ensure_worker_result_enqueued(&reservation_id)?;
        validate_correlated_collection(
            &validated_record,
            &parent_dir_before_lock,
            &parent_identity,
        )?;
    }
    let mut record = fanout.transaction(|records, lifecycle, registry| {
        let mut record = record_by_runtime_session_id(records, runtime_session_id)?.clone();
        if record.parent_session_id != parent_session_id {
            return Err("fanout collect parent does not own this worker".to_string());
        }
        if validated_record.correlation_id.is_some() && record.version != validated_record.version {
            return Err("fanout collection authority changed after result validation".to_string());
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
        match merge_prepared_handback(&parent_dir, &worktree, &record)? {
            PreparedMergeOutcome::Merged => {}
            PreparedMergeOutcome::Aborted { merge_error } => {
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
        }
        record = advance_collection(fanout, &record, CollectionPhase::Merged)?;
    }
    if record.collection_phase == Some(CollectionPhase::Merged) {
        remove_merged_worktree(&parent_dir, &worktree, &record)?;
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
    if record.state != FanoutState::Collected {
        fanout_die("fanout collection did not reach Collected");
    }
    if args.has("result-json") {
        let result = record
            .worker_result
            .as_ref()
            .unwrap_or_else(|| fanout_die("fanout collection has no correlated worker result"));
        let digest = record
            .worker_result_sha256
            .as_deref()
            .unwrap_or_else(|| fanout_die("fanout collection has no worker result digest"));
        let result_json = String::from_utf8(result.canonical_bytes())
            .unwrap_or_else(|_| fanout_die("worker result canonical JSON is not UTF-8"));
        println!(r#"{{"result":{result_json},"sha256":"{digest}"}}"#);
    } else {
        println!(
            "collected {} into {}",
            runtime_session_id, parent_session_id
        );
    }
    std::process::exit(0);
}

fn fanout_die(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

fn validate_correlated_collection(
    record: &FanoutRecord,
    parent_dir: &Path,
    parent_identity: &git::RepoIdentity,
) -> Result<(), String> {
    let (result, _) = validated_worker_result(record)?;
    if !parent_identity.matches_record(record)
        || result.repo_common_dir != parent_identity.common_dir
        || result.repo_dev != parent_identity.dev
        || result.repo_ino != parent_identity.ino
        || result.object_format != parent_identity.object_format
    {
        return Err("worker result repository binding mismatch".to_string());
    }
    let actual_paths = changed_paths(parent_dir, &result.base_commit, &result.handback_commit)?;
    if actual_paths != result.changed_paths {
        return Err("worker result changed-path binding mismatch".to_string());
    }
    let worktree = Path::new(&record.worktree);
    let worktree_required = record.state != FanoutState::Collected
        && matches!(
            record.collection_phase,
            None | Some(CollectionPhase::Prepared)
        );
    if worktree_required && !worktree.is_dir() {
        return Err("fanout child worktree disappeared before collection".to_string());
    }
    if worktree.exists() {
        let worktree_identity = repo_identity(worktree)?;
        if !worktree_identity.matches_record(record)
            || worktree_identity.object_format != result.object_format
        {
            return Err("worker result repository binding mismatch".to_string());
        }
        let current_head = repository_head(worktree)?;
        if current_head != result.handback_commit {
            return Err(format!(
                "fanout child HEAD changed after handback; restore {} to {} before retrying collection",
                worktree.display(),
                result.handback_commit
            ));
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_report_keeps_reason_discriminants() {
        let protective_reasons = [
            legacy_shape_reason(&["reservation".to_string()]).expect("legacy shape"),
            changed_candidate_reason(false, None).expect("repository identity change"),
            changed_candidate_reason(true, Some(false)).expect("worktree snapshot change"),
            commit_inspection_reason(Ok(1)).expect("uncollected commit"),
            commit_inspection_reason(Err("inspect".into())).expect("commit inspection failure"),
            cleanliness_reason(Err("dirty".into())).expect("dirty worktree"),
        ];
        let mut report = FanoutGcReport::default();
        for (index, reason) in protective_reasons.into_iter().enumerate() {
            report.record(FanoutGcOutcome::retained(format!("branch-{index}"), reason));
        }

        assert_eq!(
            report
                .entries
                .iter()
                .map(|entry| entry.reason)
                .collect::<Vec<_>>(),
            vec![
                FanoutGcReason::LegacyShape,
                FanoutGcReason::RepositoryIdentityChanged,
                FanoutGcReason::WorktreeChanged,
                FanoutGcReason::UncollectedCommits,
                FanoutGcReason::CommitInspectionFailed,
                FanoutGcReason::WorktreeNotClean,
            ]
        );
    }

    #[test]
    fn removal_failure_has_distinct_report_reason() {
        let mut report = FanoutGcReport::default();
        report.record(removal_outcome(
            "relay/fanout-failed".into(),
            Err("git worktree remove failed".into()),
        ));

        assert_eq!(
            report.entries,
            vec![FanoutGcReportEntry {
                branch: Some("relay/fanout-failed".into()),
                reason: FanoutGcReason::RemovalFailed,
            }]
        );
        assert_eq!(report.removed_worktrees, 0);
    }
}
