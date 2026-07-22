//! Bounded worktree fan-out authority and explicit handback collection.
//!
//! Fan-out state is deliberately separate from `lifecycle-v1.json` so an
//! already-running older relay never encounters an unknown lifecycle key.
//! Both authorities share the store's kernel lock; cross-file ordering always
//! retains capacity on an interrupted write.

mod authority;
mod git;

pub use authority::{CollectionPhase, FanoutMode, FanoutRecord, FanoutState, FanoutStore};

use crate::lifecycle::LifecycleStore;
use crate::store;
use crate::workspace::authority::{
    AuthorityRootProvider, AuthorityRoots, SystemAuthorityRootProvider, WorkspaceAuthority,
};
use crate::workspace::repository_gate::RepositoryGate;
use authority::{
    ReservationRequest, acquire_collection_lock, increment_record_version, lifecycle_worker,
    optional_string, record_by_runtime_session_id, registered_entry, resolve_entry,
};
use git::{
    PreparedMergeOutcome, add_worktree, canonicalize_repository, ensure_clean,
    ensure_worktree_root, merge_prepared_handback, remove_merged_worktree,
    remove_unstarted_worktree, repo_identity, repository_head, validate_sha,
};
use std::path::{Path, PathBuf};

fn acquire_legacy_gate(
    identity: &git::RepoIdentity,
) -> Result<(RepositoryGate, AuthorityRoots), String> {
    let roots = SystemAuthorityRootProvider.roots()?;
    WorkspaceAuthority::new(roots.clone())?;
    let gate = RepositoryGate::acquire(&roots, identity.workspace_identity())?;
    gate.refuse_legacy_if_managed(&roots, identity.workspace_identity())?;
    Ok((gate, roots))
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
    let expected_worktree = fanout.root().join("worktrees").join(reservation_id);
    if Path::new(&snapshot.worktree) != expected_worktree {
        return Err("fanout worktree path is not the exact reserved path".to_string());
    }
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
    let worktree_identity = repo_identity(Path::new(&snapshot.worktree))?;
    let (_repository_gate, _roots) = acquire_legacy_gate(&worktree_identity)?;
    ensure_clean(Path::new(&snapshot.worktree), "handback worktree")?;
    let head = repository_head(Path::new(&snapshot.worktree))?;
    validate_sha(&head, worktree_identity.object_format)?;
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
    let parent_before_lock = registered_entry(fanout, parent_session_id)?;
    let parent_dir_before_lock = PathBuf::from(
        parent_before_lock
            .dir
            .ok_or_else(|| "fanout collect parent has no directory".to_string())?,
    );
    let _collection_lock = acquire_collection_lock(fanout.root(), &reservation_id)?;
    let parent_identity = repo_identity(&parent_dir_before_lock)?;
    let (_repository_gate, _roots) = acquire_legacy_gate(&parent_identity)?;
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
